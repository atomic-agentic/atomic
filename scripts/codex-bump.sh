#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Move atomic's Codex pin to another Codex release tag, as the first step of a
# bump. Run it on a branch named codex-bump/<tag>, push, and start the Codex
# bump workflow on that branch by hand: it rebuilds the durare fork against
# the candidate's sqlx and reports which milestone suites pass.
#
#   scripts/codex-bump.sh rust-v0.170.0
#   scripts/codex-bump.sh rust-v0.170.0 <fork commit>
#
# When deps/codex.toml names a fork (a `branch`), rebase the fork's branch onto
# the new tag first, push it, and pass that commit as the second argument: the
# pin moves to the fork commit, and `upstream_rev` records the tag's commit.
#
# It rewrites, from Codex's files at the tag's commit:
#
#   deps/codex.toml        tag, rev, upstream_rev and toolchain
#   rust-toolchain.toml    the toolchain channel
#   Cargo.toml and crates/*/Cargo.toml
#                          the rev of every git dependency on Codex
#   Cargo.toml             the [patch] block (through scripts/codex-pin.sh write)
#   deps/forks.toml        the sqlx fork's rev, when Codex moved it
#   deps/forks.toml        the Codex fork's rev and upstream_base, with a fork
#   Cargo.lock             reseeded from Codex's lockfile; the next cargo
#                          command adds atomic's own packages to it
#
# It needs git and curl, not cargo, so it runs on a host or in the
# development container alike. It changes no file when the tag is unknown.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TAG="${1:?usage: scripts/codex-bump.sh <codex release tag> [fork commit]}"
PIN="${ROOT}/deps/codex.toml"

pin_value() { sed -n "s/^$1 = \"\\(.*\\)\"\$/\\1/p" "${PIN}" | head -n1; }
REPOSITORY="$(pin_value repository)"
UPSTREAM="$(pin_value upstream)"
[ -n "${UPSTREAM}" ] || UPSTREAM="${REPOSITORY}"
BRANCH="$(pin_value branch)"
OLD_TAG="$(pin_value tag)"
OLD_REV="$(pin_value rev)"
OLD_TOOLCHAIN="$(pin_value toolchain)"
OLD_UPSTREAM_REV="$(pin_value upstream_rev)"

# The commit the tag names (an annotated tag is peeled to its commit).
refs="$(git ls-remote "${UPSTREAM}" "refs/tags/${TAG}" "refs/tags/${TAG}^{}")"
TAG_REV="$(awk '/\^\{\}$/ { print $1 }' <<<"${refs}")"
[ -n "${TAG_REV}" ] || TAG_REV="$(awk '{ print $1; exit }' <<<"${refs}")"
if [ -z "${TAG_REV}" ]; then
    echo "scripts/codex-bump.sh: ${UPSTREAM} has no tag ${TAG}" >&2
    exit 1
fi
# With a fork, atomic builds the fork's commit on top of the tag.
if [ -n "${BRANCH}" ]; then
    REV="${2:?deps/codex.toml names a fork: rebase its ${BRANCH} branch onto ${TAG}, push it, and pass that commit as the second argument}"
else
    REV="${TAG_REV}"
fi

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT
for f in rust-toolchain.toml Cargo.lock Cargo.toml; do
    curl -fsSL --retry 3 -o "${WORK}/${f}" "https://raw.githubusercontent.com/${REPOSITORY#https://github.com/}/${REV}/codex-rs/${f}"
done
TOOLCHAIN="$(sed -n 's/^channel = "\(.*\)"$/\1/p' "${WORK}/rust-toolchain.toml")"
[ -n "${TOOLCHAIN}" ] || { echo "scripts/codex-bump.sh: no toolchain channel at ${REV}" >&2; exit 1; }
sqlx_rev() { sed -n 's/^sqlx-core = { git = "https:\/\/github.com\/transact-rs\/sqlx", rev = "\([0-9a-f]*\)".*/\1/p' "$1" | head -n1; }
OLD_SQLX="$(sqlx_rev "${ROOT}/Cargo.toml")"
SQLX="$(sqlx_rev "${WORK}/Cargo.toml")"

sed -i.bak \
    -e "s/^tag = \".*\"\$/tag = \"${TAG}\"/" \
    -e "s/^rev = \".*\"\$/rev = \"${REV}\"/" \
    -e "s/^upstream_rev = \".*\"\$/upstream_rev = \"${TAG_REV}\"/" \
    -e "s/^toolchain = \".*\"\$/toolchain = \"${TOOLCHAIN}\"/" "${PIN}"
sed -i.bak "s/^channel = \".*\"\$/channel = \"${TOOLCHAIN}\"/" "${ROOT}/rust-toolchain.toml"
for manifest in "${ROOT}/Cargo.toml" "${ROOT}"/crates/*/Cargo.toml; do
    sed -i.bak "\\|git = \"${REPOSITORY}\"|s/rev = \"${OLD_REV}\"/rev = \"${REV}\"/" "${manifest}"
done
# The sqlx fork atomic records follows Codex's.
if [ -n "${OLD_SQLX}" ] && [ -n "${SQLX}" ] && [ "${OLD_SQLX}" != "${SQLX}" ]; then
    sed -i.bak "s/${OLD_SQLX}/${SQLX}/" "${ROOT}/deps/forks.toml"
    rm -f "${ROOT}/deps/forks.toml.bak"
fi
# The Codex fork's entry follows the pin.
if [ -n "${BRANCH}" ]; then
    sed -i.bak -e "s/${OLD_REV}/${REV}/" -e "s/^upstream_base = \"${OLD_UPSTREAM_REV}\"/upstream_base = \"${TAG_REV}\"/" "${ROOT}/deps/forks.toml"
    rm -f "${ROOT}/deps/forks.toml.bak"
fi
rm -f "${PIN}.bak" "${ROOT}/rust-toolchain.toml.bak" "${ROOT}/Cargo.toml.bak" "${ROOT}"/crates/*/Cargo.toml.bak
CODEX_SRC="" "${ROOT}/scripts/codex-pin.sh" write >/dev/null
cp "${WORK}/Cargo.lock" "${ROOT}/Cargo.lock"

cat <<EOF
Codex ${OLD_TAG} (${OLD_REV:0:9}) -> ${TAG} (${REV:0:9})
toolchain ${OLD_TOOLCHAIN} -> ${TOOLCHAIN}
sqlx fork ${OLD_SQLX:0:9} -> ${SQLX:0:9}$([ "${OLD_SQLX}" = "${SQLX}" ] && echo " (unchanged)")

Next:
  - commit on a branch named codex-bump/${TAG}, push it and start the Codex
    bump workflow on it by hand (no tag); it rebuilds the durare fork at
    sqlx ${SQLX:0:9} and runs the milestone suites;
  - commit the Cargo.lock that workflow uploads, then fix what lock-check
    reports in deps/lock-exceptions.toml;
EOF
if [ "${OLD_TOOLCHAIN}" != "${TOOLCHAIN}" ]; then
    echo "  - move dev/Dockerfile's base image to Rust ${TOOLCHAIN} (by digest);"
fi
if [ "${OLD_SQLX}" != "${SQLX}" ]; then
    echo "  - move the durare fork's own sqlx entries to ${SQLX:0:9}, then its rev in deps/forks.toml;"
fi
echo "  - update deps/README.md's re-checked facts for the new tag."
