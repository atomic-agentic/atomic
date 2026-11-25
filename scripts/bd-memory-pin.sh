#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Install the official Dolt-backed bd used only for M5 memory JSONL interop.
# The historical SQLite bd pin remains the readiness differential oracle.
set -euo pipefail

if [ "$#" -ne 1 ] || [ -z "$1" ]; then
    echo "usage: scripts/bd-memory-pin.sh <destination-directory>" >&2
    exit 2
fi
if [ "$(uname -s):$(uname -m)" != Linux:x86_64 ]; then
    echo "no verified memory bd asset for $(uname -s):$(uname -m)" >&2
    exit 1
fi

destination="$1"
archive_name=beads_1.3.2_linux_amd64.tar.gz
archive_sha=c6ea3281f6ff82628351b2d0967c9eb85862c8a0ec0ebc24e63e645c8b02462e
url="https://github.com/gastownhall/beads/releases/download/v1.3.2/${archive_name}"
mkdir -p "$destination"
archive="$destination/$archive_name"
if [ -n "${ATOMIC_TEST_BD_MEMORY_ARCHIVE:-}" ]; then
    # Test seam: a supplied archive still has to match the same official digest.
    cp -- "$ATOMIC_TEST_BD_MEMORY_ARCHIVE" "$archive"
else
    curl -fsSL --retry 2 --max-time 90 "$url" -o "$archive"
fi
actual_sha="$(sha256sum "$archive" | cut -d ' ' -f 1)"
if [ "$actual_sha" != "$archive_sha" ]; then
    echo "pinned memory bd archive checksum mismatch: $actual_sha" >&2
    exit 1
fi
tar -xzf "$archive" -C "$destination" bd
chmod 0755 "$destination/bd"
version="$("$destination/bd" version)"
if [ "$version" != 'bd version 1.3.2 (9b65b9e19: HEAD@9b65b9e198ac)' ]; then
    echo "pinned memory bd reports unexpected version: $version" >&2
    exit 1
fi
echo "$destination/bd"
