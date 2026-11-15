"""验证两个内核版本的复制、校验记录和参数检查。"""
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[2] / 'sandbox/templates/install-kernels.sh'


class InstallKernelsTest(unittest.TestCase):
    def test_copies_both_variants_and_records_checksums(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output = root / 'output'
            output.mkdir()
            for name in ['vmlinux-bm', 'vmlinux-pvm']:
                (root / name).write_bytes(name.encode())
            subprocess.run(['bash', str(SCRIPT), str(root / 'vmlinux-bm'), str(output)], check=True)
            manifest = json.loads((output / 'kernels.json').read_text())
            for mode, name in [('kvm', 'vmlinux-bm'), ('pvm', 'vmlinux-pvm')]:
                self.assertEqual((output / name).read_bytes(), name.encode())
                self.assertEqual(manifest[mode], {
                    'file': name, 'sha256': hashlib.sha256(name.encode()).hexdigest(),
                })
            self.assertEqual((output / 'vmlinux').read_bytes(), b'vmlinux-bm')

    def test_kvm_only_and_invalid_explicit_pvm(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output = root / 'output'
            output.mkdir()
            kernel = root / 'vmlinux'
            kernel.write_bytes(b'kvm')
            command = ['bash', str(SCRIPT), str(kernel), str(output)]
            subprocess.run(command, check=True)
            self.assertEqual(set(json.loads((output / 'kernels.json').read_text())), {'kvm'})
            result = subprocess.run(command + [str(root / 'missing')], capture_output=True)
            self.assertNotEqual(result.returncode, 0)

    def test_rejects_pvm_as_kvm(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            pvm = root / 'vmlinux-pvm'
            pvm.write_bytes(b'pvm')
            result = subprocess.run(['bash', str(SCRIPT), str(pvm), str(root)], capture_output=True)
            self.assertNotEqual(result.returncode, 0)


if __name__ == '__main__':
    unittest.main()
