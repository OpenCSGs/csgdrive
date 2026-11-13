"""验证 MCP 环境说明保留上游响应及工具接口。"""
import copy
import json
import sys
import subprocess
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'build/sandbox'))
from desktop_environment import environment_instructions
from desktop_mcp import enrich, paste_request


class DesktopMCPTest(unittest.TestCase):
    def test_stream_adapter_drains_responses_after_client_eof(self):
        with tempfile.TemporaryDirectory() as directory:
            server = Path(directory) / 'server.py'
            server.write_text('''import json,sys,time
for line in sys.stdin:
    request=json.loads(line)
    time.sleep(0.02)
    result={'tools':[]} if request['method']=='tools/list' else {'content':[{'type':'text','text':'unchanged'}]}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}),flush=True)
''')
            source = str(Path(__file__).resolve().parents[2] / 'build/sandbox')
            program = ('import sys;sys.path.insert(0,sys.argv[1]);import desktop_mcp as m;'
                       'm.environment_info=lambda: {};m.environment_instructions=lambda info: "environment";'
                       'original=m.subprocess.Popen;'
                       'm.subprocess.Popen=lambda command,**kwargs: original([sys.executable,sys.argv[2]],**kwargs);'
                       'sys.exit(m.main())')
            messages = [{'jsonrpc': '2.0', 'id': 1, 'method': 'tools/list'},
                        {'jsonrpc': '2.0', 'id': 2, 'method': 'tools/call'}]
            result = subprocess.run([sys.executable, '-c', program, source, str(server)],
                                    input=''.join(json.dumps(m)+'\n' for m in messages),
                                    text=True, capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            output = [json.loads(line) for line in result.stdout.splitlines()]
            self.assertEqual([item['id'] for item in output], [1, 2])
            self.assertEqual(output[0]['result']['tools'][0]['name'], 'paste_text')
            self.assertEqual(output[1]['result']['content'][0]['text'], 'unchanged')

    def test_paste_preserves_request_and_window_identity(self):
        original = {'jsonrpc': '2.0', 'id': 'paste-1', 'method': 'tools/call',
                    'params': {'name': 'paste_text', 'arguments': {'window_id': 123, 'text': '中文😀'}}}
        value, request = paste_request(original)
        self.assertEqual(value, '中文😀')
        self.assertEqual(request['id'], 'paste-1')
        self.assertEqual(request['params'], {'name': 'press_key', 'arguments': {'window_id': 123, 'key': 'ctrl+v'}})
        self.assertEqual(original['params']['name'], 'paste_text')

    def test_paste_requires_an_explicit_window(self):
        for window in (None, True, 0, '123'):
            with self.assertRaises(ValueError):
                paste_request({'id': 1, 'method': 'tools/call', 'params': {
                    'name': 'paste_text', 'arguments': {'text': '中文', 'window_id': window}}})

    def test_initialization_preserves_capabilities_and_upstream_instructions(self):
        message = {'jsonrpc': '2.0', 'id': 3, 'result': {
            'protocolVersion': '2024-11-05', 'serverInfo': {'name': 'computer-use-linux'},
            'capabilities': {'tools': {}}, 'instructions': 'upstream guidance'}}
        result = enrich(message, 'desktop environment')
        self.assertEqual(result['id'], 3)
        self.assertEqual(result['result']['capabilities'], {'tools': {}})
        self.assertEqual(result['result']['instructions'], 'desktop environment\n\nupstream guidance')

    def test_environment_is_visible_in_discovery_without_changing_tool_schema(self):
        tool = {'name': 'list_apps', 'description': 'installed applications',
                'inputSchema': {'type': 'object'}, 'annotations': {'readOnlyHint': True}}
        original = copy.deepcopy(tool)
        output = enrich({'result': {'tools': [tool]}}, 'Chromium: chromium')['result']['tools'][0]
        self.assertIn('chromium', output['description'])
        self.assertTrue(output['description'].endswith(original['description']))
        for key in ('inputSchema', 'annotations', 'name'):
            self.assertEqual(output[key], original[key])

    def test_screenshots_errors_and_notifications_are_unchanged(self):
        for message in (
            {'id': 4, 'result': {'content': [{'type': 'image', 'data': 'image bytes'}]}},
            {'id': 5, 'error': {'code': -32602, 'message': 'invalid arguments'}},
            {'method': 'notifications/message', 'params': {'data': 'status'}},
        ):
            self.assertEqual(enrich(copy.deepcopy(message), 'environment'), message)

    def test_missing_browser_and_mount_are_reported_without_claiming_availability(self):
        info = {'browser': {'command': None, 'open_url_command': None},
                'workspace': {'mounted': False}, 'network': {}}
        context = environment_instructions(info)
        self.assertNotIn('已安装 Chromium', context)
        self.assertNotIn('已挂载到 /workspace', context)
        self.assertIn('/workspace 当前未挂载', context)


if __name__ == '__main__':
    unittest.main()
