#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Install the compiler's pinned Deno into an explicit external directory.

    python3 scripts/setup-deno.py <directory outside the source tree>

The archive deps/deno.json pins is downloaded from Deno's release and must
match its SHA-256, or nothing is installed. A download that fails in a way a
retry can fix (HTTP 403, 429 or 5xx, which GitHub's release storage answers
now and then, a connection that fails or drops, a timeout) is tried again
after 2, 4, 8 and 16 seconds, five tries in all; any other failure, and an
archive that does not match its checksum, stops at once. A mismatch is never
retried: a different archive is refused, however often it is offered.
scripts/setup-deno-test.py tests this against a stand-in server.
"""
import hashlib
import http.client
import io
import json
import os
from pathlib import Path
import platform
import sys
import time
import urllib.error
import urllib.request
import zipfile

ROOT = Path(__file__).resolve().parents[1]
LIMIT = 100_000_000
# The wait before each try after the first.
DELAYS = (2, 4, 8, 16)


def transient(error):
    """Whether a failed download may succeed if tried again."""
    if isinstance(error, urllib.error.HTTPError):
        return error.code in (403, 429) or 500 <= error.code <= 599
    return isinstance(error, (urllib.error.URLError, http.client.HTTPException, TimeoutError, ConnectionError))


def download(url, sleep=time.sleep, delays=DELAYS):
    """The bytes at `url`, read to at most LIMIT + 1, after at most
    len(delays) + 1 tries."""
    tries = len(delays) + 1
    for attempt in range(1, tries + 1):
        try:
            with urllib.request.urlopen(url, timeout=60) as response:
                return response.read(LIMIT + 1)
        except Exception as error:  # noqa: BLE001 -- sorted by transient()
            if not transient(error):
                raise SystemExit(f'Deno archive download failed: {error}')
            if attempt == tries:
                raise SystemExit(f'Deno archive download failed {tries} times, the last with: {error}')
            delay = delays[attempt - 1]
            print(f'setup-deno.py: try {attempt} of {tries} failed ({error}); trying again in {delay} s',
                  file=sys.stderr)
            sleep(delay)
    raise AssertionError('unreachable')


def install(url, sha256, destination, sleep=time.sleep, delays=DELAYS):
    """Download, verify and install the archive's `deno`; its path."""
    archive = download(url, sleep, delays)
    if len(archive) > LIMIT or hashlib.sha256(archive).hexdigest() != sha256:
        raise SystemExit('Deno archive checksum mismatch')
    with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
        binary = bundle.read('deno')
    destination.mkdir(parents=True, exist_ok=True)
    file = destination / 'deno'
    temporary = destination / f'deno.{os.getpid()}.tmp'
    with temporary.open('xb') as out:
        out.write(binary)
    temporary.chmod(0o755)
    temporary.replace(file)
    return file


def main(argv):
    pin = json.loads((ROOT / 'deps/deno.json').read_text())
    arch = {'x86_64': 'x86_64', 'AMD64': 'x86_64', 'aarch64': 'aarch64', 'arm64': 'aarch64'}.get(platform.machine())
    os_name = {'Linux': 'unknown-linux-gnu', 'Darwin': 'apple-darwin'}.get(platform.system())
    if not arch or not os_name:
        raise SystemExit('supported compiler hosts: Linux/macOS on x86_64/aarch64')
    target = f'{arch}-{os_name}'
    if len(argv) != 2:
        raise SystemExit('usage: scripts/setup-deno.py <directory outside the source tree>')
    destination = Path(argv[1]).expanduser().resolve()
    if destination.is_relative_to(ROOT):
        raise SystemExit('install runtime outside the source tree')
    url = f'https://github.com/denoland/deno/releases/download/v{pin["version"]}/deno-{target}.zip'
    print(install(url, pin['archives'][target], destination))


if __name__ == '__main__':
    main(sys.argv)
