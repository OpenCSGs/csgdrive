"""为 computer-use-linux 的 MCP 响应补充当前实例信息。"""
import json
import subprocess
import sys
import threading

from desktop_environment import environment_info, environment_instructions

PASTE_TOOL = {
    'name': 'paste_text',
    'description': '在已点击的 Chromium 输入框中粘贴文本，支持完整中文和表情。先通过 get_app_state 与 click 选中输入框，并从 list_windows 获取 window_id。工具确认目标窗口后粘贴；完成后读取输入框核对内容。此操作会替换 Sandbox 桌面的剪贴板。',
    'inputSchema': {'type': 'object', 'properties': {
        'text': {'type': 'string'},
        'window_id': {'type': 'integer', 'minimum': 1}},
        'required': ['text', 'window_id'], 'additionalProperties': False},
    'annotations': {'readOnlyHint': False, 'destructiveHint': True,
                    'idempotentHint': False, 'openWorldHint': True},
}


def paste_request(message):
    if not isinstance(message, dict) or 'id' not in message:
        return None
    params = message.get('params', {})
    if message.get('method') != 'tools/call' or not isinstance(params, dict) or params.get('name') != 'paste_text':
        return None
    arguments = params.get('arguments', {})
    if not isinstance(arguments, dict):
        raise ValueError('paste_text 参数必须为对象')
    value, window = arguments.get('text'), arguments.get('window_id')
    if not isinstance(value, str) or isinstance(window, bool) or not isinstance(window, int) or window < 1:
        raise ValueError('paste_text 需要 text 和有效的 window_id')
    if len(value.encode('utf-8')) > 131072 or '\0' in value:
        raise ValueError('粘贴内容超过长度限制或包含无效字符')
    return value, {'jsonrpc': '2.0', 'id': message['id'], 'method': 'tools/call',
                   'params': {'name': 'press_key', 'arguments': {'window_id': window, 'key': 'ctrl+v'}}}


def enrich(message, context):
    result = message.get('result')
    if not isinstance(result, dict):
        return message
    if 'protocolVersion' in result and 'serverInfo' in result:
        result['instructions'] = context + '\n\n' + result.get('instructions', '')
    for tool in result.get('tools', []):
        if tool.get('name') in ('list_apps', 'get_app_state'):
            tool['description'] = context + '\n\n' + tool.get('description', '')
        if tool.get('name') == 'type_text':
            tool['description'] = 'Chromium 输入中文或表情时请使用 paste_text，并读取输入框核对结果。\n' + tool.get('description', '')
    if 'tools' in result:
        result['tools'].append(PASTE_TOOL)
    return message


def main():
    context = environment_instructions(environment_info())
    child = subprocess.Popen(['/usr/local/bin/computer-use-linux', 'mcp'],
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    output_lock = threading.Lock()
    pending_pastes = set()

    def emit(data):
        with output_lock:
            sys.stdout.buffer.write(data)
            sys.stdout.buffer.flush()

    def responses():
        try:
            for line in child.stdout:
                try:
                    message = json.loads(line)
                    if message.get('id') in pending_pastes:
                        pending_pastes.discard(message['id'])
                        result = message.get('result', {})
                        if result and not result.get('isError'):
                            for item in result.get('content', []):
                                if item.get('type') == 'text':
                                    detail = json.loads(item['text'])
                                    detail['action'] = 'paste_text'
                                    detail['message'] = '已发送粘贴操作，请读取输入框核对内容。 ' + detail.get('message', '')
                                    item['text'] = json.dumps(detail, ensure_ascii=False)
                                    result['structuredContent'] = detail
                    output = (json.dumps(enrich(message, context), ensure_ascii=False) + '\n').encode()
                except (ValueError, AttributeError, TypeError):
                    output = line
                emit(output)
        except (BrokenPipeError, OSError):
            child.terminate()

    reader = threading.Thread(target=responses, daemon=True)
    reader.start()
    try:
        for line in sys.stdin.buffer:
            try:
                message = json.loads(line)
            except ValueError:
                child.stdin.write(line)
                child.stdin.flush()
                continue
            try:
                paste = paste_request(message)
                if paste is not None:
                    value, request = paste
                    subprocess.run(['xclip', '-selection', 'clipboard', '-in'], input=value.encode(),
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5, check=True)
                    pending_pastes.add(message['id'])
                    line = (json.dumps(request) + '\n').encode()
            except (ValueError, subprocess.SubprocessError, OSError) as error:
                emit((json.dumps({'jsonrpc': '2.0', 'id': message.get('id'), 'result': {
                    'isError': True, 'content': [{'type': 'text', 'text': str(error)}]}}) + '\n').encode())
                continue
            child.stdin.write(line)
            child.stdin.flush()
        child.stdin.close()
        code = child.wait(timeout=10)
        reader.join(timeout=5)
        return code
    finally:
        if child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()


if __name__ == '__main__':
    sys.exit(main())
