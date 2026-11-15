"""启动分支助手及其代理连接，助手数据保存在独立的持久目录。"""
import json
import os
from pathlib import Path
import pwd
import select
import socket
import subprocess
import threading
import time
import urllib.error
import urllib.request
from urllib.parse import urlsplit
from uuid import UUID

STATE = Path('/agent-state/csgclaw')
USER = pwd.getpwnam('user')
BASE_URL = 'http://127.0.0.1:18080'
CLIENT = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def assistant_command(metadata):
    origin = metadata['advertise_base_url']
    url = urlsplit(origin)
    labels = (url.hostname or '').split('.')
    if (url.scheme != 'http' or url.username or url.password or not url.port
            or url.path or url.query or url.fragment or len(labels) != 4
            or labels[0] != 'csgclaw' or labels[3] != 'localhost'):
        raise ValueError('助手访问地址无效')
    if any(str(UUID(value)) != value for value in labels[1:3]):
        raise ValueError('助手工作区或分支标识无效')
    # 启动参数覆盖已有配置，使访问地址随宿主机端口和分支自动更新。
    return ['/opt/csgclaw/bin/csgclaw', '--endpoint', origin, 'serve', '--no-browser']


def request(path, data=None):
    payload = None if data is None else json.dumps(data).encode()
    req = urllib.request.Request(BASE_URL + path, data=payload,
                                 headers={'Content-Type': 'application/json'})
    with CLIENT.open(req, timeout=90) as response:
        return json.load(response)


def ensure_participant():
    try:
        request('/api/v1/channels/csgclaw/participants/pt-agentdrive')
    except urllib.error.HTTPError as error:
        if error.code != 404:
            raise
        request('/api/v1/channels/csgclaw/participants', {
            'id': 'agentdrive', 'type': 'agent', 'name': 'agentdrive',
            'channel_user': {'ref':'user-agentdrive', 'kind':'local_user_id'},
            'agent_binding': {'mode':'reuse', 'agent_id':'agent-agentdrive'},
        })


def bootstrap():
    """通过 CSGClaw API 配置工作目录与统一 MCP，保留用户已有设置。"""
    while True:
        try:
            try:
                request('/api/v1/agents/agentdrive')
                ensure_participant()
                return
            except urllib.error.HTTPError as error:
                if error.code != 404:
                    raise
            request('/api/v1/agents', {
                'id': 'agentdrive', 'name': 'agentdrive', 'runtime': {'name':'codex','sandbox_enabled':False},
                'description': '操作当前 AgentDrive 分支文件和桌面浏览器',
                'instructions': '工作目录为 /workspace，它对应当前 AgentDrive 分支。文件操作必须保存在此目录。网页操作使用 ubuntu_desktop MCP 中的 browser_* 工具，与桌面共用 Chromium。下载保存到 /workspace/Downloads。桌面操作使用同一 MCP 的桌面工具。',
                'runtime_options': {'local_workspace_dir': '/workspace'},
                'mcpServers': {'ubuntu_desktop': {
                    'command': '/usr/bin/python3',
                    'args': ['/usr/local/bin/assistant-mcp.py'],
                    'startup_timeout_sec': 60, 'tool_timeout_sec': 120,
                }},
            })
            ensure_participant()
            return
        except Exception as error:
            # 模型尚未配置时，保留 Web UI 供用户填写模型配置。
            print('Assistant initialization pending:', type(error).__name__, flush=True)
            time.sleep(15)


def forward(client):
    with client:
        try:
            with socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM) as upstream:
                upstream.settimeout(10)
                upstream.connect((2, 18081))
                upstream.settimeout(None)
                while True:
                    readable, _, _ = select.select([client, upstream], [], [])
                    for source in readable:
                        data = source.recv(65536)
                        if not data:
                            return
                        (upstream if source is client else client).sendall(data)
        except OSError:
            return


def proxy_listener():
    with socket.socket() as server:
        server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        server.bind(('127.0.0.1', 18081))
        server.listen(32)
        while True:
            client, _ = server.accept()
            threading.Thread(target=forward, args=(client,), daemon=True).start()


def main():
    if not os.path.ismount('/workspace') or not os.path.ismount('/agent-state'):
        raise RuntimeError('助手需要分支目录和状态目录挂载完成')
    command = assistant_command(json.loads(Path('/agent-state/assistant.json').read_text()))
    STATE.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chown(STATE, USER.pw_uid, USER.pw_gid)
    home_state = Path(USER.pw_dir) / '.csgclaw'
    if not home_state.exists():
        home_state.symlink_to(STATE, target_is_directory=True)
    if home_state.resolve() != STATE:
        raise RuntimeError('助手状态目录无效')
    config = STATE / 'config.toml'
    if not config.exists():
        config.write_text('[server]\nlisten_addr = "127.0.0.1:18080"\nno_auth = true\nshow_upgrade = false\n')
        config.chmod(0o600)
        os.chown(config, USER.pw_uid, USER.pw_gid)
    auth = Path('/agent-state/codex-auth')
    if auth.is_dir() and not (Path(USER.pw_dir) / '.codex').exists():
        (Path(USER.pw_dir) / '.codex').symlink_to(auth, target_is_directory=True)
    environment = dict(os.environ, HOME=USER.pw_dir, USER='user', LANG='C.UTF-8',
                       PATH='/opt/csgclaw/bin:/usr/local/bin:/usr/bin:/bin',
                       NO_PROXY='localhost,127.0.0.1,::1')
    if json.loads(Path('/agent-state/network.json').read_text()).get('proxy_enabled'):
        environment.update(HTTP_PROXY='http://127.0.0.1:18081', HTTPS_PROXY='http://127.0.0.1:18081',
                           http_proxy='http://127.0.0.1:18081', https_proxy='http://127.0.0.1:18081')
    threading.Thread(target=proxy_listener, daemon=True).start()
    threading.Thread(target=bootstrap, daemon=True).start()
    while True:
        with open('/var/log/csgclaw.log', 'ab') as log:
            process = subprocess.Popen(command,
                                       env=environment, cwd='/workspace', user='user', group='user',
                                       extra_groups=[], stdout=log, stderr=log)
            process.wait()
        time.sleep(3)


if __name__ == '__main__':
    main()
