"""Verify outbound HTTPS using the guest browser and computer-use-linux MCP."""
import argparse
import base64
import json
import socket
import time
import uuid
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--socket', required=True)
parser.add_argument('--artifacts', required=True)
args = parser.parse_args()
artifacts = Path(args.artifacts)
artifacts.mkdir(parents=True, exist_ok=True)
connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
connection.settimeout(60)
connection.connect(args.socket)
stream = connection.makefile('rwb', buffering=0)
stream.write(b'CONNECT 7000\n')
assert stream.readline().startswith(b'OK ')
request_id = 0
report = []


def rpc(method, params):
    global request_id
    request_id += 1
    stream.write((json.dumps(dict(jsonrpc='2.0', id=request_id,
                                 method=method, params=params)) + '\n').encode())
    while True:
        line = stream.readline()
        assert line, 'MCP connection closed'
        response = json.loads(line)
        if response.get('id') == request_id:
            assert 'error' not in response, response
            return response['result']


def call(name, arguments, screenshot=None):
    result = rpc('tools/call', dict(name=name, arguments=arguments))
    assert not result.get('isError'), result
    texts = [entry['text'] for entry in result.get('content', []) if entry['type'] == 'text']
    report.append(dict(tool=name, arguments=arguments, result=texts))
    (artifacts / 'calls.json').write_text(json.dumps(report, indent=2))
    for entry in result.get('content', []):
        if entry['type'] == 'image' and screenshot:
            (artifacts / screenshot).write_bytes(base64.b64decode(entry['data']))
    print(name, str(texts)[:300], flush=True)
    return '\n'.join(texts)


initialization = rpc('initialize', dict(protocolVersion='2024-11-05', capabilities={},
                       clientInfo=dict(name='agentdrive-network-test', version='1')))
assert all(value in initialization['instructions'] for value in ('chromium', 'xdg-open', '/workspace'))
stream.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
tools = rpc('tools/list', {})['tools']
assert 'chromium' in next(tool['description'] for tool in tools if tool['name'] == 'list_apps')
call('get_app_state', {'include_screenshot': False})
call('list_apps', {})
call('setup_accessibility', {})
call('press_key', {'key': 'Escape'})
call('press_key', {'key': 'alt+F2'})
time.sleep(1)
call('type_text', {'text': 'xdg-open https://example.com'})
call('press_key', {'key': 'Return'})
time.sleep(8)
call('list_windows', {})
for index, (url, expected) in enumerate([
    ('https://example.com', 'Example Domain'),
    ('https://www.python.org/', 'Welcome to Python.org'),
    ('https://opencsg.com/', 'OpenCSG'),
]):
    call('press_key', {'key': 'ctrl+l', 'wm_class': 'chromium'})
    target = url + ('&' if '?' in url else '?') + 'agentdrive_probe=' + uuid.uuid4().hex
    call('type_text', {'text': target, 'wm_class': 'chromium'})
    call('press_key', {'key': 'Return', 'wm_class': 'chromium'})
    deadline = time.monotonic() + 60
    while True:
        time.sleep(3)
        state = call('get_app_state', {'app_name_or_bundle_identifier': 'chromium',
                                      'include_screenshot': False})
        tree = json.loads(state).get('accessibility_tree', [])
        if any(node.get('role') == 'frame' and expected.lower() in node.get('name', '').lower() for node in tree):
            break
        assert time.monotonic() < deadline, f'Browser did not load {url}: {state}'
    call('get_app_state', {'app_name_or_bundle_identifier': 'chromium',
                          'include_screenshot': True}, f'website-{index + 1}.png')
(artifacts / 'report.json').write_text(json.dumps(dict(
    verified=True, websites=['https://example.com', 'https://www.python.org/', 'https://opencsg.com/'],
    method='computer-use-linux MCP keyboard input and browser accessibility',
    calls=report), indent=2))
connection.close()
print('VERIFIED: Ubuntu browser loaded all HTTPS websites through computer-use-linux')
