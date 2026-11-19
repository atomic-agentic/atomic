#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Fetch and verify the prebuilt V8 archive and bindings deps/v8.toml pins for
# the V8 version in Cargo.lock, laid out as the local mirror rusty_v8's build
# script reads through RUSTY_V8_MIRROR.
#
#   scripts/v8-archive.sh [--mirror DIR] [TARGET]
#
# TARGET defaults to the host's triple. DIR defaults to target/v8-mirror, the
# path the workspace's .cargo/config.toml gives RUSTY_V8_MIRROR. A cached file
# is checked again on every run, so a tampered cache fails; a missing one is
# downloaded once from the release URL. With no V8 in Cargo.lock it does
# nothing. Prints the mirror directory.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MIRROR="${ROOT}/target/v8-mirror"
TARGET=""
while [ $# -gt 0 ]; do
    case "$1" in
        --mirror) MIRROR="$2"; shift 2 ;;
        -*) echo "usage: scripts/v8-archive.sh [--mirror DIR] [TARGET]" >&2; exit 2 ;;
        *) TARGET="$1"; shift ;;
    esac
done
if [ -z "${TARGET}" ]; then
    case "$(uname -s)-$(uname -m)" in
        Linux-x86_64) TARGET=x86_64-unknown-linux-gnu ;;
        Linux-aarch64 | Linux-arm64) TARGET=aarch64-unknown-linux-gnu ;;
        Darwin-arm64) TARGET=aarch64-apple-darwin ;;
        *) echo "scripts/v8-archive.sh: name the target triple for $(uname -s) $(uname -m)" >&2; exit 2 ;;
    esac
fi

# The built V8 version: v8 entries under [[package]] in Cargo.lock.
VERSION="$(awk '
    /^\[\[/ { in_package = ($0 == "[[package]]"); name = "" }
    in_package && /^name = / { gsub(/"/, "", $3); name = $3 }
    in_package && /^version = / && name == "v8" { gsub(/"/, "", $3); print $3 }
' "${ROOT}/Cargo.lock" | sort -u)"
if [ -z "${VERSION}" ]; then
    echo "${MIRROR}"
    exit 0
fi
if [ "$(wc -l <<<"${VERSION}")" -ne 1 ]; then
    echo "scripts/v8-archive.sh: Cargo.lock holds more than one v8 version; run scripts/lock-check.sh" >&2
    exit 1
fi

# "file sha256 binding binding_sha256" for this version and target.
ENTRY="$(awk -v version="${VERSION}" -v target="${TARGET}" '
    function flush() { if (crate == "v8" && v == version && t == target) print file, sha, binding, bsha }
    /^\[\[archive\]\]/ { flush(); crate = v = t = file = sha = binding = bsha = "" }
    /^crate = / { gsub(/"/, "", $3); crate = $3 }
    /^version = / { gsub(/"/, "", $3); v = $3 }
    /^target = / { gsub(/"/, "", $3); t = $3 }
    /^file = / { gsub(/"/, "", $3); file = $3 }
    /^sha256 = / { gsub(/"/, "", $3); sha = $3 }
    /^binding = / { gsub(/"/, "", $3); binding = $3 }
    /^binding_sha256 = / { gsub(/"/, "", $3); bsha = $3 }
    END { flush() }
' "${ROOT}/deps/v8.toml")"
if [ -z "${ENTRY}" ]; then
    echo "scripts/v8-archive.sh: deps/v8.toml pins no V8 ${VERSION} archive for ${TARGET}" >&2
    exit 1
fi
read -r FILE SHA BINDING BINDING_SHA <<<"${ENTRY}"

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# fetch NAME SHA256: the file at MIRROR/vVERSION/NAME, downloaded if missing,
# then verified.
fetch() {
    local name="$1" want="$2" dir="${MIRROR}/v${VERSION}" got
    mkdir -p "${dir}"
    if [ ! -f "${dir}/${name}" ]; then
        local url="https://github.com/denoland/rusty_v8/releases/download/v${VERSION}/${name}"
        echo "scripts/v8-archive.sh: downloading ${url}" >&2
        curl -fsSL --retry 3 -o "${dir}/${name}.tmp" "${url}"
        mv "${dir}/${name}.tmp" "${dir}/${name}"
    fi
    got="$(sha256 "${dir}/${name}")"
    if [ "${got}" != "${want}" ]; then
        echo "scripts/v8-archive.sh: ${dir}/${name} has SHA-256 ${got}; deps/v8.toml pins ${want}. Delete it to fetch it again." >&2
        return 1
    fi
}
fetch "${FILE}" "${SHA}"
fetch "${BINDING}" "${BINDING_SHA}"
echo "${MIRROR}"
