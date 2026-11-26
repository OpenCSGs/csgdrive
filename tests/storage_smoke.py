"""Verify workspace storage selection, immutable destinations and history after restart."""
import argparse
import copy
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid

from fixtures.codes_storage import codes_storage

parser = argparse.ArgumentParser()
parser.add_argument('--bin-dir', default='target/debug')
args = parser.parse_args()
binaries = Path(args.bin_dir).resolve()
exe = '.exe' if os.name == 'nt' else ''
endpoint = os.environ['AGENTDRIVE_TEST_S3_ENDPOINT']

with tempfile.TemporaryDirectory(prefix='agentdrive-storage-') as temporary:
    root = Path(temporary)
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        port = listener.getsockname()[1]
    subprocess.run([str(binaries / ('agentdrived' + exe)), 'init', '--directory', str(root), '--listen', f'127.0.0.1:{port}'], check=True, capture_output=True)
    config_path = root / 'agentdrive.json'

    client = [str(binaries / ('agentdrive' + exe)), '--endpoint', f'http://127.0.0.1:{port}/mcp']
    # Changing to XNet checks the Python module and the Codes access token. This
    # test reads an existing regular Git object and never invokes an XNet upload.
    modules = root / 'test-python'
    modules.mkdir()
    (modules / 'hf_xet.py').write_text('', encoding='utf-8')
    process_env = {**os.environ, 'AGENTDRIVE_XNET_PYTHON': sys.executable, 'PYTHONPATH': os.pathsep.join([str(modules), os.environ.get('PYTHONPATH', '')])}
    log = (root / 'daemon.log').open('w')
    daemon = None

    def settings(payload=None):
        request = urllib.request.Request(f'http://127.0.0.1:{port}/api/settings',
            data=json.dumps(payload).encode() if payload else None,
            headers={'Content-Type': 'application/json'})
        request.method = 'PUT' if payload else 'GET'
        try:
            with urllib.request.urlopen(request, timeout=180) as response:
                return response.status, json.load(response)
        except urllib.error.HTTPError as error:
            return error.code, json.load(error)

    def create_workspace(payload):
        request = urllib.request.Request(f'http://127.0.0.1:{port}/api/workspaces',
            data=json.dumps(payload).encode(), headers={'Content-Type': 'application/json'})
        with urllib.request.urlopen(request, timeout=90) as response:
            return json.load(response)

    def save(current, runtime):
        status, response = settings({'revision': current['revision'], 'runtime': runtime})
        assert status == 200, response
        return response

    def call(tool, request):
        command = client + ['call', tool, '--json', json.dumps(request)]
        if tool != 'workspace_status':
            command += ['--request-id', str(uuid.uuid4())]
        result = subprocess.run(command, capture_output=True, text=True, encoding='utf-8', timeout=120)
        assert result.returncode == 0, (tool, result.stderr, result.stdout)
        result = json.loads(result.stdout)
        assert not result.get('error'), result
        return result

    def start():
        process = subprocess.Popen([str(binaries / ('agentdrived' + exe)), 'serve', '--config', str(config_path)], stdout=log, stderr=log, env=process_env)
        for _ in range(100):
            try:
                if settings()[0] == 200:
                    return process
            except OSError:
                pass
            if process.poll() is not None:
                raise AssertionError((root / 'daemon.log').read_text(encoding='utf-8'))
            time.sleep(0.05)
        process.terminate()
        process.wait(timeout=20)
        raise AssertionError('daemon startup timed out')

    try:
        daemon = start()
        profile = {'backend': 's3', 'bucket': 'agentdrive-integration', 'prefix': f'storage-tests/{uuid.uuid4()}', 'region': 'us-east-1', 'endpoint': endpoint, 'allow_http': True,
                   'credentials': {'access_key_id': os.environ['AWS_ACCESS_KEY_ID'], 'secret_access_key': os.environ['AWS_SECRET_ACCESS_KEY']}}
        request = {'name': 'workspace history', 'request_id': str(uuid.uuid4()), 'storage': profile}
        workspace = create_workspace(request)['result']['workspace']['id']
        assert create_workspace(request)['result']['workspace']['id'] == workspace
        source = root / 'exchange' / 'source'
        source.mkdir()
        payload = bytes(range(256)) * 4096
        (source / 'image.bin').write_bytes(payload)
        branch = call('workspace_import', {'workspace': workspace, 'source': str(source)})['result']['branch']
        revision = branch['formal_head']
        local_request = {'request_id': str(uuid.uuid4()), 'name': 'private notes', 'storage': None}
        local = create_workspace(local_request)['result']['workspace']['id']
        assert create_workspace(local_request)['result']['workspace']['id'] == local
        own_store = copy.deepcopy(profile)
        own_store['prefix'] = f'workspace-tests/{uuid.uuid4()}'
        shared = create_workspace({'request_id': str(uuid.uuid4()), 'name': 'shared files',
                                   'storage': own_store})['result']['workspace']['id']
        assert call('workspace_status', {'workspace': shared})['capabilities']['remote_configured']
        assert not call('workspace_status', {'workspace': local})['capabilities']['remote_configured']
        call('sync', {'workspace': shared, 'direction': 'push'})
        current = settings()[1]
        assert current['runtime']['workspace_remotes'].get(local) is None
        assert current['runtime']['workspace_remotes'][shared]['prefix'] == own_store['prefix']
        assert os.environ['AWS_SECRET_ACCESS_KEY'] not in json.dumps(current)
        assert call('workspace_status', {'workspace': workspace})['capabilities']['remote_configured']
        call('sync', {'workspace': workspace, 'direction': 'push'})
        # Rotate to environment credentials for the same destination.
        runtime = copy.deepcopy(current['runtime'])
        runtime['workspace_remotes'][workspace].pop('credentials')
        current = save(current, runtime)
        before = config_path.read_bytes()
        for target, replacement in [
            (workspace, None),
            (workspace, {**profile, 'prefix': 'another/directory'}),
            (workspace, {**profile, 'bucket': 'another-bucket'}),
            (workspace, {'backend': 'opencsg', 'repository': 'team/files', 'endpoint': 'https://example.invalid', 'token': 'test-token'}),
            (local, profile),
        ]:
            runtime = copy.deepcopy(current['runtime'])
            runtime['workspace_remotes'][target] = replacement
            status, failure = settings({'revision': current['revision'], 'runtime': runtime})
            assert status == 409 and 'fixed at creation' in str(failure), (status, failure)
            assert config_path.read_bytes() == before
        assert 'workspace_remotes' not in json.loads(before)
        with sqlite3.connect(root / 'state' / 'meta.db') as db:
            routes = {key: json.loads(value) for key, value in
                      db.execute('SELECT workspace,config FROM workspace_storage')}
            assert routes[shared]['prefix'] == own_store['prefix']
            assert routes[workspace]['bucket'] == profile['bucket']
            assert db.execute('SELECT count(*) FROM metadata_objects').fetchone()[0] > 0
        daemon.terminate()
        daemon.wait(timeout=30)
        daemon = start()
        assert settings()[1]['runtime']['workspace_remotes'][shared]['credentials']['secret_access_key'] == ''
        destination = root / 'exchange' / 'after-restart'
        call('workspace_export', {'workspace': workspace, 'revision': revision, 'destination': str(destination)})
        assert (destination / 'image.bin').read_bytes() == payload
        assert not call('workspace_status', {'workspace': local})['capabilities']['remote_configured']
        assert call('workspace_status', {'workspace': shared})['capabilities']['remote_configured']
        with codes_storage() as (codes_endpoint, fetched):
            codes_profile = {'backend': 'opencsg', 'endpoint': codes_endpoint, 'repository': 'test/storage',
                             'token': 'smoke-test', 'allow_http': True, 'xnet': False}
            codes = create_workspace({'request_id': str(uuid.uuid4()), 'name': 'codes files',
                                      'storage': codes_profile})['result']['workspace']['id']
            codes_branch = call('workspace_import', {'workspace': codes, 'source': str(source)})['result']['branch']
            call('sync', {'workspace': codes, 'direction': 'push'})
            current = settings()[1]
            runtime = copy.deepcopy(current['runtime'])
            runtime['workspace_remotes'][codes]['xnet'] = True
            saved = save(current, runtime)
            assert saved['runtime']['workspace_remotes'][codes]['xnet']
            with sqlite3.connect(root / 'state' / 'meta.db') as db:
                [(file_id, placement)] = db.execute(
                    'SELECT hash,format FROM file_placements WHERE workspace=?', (codes,)).fetchall()
                assert placement == 'git_chunks'
            (root / 'state' / 'objects' / codes / file_id).unlink()
            destination = root / 'exchange' / 'after-policy-change'
            call('workspace_export', {'workspace': codes, 'revision': codes_branch['formal_head'],
                                      'destination': str(destination)})
            assert (destination / 'image.bin').read_bytes() == payload
            assert any('/chunks/' in path for path in fetched), fetched
        print('Codes write policy changed through the settings API; previous Git contents restored')
        print('Workspace creation, immutable storage, credential refresh and history after restart passed')
    finally:
        if daemon and daemon.poll() is None:
            daemon.terminate()
            try:
                daemon.wait(timeout=30)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait()
        log.close()
