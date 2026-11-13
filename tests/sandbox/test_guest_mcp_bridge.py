"""验证统一 MCP 使用标准管道传输并处理客户端 EOF。"""
import importlib.util
import json
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('guest_helper', Path(__file__).resolve().parents[2] / 'build/sandbox/guest-helper.py')
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)


class GuestMcpBridgeTest(unittest.TestCase):
    def test_client_half_close_receives_complete_response_and_releases_process(self):
        left, right = socket.socketpair()
        left.settimeout(5)
        original = subprocess.Popen
        children = []
        def launch(command, **kwargs):
            self.assertEqual(kwargs['stdin'], subprocess.PIPE)
            self.assertEqual(kwargs['stdout'], subprocess.PIPE)
            child = original([sys.executable, '-c', 'import sys,time;data=sys.stdin.buffer.read();time.sleep(0.05);sys.stdout.buffer.write(data)'], stdin=kwargs['stdin'], stdout=kwargs['stdout'], stderr=kwargs['stderr'])
            children.append(child)
            return child
        with tempfile.TemporaryFile() as log, patch.object(helper, 'open', return_value=log, create=True), patch.object(helper.subprocess, 'Popen', side_effect=launch):
            thread = threading.Thread(target=helper.mcp_connection, args=(right, {}))
            thread.start()
            payload = json.dumps({'id': 1, 'result': {'content': [{'type': 'text', 'text': '中文验证'}]}}).encode() + b'\n'
            left.sendall(payload)
            left.shutdown(socket.SHUT_WR)
            response = bytearray()
            while len(response) < len(payload):
                response.extend(left.recv(65536))
            thread.join(5)
            self.assertFalse(thread.is_alive())
            self.assertEqual(response, payload)
            self.assertIsNotNone(children[0].poll())
        left.close()
        right.close()


if __name__ == '__main__':
    unittest.main()
