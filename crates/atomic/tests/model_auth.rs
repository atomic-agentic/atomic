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
//! - the sign-in is personal: on a shared (PostgreSQL) store, however its
//!   URL is set, and for `atomic serve`, it is refused, with what to
//!   configure instead;
//! - a configured endpoint comes before the sign-in, and the sign-in before
//!   detection, as `atomic config --explain` says;
//! - a running atomic cannot be read through /proc by its own user.
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
    let out = f.atomic(&["model", "status"], &[]);
    assert!(text(&out).contains("no one is signed in"), "{}", text(&out));
    for home in [f.state_home(), f.credentials()] {
        assert!(
            !home.exists(),
            "status created {:?}",
            std::fs::read_dir(&home).map(|d| d.flatten().map(|e| e.path()).collect::<Vec<_>>())
        );
    }
}

#[test]
fn logout_forgets_the_file_copy_whichever_store_is_chosen() {
    let f = Fixture::new("logout-stores");
    let kept = f.credentials().join("codex-auth/auth.json");
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
        // The keyring's copy is looked for too: forgotten, absent, or not
        // reachable here, and said so.
        let json: serde_json::Value = serde_json::from_slice(
            &f.atomic(
                &["--json", "model", "logout"],
                &[("ATOMIC_CREDENTIAL_STORE", store)],
            )
            .stdout,
        )
        .expect("json");
        assert!(
            json["keyring"].is_boolean() || json["keyring_error"].is_string(),
            "{store}: {json}"
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
    // The same store named by the person's own configuration file.
    f.person_configures(
        "database:\n  url: postgres://atomic:atomic@127.0.0.1:9/atomic\nexecutor_id: host-1\n",
    );
    let out = f.atomic(
        &["run", "tidy-codex", "--data", r#"{"text": "a note"}"#],
        &model,
    );
    f.person_configures("runner: local\n");
    let t = text(&out);
    assert!(!out.status.success(), "{t}");
    assert!(t.contains("the store is shared"), "{t}");
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
fn where_the_sign_in_is_refused_nothing_takes_its_place_and_it_says_what_to_do() {
    let f = Fixture::new("refused-key");
    f.sign_in();
    // A key that detection would take, were no one signed in.
    let env = [
        ("ATOMIC_MODEL", "synthetic"),
        ("OPENAI_API_KEY", "sk-not-used"),
    ];
    let out = f
        .atomic_within(&["serve"], &env, 60)
        .expect("serve stopped");
    let t = text(&out);
    assert!(!out.status.success(), "it fell back to the key: {t}");
    assert!(
        t.contains("you are signed in with ChatGPT, and a sign-in is personal"),
        "{t}"
    );
    assert!(t.contains("nothing else is used in its place"), "{t}");
    assert!(t.contains("configure model.base_url"), "{t}");
    assert!(t.contains("OPENAI_API_KEY for OpenAI's API"), "{t}");
    assert!(t.contains("run `atomic model logout`"), "{t}");
    assert!(!t.contains("serving until stopped"), "{t}");
}

#[test]
fn a_configured_endpoint_comes_before_the_sign_in_and_the_sign_in_before_detection() {
    let f = Fixture::new("ranked");
    f.sign_in();
    let explain = |env: &[(&str, &str)]| {
        let out = f.atomic(&["config", "--explain"], env);
        assert!(out.status.success(), "{}", text(&out));
        text(&out)
    };
    // Signed in, with OPENAI_API_KEY set: the sign-in, and the key unused.
    let t = explain(&[("OPENAI_API_KEY", "sk-unused")]);
    assert!(t.contains("model.sign_in"), "{t}");
    assert!(t.contains("signed in with"), "{t}");
    assert!(!t.contains("not used: a model endpoint"), "{t}");
    assert!(t.contains("OPENAI_API_KEY is set and not used"), "{t}");
    assert!(!t.contains("detected: OPENAI_API_KEY"), "{t}");
    assert!(
        !t.contains("dev@example.invalid"),
        "the email was shown: {t}"
    );
    // A configured endpoint comes first, and the sign-in says it is unused.
    let t = explain(&[("ATOMIC_MODEL_BASE_URL", "http://127.0.0.1:9/v1")]);
    assert!(
        t.contains("not used: a model endpoint is configured"),
        "{t}"
    );
    // A shared store: the sign-in is shown as not used there.
    let t = explain(&[(
        "ATOMIC_DATABASE_URL",
        "postgres://atomic:atomic@127.0.0.1:9/atomic",
    )]);
    assert!(t.contains("not used: the store is shared"), "{t}");
}

#[test]
fn no_one_signed_in_to_a_keyring_that_cannot_be_reached_is_simply_not_signed_in() {
    let f = Fixture::new("no-keyring");
    let keyring = [
        ("ATOMIC_CREDENTIAL_STORE", "keyring"),
        ("OPENAI_API_KEY", "sk-detected"),
    ];
    // Never signed in, and then with the sign-in's directory there (as a
    // login that did not finish leaves it): the keyring, with no Secret
    // Service here, is not taken for a sign-in that cannot be read.
    for made in [false, true] {
        if made {
            std::fs::create_dir_all(f.credentials().join("codex-auth")).unwrap();
        }
        let out = f.atomic(&["config", "--explain"], &keyring);
        let t = text(&out);
        assert!(out.status.success(), "{t}");
        assert!(!t.contains("could not be read"), "{made}: {t}");
        assert!(t.contains("detected: OPENAI_API_KEY"), "{made}: {t}");
    }
}

#[test]
fn a_sign_in_inside_the_checkout_holding_the_project_is_refused_and_explain_says_so() {
    let f = Fixture::new("in-checkout");
    // The fixture's directory as a checkout, holding the project, and the
    // sign-in kept inside it.
    std::fs::create_dir_all(f.dir.join(".git")).unwrap();
    let inside = f.dir.join("atomic-home");
    let home = [("ATOMIC_HOME", inside.to_str().unwrap())];
    let out = f.atomic(&["model", "status"], &home);
    let t = text(&out);
    assert!(!out.status.success(), "{t}");
    assert!(t.contains("inside the checkout holding the project"), "{t}");
    // `atomic config` still shows the settings, and says why.
    let out = f.atomic(&["config", "--explain"], &home);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains("the ChatGPT sign-in could not be read"), "{t}");
    assert!(t.contains("inside the checkout holding the project"), "{t}");
}

/// Start `atomic args`, wait for a stderr line containing `ready`, then
/// read its environment through /proc from this test process: the same
/// user, not sandboxed, and a process whose own environment it can read.
fn read_while_running(f: &Fixture, args: &[&str], env: &[(&str, &str)], ready: &str) {
    use std::io::BufRead;
    let mut env = env.to_vec();
    env.push(("ATOMIC_TEST_CANARY", "canary-in-atomics-environment"));
    let mut child = f.spawn(args, &env);
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr)
            .lines()
            .map_while(Result::ok)
        {
            let _ = tx.send(line);
        }
    });
    let started = rx
        .iter()
        .find(|l| l.contains(ready) || l.starts_with("error"));
    let read = std::fs::read(format!("/proc/{}/environ", child.id()));
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        started.as_deref().is_some_and(|l| l.contains(ready)),
        "atomic {args:?} did not start: {started:?}"
    );
    assert!(std::fs::read("/proc/self/environ").is_ok());
    match read {
        Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{e}"),
        Ok(env) => panic!(
            "read atomic {args:?}'s environment through /proc ({} bytes, the canary {})",
            env.len(),
            if String::from_utf8_lossy(&env).contains("canary-in-atomics-environment") {
                "in it"
            } else {
                "not in it"
            }
        ),
    }
}

