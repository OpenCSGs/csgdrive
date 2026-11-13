#!/usr/bin/python3
import json
import os
import select
import socket
import subprocess
import threading


def runtime_config():
    try:
        with open('/etc/agentdrive-runtime.json') as source:
            return json.load(source)
    except FileNotFoundError:
        return {}


RUNTIME = runtime_config()
DESKTOP_USER = RUNTIME.get('user', 'user')


def desktop_session():
    for entry in os.scandir('/proc'):
        if not entry.name.isdigit():
            continue
        try:
            command = open(entry.path + '/comm').read().strip()
            if command == RUNTIME.get('session_process', 'gnome-session-b'):
                environment = dict(item.split('=', 1) for item in open(entry.path + '/environ', 'rb').read().decode().split('\0') if '=' in item)
                return environment
        except (OSError, ValueError):
            continue
    raise RuntimeError('Desktop session is not ready')


def bridge(left, right):
    try:
        while True:
            readable, _, _ = select.select([left, right], [], [])
            for source in readable:
                data = source.recv(65536)
                if not data:
                    return
                (right if source is left else left).sendall(data)
    finally:
        right.close()


def mcp_connection(client, environment):
    with open('/var/log/agentdrive-mcp.log', 'ab') as log:
        process = subprocess.Popen(['/usr/local/bin/node', '/opt/agentdrive-mcp/gateway.mjs'],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log,
                                   env=environment, user=DESKTOP_USER, group=DESKTOP_USER, extra_groups=[], cwd='/workspace')
        try:
            inputs = [client, process.stdout]
            while inputs:
                readable, _, _ = select.select(inputs, [], [])
                for source in readable:
                    if source is client:
                        data = client.recv(65536)
                        if data:
                            process.stdin.write(data)
                            process.stdin.flush()
                        else:
                            inputs.remove(client)
                            process.stdin.close()
                    else:
                        data = os.read(process.stdout.fileno(), 65536)
                        if not data:
                            return
                        client.sendall(data)
        finally:
            if not process.stdin.closed:
                process.stdin.close()
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.terminate()
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


def connection(client, port):
    with client:
        try:
            if port == 7011 and RUNTIME.get('assistant') == 'csgclaw':
                upstream = socket.create_connection(('127.0.0.1', 18080), timeout=10)
                upstream.settimeout(None)
                bridge(client, upstream)
                return
            if port == 6080:
                bridge(client, socket.create_connection(('127.0.0.1', 6080), timeout=10))
                return
            if port == 7002:
                os.sync()
                client.sendall(b'{"flushed":true}\n')
                return
            environment = desktop_session()
            if port == 7000:
                mcp_connection(client, environment)
                return
            manager = subprocess.run(['wmctrl', '-m'], env=environment, user=DESKTOP_USER, group=DESKTOP_USER, extra_groups=[], capture_output=True, timeout=5)
            if manager.returncode or RUNTIME.get('window_manager', 'GNOME Shell').encode() not in manager.stdout:
                raise RuntimeError('Window manager is not ready')
            if not os.path.ismount('/workspace'):
                raise RuntimeError('当前分支目录 /workspace 尚未挂载')
            with socket.create_connection(('127.0.0.1', 5900), timeout=5) as vnc:
                if not vnc.recv(12).startswith(b'RFB '):
                    raise RuntimeError('Desktop connection is not ready')
            with socket.create_connection(('127.0.0.1', 6080), timeout=5):
                pass
            client.sendall(b'{"ready":true}\n')
        except Exception as error:
            client.sendall(json.dumps({'ready': False, 'error': str(error)}).encode() + b'\n')


def listen(port):
    server = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
    server.bind((socket.VMADDR_CID_ANY, port))
    server.listen(16)
    while True:
        client, _ = server.accept()
        threading.Thread(target=connection, args=(client, port), daemon=True).start()


if __name__ == '__main__':
    for port in [6080, 7000, 7002] + ([7011] if RUNTIME.get('assistant') == 'csgclaw' else []):
        threading.Thread(target=listen, args=(port,), daemon=True).start()
    listen(7001)
