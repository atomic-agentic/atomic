# SPDX-License-Identifier: Apache-2.0
#
# atomic's development image: the Rust toolchain Codex pins (deps/codex.toml),
# the system libraries Codex needs on Linux, and the tools atomic's checks use.
# scripts/dev.sh builds it when missing and tags it with a hash of this file,
# so any change here builds a new image.

FROM rust:1.95.0-slim-trixie@sha256:e14e87345b4d5964ddcc3491d27ee046a0f23820f340c3c1e24da6880141f7c0

# The container runs as the invoking user, who cannot write to RUSTUP_HOME, so
# every component rust-toolchain.toml asks for must already be installed.
RUN rustup component add clippy rustfmt rust-src

# Codex's Linux build needs a C toolchain and the libcap, OpenSSL and D-Bus
# (keyring) headers; its sandbox uses bubblewrap. git is needed by tests that
# make real commits; psql is for inspecting the test database; python3 is for
# the release packaging scripts.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential pkg-config libcap-dev libssl-dev libdbus-1-dev bubblewrap git ca-certificates curl xz-utils \
        postgresql-client python3 \
    && rm -rf /var/lib/apt/lists/*

ARG TARGETARCH

# Prebuilt releases, each verified against a pinned SHA-256.
ARG CARGO_DENY_VERSION=0.21.0
ARG COSIGN_VERSION=3.1.4
# Matches the bd that manages this repository's beads, so the container never
# migrates the bead database to a newer format.
ARG BD_VERSION=1.3.2
# Writes the CycloneDX SBOM for a release.
ARG CYCLONEDX_VERSION=0.5.10

RUN set -eu; \
    case "${TARGETARCH:-amd64}" in \
      amd64) deny_arch=x86_64; go_arch=amd64; \
             cdx_sum=2e4ef64a53c0a38d4b7d805c593b5f47ecd29821e98d81e484d03abaa0edb95f; \
             deny_sum=a2170b854ff697e79e39c4c538981c9c1bdd803addbae6719a4cb66b4585a903; \
             cosign_sum=424e2c97d0b45fd7a6b608a87f090f35633f4c23ece5f092d48bba11aaf4bbfa; \
             bd_sum=c6ea3281f6ff82628351b2d0967c9eb85862c8a0ec0ebc24e63e645c8b02462e ;; \
      arm64) deny_arch=aarch64; go_arch=arm64; \
             cdx_sum=6931b234914fa9be8686f46bd1ce71358b4be6d4cd3ce6b7082bec2313e59a8e; \
             deny_sum=47cc5d88d43cd568028080fece02d729f56f0ff8e6c168563ce6589e3c0f4738; \
             cosign_sum=ade83b7aac62e131cacfe96d958948b2150cc9efec46a570a2649b8e27dedad2; \
             bd_sum=39b34a1246b7216031a98ae838c785a7dbba29cf9f50f109114ac462a4b85e7c ;; \
      *) echo "unsupported architecture: ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    cd /tmp; \
    deny="cargo-deny-${CARGO_DENY_VERSION}-${deny_arch}-unknown-linux-musl"; \
    curl -fsSLo deny.tar.gz "https://github.com/EmbarkStudios/cargo-deny/releases/download/${CARGO_DENY_VERSION}/${deny}.tar.gz"; \
    echo "${deny_sum}  deny.tar.gz" | sha256sum -c -; \
    tar -xzf deny.tar.gz -C /usr/local/bin --strip-components=1 "${deny}/cargo-deny"; \
    curl -fsSLo /usr/local/bin/cosign "https://github.com/sigstore/cosign/releases/download/v${COSIGN_VERSION}/cosign-linux-${go_arch}"; \
    echo "${cosign_sum}  /usr/local/bin/cosign" | sha256sum -c -; \
    chmod 0755 /usr/local/bin/cosign; \
    curl -fsSLo bd.tar.gz "https://github.com/gastownhall/beads/releases/download/v${BD_VERSION}/beads_${BD_VERSION}_linux_${go_arch}.tar.gz"; \
    echo "${bd_sum}  bd.tar.gz" | sha256sum -c -; \
    mkdir bd && tar -xzf bd.tar.gz -C bd && install -m 0755 "$(find bd -type f -name bd | head -n1)" /usr/local/bin/bd; \
    cdx="cargo-cyclonedx-${deny_arch}-unknown-linux-gnu"; \
    curl -fsSLo cdx.tar.xz "https://github.com/CycloneDX/cyclonedx-rust-cargo/releases/download/cargo-cyclonedx-${CYCLONEDX_VERSION}/${cdx}.tar.xz"; \
    echo "${cdx_sum}  cdx.tar.xz" | sha256sum -c -; \
    tar -xJf cdx.tar.xz -C /usr/local/bin --strip-components=1 "${cdx}/cargo-cyclonedx"; \
    rm -rf /tmp/*; \
    cargo-deny --version && cosign version >/dev/null && bd version && cargo-cyclonedx cyclonedx --version
