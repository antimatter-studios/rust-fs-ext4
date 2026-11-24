#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
PYTHONDONTWRITEBYTECODE=1 python3 - <<'PY'
import importlib.util
import io
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
        body = b'\x7fELF\x02\x01' + bytes(12) + (183).to_bytes(2, 'little') + b'test'
        compiler = unittest.mock.Mock(return_value=({name}, [name]))
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / 'build'
            executable = source / name
            executable.parent.mkdir(parents=True)
            executable.write_bytes(body)
            archive = root / 'bundle.tar'
            with (patch.object(bundle, 'REPO', source), patch.object(bundle, 'CI_REPO', source),
                  patch.object(bundle, 'selections', return_value=[flags]),
                  patch.object(bundle, 'compile_targets', compiler)):
                bundle.build(arch, key, archive)
                compiler.assert_called_once_with(['test', '--no-run', *flags])
                bundle.verify(arch, key, archive)
                with self.assertRaisesRegex(ValueError, 'architecture or input key mismatch'):
                    bundle.verify(other, key, archive)
                with self.assertRaisesRegex(ValueError, 'architecture or input key mismatch'):
                    bundle.verify(arch, '2' * 64, archive)
                with self.assertRaises(FileNotFoundError):
                    bundle.verify(arch, key, root / 'not-cached.tar')
                destination = root / 'test'
                with patch.object(bundle, 'REPO', destination), patch.object(bundle, 'CI_REPO', destination):
                    bundle.verify(arch, key, archive, install=True)
                    self.assertEqual((destination / name).read_bytes(), body)
                    self.assertTrue(os.access(destination / name, os.X_OK))

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

    def test_test_dispatch_never_calls_cargo(self):
        manifest = {'arch': 'x86_64', 'invocations': [{'flags': ['--locked'], 'tests': ['target/probe']}]}
        with (patch.dict(os.environ, FS_EXT4_GUEST_ARCH='x86_64', FS_EXT4_GUEST_SSH='test-config'),
              patch.object(bundle.subprocess, 'run') as runner):
            runner.return_value.returncode = 0
            self.assertEqual(bundle.run(['--locked', '--', '--skip', 'needs_host::'], manifest), 0)
            runner.assert_called_once_with(['bash', bundle.REPO / 'scripts/ci-guest-run.sh',
                                            bundle.REPO / 'target/probe', '--skip', 'needs_host::'])
            with self.assertRaisesRegex(ValueError, 'refusing to compile'):
                bundle.run(['--release'], manifest)
            with patch.dict(os.environ, FS_EXT4_GUEST_ARCH='aarch64'):
                with self.assertRaisesRegex(ValueError, 'matching QEMU guest'):
                    bundle.run(['--locked'], manifest)


unittest.main()
PY
