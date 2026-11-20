#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Package a built release binary with its licences and its SBOM.
#
#   cargo build --release --locked -p atomic-agentic --target <triple>
#   scripts/release-package.sh <triple>
#
# Writes to target/dist/:
#
#   atomic-<version>-<triple>.tar.gz          the binary, LICENSE, NOTICE and
#                                             THIRD-PARTY-NOTICES.md
#   atomic-<version>-<triple>.cdx.json        the CycloneDX SBOM, which records
#                                             the binary's SHA-256
#   <each of the above>.sha256                its checksum, in sha256sum format
#
# The archive is deterministic: entries are sorted, owned by 0:0, and stamped
# with SOURCE_DATE_EPOCH (the commit time when unset), and gzip records no
# name or time. So one binary always packages to one archive, and the SBOM's
# hash names the binary inside it.
#
# Needs cargo-cyclonedx and python3.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="${1:?usage: scripts/release-package.sh <target-triple>}"
BIN="${ROOT}/target/${TARGET}/release/atomic"
[ -x "${BIN}" ] || { echo "scripts/release-package.sh: ${BIN} is not built" >&2; exit 1; }
# A binary built with the crash tests' failpoints features compiled in could
# be killed by an environment variable. Never ship one.
if grep -qa "ATOMIC_FAILPOINT" "${BIN}"; then
    echo "scripts/release-package.sh: ${BIN} has crash-test failpoints compiled in; build it on its own, as above" >&2
    exit 1
fi

VERSION="$(cd "${ROOT}" && cargo pkgid -p atomic-agentic)"
VERSION="${VERSION##*@}"
NAME="atomic-${VERSION}-${TARGET}"
DIST="${ROOT}/target/dist"
mkdir -p "${DIST}"

if [ -z "${SOURCE_DATE_EPOCH:-}" ]; then
    SOURCE_DATE_EPOCH="$(git -C "${ROOT}" log -1 --format=%ct)"
fi
export SOURCE_DATE_EPOCH

"${ROOT}/scripts/sbom.sh" --target "${TARGET}" --binary "${BIN}"
cp "${ROOT}/target/sbom/atomic-agentic.cdx.json" "${DIST}/${NAME}.cdx.json"

python3 -I - "${ROOT}" "${BIN}" "${DIST}" "${NAME}" "${SOURCE_DATE_EPOCH}" <<'PY'
import gzip, hashlib, io, os, sys, tarfile
root, binary, dist, name, epoch = sys.argv[1:6]
epoch = int(epoch)
members = [("atomic", binary, 0o755)] + [
    (f, os.path.join(root, f), 0o644)
    for f in ("LICENSE", "NOTICE", "THIRD-PARTY-NOTICES.md")
]
license_dir = os.path.join(root, "crates", "atomic-core", "assets")
for filename in sorted(os.listdir(license_dir)):
    if "LICENSE" in filename or filename.endswith("LEGAL.txt"):
        members.append(("licenses/" + filename, os.path.join(license_dir, filename), 0o644))
buf = io.BytesIO()
with tarfile.open(fileobj=buf, mode="w", format=tarfile.PAX_FORMAT) as tar:
    top = tarfile.TarInfo(name)
    top.type, top.mode, top.mtime = tarfile.DIRTYPE, 0o755, epoch
    top.uname = top.gname = ""
    tar.addfile(top)
    for member, source, mode in sorted(members):
        data = open(source, "rb").read()
        info = tarfile.TarInfo(f"{name}/{member}")
        info.size, info.mode, info.mtime = len(data), mode, epoch
        info.uid = info.gid = 0
        info.uname = info.gname = ""
        tar.addfile(info, io.BytesIO(data))
archive = os.path.join(dist, f"{name}.tar.gz")
with open(archive, "wb") as raw:
    with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0, compresslevel=9) as gz:
        gz.write(buf.getvalue())
for f in (f"{name}.tar.gz", f"{name}.cdx.json"):
    digest = hashlib.sha256(open(os.path.join(dist, f), "rb").read()).hexdigest()
    with open(os.path.join(dist, f + ".sha256"), "w") as out:
        out.write(f"{digest}  {f}\n")
    print(f"{digest}  {f}")
PY
