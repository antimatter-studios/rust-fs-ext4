#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
PYTHONDONTWRITEBYTECODE=1 python3 - <<'PY'
import importlib.util
import io
import json
import os
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('bundle', 'scripts/ci-test-bundle.py')
bundle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bundle)


class BundleTest(unittest.TestCase):
    def test_key_changes_with_build_inputs(self):
        image = 'ghcr.io/example/test-builder@sha256:'
        with patch.object(bundle, 'command', side_effect=['tree', 'core', 'harness'] * 3):
            original = bundle.identity('x86_64', image + '1' * 64)
            self.assertNotEqual(original, bundle.identity('aarch64', image + '1' * 64))
            self.assertNotEqual(original, bundle.identity('x86_64', image + '2' * 64))
        with patch.object(bundle, 'command', side_effect=['new tree', 'core', 'harness']):
            self.assertNotEqual(original, bundle.identity('x86_64', image + '1' * 64))
        with self.assertRaisesRegex(ValueError, 'immutable'):
            bundle.identity('x86_64', 'ghcr.io/example/test-builder:latest')

    def test_seal_restore_and_reject_wrong_bundle(self):
        arch, other = 'aarch64', 'x86_64'
        key = '1' * 64
        flags = ('--locked', '--lib')
        name = 'target/aarch64-unknown-linux-gnu/debug/deps/probe'
        cli = 'target/aarch64-unknown-linux-gnu/release/rust-fs-ext4'
        body = b'\x7fELF\x02\x01' + bytes(12) + (183).to_bytes(2, 'little') + b'test'
        compiler = unittest.mock.Mock(side_effect=[({name}, [name]), ({cli}, [])])
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / 'build'
            executable = source / name
            executable.parent.mkdir(parents=True)
            executable.write_bytes(body)
            (source / cli).parent.mkdir(parents=True)
            (source / cli).write_bytes(body + b'cli')
            archive = root / 'bundle.tar'
            with (patch.object(bundle, 'REPO', source), patch.object(bundle, 'CI_REPO', source),
                  patch.object(bundle, 'selections', return_value=[flags]),
                  patch.object(bundle, 'compile_targets', compiler),
                  patch.object(bundle, 'cargo_metadata', return_value={'packages': ['probe']})):
                bundle.build(arch, key, archive)
                self.assertEqual(compiler.call_args_list, [
                    unittest.mock.call(['test', '--no-run', *flags]),
                    unittest.mock.call(['build', '--locked', '--release', '--bin', 'rust-fs-ext4']),
                ])
                bundle.verify(arch, key, archive)
                with self.assertRaisesRegex(ValueError, 'architecture or input key mismatch'):
                    bundle.verify(other, key, archive)
                with self.assertRaisesRegex(ValueError, 'architecture or input key mismatch'):
                    bundle.verify(arch, '2' * 64, archive)
                with self.assertRaisesRegex(ValueError, 'scope'):
                    bundle.verify(arch, key, archive, scope='local')
                with self.assertRaises(FileNotFoundError):
                    bundle.verify(arch, key, root / 'not-cached.tar')
                destination = root / 'test'
                with patch.object(bundle, 'REPO', destination), patch.object(bundle, 'CI_REPO', destination):
                    bundle.verify(arch, key, archive, install=True)
                    self.assertEqual((destination / name).read_bytes(), body)
                    self.assertTrue(os.access(destination / name, os.X_OK))
                    self.assertEqual((destination / cli).read_bytes(), body + b'cli')
                    self.assertTrue(os.access(destination / cli, os.X_OK))
                    manifest = json.loads((destination / 'target/ci-test-manifest.json').read_text())
                    self.assertEqual(manifest['cli'], cli)
                    # The guest has no Cargo: what the build's Cargo said is installed.
                    self.assertEqual(json.loads((destination / 'target/cargo-metadata.json').read_text()),
                                     {'packages': ['probe']})

                altered = root / 'altered.tar'
                with tarfile.open(archive) as original, tarfile.open(altered, 'w') as corrupted:
                    for member in original.getmembers():
                        with original.extractfile(member) as stream:
                            data = stream.read()
                        if member.name == name:
                            data = data[:-1] + b'!'
                        corrupted.addfile(member, io.BytesIO(data))
                with self.assertRaisesRegex(ValueError, 'checksum mismatch'):
                    bundle.verify(arch, key, altered)

    def test_missing_cli_is_rejected_for_each_architecture(self):
        flags = ('--locked', '--lib')
        for arch, machine in bundle.MACHINES.items():
            with self.subTest(arch=arch), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                name = f'target/{arch}-unknown-linux-gnu/debug/deps/probe'
                body = b'\x7fELF\x02\x01' + bytes(12) + machine.to_bytes(2, 'little')
                executable = root / 'probe'
                executable.write_bytes(body)
                manifest = {'version': bundle.VERSION, 'arch': arch, 'key': '1' * 64,
                            'cli': None, 'files': {name: bundle.digest(executable)},
                            'invocations': [{'flags': flags, 'tests': [name]}]}
                record = root / 'manifest.json'
                record.write_text(json.dumps(manifest))
                archive = root / 'bundle.tar'
                with tarfile.open(archive, 'w') as output:
                    output.add(executable, arcname=name)
                    output.add(record, arcname='manifest.json')
                with (patch.object(bundle, 'selections', return_value=[flags]),
                      self.assertRaisesRegex(ValueError, 'required test selections')):
                    bundle.verify(arch, '1' * 64, archive)

    def test_test_dispatch_never_calls_cargo(self):
        manifest = {'arch': 'x86_64', 'invocations': [{'flags': ['--locked'], 'tests': ['target/probe']}]}
        with (patch.dict(os.environ, FLTH_GUEST='1'),
              patch.object(bundle.platform, 'machine', return_value='x86_64'),
              patch.object(bundle.subprocess, 'run') as runner):
            runner.return_value.returncode = 0
            self.assertEqual(bundle.run(['--locked', '--', '--skip', 'needs_host::'], manifest), 0)
            runner.assert_called_once_with([bundle.REPO / 'target/probe', '--skip', 'needs_host::'],
                                           cwd=bundle.REPO)
            with self.assertRaisesRegex(ValueError, 'refusing to compile'):
                bundle.run(['--release'], manifest)
        with patch.dict(os.environ, {}, clear=False):
            os.environ.pop('FLTH_GUEST', None)
            with self.assertRaisesRegex(ValueError, 'harness guest of their architecture'):
                bundle.run(['--locked'], manifest)

    def test_local_scope_is_the_whole_suite_in_one_invocation(self):
        for arch in bundle.MACHINES:
            with self.subTest(arch=arch):
                self.assertEqual(list(bundle.selections(arch, 'local')),
                                 [('--locked', '--profile', 'test-opt')])
        with self.assertRaisesRegex(ValueError, 'invalid bundle architecture, key or scope'):
            bundle.main('verify', 'aarch64', '1' * 64, 'bundle.tar', 'nightly')

    def test_in_the_guest_the_bundle_runs_its_executables_directly(self):
        manifest = {'arch': 'aarch64', 'invocations': [{'flags': ['--locked'], 'tests': ['target/probe']}]}
        with (patch.dict(os.environ, FLTH_GUEST='1'),
              patch.object(bundle.platform, 'machine', return_value='aarch64'),
              patch.object(bundle.subprocess, 'run') as runner):
            runner.return_value.returncode = 0
            self.assertEqual(bundle.run(['--locked', '--', 'filter'], manifest), 0)
            runner.assert_called_once_with([bundle.REPO / 'target/probe', 'filter'], cwd=bundle.REPO)
        with (patch.dict(os.environ, FLTH_GUEST='1'),
              patch.object(bundle.platform, 'machine', return_value='x86_64')):
            with self.assertRaisesRegex(ValueError, 'harness guest of their architecture'):
                bundle.run(['--locked'], manifest)


unittest.main()
PY
