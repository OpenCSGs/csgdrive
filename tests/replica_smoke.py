"""Verify two shipped daemons against a disposable, existing S3 test bucket."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
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

class Daemon:
    def __init__(self, root):
        self.root = root
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            port = listener.getsockname()[1]
        subprocess.run([str(binaries / ('agentdrived' + exe)), 'init', '--directory', str(root), '--listen', f'127.0.0.1:{port}'], capture_output=True, text=True, encoding='utf-8', check=True)
        config_path = root / 'agentdrive.json'
        config = json.loads(config_path.read_text(encoding='utf-8'))
        config['remote'] = {'bucket': 'agentdrive-integration', 'prefix': prefix, 'region': 'us-east-1', 'endpoint': endpoint, 'allow_http': True}
        config_path.write_text(json.dumps(config))
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

    def event(self, binding, turn, phase):
        return self.api('/api/collaboration/events', {'mount': binding['id'], 'source': 'replica-smoke',
            'thread_id': binding['workspace'], 'turn_id': turn, 'phase': phase}, 'POST')

    def collaboration(self, binding):
        states = self.api(f"/api/workspaces/{binding['workspace']}/collaboration")
        return next(item for item in states if item['branch'] == binding['branch'])

    def disable_collaboration(self):
        settings = self.api('/api/settings')
        settings['runtime']['collaboration']['enabled'] = False
        changed = self.api('/api/settings', {'revision': settings['revision'], 'runtime': settings['runtime']}, 'PUT')
        assert not changed['runtime']['collaboration']['enabled'], changed

    def stop(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.log.close()

def wait_for(description, predicate):
    deadline = time.monotonic() + 45
    last = None
    while time.monotonic() < deadline:
        try:
            last = predicate()
            if last:
                return
        except OSError as error:
            last = str(error)
        time.sleep(0.2)
    raise AssertionError((description, last))

def released(daemon, binding):
    state = daemon.collaboration(binding)
    return state['phase'] == 'following' and state['writer'] is None and state['remote_confirmed']

with tempfile.TemporaryDirectory(prefix='agentdrive-replica-') as temporary:
    daemons = []
    try:
        for name in ['one', 'two']:
            daemons.append(Daemon(Path(temporary) / name))
        first, second = daemons
        workspace = first.call('workspace_create', {'name': 'replica-integration'}, 'create')['result']['workspace']['id']
        source = first.root / 'exchange' / 'source'
        source.mkdir()
        payload = bytes(range(251)) * 84000
        (source / 'multipart').write_bytes(payload)
        imported = first.call('workspace_import', {'workspace': workspace, 'source': str(source)}, 'import')
        branch = imported['result']['branch']
        revision = branch['formal_head']
        first.call('sync', {'workspace': workspace, 'direction': 'push'}, 'push')
        receipt = first.call('operation_get', {'operation': imported['id']})
        assert receipt['remote_confirmed'], receipt
        second.call('sync', {'workspace': workspace, 'direction': 'pull'}, 'pull')
        destination = second.root / 'exchange' / 'export'
        second.call('workspace_export', {'workspace': workspace, 'revision': revision, 'destination': str(destination)}, 'export')
        assert hashlib.sha256((destination / 'multipart').read_bytes()).digest() == hashlib.sha256(payload).digest()
        status = second.call('workspace_status', {'workspace': workspace})
        current = next(item for item in first.call('workspace_status', {'workspace': workspace})['branches'] if item['id'] == branch['id'])
        guard = {'branch': current['id'], 'authority_epoch': current['authority_epoch'], 'generation': current['generation'], 'expected_head': current['formal_head'], 'mutation_seq': current['mutation_seq']}
        transfer = first.call('ownership_transfer', {'guard': guard, 'target': status['location']}, 'transfer')
        assert transfer['remote_confirmed'], transfer
        second.call('sync', {'workspace': workspace, 'direction': 'pull'}, 'adopt')
        adopted = next(item for item in second.call('workspace_status', {'workspace': workspace})['branches'] if item['id'] == branch['id'])
        assert adopted['owner'] == status['location'] and adopted['state'] == 'writable', adopted
        assert adopted['authority_epoch'] == current['authority_epoch'] + 1
        if status['capabilities']['platform_mount'] in ('linux-fuse', 'windows-winfsp', 'macos-macfuse', 'macos-fuse-t'):
            mounted_workspace = first.call('workspace_create', {'name': 'shared-mounts'}, 'mount-create')['result']['workspace']['id']
            mount_path = first.root / 'mounts' / 'shared'
            original = first.call('mount_prepare', {'workspace': mounted_workspace, 'path': str(mount_path)}, 'mount-root')['result']['binding']
            assert first.api('/api/settings')['runtime']['collaboration']['enabled']
            first.event(original, 'publish-root', 'started')
            contents = 'published root content'
            (mount_path / 'shared.txt').write_text(contents, encoding='utf-8')
            saved = first.call('mount_save', {'mount': original['id'], 'expected_binding_generation': original['generation'], 'durability': 'remote'}, 'save-root')
            assert saved['remote_confirmed'], saved
            first.event(original, 'publish-root', 'completed')
            wait_for('root publication and authority release', lambda: released(first, original))
            third = Daemon(Path(temporary) / 'three')
            daemons.append(third)
            mounts = [(first, original, mount_path)]
            for remote_daemon in (second, third):
                assert remote_daemon.api('/api/settings')['runtime']['collaboration']['enabled']
                remote_path = remote_daemon.root / 'mounts' / 'shared'
                request = {'workspace': mounted_workspace, 'path': str(remote_path)}
                mounted = remote_daemon.call('mount_prepare', request, 'mount-location')['result']['binding']
                assert mounted['branch'] == original['branch'], mounted
                assert (remote_path / 'shared.txt').read_text(encoding='utf-8') == contents
                repeated = remote_daemon.call('mount_prepare', request, 'mount-again')['result']['binding']
                assert repeated == mounted, (repeated, mounted)
                mounts.append((remote_daemon, mounted, remote_path))
            for index in (1, 2, 0):
                writer, binding, writer_path = mounts[index]
                turn = f'continue-{index}'
                writer.event(binding, turn, 'started')
                contents += f'\ncontinued at {writer.root.name}'
                (writer_path / 'shared.txt').write_text(contents, encoding='utf-8')
                writer.event(binding, turn, 'completed')
                wait_for(f'{turn} publication and authority release', lambda: released(writer, binding))
                for reader, _, reader_path in mounts:
                    wait_for(f'{reader.root.name} receives {turn}', lambda: (reader_path / 'shared.txt').read_text(encoding='utf-8') == contents)
                    local_status = reader.call('workspace_status', {'workspace': mounted_workspace})
                    assert local_status['selected_branch'] == original['branch'], local_status
                    assert {item['id'] for item in local_status['branches']} == {original['branch']}, local_status
            print('Default shared branch, three-location turn handoff and automatic S3 publication passed')

            for daemon, binding, _ in mounts:
                daemon.disable_collaboration()
                wait_for(f'{daemon.root.name} disables collaboration', lambda: daemon.collaboration(binding)['phase'] == 'disabled')
            mounted_workspace = first.call('workspace_create', {'name': 'manual-location-mounts'}, 'manual-create')['result']['workspace']['id']
            mount_path = first.root / 'mounts' / 'manual'
            original = first.call('mount_prepare', {'workspace': mounted_workspace, 'path': str(mount_path)}, 'manual-root')['result']['binding']
            (mount_path / 'shared.txt').write_text('published root content', encoding='utf-8')
            saved = first.call('mount_save', {'mount': original['id'], 'expected_binding_generation': original['generation'], 'durability': 'remote'}, 'manual-save-root')
            assert saved['remote_confirmed'], saved
            branches = {original['branch']}
            for remote_daemon in (second, third):
                remote_path = remote_daemon.root / 'mounts' / 'manual'
                request = {'workspace': mounted_workspace, 'path': str(remote_path)}
                mounted = remote_daemon.call('mount_prepare', request, 'manual-location')['result']['binding']
                assert mounted['branch'] not in branches, mounted
                branches.add(mounted['branch'])
                assert (remote_path / 'shared.txt').read_text(encoding='utf-8') == 'published root content'
                local_status = remote_daemon.call('workspace_status', {'workspace': mounted_workspace})
                assert local_status['selected_branch'] == mounted['branch'], local_status
                local_branch = next(item for item in local_status['branches'] if item['id'] == mounted['branch'])
                assert local_branch['owner'] == local_status['location'], local_branch
                assert local_branch['source_branch'] == original['branch'], local_branch
                assert local_branch['base_revision'] == saved['result']['revision'], local_branch
                repeated = remote_daemon.call('mount_prepare', request, 'manual-again')['result']['binding']
                assert repeated == mounted, (repeated, mounted)
                (remote_path / 'local.txt').write_text(local_status['location'], encoding='utf-8')
                local_saved = remote_daemon.call('mount_save', {'mount': mounted['id'], 'expected_binding_generation': mounted['generation'], 'durability': 'remote'}, 'manual-save-location')
                assert local_saved['remote_confirmed'], local_saved
                assert not (mount_path / 'local.txt').exists()
            print('Explicit global disable, isolated location branches and repeated binding reuse passed')
        print('Two-daemon S3 replication, multipart content, remote receipts and ownership transfer passed')
    finally:
        for daemon in reversed(daemons):
            daemon.stop()
