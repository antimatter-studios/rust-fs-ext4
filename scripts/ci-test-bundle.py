#!/usr/bin/env python3
import hashlib
import json
import os
import platform
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile


REPO = Path(__file__).resolve().parent.parent
CI_REPO = Path('/opt/ext4-ci/rust-fs-ext4')
MACHINES = {'x86_64': 62, 'aarch64': 183}
VERSION = 1
# ci: the tiers the CI jobs split between them. local: the whole suite in one
# invocation, which is what `chore test:vm` runs in the harness guest.
SCOPES = ('ci', 'local')


def command(*args):
    return subprocess.check_output(args, cwd=REPO, text=True).strip()


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def identity(arch, image):
    if arch not in MACHINES or not re.fullmatch(r'ghcr\.io/[^@]+@sha256:[a-f0-9]{64}', image):
        raise ValueError('expected a supported guest architecture and an immutable builder image')
    inputs = [str(VERSION), arch, image, command('git', 'rev-parse', 'HEAD^{tree}')]
    for sibling in ('rust-fs-core', 'fs-linux-test-harness'):
        inputs.append(command('git', '-C', f'../{sibling}', 'rev-parse', 'HEAD'))
    return hashlib.sha256('\n'.join(inputs).encode()).hexdigest()


def split_args(args):
    if '--' not in args:
        return tuple(args), []
    boundary = args.index('--')
    return tuple(args[:boundary]), args[boundary + 1:]


def selections(arch, scope='ci'):
    options = ['--locked', '--profile', 'test-opt']
    if scope == 'local':
        yield tuple(options)
        return
    # One selection per tier, so CI tests one tier per job. The oracle,
    # kernel and lwext4 tiers read the same bytes on every architecture and
    # run on x86_64 only.
    yield ('--locked', *split_args(command('bash', 'scripts/test-targets.sh', 'unit').splitlines())[0])
    tiers = ('images',) if arch == 'aarch64' else ('images', 'oracle', 'kernel', 'lwext4')
    for tier in tiers:
        targets = command('bash', 'scripts/test-targets.sh', tier).splitlines()
        if not targets:
            raise ValueError(f'{tier} selected no targets')
        yield (*options, *targets)
    if arch == 'x86_64':
        # tests/scripts/test-scratch-left-behind.sh runs this one binary.
        yield (*options, '--test', 'repro_wants_dir_symlinks')


def check_elf(stream, arch, name):
    header = stream.read(20)
    if (header[:6] != b'\x7fELF\x02\x01'
            or int.from_bytes(header[18:20], 'little') != MACHINES[arch]):
        raise ValueError(f'{name} is not a Linux {arch} ELF executable')


def safe_path(name, arch):
    path = PurePosixPath(name)
    if (str(path) != name or path.parts[:2] != ('target', f'{arch}-unknown-linux-gnu')
            or '..' in path.parts or len(path.parts) not in (4, 5)):
        raise ValueError(f'invalid bundle executable path: {name}')
    return path


def compile_targets(args):
    executables, tests = set(), []
    with tempfile.TemporaryFile(mode='w+t') as records:
        subprocess.run(['cargo', *args, '--features', 'cli', '--message-format=json-render-diagnostics'],
                       cwd=REPO, stdout=records, check=True)
        records.seek(0)
        for line in records:
            event = json.loads(line)
            executable = event.get('executable')
            if event['reason'] != 'compiler-artifact' or not executable:
                continue
            name = Path(executable).relative_to(REPO).as_posix()
            executables.add(name)
            if event['profile']['test']:
                tests.append(name)
    return executables, tests


def build(arch, key, archive, scope='ci'):
    if REPO != CI_REPO:
        raise ValueError(f'build tests from {CI_REPO}, the path their binaries use in the guest')
    executables, invocations = set(), []
    for flags in selections(arch, scope):
        files, tests = compile_targets(['test', '--no-run', *flags])
        if not tests:
            raise ValueError(f'Cargo produced no test executables for {flags}')
        executables.update(files)
        invocations.append({'flags': flags, 'tests': tests})
    if arch == 'x86_64' or scope == 'local':
        docs = command('cargo', 'test', '--locked', '--features', 'cli', '--doc', '--', '--list')
        if re.search(r'\(line \d+\): test$|^[1-9]\d* tests?,', docs, re.MULTILINE):
            raise ValueError('there are executable doctests; add a guest handoff for them first')
    files, _ = compile_targets(['build', '--locked', '--release', '--bin', 'rust-fs-ext4'])
    cli = f'target/{arch}-unknown-linux-gnu/release/rust-fs-ext4'
    if cli not in files:
        raise ValueError('Cargo did not build the staged CLI')
    executables.update(files)
    manifest = {'version': VERSION, 'arch': arch, 'key': key, 'scope': scope,
                'invocations': invocations, 'cli': cli, 'files': {}}
    archive.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=archive.parent) as temporary:
        temporary = Path(temporary)
        with tarfile.open(temporary / 'bundle.tar', 'w', dereference=True) as bundle:
            for name in sorted(executables):
                path = REPO / safe_path(name, arch)
                with path.open('rb') as stream:
                    check_elf(stream, arch, name)
                manifest['files'][name] = digest(path)
                bundle.add(path, arcname=name)
            record = temporary / 'manifest.json'
            record.write_text(json.dumps(manifest))
            bundle.add(record, arcname='manifest.json')
        (temporary / 'bundle.tar').replace(archive)