#[test]
fn a_running_atomic_cannot_be_read_through_proc_by_its_own_user() {
    let f = Fixture::new("hardened");
    // A command that starts Codex: atomic serve.
    read_while_running(
        &f,
        &["serve"],
        &[
            ("ATOMIC_MODEL_BASE_URL", "http://127.0.0.1:9/v1"),
            ("ATOMIC_MODEL", "synthetic"),
        ],
        "serving until stopped",
    );
    // One that does not, and holds a sign-in while it runs: the login,
    // waiting for the browser (which opens nothing here).
    read_while_running(
        &f,
        &["model", "login"],
        &[("BROWSER", "true")],
        "Sign in in your browser",
    );
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
fn model_commands_work_from_above_the_store_outside_any_project() {
    let f = Fixture::new("above");
    f.sign_in_unrevocable();
    // The working directory holds the credential store, and no project.
    let out = f.atomic_from(&f.outside, &["model", "status"], &[]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(t.contains("signed in with chatgpt"), "{t}");
}

#[test]
fn model_commands_load_no_codex_home_from_the_temporary_directory() {
    let f = Fixture::new("no-codex-home");
    f.sign_in_unrevocable();
    // Another user's planted .env, where an unowned Codex home would be.
    let tmp = f.dir.join("tmp");
    std::fs::create_dir_all(tmp.join("atomic-no-state-directory")).unwrap();
    std::fs::write(
        tmp.join("atomic-no-state-directory/.env"),
        "ATOMIC_CREDENTIAL_STORE=planted\n",
    )
    .unwrap();
    let out = f.atomic(&["model", "status"], &[("TMPDIR", tmp.to_str().unwrap())]);
    let t = text(&out);
    assert!(out.status.success(), "{t}");
    assert!(!t.contains("planted"), "a planted .env was loaded: {t}");
}
