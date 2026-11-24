#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Install the historical SQLite-capable bd used by the M5 differential test.
# The current bd release uses Dolt only, so its output is not this test's oracle.
set -euo pipefail

if [ "$#" -ne 1 ] || [ -z "$1" ]; then
    echo "usage: scripts/bd-sqlite-pin.sh <destination-directory>" >&2
    exit 2
fi

destination="$1"
case "$(uname -s):$(uname -m)" in
    Linux:x86_64)
        platform=linux_amd64
        archive_sha=e94b09e0b6a9324bbc0e81ea36bccaaa42172a926bfedfb389e9a26dedb63184
        ;;
    Darwin:x86_64)
        platform=darwin_amd64
        archive_sha=88f665ee7b03a7768b7b11b597929df1edbf91864cec63ba56da3890609f5bbd
        ;;
    Darwin:arm64)
        platform=darwin_arm64
        archive_sha=26c15b93d82be45c3fa9b6318ffee9a6ae89841ba7ec4ad5c8db6fe6e42d3d3a
        ;;
    *)
        echo "no verified SQLite bd asset for $(uname -s):$(uname -m)" >&2
        exit 1
        ;;
esac
archive_name="beads_0.50.3_${platform}.tar.gz"
url="https://github.com/gastownhall/beads/releases/download/v0.50.3/${archive_name}"

mkdir -p "$destination"
archive="$destination/$archive_name"
curl -fL --retry 2 --max-time 90 "$url" -o "$archive"
if command -v sha256sum >/dev/null 2>&1; then
    actual_sha="$(sha256sum "$archive" | cut -d ' ' -f 1)"
else
    actual_sha="$(shasum -a 256 "$archive" | cut -d ' ' -f 1)"
fi
if [ "$actual_sha" != "$archive_sha" ]; then
    echo "pinned SQLite bd archive checksum mismatch: $actual_sha" >&2
    exit 1
fi
tar -xzf "$archive" -C "$destination" bd
chmod 0755 "$destination/bd"
version="$("$destination/bd" version)"
if [ "$version" != 'bd version 0.50.3 (bd25acbc: HEAD@bd25acbcb1e7)' ]; then
    echo "pinned SQLite bd reports unexpected version: $version" >&2
    exit 1
fi
echo "$destination/bd"
