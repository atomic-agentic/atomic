#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Write the CycloneDX SBOM for the atomic binary and check that it lists every
# fork in deps/forks.toml at its pinned revision. It also lists the bundled
# Astro compiler payloads and the prebuilt V8 archive deps/v8.toml pins.
#
#   scripts/sbom.sh                       # writes target/sbom/atomic-agentic.cdx.json
#   scripts/sbom.sh --target <triple> --binary <path>
#
# With --target the SBOM lists the dependencies built for that target instead
# of the host's. With --binary it also records the SHA-256 of that built
# binary on the SBOM's `atomic` component, so the SBOM names the exact file it
# describes, and replaces the checkout's absolute path in component
# references with /atomic, so two checkouts of one commit give the same SBOM.
# SOURCE_DATE_EPOCH, when set, fixes the SBOM's timestamp.
#
# Needs cargo-cyclonedx (in the development image); --binary also needs python3.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${ROOT}/target/sbom"
TARGET=""
BINARY=""
while [ $# -gt 0 ]; do
    case "$1" in
        --target) TARGET="$2"; shift 2 ;;
        --binary) BINARY="$2"; shift 2 ;;
        *) echo "scripts/sbom.sh: unknown argument $1" >&2; exit 2 ;;
    esac
done
mkdir -p "${OUT}"

args=(--format json --spec-version 1.5)
[ -n "${TARGET}" ] && args+=(--target "${TARGET}")
if ! log="$(cd "${ROOT}/crates/atomic" && cargo cyclonedx "${args[@]}" 2>&1)"; then
    echo "${log}" >&2
    exit 1
