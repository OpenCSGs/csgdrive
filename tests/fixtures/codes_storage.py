"""In-process Codes API for the workspace storage smoke test."""
import base64
import contextlib
import json
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


@contextlib.contextmanager
def codes_storage():
    snapshots = {f'{0:040x}': {}}
    fetched = []
    lock = threading.Lock()

    class Handler(BaseHTTPRequestHandler):
        def reply(self, payload, content_type='application/json'):
            self.send_response(200)
            self.send_header('Content-Type', content_type)
            self.send_header('Content-Length', str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def do_GET(self):
            path = urllib.parse.urlsplit(self.path).path
            if self.headers.get('Authorization') != 'Bearer smoke-test':
                return self.send_error(403)
            with lock:
                if path == '/api/v1/codes/test/storage/branches':
                    head = next(reversed(snapshots))
                    self.reply(json.dumps({'data': [{'name': 'main', 'commit': {'id': head}}]}).encode())
                elif path == '/hf/codes/test/storage/xet-write-token/main':
                    info = {'accessToken': 'smoke', 'exp': int(time.time()) + 600,
                            'casUrl': f'http://127.0.0.1:{self.server.server_port}'}
                    self.reply(json.dumps(info).encode())
                elif path.startswith('/hf/codes/test/storage/resolve/'):
                    revision, key = path.removeprefix('/hf/codes/test/storage/resolve/').split('/', 1)
                    if key not in snapshots.get(revision, {}):
                        return self.send_error(404)
                    fetched.append(key)
                    self.reply(snapshots[revision][key], 'application/octet-stream')
                else:
                    self.send_error(404)

        def do_POST(self):
            if self.path != '/api/v1/codes/test/storage/commit/main':
                return self.send_error(404)
            if self.headers.get('Authorization') != 'Bearer smoke-test':
                return self.send_error(403)
            commit = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            with lock:
                files = snapshots[next(reversed(snapshots))].copy()
                for file in commit['files']:
                    files[file['path']] = base64.b64decode(file['content'], validate=True)
                snapshots[f'{len(snapshots):040x}'] = files
            self.reply(b'{"data":null}')

        def log_message(self, *_):
            pass

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f'http://127.0.0.1:{server.server_port}', fetched
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
