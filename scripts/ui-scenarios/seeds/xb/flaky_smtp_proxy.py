#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""Loopback-only helper for xb-*-retry scenarios. Usage: flaky_smtp_proxy.py [port=3925] [fail=1]
Listens on 127.0.0.1:<port>. The first <fail> connections are closed at once (an SMTP failure);
every later one is forwarded byte-for-byte to GreenMail SMTP on 127.0.0.1:3025. Prints one line
per connection. Start it fresh before each scenario run; kill it afterwards."""
import socket, sys, threading
port = int(sys.argv[1]) if len(sys.argv) > 1 else 3925
fail = int(sys.argv[2]) if len(sys.argv) > 2 else 1
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port)); srv.listen(16)
def pipe(a, b):
    try:
        while (d := a.recv(65536)): b.sendall(d)
    except OSError: pass
    for s in (a, b):
        try: s.shutdown(socket.SHUT_RDWR)
        except OSError: pass
n = 0
while True:
    c, _ = srv.accept(); n += 1
    if n <= fail:
        print(f"conn {n}: dropped", flush=True); c.close(); continue
    print(f"conn {n}: forwarded", flush=True)
    u = socket.create_connection(("127.0.0.1", 3025))
    threading.Thread(target=pipe, args=(c, u), daemon=True).start()
    threading.Thread(target=pipe, args=(u, c), daemon=True).start()
