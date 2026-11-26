"""Verify shared clients, database isolation, multipart file storage and restore from S3."""
import argparse
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid

parser = argparse.ArgumentParser()
parser.add_argument('--bin-dir', default='target/debug')
args = parser.parse_args()
binaries = Path(args.bin_dir).resolve()
exe = '.exe' if os.name == 'nt' else ''
endpoint = os.environ['AGENTDRIVE_TEST_S3_ENDPOINT']
prefix = 'daemon-tests/' + str(uuid.uuid4())

profile = {
    'backend': 's3', 'bucket': 'agentdrive-integration', 'prefix': prefix,
    'region': 'us-east-1', 'endpoint': endpoint, 'allow_http': True,
}

class Daemon:
    def __init__(self, root):
        self.root = root
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            port = listener.getsockname()[1]
        subprocess.run([str(binaries / ('agentdrived' + exe)), 'init', '--directory', str(root), '--listen', f'127.0.0.1:{port}'], capture_output=True, text=True, encoding='utf-8', check=True)
        config_path = root / 'agentdrive.json'
        self.endpoint = f'http://127.0.0.1:{port}'

        self.client = [str(binaries / ('agentdrive' + exe)), '--endpoint', f'http://127.0.0.1:{port}/mcp']
        self.log = (root / 'daemon.log').open('w')
        self.process = subprocess.Popen([str(binaries / ('agentdrived' + exe)), 'serve', '--config', str(config_path)], stdout=self.log, stderr=self.log)
        for _ in range(100):
            if self.process.poll() is not None:
                raise AssertionError((root / 'daemon.log').read_text(encoding='utf-8'))
            try:
                with socket.create_connection(('127.0.0.1', port), timeout=0.1):
                    return
            except OSError:
                time.sleep(0.05)
        self.stop()
        raise AssertionError('daemon startup timed out')

    def call(self, tool, request, request_id=None):
        command = self.client + ['call', tool, '--json', json.dumps(request)]
        if request_id:
            command += ['--request-id', request_id]
        output = subprocess.run(command, capture_output=True, text=True, encoding='utf-8', timeout=120)
        assert output.returncode == 0, (output.stdout, output.stderr, (self.root / 'daemon.log').read_text(encoding='utf-8'))
        return json.loads(output.stdout)

    def api(self, path, payload=None, method='GET'):
        request = urllib.request.Request(self.endpoint + path, method=method,
            data=None if payload is None else json.dumps(payload).encode(),
            headers={'Content-Type': 'application/json'})
        deadline = time.monotonic() + 30
        while True:
            try:
                with urllib.request.urlopen(request, timeout=10) as response:
                    return json.load(response)
            except urllib.error.HTTPError as error:
                failure = json.loads(error.read())
                if not failure.get('error', {}).get('retryable') or time.monotonic() >= deadline:
                    raise AssertionError((path, failure, (self.root / 'daemon.log').read_text(encoding='utf-8'))) from error
                time.sleep(0.1)

    def create_workspace(self, name):
        result = self.api('/api/workspaces', {'request_id': str(uuid.uuid4()), 'name': name, 'storage': profile}, 'POST')
        return result['result']['workspace']['id']

    def stop(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.log.close()

with tempfile.TemporaryDirectory(prefix='agentdrive-shared-') as temporary:
    daemons = []
    try:
        first = Daemon(Path(temporary) / 'first')
        daemons.append(first)
        workspace = first.create_workspace('shared')
        source = first.root / 'exchange' / 'incoming'
        source.mkdir()
        payload = bytes(range(251)) * 84000
        (source / 'multipart').write_bytes(payload)
        imported = first.call('workspace_import', {'workspace': workspace, 'source': str(source)}, 'import')
        branch = imported['result']['branch']
        published = first.call('sync', {'workspace': workspace, 'direction': 'push'}, 'publish')
        assert not published.get('error'), published

        # A second CLI process and the dashboard query the same daemon.
        status = first.call('workspace_status', {'workspace': workspace})
        assert any(row['id'] == branch['id'] for row in status['branches'])
        listing = first.api(f"/api/workspaces/{workspace}/branches/{branch['id']}/files?path=/")
        assert any(entry['path'] == '/multipart' for entry in listing['entries']), listing
        database = first.root / 'state' / 'meta.db'
        with sqlite3.connect(database) as db:
            refs = db.execute('SELECT count(*) FROM published_refs WHERE workspace_id=?', (workspace,)).fetchone()[0]
            assert refs > 0

        # Use the same destination in an independent database. Its namespace is private.
        independent = Daemon(Path(temporary) / 'independent')
        daemons.append(independent)
        another = independent.create_workspace('independent')
        assert independent.call('workspace_status', {'workspace': another})['workspace']['id'] == another
        visible = independent.call('workspace_list', {})
        assert workspace not in json.dumps(visible), visible
        # Remove the disposable confirmed cache entry to exercise read-through from S3.
        with sqlite3.connect(database) as db:
            [(cached_hash,)] = db.execute(
                "SELECT hash FROM objects WHERE workspace_id=? AND size=? AND remote_confirmed=1",
                (workspace, len(payload))).fetchall()
        (first.root / 'state' / 'objects' / workspace / cached_hash).unlink()
        destination = first.root / 'exchange' / 'restored'
        exported = first.call('workspace_export', {'workspace': workspace, 'revision': branch['formal_head'],
                                                   'destination': str(destination)}, 'export')
        assert not exported.get('error'), exported
        assert (destination / 'multipart').read_bytes() == payload
        print('Shared CLI and dashboard views, multipart bytes, and independent database namespaces passed')
    finally:
        for daemon in reversed(daemons):
            daemon.stop()
