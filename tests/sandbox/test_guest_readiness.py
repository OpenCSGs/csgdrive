"""验证启动只等待桌面连接和当前分支目录。"""
import importlib.util
import json
from pathlib import Path
import socket
import subprocess
import unittest
from unittest.mock import MagicMock, patch

spec = importlib.util.spec_from_file_location('guest_helper', Path(__file__).resolve().parents[2] / 'build/sandbox/guest-helper.py')
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)


class GuestReadinessTest(unittest.TestCase):
    def probe(self, mounted=True, manager=True, banner=b'RFB 003.008\n', connection_error=False):
        client, server = socket.socketpair()
        client.settimeout(2)
        connection = MagicMock()
        connection.__enter__.return_value.recv.return_value = banner
        result = subprocess.CompletedProcess(['wmctrl', '-m'], 0 if manager else 1, b'GNOME Shell', b'')
        try:
            with patch.object(helper, 'desktop_session', return_value={'DISPLAY': ':0'}), \
                 patch.object(helper.os.path, 'ismount', return_value=mounted), \
                 patch.object(helper.subprocess, 'run', return_value=result) as run, \
                 patch.object(helper.socket, 'create_connection', return_value=connection,
                              side_effect=OSError('连接尚未就绪') if connection_error else None) as connect:
                helper.connection(server, 7001)
                response = json.loads(client.recv(4096))
                self.assertEqual(run.call_count, 1)
                self.assertEqual(run.call_args.args[0], ['wmctrl', '-m'])
                return response, connect.call_args_list
        finally:
            client.close()
            server.close()

    def test_ready_without_browser_launch_software_inspection_or_screenshot(self):
        response, connections = self.probe()
        self.assertEqual(response, {'ready': True})
        self.assertEqual([call.args[0] for call in connections], [('127.0.0.1', 5900), ('127.0.0.1', 6080)])

    def test_incomplete_desktop_or_branch_mount_is_not_ready(self):
        for options in ({'mounted': False}, {'manager': False}, {'banner': b'closed'}, {'connection_error': True}):
            with self.subTest(options=options):
                response, _ = self.probe(**options)
                self.assertFalse(response['ready'])
                self.assertTrue(response['error'])


if __name__ == '__main__':
    unittest.main()
