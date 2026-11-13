#!/usr/bin/env python3
"""Connect a host stdio MCP client to a running Sandbox desktop."""
import argparse
import select
import socket
import sys


def connect(path):
    channel = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        channel.settimeout(10)
        channel.connect(path)
        channel.sendall(b'CONNECT 7000\n')
        response = bytearray()
        while not response.endswith(b'\n'):
            byte = channel.recv(1)
            if not byte or len(response) >= 128:
                raise ConnectionError('Invalid Sandbox connection response')
            response.extend(byte)
        if not response.startswith(b'OK '):
            raise ConnectionError('Sandbox refused the MCP connection')
        channel.settimeout(None)
        return channel
    except BaseException:
        channel.close()
        raise


def forward(channel, source, destination):
    inputs = [source, channel]
    while True:
        ready, _, _ = select.select(inputs, [], [])
        for current in ready:
            if current is channel:
                data = channel.recv(65536)
                if not data:
                    return
                destination.write(data)
                destination.flush()
            else:
                data = source.read1(65536)
                if data:
                    channel.sendall(data)
                else:
                    inputs.remove(source)
                    channel.shutdown(socket.SHUT_WR)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--socket', required=True, help='Absolute path to the instance vsock.sock')
    args = parser.parse_args()
    try:
        with connect(args.socket) as channel:
            forward(channel, sys.stdin.buffer, sys.stdout.buffer)
    except (OSError, ConnectionError) as error:
        print(f'Sandbox MCP connection failed: {error}. Ensure the Sandbox is running.', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
