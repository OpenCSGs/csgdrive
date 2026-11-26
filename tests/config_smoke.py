"""Verify runtime settings, revision conflicts, and SQLite recovery after restart."""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

parser = argparse.ArgumentParser()
parser.add_argument('--bin-dir', default='target/debug')
args = parser.parse_args()
executable = Path(args.bin_dir).resolve() / ('agentdrived.exe' if os.name == 'nt' else 'agentdrived')

with tempfile.TemporaryDirectory(prefix='agentdrive-config-') as temporary:
    root = Path(temporary)
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        address = f'127.0.0.1:{listener.getsockname()[1]}'
    subprocess.run([str(executable), 'init', '--directory', str(root), '--listen', address], check=True, capture_output=True)
    path = root / 'agentdrive.json'
    original = json.loads(path.read_text(encoding='utf-8'))

    endpoint = f'http://{address}/api/settings'
    daemon = None
    log = (root / 'daemon.log').open('w')

    def request(method='GET', payload=None):
        headers = {}
        data = None
        if payload is not None:
            headers['Content-Type'] = 'application/json'
            data = json.dumps(payload).encode()
        try:
            response = urllib.request.urlopen(urllib.request.Request(endpoint, data=data, headers=headers, method=method), timeout=5)
        except urllib.error.HTTPError as error:
            return error.code, error.read().decode()
        with response:
            body = response.read().decode()
            return response.status, json.loads(body)

    def wait_for(predicate):
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            assert daemon.poll() is None, (root / 'daemon.log').read_text(encoding='utf-8')
            try:
                status, data = request()
                if status == 200 and predicate(data):
                    return data
            except (urllib.error.URLError, ConnectionError):
                pass
            time.sleep(.1)
        raise AssertionError('configuration did not reach the expected state')

    def start():
        return subprocess.Popen([str(executable), 'serve', '--config', str(path)], stdin=subprocess.DEVNULL, stdout=log, stderr=log)

    try:
        daemon = start()
        initial = wait_for(lambda data: data['reload_error'] is None)
        runtime = dict(initial['runtime'], cache={'max_bytes': 3 * 1024 ** 3},
                       allowed_origins=['https://example.com'])
        update = {'revision': initial['revision'], 'runtime': runtime}
        status, saved = request('PUT', update)
        assert status == 200, saved
        assert request('PUT', update)[0] == 409
        invalid = dict(runtime, allowed_origins=['invalid'])
        assert request('PUT', {'revision': saved['revision'], 'runtime': invalid})[0] == 400
        assert request()[1]['runtime'] == runtime
        assert json.loads(path.read_text(encoding='utf-8')) == original
        assert daemon.poll() is None
        daemon.terminate()
        daemon.wait(timeout=15)
        daemon = start()
        restarted = wait_for(lambda data: data['runtime'] == saved['runtime'])
        assert restarted['revision'] == saved['revision']
        print('SQLite settings, revision conflicts, validation and restart checks passed')
    finally:
        if daemon is not None and daemon.poll() is None:
            daemon.terminate()
            try:
                daemon.wait(timeout=15)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait(timeout=10)
        log.close()
