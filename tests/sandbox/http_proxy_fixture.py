"""仅监听本机的代理验证服务，记录目标地址，不记录请求头或内容。"""
import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import select
import socket


class Proxy(BaseHTTPRequestHandler):
    def do_CONNECT(self):
        host, port = self.path.rsplit(':', 1)
        try:
            remote = socket.create_connection((host, int(port)), timeout=10)
        except OSError:
            self.send_error(502)
            return
        print('CONNECT', self.path, flush=True)
        self.send_response(200)
        self.end_headers()
        remote.settimeout(None)
        with remote:
            while True:
                readable, _, _ = select.select([self.connection, remote], [], [], 60)
                if not readable:
                    return
                for source in readable:
                    data = source.recv(65536)
                    if not data:
                        return
                    (remote if source is self.connection else self.connection).sendall(data)

    def log_message(self, *_args):
        pass


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--port', type=int, default=18089)
    args = parser.parse_args()
    ThreadingHTTPServer(('127.0.0.1', args.port), Proxy).serve_forever()
