"""通过桌面 MCP 验证 Chromium 的中文显示、鼠标点击和中文输入。"""
import argparse
import base64
import json
from pathlib import Path
import socket
import time
import uuid

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--socket', required=True)
parser.add_argument('--mount', required=True)
parser.add_argument('--artifacts', required=True)
args = parser.parse_args()
artifacts = Path(args.artifacts)
artifacts.mkdir(parents=True, exist_ok=True)
filename = 'chromium-font-check-' + uuid.uuid4().hex + '.html'
fixture = Path(args.mount) / filename
fixture.write_text('''<!doctype html><html lang="zh-CN"><meta charset="UTF-8">
<title>Chromium 中文字体验证</title>
<style>body{font:24px sans-serif;margin:60px;background:#f3f6fa;color:#203040}
input{font:24px sans-serif;padding:16px;width:80%}h1{font-size:36px}
button{font:22px sans-serif;padding:12px;margin-top:20px}</style>
<h1>中文字体验证：开放传神 · AgentDrive</h1>
<p>简体中文：浏览器可以正常显示中文。</p>
<p>繁體中文：瀏覽器可以正常顯示中文。</p>
<p>表情符号：😀 🚀 ✅ 🌏</p>
<label for="text">中文输入</label><br><input id="text" aria-label="中文输入">
<br><button onclick="document.getElementById('status').textContent='按钮点击成功'">点击验证</button>
<p id="status" role="status">等待点击按钮</p>
</html>''')
connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
connection.settimeout(60)
connection.connect(args.socket)
stream = connection.makefile('rwb', buffering=0)
stream.write(b'CONNECT 7000\n')
assert stream.readline().startswith(b'OK ')
request_id = 0
calls = []


def rpc(method, params):
    global request_id
    request_id += 1
    stream.write((json.dumps({'jsonrpc': '2.0', 'id': request_id,
                             'method': method, 'params': params}) + '\n').encode())
    while True:
        response = json.loads(stream.readline())
        if response.get('id') == request_id:
            assert 'error' not in response, response
            return response['result']


def call(name, arguments, screenshot=None):
    result = rpc('tools/call', {'name': name, 'arguments': arguments})
    assert not result.get('isError'), result
    text = [item['text'] for item in result.get('content', []) if item['type'] == 'text']
    calls.append({'tool': name, 'arguments': arguments, 'result': text})
    (artifacts / 'calls.json').write_text(json.dumps(calls, ensure_ascii=False, indent=2))
    for item in result.get('content', []):
        if item['type'] == 'image' and screenshot:
            (artifacts / screenshot).write_bytes(base64.b64decode(item['data']))
    print(name, flush=True)
    return json.loads(text[0]) if text else {}


try:
    initialization = rpc('initialize', {'protocolVersion': '2024-11-05', 'capabilities': {},
                                        'clientInfo': {'name': 'chromium-font-test', 'version': '1'}})
    assert 'Chromium' in initialization['instructions']
    stream.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
    call('get_app_state', {'include_screenshot': False})
    call('press_key', {'key': 'Escape'})
    call('press_key', {'key': 'alt+F2'})
    time.sleep(1)
    call('type_text', {'text': 'xdg-open file:///workspace/' + filename})
    call('press_key', {'key': 'Return'})
    deadline = time.monotonic() + 45
    while True:
        time.sleep(2)
        windows = call('list_windows', {})['windows']
        if any(window.get('wm_class') == 'chromium' for window in windows):
            break
        assert time.monotonic() < deadline, windows
    call('press_key', {'wm_class': 'chromium', 'key': 'ctrl+t'})
    call('type_text', {'wm_class': 'chromium', 'text': 'file:///workspace/' + filename})
    call('press_key', {'wm_class': 'chromium', 'key': 'Return'})
    deadline = time.monotonic() + 45
    while True:
        time.sleep(2)
        state = call('get_app_state', {'wm_class': 'chromium', 'include_screenshot': False,
                                      'max_depth': 20, 'max_nodes': 300})
        nodes = state.get('accessibility_tree', [])
        fields = [node for node in nodes if node.get('name') == '中文输入'
                  and ('editable' in node.get('states', []) or node.get('supports_editable_text'))]
        if fields:
            break
        assert time.monotonic() < deadline, state
    call('list_windows', {})
    call('click', {'element_index': fields[0]['index'], 'snapshot_id': state['snapshot_id']})
    expected = 'Chromium 中文输入成功：当前分支文件可访问。'
    call('type_text', {'wm_class': 'chromium', 'text': 'Chromium input test'})
    state = call('get_app_state', {'wm_class': 'chromium', 'include_screenshot': False,
                                  'max_depth': 20, 'max_nodes': 300})
    field = next(node for node in state['accessibility_tree']
                 if node.get('name') == '中文输入' and node.get('role') == 'entry')
    assert field.get('text', {}).get('content') == 'Chromium input test', field
    windows = call('list_windows', {})['windows']
    window = next(window for window in windows if window.get('wm_class') == 'chromium' and window.get('focused'))
    call('press_key', {'window_id': window['window_id'], 'key': 'ctrl+a'})
    call('paste_text', {'window_id': window['window_id'], 'text': expected})
    state = call('get_app_state', {'wm_class': 'chromium', 'include_screenshot': False,
                                  'max_depth': 20, 'max_nodes': 300})
    assert any(expected in json.dumps(node, ensure_ascii=False)
               for node in state.get('accessibility_tree', [])), state
    button = next(node for node in state['accessibility_tree'] if node.get('name') == '点击验证')
    call('click', {'element_index': button['index'], 'snapshot_id': state['snapshot_id']})
    state = call('get_app_state', {'wm_class': 'chromium', 'include_screenshot': True,
                                  'max_depth': 20, 'max_nodes': 300}, 'chinese-fonts.png')
    assert any('按钮点击成功' in json.dumps(node, ensure_ascii=False)
               for node in state.get('accessibility_tree', [])), state
    (artifacts / 'report.json').write_text(json.dumps({
        'verified': True, 'browser': 'Chromium', 'chinese_input': expected,
        'new_tab': True, 'button_clicked': True,
        'workspace_file': filename, 'method': 'computer-use-linux MCP'}, ensure_ascii=False, indent=2))
    print('VERIFIED: Chromium 中文输入及当前分支网页访问成功')
finally:
    connection.close()
    fixture.unlink(missing_ok=True)
