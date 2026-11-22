#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Tests for scripts/setup-deno.py, against a stand-in release server on
loopback. No network and no real Deno archive are needed.

    python3 scripts/setup-deno-test.py
"""
import hashlib
import http.server
import importlib.util
import io
import os
from pathlib import Path
import sys
import tempfile
import threading
import zipfile

ROOT = Path(__file__).resolve().parents[1]
failures = []


def load(path):
    spec = importlib.util.spec_from_file_location('setup_deno', path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def archive(body):
    data = io.BytesIO()
    with zipfile.ZipFile(data, 'w') as bundle:
        bundle.writestr('deno', body)
    return data.getvalue()


GOOD = archive(b'#!/bin/sh\necho deno\n')
GOOD_SHA = hashlib.sha256(GOOD).hexdigest()
OTHER = archive(b'#!/bin/sh\necho not deno\n')


class Server:
    """Answers each GET with the next of `answers`, an HTTP status or bytes
    to send with 200; the last answer repeats. `requests` counts them."""

    def __init__(self, answers):
        self.answers = list(answers)
        self.requests = 0
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                answer = outer.answers[min(outer.requests, len(outer.answers) - 1)]
                outer.requests += 1
                if isinstance(answer, int):
                    self.send_error(answer)
                    return
                self.send_response(200)
                self.send_header('Content-Length', str(len(answer)))
                self.end_headers()
                self.wfile.write(answer)

            def log_message(self, *args):
                pass

        self.httpd = http.server.HTTPServer(('127.0.0.1', 0), Handler)
        self.url = f'http://127.0.0.1:{self.httpd.server_port}/deno.zip'
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()


def attempt(module, answers):
    """Run install() against a server giving `answers`; the outcome, the
    number of requests and the waits it slept."""
    server = Server(answers)
    slept = []
    with tempfile.TemporaryDirectory() as tmp:
        try:
            file = module.install(server.url, GOOD_SHA, Path(tmp) / 'bin', sleep=slept.append)
            outcome = ('installed', file.read_bytes())
        except SystemExit as stop:
            outcome = ('refused', str(stop))
        finally:
            server.close()
    return outcome, server.requests, slept


def check(what, got, want):
    if got != want:
        failures.append(f'{what}: got {got!r}, wanted {want!r}')


def run(module):
    outcome, requests, slept = attempt(module, [403, 403, GOOD])
    check('403 twice, then the archive', (outcome[0], requests, slept), ('installed', 3, [2, 4]))
    check('the installed binary', outcome[1], b'#!/bin/sh\necho deno\n')

    outcome, requests, slept = attempt(module, [429, 503, 500, GOOD])
    check('429 and 5xx, then the archive', (outcome[0], requests, slept), ('installed', 4, [2, 4, 8]))

    outcome, requests, slept = attempt(module, [403])
    check('always 403', (outcome[0], requests, slept), ('refused', 5, [2, 4, 8, 16]))
    check('always 403, what it says', 'failed 5 times' in outcome[1], True)

    outcome, requests, slept = attempt(module, [OTHER])
    check('a different archive', (outcome, requests, slept), (('refused', 'Deno archive checksum mismatch'), 1, []))

    outcome, requests, slept = attempt(module, [403, OTHER, GOOD])
    check('a different archive after a retry', (outcome, requests), (('refused', 'Deno archive checksum mismatch'), 2))

    outcome, requests, slept = attempt(module, [404, GOOD])
    check('404 is not retried', (outcome[0], requests, slept), ('refused', 1, []))

    # A server that is not there: the connection is refused on every try.
    server = Server([GOOD])
    url = server.url
    server.close()
    slept = []
    with tempfile.TemporaryDirectory() as tmp:
        try:
            module.install(url, GOOD_SHA, Path(tmp) / 'bin', sleep=slept.append)
            got = 'installed'
        except SystemExit as stop:
            got = 'refused' if 'failed 5 times' in str(stop) else str(stop)
    check('no server', (got, slept), ('refused', [2, 4, 8, 16]))


def main():
    # The stand-in is on loopback: no proxy may stand between.
    for name in ('http_proxy', 'HTTP_PROXY', 'https_proxy', 'HTTPS_PROXY', 'all_proxy', 'ALL_PROXY'):
        os.environ.pop(name, None)
    script = ROOT / 'scripts' / 'setup-deno.py'
    run(load(script))
    found = list(failures)
    failures.clear()
    # Each rule is tested by removing it: with the checksum check or the retry
    # taken out of a copy of the script, a case above fails.
    text = script.read_text()
    for name, old, new in [
        ('checksum', "hashlib.sha256(archive).hexdigest() != sha256", 'False'),
        ('retry', 'if not transient(error):', 'if True:'),
    ]:
        if old not in text:
            found.append(f'the {name} rule to remove is not in {script}')
            continue
        with tempfile.TemporaryDirectory() as tmp:
            copy = Path(tmp) / 'setup-deno.py'
            copy.write_text(text.replace(old, new, 1))
            run(load(copy))
        if not failures:
            found.append(f'with the {name} rule removed, every case still passed')
        failures.clear()
    for failure in found:
        print(f'FAIL {failure}', file=sys.stderr)
    if found:
        sys.exit(f'scripts/setup-deno-test.py: {len(found)} failed')
    print('scripts/setup-deno-test.py: all passed')


if __name__ == '__main__':
    main()