fi
mv "${ROOT}/crates/atomic/atomic-agentic.cdx.json" "${OUT}/atomic-agentic.cdx.json"
# cargo-cyclonedx also writes one beside every other workspace crate.
rm -f "${ROOT}"/crates/*/atomic-agentic-*.cdx.json
SBOM="${OUT}/atomic-agentic.cdx.json"

# Include the non-Cargo libraries and compiler payloads shipped in the binary.
python3 -I - "${SBOM}" "${ROOT}" <<'PY_ASTRO'
import json, pathlib, sys
file, root = sys.argv[1:]
manifest = json.loads((pathlib.Path(root) / "crates/atomic-core/assets/manifest.json").read_text())
doc = json.loads(pathlib.Path(file).read_text())
refs = []
licenses = {"@astrojs/compiler":"MIT", "typescript":"Apache-2.0", "source-map-js":"BSD-3-Clause"}
for name, version in manifest["dependencies"].items():
    purl = f"pkg:npm/{name.replace('@', '%40')}@{version}"
    component = {"type":"library", "name":name, "version":version, "bom-ref":purl, "purl":purl, "licenses":[{"license":{"id":licenses[name]}}]}
    doc.setdefault("components", []).append(component)
    refs.append(purl)
for name in ("worker.mjs", "astro.wasm"):
    ref = "atomic:astro:" + name
    doc["components"].append({"type":"file", "name":name, "bom-ref":ref, "hashes":[{"alg":"SHA-256","content":manifest["assets"][name]}]})
    refs.append(ref)
owner = doc["metadata"]["component"]["bom-ref"]
dependency = next((item for item in doc.setdefault("dependencies", []) if item["ref"] == owner), None)
if dependency is None:
    dependency = {"ref":owner,"dependsOn":[]}
    doc["dependencies"].append(dependency)
dependency.setdefault("dependsOn", []).extend(refs)
pathlib.Path(file).write_text(json.dumps(doc, indent=2) + "\n")
print("SBOM includes pinned Astro/TypeScript libraries and bundled payload hashes")
PY_ASTRO

# The prebuilt V8 archive linked into the binary, which cargo metadata does not
# see: one file component per archive, with the hash deps/v8.toml pins.
V8_TARGET="${TARGET:-$(rustc -vV | sed -n 's/^host: //p')}"
python3 -I - "${SBOM}" "${ROOT}" "${V8_TARGET}" <<'PY_V8'
import json, pathlib, re, sys
file, root, target = sys.argv[1:]
lock = pathlib.Path(root, "Cargo.lock").read_text()
versions = {m.group(1) for m in re.finditer(r'\[\[package\]\]\nname = "v8"\nversion = "([^"]+)"', lock)}
if not versions:
    sys.exit(0)
archives = []
for block in pathlib.Path(root, "deps/v8.toml").read_text().split("[[archive]]")[1:]:
    fields = dict(re.findall(r'^(\w+) = "([^"]*)"', block, re.M))
    if fields.get("crate") == "v8" and fields.get("version") in versions and fields.get("target") == target:
        archives.append(fields)
if len(archives) != len(versions):
    sys.exit(f"scripts/sbom.sh: deps/v8.toml pins no V8 {sorted(versions)} archive for {target}")
doc = json.loads(pathlib.Path(file).read_text())
owner = doc["metadata"]["component"]["bom-ref"]
dependency = next(item for item in doc.setdefault("dependencies", []) if item["ref"] == owner)
for archive in archives:
    ref = f"atomic:v8:{archive['file']}"
    url = f"https://github.com/denoland/rusty_v8/releases/download/v{archive['version']}/{archive['file']}"
    doc.setdefault("components", []).append({"type": "file", "name": archive["file"], "version": archive["version"],
        "bom-ref": ref, "description": "prebuilt V8 static library linked by the v8 crate",
        "licenses": [{"license": {"id": "BSD-3-Clause"}}],
        "hashes": [{"alg": "SHA-256", "content": archive["sha256"]}],
        "externalReferences": [{"type": "distribution", "url": url}]})
    dependency.setdefault("dependsOn", []).append(ref)
    print(f"SBOM includes the V8 {archive['version']} archive for {target} (sha256 {archive['sha256']})")
pathlib.Path(file).write_text(json.dumps(doc, indent=2) + "\n")
PY_V8

if [ -n "${BINARY}" ]; then
    python3 -I - "${SBOM}" "${BINARY}" "${ROOT}" <<'PY'
import hashlib, json, sys
sbom, binary, root = sys.argv[1:4]
text = open(sbom, encoding="utf-8").read().replace("path+file://" + root, "path+file:///atomic")
doc = json.loads(text)
digest = hashlib.sha256(open(binary, "rb").read()).hexdigest()
bins = [c for c in doc["metadata"]["component"].get("components", [])
        if c.get("type") == "application" and c.get("name") == "atomic"]
if len(bins) != 1:
    sys.exit("scripts/sbom.sh: the SBOM has no single `atomic` binary component")
bins[0]["hashes"] = [{"alg": "SHA-256", "content": digest}]
with open(sbom, "w", encoding="utf-8") as f:
    json.dump(doc, f, indent=2, sort_keys=False)
    f.write("\n")
print(f"SBOM {sbom}: describes {binary} (sha256 {digest})")
PY
fi

status=0
checked=0
while IFS='|' read -r repo rev crates; do
    # cargo-cyclonedx records a git source as vcs_url=git+<repo>@<rev>, with
    # the + percent-encoded.
    want="vcs_url=git%2B${repo}%40${rev}"
    for crate in ${crates}; do
        if ! grep -q "\"pkg:cargo/${crate}@[^\"]*${want}\"" "${SBOM}"; then
            echo "scripts/sbom.sh: the SBOM does not list ${crate} from ${repo} at ${rev}" >&2
            status=1
        fi
        checked=$((checked + 1))
    done
done < <(awk '
    /^\[\[fork\]\]/ { if (repo != "") print repo "|" rev "|" crates; repo = ""; rev = ""; crates = "" }
    /^repository = / { gsub(/"/, "", $3); repo = $3 }
    /^rev = / { gsub(/"/, "", $3); rev = $3 }
    /^crates = / { line = $0; sub(/^crates = \[/, "", line); sub(/\].*$/, "", line); gsub(/[",]/, " ", line); crates = line }
    END { if (repo != "") print repo "|" rev "|" crates }
' "${ROOT}/deps/forks.toml")

[ "${status}" -eq 0 ] && echo "SBOM ${SBOM}: lists all ${checked} forked crates at their pinned revisions"
exit "${status}"
