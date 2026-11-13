"""验证助手使用实例提供的访问地址，并覆盖已有配置中的旧地址。"""
import importlib.util
from pathlib import Path
import types
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('assistant_service', Path(__file__).resolve().parents[2] / 'build/sandbox/assistant/assistant-service.py')
assistant = importlib.util.module_from_spec(spec)
with patch('pwd.getpwnam', return_value=types.SimpleNamespace(pw_uid=1000, pw_gid=1000, pw_dir='/home/user')):
    spec.loader.exec_module(assistant)


class AssistantAddressTest(unittest.TestCase):
    def test_each_instance_and_port_use_their_current_origin(self):
        for port in (17421, 18090):
            origin = f'http://csgclaw.b9e2497d-fefd-4f70-a3e5-4e330a22a1ab.25dd1802-2701-4ccb-9f5f-407268695f37.localhost:{port}'
            command = assistant.assistant_command({'advertise_base_url': origin})
            self.assertEqual(command, ['/opt/csgclaw/bin/csgclaw', '--endpoint', origin, 'serve', '--no-browser'])

    def test_rejects_external_or_malformed_origins(self):
        for value in ['http://example.com:17421', 'http://127.0.0.1:18080',
                      'http://csgclaw.invalid.branch.localhost:17421',
                      'http://csgclaw.b9e2497d-fefd-4f70-a3e5-4e330a22a1ab.25dd1802-2701-4ccb-9f5f-407268695f37.localhost:17421/path']:
            with self.subTest(value=value), self.assertRaises(ValueError):
                assistant.assistant_command({'advertise_base_url': value})
