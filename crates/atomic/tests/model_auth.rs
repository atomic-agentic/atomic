// SPDX-License-Identifier: Apache-2.0

//! Model authentication for local use, through the `atomic` executable:
//!
//! - `atomic model status` says who is signed in, the email masked, from
//!   atomic's own sign-in store and never with a token, and creates no state
//!   directory to say it;
//! - `atomic model logout` forgets the sign-in in the file store and the
//!   OS keyring, whichever `ATOMIC_CREDENTIAL_STORE` now chooses, and says
//!   what it found;
//! - with no endpoint and no sign-in, a run of a model atom says to sign in
//!   or configure an endpoint; signed in, with no model, it says to set one;
//! - signed in, a lite atom reports that the sign-in is for Codex atoms only
//!   and what it needs instead, before it calls anything;
//! - the sign-in is personal: on a shared (PostgreSQL) store, and for
//!   `atomic serve`, it is refused, with what to configure instead.
//!
//! A Codex atom running on the sign-in, with no API key set, is tested in
//! process (atomic_core's runtime tests), against the scripted fake model
//! standing in for ChatGPT's Codex backend; one through `atomic run`, which
//! needs the sandbox, is in model_auth_sandbox.rs. The test machine has no
//! OS keyring. Linux only, where sign-ins are kept.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

#[path = "model_auth/fixture.rs"]
mod fixture;

use fixture::{Fixture, text};

#[test]
fn login_status_says_who_is_signed_in_and_logout_forgets_it() {
    let f = Fixture::new("status");
    let none = f.atomic(&["model", "status"], &[]);
    assert!(!none.status.success());
    assert!(
        text(&none).contains("no one is signed in"),
        "{}",
        text(&none)
    );
    // No token to revoke: logging out below calls no issuer.
    f.sign_in_unrevocable();
    let status = f.atomic(&["model", "status"], &[]);
    let t = text(&status);
    assert!(status.status.success(), "{t}");
    assert!(
        t.contains("signed in with chatgpt as d***@example.invalid (Plus plan)"),
        "{t}"
    );
    assert!(
        !t.contains("synthetic-access-token"),
        "a token was printed: {t}"
    );
    assert!(!t.contains("dev@example"), "the email was printed: {t}");
    let json: serde_json::Value =
        serde_json::from_slice(&f.atomic(&["--json", "model", "status"], &[]).stdout)
            .expect("json");
    assert_eq!(json["signed_in"], true);
    assert_eq!(json["account"]["mode"], "chatgpt");
    assert_eq!(json["account"]["store"], "the credential file store");
    assert_eq!(json["account"]["email"], "d***@example.invalid");
    assert!(!json.to_string().contains("dev@example"), "{json}");
    let out = f.atomic(&["model", "logout"], &[]);
    assert!(text(&out).contains("signed out"), "{}", text(&out));
    assert!(!f.atomic(&["model", "status"], &[]).status.success());
}

#[test]
fn model_status_creates_no_state_directory_and_no_store() {
    let f = Fixture::new("stateless");
    let home = f.dir.join("atomic-home");
    let out = f.atomic(&["model", "status"], &[]);
    assert!(text(&out).contains("no one is signed in"), "{}", text(&out));
    assert!(
        !home.exists(),
        "status created {:?}",
        std::fs::read_dir(&home).map(|d| d.flatten().map(|e| e.path()).collect::<Vec<_>>())
    );
}

#[test]
fn logout_forgets_the_file_copy_whichever_store_is_chosen() {
    let f = Fixture::new("logout-stores");
    let kept = f.dir.join("atomic-home/credentials/codex-auth/auth.json");
    for store in ["file", "keyring"] {
        f.sign_in_unrevocable();
        assert!(kept.exists());
        let out = f.atomic(&["model", "logout"], &[("ATOMIC_CREDENTIAL_STORE", store)]);
        let t = text(&out);
        assert!(out.status.success(), "{store}: {t}");
        assert!(!kept.exists(), "{store}: the file copy was kept: {t}");
        assert!(
            t.contains("forgot the sign-in in the credential file store"),
            "{store}: {t}"
        );
        // This machine has no OS keyring, and logging out says so.
        assert!(
            t.contains("the OS keyring could not be checked"),
            "{store}: {t}"
        );
    }
}

#[test]
fn a_shared_store_or_serve_refuses_the_personal_sign_in() {
    let f = Fixture::new("shared");
    f.sign_in();
    let model = [("ATOMIC_MODEL", "synthetic")];
    // PostgreSQL, unreachable: refused before it would connect.
    let out = f.atomic(
        &["run", "tidy-codex", "--data", r#"{"text": "a note"}"#],
        &[
            model[0],
            (
                "ATOMIC_DATABASE_URL",
                "postgres://atomic:atomic@127.0.0.1:9/atomic",
            ),
            ("ATOMIC_EXECUTOR_ID", "host-1"),
        ],
    );
    let t = text(&out);
    assert!(!out.status.success(), "{t}");
    assert!(t.contains("the store is shared"), "{t}");
    assert!(t.contains("ATOMIC_MODEL_BASE_URL"), "{t}");
    // `atomic serve`, on the project's own SQLite store.
    let out = f
        .atomic_within(&["serve"], &model, 60)
        .expect("serve started with the sign-in");
    let t = text(&out);
    assert!(!out.status.success(), "{t}");
    assert!(t.contains("`atomic serve`"), "{t}");
    assert!(t.contains("ATOMIC_MODEL_BASE_URL"), "{t}");
}

#[test]
fn a_model_atom_with_no_model_access_says_how_to_get_it() {
    let f = Fixture::new("none");
    let data = ["--data", r#"{"text": "a note"}"#];
    let out = f.atomic(&["run", "tidy-codex", data[0], data[1]], &[]);
    let t = text(&out);
    assert!(!out.status.success());
    assert!(t.contains("atomic model login"), "{t}");
    assert!(t.contains("ATOMIC_MODEL_BASE_URL"), "{t}");
    f.sign_in();
    let out = f.atomic(&["run", "tidy-codex", data[0], data[1]], &[]);
    assert!(!out.status.success());
    assert!(text(&out).contains("set ATOMIC_MODEL"), "{}", text(&out));
}

#[test]
fn signed_in_a_lite_atom_says_what_it_needs() {
    let f = Fixture::new("lite");
    f.sign_in();
    let out = f.atomic(
        &["run", "tidy-lite", "--data", r#"{"text": "a note"}"#],
        &[("ATOMIC_MODEL", "synthetic")],
    );
    let t = text(&out);
    assert!(!out.status.success(), "{t}");
    assert!(t.contains("serves Codex atoms only"), "{t}");
    assert!(t.contains("ATOMIC_MODEL_BASE_URL"), "{t}");
}

#[test]
fn a_command_atomic_runs_cannot_read_its_environment() {
    let f = Fixture::new("environ");
    let out = f.atomic(
        &["run", "peek", "--data", r#"{"text": "x"}"#],
        &[("ATOMIC_TEST_CANARY", "canary-in-atomics-environment")],
    );
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(
        !t.contains("canary-in-atomics-environment"),
        "read through /proc: {t}"
    );
    assert!(t.contains("refused"), "{t}");
}
