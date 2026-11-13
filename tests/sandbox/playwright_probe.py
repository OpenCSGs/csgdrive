"""通过同一份 MCP 配置验证浏览器、桌面和分支下载。"""
import argparse
import base64
import json
from pathlib import Path
import re
import subprocess
import time
import urllib.request
import uuid

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--base', default='http://127.0.0.1:17421')
parser.add_argument('--workspace', required=True)
parser.add_argument('--branch', required=True)
parser.add_argument('--mount', required=True)
parser.add_argument('--artifacts', required=True)
args = parser.parse_args()
artifacts = Path(args.artifacts)
artifacts.mkdir(parents=True, exist_ok=True)
mount = Path(args.mount)
identifier = uuid.uuid4().hex
filename = 'playwright-proof-' + identifier + '.txt'
fixture = mount / ('playwright-proof-' + identifier + '.html')
payload = 'AgentDrive Playwright MCP 下载验证\n' + identifier + '\n'
fixture.write_text('''<!doctype html><meta charset="utf-8"><title>AgentDrive Playwright Verification</title>
<style>body{font:22px sans-serif;margin:60px}button{font:inherit;margin:12px;padding:16px}</style>
<h1>AgentDrive Playwright 验证</h1><p id="status">等待点击</p>
<button onclick="document.getElementById('status').textContent='点击成功'">点击验证</button>
<button id="download">下载到当前分支</button><script>
document.getElementById('download').onclick=()=>{const a=document.createElement('a');
a.href=URL.createObjectURL(new Blob([''' + json.dumps(payload) + '''],{type:'text/plain;charset=utf-8'}));
a.download=''' + json.dumps(filename) + ''';a.click();};</script>''')
endpoint = f'{args.base}/api/workspaces/{args.workspace}/branches/{args.branch}/sandbox/mcp-config'
config = json.load(urllib.request.urlopen(endpoint))
assert len(config['mcpServers']) == 1, config
spec = next(iter(config['mcpServers'].values()))
report = {'branch': args.branch, 'mcp_servers': list(config['mcpServers']), 'calls': []}
process = None
sequence = 0


def start():
    global process
    process = subprocess.Popen([spec['command'], *spec['args']], stdin=subprocess.PIPE,
                               stdout=subprocess.PIPE, stderr=(artifacts / 'proxy.log').open('ab'), text=True, bufsize=1)
    response = rpc('initialize', {'protocolVersion': '2024-11-05', 'capabilities': {},
                                 'clientInfo': {'name': 'agentdrive-playwright-verification', 'version': '1'}})
    assert response['serverInfo']['name'] == 'agentdrive-sandbox', response
    process.stdin.write(json.dumps({'jsonrpc': '2.0', 'method': 'notifications/initialized'}) + '\n')
    process.stdin.flush()
    return response


def rpc(method, params):
    global sequence
    sequence += 1
    process.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': sequence, 'method': method, 'params': params}) + '\n')
    process.stdin.flush()
    while True:
        line = process.stdout.readline()
        assert line, 'MCP 连接提前关闭'
        value = json.loads(line)
        if value.get('id') == sequence:
            assert 'error' not in value, value
            return value['result']


def call(name, arguments=None):
    result = rpc('tools/call', {'name': name, 'arguments': arguments or {}})
    assert not result.get('isError'), result
    texts = '\n'.join(item['text'] for item in result.get('content', []) if item['type'] == 'text')
    report['calls'].append({'tool': name, 'arguments': arguments, 'text': texts})
    for item in result.get('content', []):
        if item['type'] == 'image':
            (artifacts / (name + '.png')).write_bytes(base64.b64decode(item['data']))
    print(name, texts[:240], flush=True)
    return texts


def click_button(label):
    snapshot = call('browser_snapshot')
    match = re.search(r'button "' + re.escape(label) + r'".*?\[ref=([^\]]+)\]', snapshot)
    assert match, snapshot
    return call('browser_click', {'element': label, 'target': match.group(1)})


try:
    report['initialize'] = start()
    tools = rpc('tools/list', {})['tools']
    report['tools'] = [tool['name'] for tool in tools]
    assert len(set(report['tools'])) == len(tools)
    assert {'browser_navigate', 'browser_tabs', 'browser_click', 'list_windows', 'get_app_state', 'paste_text'} <= set(report['tools'])
    call('sandbox_tools_status')
    assert 'Example Domain' in call('browser_navigate', {'url': 'https://example.com'})
    call('browser_tabs', {'action': 'new'})
    assert 'Python' in call('browser_navigate', {'url': 'https://www.python.org'})
    call('browser_tabs', {'action': 'new'})
    call('browser_evaluate', {'function': '() => { document.open(); document.write(' + json.dumps(fixture.read_text()) + '); document.close(); return document.title; }'})
    click_button('点击验证')
    assert '点击成功' in call('browser_snapshot')
    windows = call('list_windows')
    assert 'chromium' in windows.lower(), windows
    desktop = call('get_app_state', {'app_name_or_bundle_identifier': 'chromium', 'include_screenshot': True})
    assert 'AgentDrive Playwright' in desktop, desktop
    click_button('下载到当前分支')
    expected = mount / 'Downloads' / filename
    for _ in range(100):
        if expected.exists() and expected.read_text() == payload:
            break
        time.sleep(.2)
    else:
        raise AssertionError('下载文件没有进入当前分支的 Downloads：' + str(list((mount / 'Downloads').glob('*'))))
    report['download'] = {'path': str(expected), 'content': expected.read_text()}
    print('BRANCH DOWNLOAD VERIFIED', expected, flush=True)
    call('browser_tabs', {'action': 'list'})
    process.stdin.close()
    process.wait(timeout=15)
    assert process.returncode == 0
    start()
    rpc('tools/list', {})
    assert 'AgentDrive Playwright Verification' in call('browser_tabs', {'action': 'list'})
    call('get_app_state', {'app_name_or_bundle_identifier': 'chromium', 'include_screenshot': True})
    report['reconnect_preserves_browser'] = True
    (artifacts / 'report.json').write_text(json.dumps(report, ensure_ascii=False, indent=2))
    print('UNIFIED MCP VERIFIED', flush=True)
finally:
    if process and process.poll() is None:
        if not process.stdin.closed:
            process.stdin.close()
        try:
            process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            process.terminate()
    fixture.unlink(missing_ok=True)
