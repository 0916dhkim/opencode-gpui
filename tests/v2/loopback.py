#!/usr/bin/env python3
"""Forward 127.0.0.1:PORT to HOST:TARGET_PORT inside a test container.

The client only allows plain HTTP to loopback (P1), while the harness server
is plain HTTP on the Docker network, so tests point the client at this
forwarder instead:

  loopback.py --ready FILE 14096 ocgtk-v2h-server 4096 &

The local port is 14096, never 4096/4097 (real servers on a developer host).
It exits non-zero if it cannot bind, and writes FILE once it listens, so a
caller can wait for FILE (or the process to die) instead of racing it.
"""
import socket
import sys
import threading


def pipe(source, destination):
    try:
        while True:
            chunk = source.recv(65536)
            if not chunk:
                break
            destination.sendall(chunk)
    except OSError:
        pass
    finally:
        for sock in (source, destination):
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass


def handle(client, host, port):
    try:
        upstream = socket.create_connection((host, port), timeout=10)
    except OSError:
        client.close()
        return
    upstream.settimeout(None)
    threading.Thread(target=pipe, args=(client, upstream), daemon=True).start()
    threading.Thread(target=pipe, args=(upstream, client), daemon=True).start()


def main():
    args = sys.argv[1:]
    ready = None
    if args[:1] == ["--ready"]:
        ready, args = args[1], args[2:]
    listen_port, host, port = int(args[0]), args[1], int(args[2])
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    # A reconnect test deliberately restarts this forwarder on the same port
    # while accepted connections may still be in TCP TIME_WAIT.
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        server.bind(("127.0.0.1", listen_port))
    except OSError as error:
        sys.exit(f"loopback: cannot bind 127.0.0.1:{listen_port}: {error}")
    server.listen(64)
    if ready:
        with open(ready, "w", encoding="utf-8") as stream:
            stream.write(f"{listen_port}\n")
    while True:
        client, _ = server.accept()
        handle(client, host, port)


if __name__ == "__main__":
    main()
