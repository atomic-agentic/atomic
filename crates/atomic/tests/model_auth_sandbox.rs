// SPDX-License-Identifier: Apache-2.0

//! Model authentication through `atomic run` with a Codex thread, which
//! reads its instructions through Codex's Linux sandbox helper: a configured
//! endpoint wins over a ChatGPT sign-in and is sent none of its token, and
//! an empty base URL is no endpoint.
//!
//! Needs bubblewrap and unprivileged user namespaces, so it runs with the
//! `sandbox-tests` feature (and `ATOMIC_DEV_SANDBOX=on` in the development
//! container); with the feature on, a sandbox that cannot start fails it.

#![cfg(all(target_os = "linux", feature = "sandbox-tests"))]
#![allow(clippy::unwrap_used)]

mod support;

#[path = "model_auth/fixture.rs"]
mod fixture;

use fixture::{Fixture, text};

#[test]
fn a_configured_endpoint_wins_and_gets_no_sign_in_token() {
    support::require_sandbox();
    use atomic_core::testing::{FakeModel, Reply, on_codex_runtime};
    let f = Fixture::new("precedence");
    f.sign_in();
    on_codex_runtime(async move {
        let model = FakeModel::start(vec![Reply::Text(r#"{"line": "tidied"}"#.into())])
            .await
            .unwrap();
        let base = model.base_url();
        let f = std::sync::Arc::new(f);
        let run = f.clone();
        let out = tokio::task::spawn_blocking(move || {
            run.atomic(
                &["run", "tidy-codex", "--data", r#"{"text": "a note"}"#],
                &[
                    ("ATOMIC_MODEL_BASE_URL", base.as_str()),
                    ("ATOMIC_MODEL", "synthetic"),
                ],
            )
        })
        .await
        .unwrap();
        assert!(out.status.success(), "{}", text(&out));
        assert!(model.calls() >= 1);
        for headers in model.headers() {
            for (name, value) in headers {
                assert!(
                    !value.contains("synthetic-access-token"),
                    "the sign-in's token went to the endpoint in {name}"
                );
            }
        }
        // And an empty base URL is no endpoint: the sign-in serves, and asks
        // for a model.
        let out = f.atomic(
            &["run", "tidy-codex", "--data", r#"{"text": "a note"}"#],
            &[("ATOMIC_MODEL_BASE_URL", "")],
        );
        assert!(text(&out).contains("set ATOMIC_MODEL"), "{}", text(&out));
    });
}
