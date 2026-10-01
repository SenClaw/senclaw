#!/usr/bin/env python3
"""The fixture site: static files from a directory, on loopback.

Same request log as `python3 -m http.server` (the approval check reads it),
plus one knob a static server lacks: `?delay=<ms>` holds a response back, so
a page can have a subresource that keeps its `load` event waiting the way ad
and tracker scripts do on real sites. Threaded, so a delayed file never
blocks the page that references it.

usage: serve.py <port> <directory>
"""
import functools
import sys
import time
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit


class Handler(SimpleHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        delay = parse_qs(urlsplit(self.path).query).get("delay", ["0"])[0]
        if delay.isdigit() and int(delay) > 0:
            time.sleep(min(int(delay), 30_000) / 1000)
        super().do_GET()

    def end_headers(self):
        # Every run sees the fixture as written, never a cached copy.
        self.send_header("Cache-Control", "no-store")
        super().end_headers()


def main():
    port, directory = int(sys.argv[1]), sys.argv[2]
    handler = functools.partial(Handler, directory=directory)
    with ThreadingHTTPServer(("127.0.0.1", port), handler) as server:
        server.serve_forever()


if __name__ == "__main__":
    main()
