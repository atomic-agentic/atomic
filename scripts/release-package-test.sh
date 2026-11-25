#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Tests for the armed-binary check in scripts/release-package.sh: a binary
# with the crash tests' failpoints, or the test-only compiler dispatch,
# compiled in is refused before anything is packaged, and one without them
# gets past the check. A stand-in for cargo
# stops the script right after it; no toolchain is needed.
#
#   scripts/release-package-test.sh
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)" || exit 1
trap 'rm -rf "${WORK}"' EXIT
failures=0
TRIPLE="x86_64-unknown-linux-gnu"

mkdir -p "${WORK}/bin" "${WORK}/repo/scripts" "${WORK}/repo/target/${TRIPLE}/release"
cp "${ROOT}/scripts/release-package.sh" "${WORK}/repo/scripts/"
printf '#!/bin/sh\necho "the stand-in cargo was reached" >&2\nexit 3\n' >"${WORK}/bin/cargo"
chmod +x "${WORK}/bin/cargo"
BIN="${WORK}/repo/target/${TRIPLE}/release/atomic"

# package <binary content>: sets `code` and `out`.
package() {
    printf '%s' "$1" >"${BIN}"
    chmod +x "${BIN}"
    out="$(PATH="${WORK}/bin:${PATH}" "${WORK}/repo/scripts/release-package.sh" "${TRIPLE}" 2>&1)"
    code=$?
}

package $'\x7fELF...ATOMIC_FAILPOINT_LOG...'
if [ "${code}" -eq 0 ] || ! grep -q "has crash-test failpoints compiled in" <<<"${out}" ||
    grep -q "stand-in cargo was reached" <<<"${out}"; then
    echo "FAIL an armed binary was not refused first: exit ${code}: ${out}" >&2
    failures=$((failures + 1))
fi

package $'\x7fELF...ATOMIC_COMPILER_TEST_DISPATCH...'
if [ "${code}" -eq 0 ] || ! grep -q "has the compiler-in-tests feature compiled in" <<<"${out}" ||
    grep -q "stand-in cargo was reached" <<<"${out}"; then
    echo "FAIL a binary with the test compiler dispatch was not refused first: exit ${code}: ${out}" >&2
    failures=$((failures + 1))
fi

package $'\x7fELF...an ordinary release binary...'
if grep -q "compiled in" <<<"${out}" || ! grep -q "stand-in cargo was reached" <<<"${out}"; then
    echo "FAIL an unarmed binary did not get past the check: exit ${code}: ${out}" >&2
    failures=$((failures + 1))
fi

if [ "${failures}" -ne 0 ]; then
    exit 1
fi
# Exercise the normal ZIP and SBOM paths with four distinguishable fixture
# binaries. Only cargo's package ID and CycloneDX metadata are supplied.
set -e
repo="${WORK}/repo"
TRIPLE=x86_64-pc-windows-msvc
release="${repo}/target/${TRIPLE}/release"
mkdir -p "${release}" "${repo}/crates/atomic" "${repo}/crates/atomic-core/assets" "${repo}/deps"
cp "${ROOT}/scripts/sbom.sh" "${repo}/scripts/"
cp "${ROOT}/crates/atomic-core/assets/manifest.json" "${repo}/crates/atomic-core/assets/"
cp "${ROOT}"/{LICENSE,NOTICE,THIRD-PARTY-NOTICES.md} "${repo}/"
cp "${ROOT}"/crates/atomic-core/assets/*LICENSE* "${repo}/crates/atomic-core/assets/"
touch "${repo}/deps/forks.toml"
for program in atomic codex-command-runner codex-windows-sandbox-setup codex-windows-sandbox-service; do
    echo "fixture: ${program}" >"${release}/${program}.exe"
    chmod +x "${release}/${program}.exe"
done
cat >"${WORK}/bin/cargo" <<'SH'
#!/bin/sh
case "$1" in
    pkgid) echo 'atomic-agentic@0.1.0';;
    cyclonedx) echo '{"metadata":{"component":{"bom-ref":"pkg:atomic","components":[{"name":"atomic","type":"application"}]}},"components":[],"dependencies":[]}' >atomic-agentic.cdx.json;;
    *) exit 1;;
esac
SH
export SOURCE_DATE_EPOCH=1700000000
export PATH="${WORK}/bin:${PATH}"
"${repo}/scripts/release-package.sh" "${TRIPLE}"
dist="${repo}/target/dist"
prefix="atomic-0.1.0-${TRIPLE}"
cp "${dist}/${prefix}.zip" "${WORK}/first.zip"
"${repo}/scripts/release-package.sh" "${TRIPLE}"
cmp "${WORK}/first.zip" "${dist}/${prefix}.zip"
(cd "${dist}" && sha256sum -c ./*.sha256)
python3 -I - "${dist}" "${prefix}" <<'PYZIP'
import hashlib, json, pathlib, sys, zipfile
dist, prefix = pathlib.Path(sys.argv[1]), sys.argv[2]
doc = json.loads((dist / (prefix + ".cdx.json")).read_text())
app, = doc["metadata"]["component"]["components"]
files = {c["name"]: c for c in doc["components"] if c["name"].endswith(".exe")}
assert set(files) == {"codex-command-runner.exe", "codex-windows-sandbox-setup.exe", "codex-windows-sandbox-service.exe"}
files["atomic.exe"] = app
with zipfile.ZipFile(dist / (prefix + ".zip")) as archive:
    for name, component in files.items():
        payload = archive.read(prefix + "/" + name)
        assert payload == ("fixture: " + name[:-4] + "\n").encode(), name
        assert component["hashes"] == [{"alg": "SHA-256", "content": hashlib.sha256(payload).hexdigest()}], name
    for name in ("LICENSE", "NOTICE", "THIRD-PARTY-NOTICES.md"):
        assert archive.read(prefix + "/" + name)
    assert any(n.startswith(prefix + "/licenses/") for n in archive.namelist())
PYZIP
echo "release-package tests passed" >&2
