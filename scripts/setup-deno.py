#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Install the compiler's pinned Deno into an explicit external directory."""
import hashlib
import io
import json
from pathlib import Path
import platform
import sys
import urllib.request
import zipfile

root = Path(__file__).resolve().parents[1]
pin = json.loads((root / 'deps/deno.json').read_text())
arch = {'x86_64':'x86_64', 'AMD64':'x86_64', 'aarch64':'aarch64', 'arm64':'aarch64'}.get(platform.machine())
os_name = {'Linux':'unknown-linux-gnu', 'Darwin':'apple-darwin'}.get(platform.system())
if not arch or not os_name:
    raise SystemExit('supported compiler hosts: Linux/macOS on x86_64/aarch64')
target = f'{arch}-{os_name}'
destination = Path(sys.argv[1]).expanduser().resolve()
if destination.is_relative_to(root):
    raise SystemExit('install runtime outside the source tree')
url = f'https://github.com/denoland/deno/releases/download/v{pin["version"]}/deno-{target}.zip'
with urllib.request.urlopen(url, timeout=60) as response:
    archive = response.read(100_000_001)
if len(archive) > 100_000_000 or hashlib.sha256(archive).hexdigest() != pin['archives'][target]:
    raise SystemExit('Deno archive checksum mismatch')
with zipfile.ZipFile(io.BytesIO(archive)) as bundle:
    binary = bundle.read('deno')
destination.mkdir(parents=True, exist_ok=True)
file = destination / 'deno'
temporary = destination / f'deno.{__import__("os").getpid()}.tmp'
with temporary.open('xb') as out:
    out.write(binary)
temporary.chmod(0o755)
temporary.replace(file)
print(file)
