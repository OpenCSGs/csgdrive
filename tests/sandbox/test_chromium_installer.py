"""验证 Chromium 安装包校验和安装权限。"""
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
import zipfile

spec = importlib.util.spec_from_file_location(
    'install_chromium', Path(__file__).resolve().parents[2] / 'build/sandbox/install-chromium.py')
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


class ChromiumInstallerTest(unittest.TestCase):
    def bundle(self, root, extra=None):
        archive = root / 'chromium.zip'
        with zipfile.ZipFile(archive, 'w') as output:
            for name in ('chrome', 'chrome_crashpad_handler', 'chrome_sandbox'):
                output.writestr('chrome-linux/' + name, b'fixture')
            if extra:
                output.writestr(extra, b'fixture')
        manifest = root / 'chromium.json'
        manifest.write_text(json.dumps({'revision': 'fixture',
                                       'sha256': hashlib.sha256(archive.read_bytes()).hexdigest()}))
        return archive, manifest

    def test_checksum_mismatch_leaves_destination_absent(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive, manifest = self.bundle(root)
            archive.write_bytes(archive.read_bytes() + b'corrupted')
            with self.assertRaises(ValueError):
                installer.install(archive, manifest, root / 'installed')
            self.assertFalse((root / 'installed').exists())

    def test_browser_and_sandbox_permissions_and_provenance(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive, manifest = self.bundle(root)
            destination = root / 'installed'
            installer.install(archive, manifest, destination)
            self.assertEqual((destination / 'chrome').stat().st_mode & 0o7777, 0o755)
            self.assertEqual((destination / 'chrome_sandbox').stat().st_mode & 0o7777, 0o4755)
            self.assertEqual(json.loads((destination / 'agentdrive-build.json').read_text()),
                             json.loads(manifest.read_text()))

    def test_archive_paths_cannot_escape_extraction_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive, manifest = self.bundle(root, '../outside')
            with self.assertRaises(ValueError):
                installer.install(archive, manifest, root / 'installed')
            self.assertFalse((root / 'outside').exists())


if __name__ == '__main__':
    unittest.main()
