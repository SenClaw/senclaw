#!/usr/bin/env python3
"""Experiment 4 — "only this one website", after the loopback fix.

Seatbelt cannot filter by host (measured: its parser refuses anything but `*`
and `localhost`), so per-site egress cannot be an OS rule. The pattern that does
work: an allowlisting proxy on loopback, the sandbox given `loopback:[proxy]`
and NO direct egress. Anything that ignores the proxy hits the sandbox wall, so
the failure mode is closed rather than open.

This runs a real CONNECT proxy on the host and measures all three paths.
"""
import http.server
import os
import socket
import socketserver
import sys
import threading
import urllib.parse

sys.path.insert(0, os.path.dirname(__file__))
from harness import api, create, sh, probe, cleanup, dump  # noqa: E402

ALLOWED = "example.com"
DENIED = "www.wikipedia.org"
PROXY_PORT = 8899


class AllowlistProxy(http.server.BaseHTTPRequestHandler):
    """Minimal HTTPS (CONNECT) proxy that only tunnels to ALLOWED."""

    protocol_version = "HTTP/1.1"

    def log_message(self, *a):  # keep the experiment output readable
        pass

    def do_CONNECT(self):  # noqa: N802 - stdlib naming
        host, _, port = self.path.partition(":")
        if host != ALLOWED:
            self.send_error(403, "not on the allowlist")
            return
        try:
            up = socket.create_connection((host, int(port or 443)), timeout=8)
        except OSError as e:
            self.send_error(502, str(e))
            return
        self.send_response(200, "Connection established")
        self.end_headers()
        self._pump(self.connection, up)

    @staticmethod
    def _pump(a, b):
        def copy(src, dst):
            try:
                while chunk := src.recv(65536):
                    dst.sendall(chunk)
            except OSError:
                pass
            finally:
                for s in (src, dst):
                    try:
                        s.shutdown(socket.SHUT_RDWR)
                    except OSError:
                        pass
        t = threading.Thread(target=copy, args=(a, b), daemon=True)
        t.start()
        copy(b, a)
        t.join(timeout=5)


class Threaded(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


srv = Threaded(("127.0.0.1", PROXY_PORT), AllowlistProxy)
threading.Thread(target=srv.serve_forever, daemon=True).start()
print(f"allowlist proxy on 127.0.0.1:{PROXY_PORT}, allowing only {ALLOWED}")

sid = create("exp4-proxy", network=False, fsMode="strict", timeoutMs=90000)
try:
    # No direct egress at all; the proxy is the sandbox's only door out.
    print(api(f"/sandboxes/{sid}/ports", "POST",
              {"listen": [], "connect": [], "loopback": [PROXY_PORT]}))

    proxy = f"http://127.0.0.1:{PROXY_PORT}"
    e, out, err = sh(sid, f"curl -s -o /dev/null -m 15 -w '%{{http_code}}' "
                          f"--proxy {proxy} https://{ALLOWED}/ 2>&1", 40000)
    probe("one-site", f"through the proxy, the allowed site works ({ALLOWED})",
          True, out.strip().startswith(("2", "3")), out.strip()[-60:])

    e, out, err = sh(sid, f"curl -s -o /dev/null -m 15 -w '%{{http_code}}' "
                          f"--proxy {proxy} https://{DENIED}/ 2>&1", 40000)
    probe("one-site", f"through the proxy, any other site is refused ({DENIED})",
          False, out.strip().startswith(("2", "3")), f"http_code={out.strip()[-12:]}")

    e, out, err = sh(sid, f"curl -s -o /dev/null -m 10 -w '%{{http_code}}' "
                          f"https://{ALLOWED}/ 2>&1", 30000)
    probe("one-site", "bypassing the proxy fails — the sandbox has no direct egress",
          False, out.strip().startswith(("2", "3")), f"http_code={out.strip()[-12:]}")

    # With no `connect` ports the sandbox gets no resolver either, so hostnames
    # cannot even be looked up — DNS tunnelling is off the table.
    e, out, err = sh(sid, "python3 -c \"import socket;socket.gethostbyname('example.com')\" 2>&1", 25000)
    probe("one-site", "the sandbox cannot resolve names on its own", False, e == 0,
          (out or err).strip()[-70:])
finally:
    cleanup(sid)
    srv.shutdown()

dump(os.path.join(os.path.dirname(__file__), "exp4_results.json"))
