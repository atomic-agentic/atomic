#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Reject stale source or changed bundled compiler assets without running npm."""
import hashlib
import json
from pathlib import Path
root = Path(__file__).resolve().parents[1]
assets = root / 'crates/atomic-core/assets'
manifest = json.loads((assets / 'manifest.json').read_text())
for base, entries in [(root,manifest['sources']),(assets,manifest['assets'])]:
    for name, expected in entries.items():
        if hashlib.sha256((base/name).read_bytes()).hexdigest() != expected:
            raise SystemExit(f'{name}: Astro compiler assets are stale; run npm run build in authoring/')
print('Astro compiler sources and bundled assets match')
