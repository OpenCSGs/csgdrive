"""Check byte forwarding and connection failures in the stdio bridge."""
import socket
import subprocess
import sys
import tempfile
import threading
import unittest
from pathlib import Path

PROXY = Path(__file__).resolve().parents[2] / 'build/sandbox/mcp-proxy.py'


class ProxyTest(unittest.TestCase):
    def test_preserves_response_after_handshake_and_drains_after_stdin_eof(self):
        with tempfile.TemporaryDirectory() as directory:
            path = str(Path(directory) / 'vsock.sock')
            with socket.socket(socket.AF_UNIX) as server:
                server.bind(path)
                server.listen(1)
                observed = []
                request = b'{"jsonrpc":"2.0","id":1,"method":"tools/list"}\n' * 4000
                response = b'{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}\n'

                def guest():
                    with server.accept()[0] as connection:
                        handshake = bytearray()
                        while not handshake.endswith(b'\n'):
                            handshake.extend(connection.recv(1))
                        observed.append(bytes(handshake))
                        connection.sendall(b'OK 1234\n' + response)
                        data = bytearray()
                        while True:
                            chunk = connection.recv(65536)
                            if not chunk:
                                break
                            data.extend(chunk)
                        observed.append(bytes(data))
                        connection.sendall(response)

                thread = threading.Thread(target=guest, daemon=True)
                thread.start()
                result = subprocess.run([sys.executable, str(PROXY), '--socket', path],
                                        input=request, capture_output=True, timeout=10)
                thread.join(5)
                self.assertFalse(thread.is_alive())
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(observed, [b'CONNECT 7000\n', request])
                self.assertEqual(result.stdout, response * 2)

    def test_missing_instance_reports_to_stderr(self):
        with tempfile.TemporaryDirectory() as directory:
            result = subprocess.run([sys.executable, str(PROXY), '--socket',
                                     str(Path(directory) / 'missing.sock')],
                                    input=b'', capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, b'')
        self.assertIn(b'Ensure the Sandbox is running', result.stderr)


if __name__ == '__main__':
    unittest.main()
