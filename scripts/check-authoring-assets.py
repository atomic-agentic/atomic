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
# A controls row that switches a check off inside the bundled worker names
# text the minifier wrote; a rebuild can rename it. Each such switch must
# still occur exactly once, or its control would no longer switch anything.
def control_rows():
    rows = sorted((root / 'scripts/controls').glob('*.row'))
    if not rows:
        raise SystemExit('scripts/controls holds no controls rows; the bundled worker\'s switches cannot be checked')
    for row in rows:
        text = row.read_text().strip()
        yield text.split('|', 1)[0], text
worker = (assets / 'worker.mjs').read_text()
for name, row in control_rows():
    fields = row.split('|')
    if len(fields) >= 3 and fields[1] == 'crates/atomic-core/assets/worker.mjs':
        count = worker.count(fields[2])
        if count != 1:
            raise SystemExit(f'control {name}: its switch occurs {count} times in the bundled worker, not once; update it after a rebuild')
print('Astro compiler sources and bundled assets match')
