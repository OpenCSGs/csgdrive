"""用于验证统一入口的 MCP 子进程。"""
import json
import sys
import time

for line in sys.stdin:
    request = json.loads(line)
    if 'id' not in request:
        continue
    method = request['method']
    if method == 'initialize':
        result = {'protocolVersion': request['params']['protocolVersion'],
                  'serverInfo': {'name': sys.argv[1], 'version': '1'}, 'capabilities': {'tools': {}}}
    elif method == 'tools/list':
        second = request.get('params', {}).get('cursor') == 'second'
        result = {'tools': [{'name': 'last' if second else 'echo', 'description': '验证工具',
                            'inputSchema': {'type': 'object', 'properties': {'value': {'type': 'string'}}}}]}
        if not second:
            result['nextCursor'] = 'second'
    else:
        time.sleep(0.05)
        params = request.get('params', {})
        result = {'content': [{'type': 'text', 'text': json.dumps({'provider': sys.argv[1], 'name': params.get('name'), 'arguments': params.get('arguments')})},
                              {'type': 'image', 'mimeType': 'image/png', 'data': 'dGVzdA=='}]}
        if params.get('arguments', {}).get('value') == 'failure':
            result['isError'] = True
    print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}), flush=True)