def verify(arch, key, archive, install=False, scope='ci'):
    with tarfile.open(archive, 'r:') as bundle:
        members = bundle.getmembers()
        names = {member.name for member in members}
        if len(names) != len(members) or any(not member.isfile() for member in members):
            raise ValueError('bundle must contain distinct regular files only')
        with bundle.extractfile('manifest.json') as stream:
            manifest = json.load(stream)
        if ((manifest['version'], manifest['arch'], manifest['key'], manifest.get('scope', 'ci'))
                != (VERSION, arch, key, scope)):
            raise ValueError('bundle version, scope, architecture or input key mismatch')
        files = manifest['files']
        if names != {'manifest.json', *files}:
            raise ValueError('bundle contents do not match its manifest')
        invocations = {tuple(entry['flags']): entry['tests'] for entry in manifest['invocations']}
        if (set(invocations) != set(selections(arch, scope))
                or any(not tests or not set(tests) <= files.keys() for tests in invocations.values())
                or manifest['cli'] != f'target/{arch}-unknown-linux-gnu/release/rust-fs-ext4'
                or manifest['cli'] not in files):
            raise ValueError('bundle does not cover the required test selections')
        if install and (REPO != CI_REPO or (REPO / 'target').exists()):
            raise ValueError(f'install needs a fresh checkout at {CI_REPO} with no target directory')
        for name, checksum in files.items():
            path = safe_path(name, arch)
            with bundle.extractfile(name) as stream:
                check_elf(stream, arch, name)
                stream.seek(0)
                if hashlib.file_digest(stream, 'sha256').hexdigest() != checksum:
                    raise ValueError(f'checksum mismatch: {name}')
                if install:
                    stream.seek(0)
                    destination = REPO / path
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    with destination.open('xb') as output:
                        shutil.copyfileobj(stream, output)
                    destination.chmod(0o755)
        if install:
            (REPO / 'target/ci-test-manifest.json').write_text(json.dumps(manifest))
    print(f'{arch}: verified {len(files)} prebuilt executables ({key})')


def in_guest(manifest):
    # Already in the guest the bundle is for (the harness's guest-test): the
    # executables run here, as cargo would run them, from the package root.
    return os.environ.get('FLTH_GUEST') == '1' and platform.machine() == manifest['arch']


def run(args, manifest):
    flags, test_args = split_args(args)
    local = in_guest(manifest)
    if not local and (os.environ.get('FS_EXT4_GUEST_ARCH') != manifest['arch']
                      or not os.environ.get('FS_EXT4_GUEST_SSH')):
        raise ValueError('prebuilt tests require the matching QEMU guest')
    for invocation in manifest['invocations']:
        if tuple(invocation['flags']) == flags:
            for test in invocation['tests']:
                print(f'     Running {test}', flush=True)
                if local:
                    result = subprocess.run([REPO / test, *test_args], cwd=REPO)
                else:
                    result = subprocess.run(['bash', REPO / 'scripts/ci-guest-run.sh', REPO / test, *test_args])
                if result.returncode:
                    return result.returncode if result.returncode > 0 else 128 - result.returncode
            return 0
    raise ValueError(f'no prebuilts for {flags}; refusing to compile in the test job')


def main(mode, *args):
    if mode == 'key':
        print(identity(*args))
    elif mode in {'build', 'verify', 'install'}:
        arch, key, filename, *rest = args
        scope = rest[0] if rest else 'ci'
        if (arch not in MACHINES or not re.fullmatch(r'[a-f0-9]{64}', key)
                or scope not in SCOPES or len(rest) > 1):
            raise ValueError('invalid bundle architecture, key or scope')
        if mode == 'build':
            build(arch, key, Path(filename).resolve(), scope)
        else:
            verify(arch, key, Path(filename), install=mode == 'install', scope=scope)
    elif mode in {'run', 'cli'}:
        manifest = json.loads((REPO / 'target/ci-test-manifest.json').read_text())
        if mode == 'cli' and manifest['cli']:
            print(REPO / manifest['cli'])
        elif mode == 'run':
            return run(args, manifest)
        else:
            raise ValueError('no prebuilt release CLI for this job')
    else:
        raise ValueError(f'unknown bundle command: {mode}')
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main(*sys.argv[1:]))
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError, tarfile.TarError) as error:
        sys.exit(f'ci-test-bundle: {error}')
