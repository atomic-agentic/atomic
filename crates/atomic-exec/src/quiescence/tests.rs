// SPDX-License-Identifier: Apache-2.0

use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

/// A scratch directory holding a fake `/proc`, a durable directory and
/// whatever else a test writes; removed when dropped.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "atomic-quiescence-{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("proc/net")).expect("fixture dir");
        std::fs::create_dir_all(root.join("work")).expect("durable dir");
        Self { root }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn write(&self, rel: &str, content: &[u8]) {
        let path = self.path(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(path, content).expect("write");
    }

    /// A process in the fake `/proc`, with `comm` as its stat name.
    fn process(&self, pid: u32, argv: &[&str], comm: &str, state: char) {
        let mut cmdline = Vec::new();
        for arg in argv {
            cmdline.extend_from_slice(arg.as_bytes());
            cmdline.push(0);
        }
        self.write(&format!("proc/{pid}/cmdline"), &cmdline);
        self.write(
            &format!("proc/{pid}/stat"),
            format!("{pid} ({comm}) {state} 1 {pid} {pid} 0 -1 4194560 0 0").as_bytes(),
        );
    }

    /// `/proc/net/tcp` rows of (local port, state).
    fn tcp(&self, rows: &[(u16, &str)]) {
        let mut table = String::from(
            "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n",
        );
        for (n, (port, state)) in rows.iter().enumerate() {
            table.push_str(&format!(
                "   {n}: 0100007F:{port:04X} 0100007F:D3A2 {state} 00000000:00000000 00:00000000 00000000     0        0 1000{n}\n"
            ));
        }
        self.write("proc/net/tcp", table.as_bytes());
    }

    /// A sandbox that is quiescent: PID 1 and the runner's own process,
    /// exec-server listening with no session, a finished Codex turn, and
    /// `CODEX_HOME` on the durable directory.
    fn quiescent(name: &str) -> (Self, Inputs) {
        let f = Self::new(name);
        f.process(1, &["/sbin/init"], "init", 'S');
        f.process(
            20,
            &[
                "/usr/local/bin/codex-exec-server",
                "--listen",
                "ws://0.0.0.0:8080",
            ],
            "codex-exec-serv",
            'S',
        );
        f.tcp(&[(8080, "0A")]);
        f.write("work/run/events.jsonl", FINISHED.as_bytes());
        let mut inputs = Inputs::new(f.path("work"));
        inputs.proc_root = f.path("proc");
        inputs.events = Some(f.path("work/run/events.jsonl"));
        inputs.codex_home = Some(f.path("work/codex-home"));
        inputs.exec_server_port = Some(8080);
        inputs.runner_pids = BTreeSet::from([20]);
        (f, inputs)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const FINISHED: &str = r#"{"type":"thread.started","thread_id":"t1"}
{"type":"turn.started"}
{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"./slow.sh"}}
{"type":"item.completed","item":{"id":"item_1","type":"command_execution","exit_code":0}}
{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"done"}}
{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}
"#;

/// Spike 11's lost turn: the log stops with a command in progress.
const IN_PROGRESS: &str = r#"{"type":"thread.started","thread_id":"t1"}
{"type":"turn.started"}
{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"./slow.sh"}}
"#;

// --- Codex's event log --------------------------------------------------

#[test]
fn a_finished_turn_is_closed() {
    let turn = codex_turn(FINISHED).expect("parses");
    assert_eq!(turn, Turn::default());
    assert!(!turn.in_progress());
}

#[test]
fn a_turn_with_an_item_in_progress_is_open() {
    let turn = codex_turn(IN_PROGRESS).expect("parses");
    assert!(turn.open);
    assert_eq!(turn.open_items, vec!["command_execution".to_owned()]);
}

#[test]
fn a_failed_turn_is_closed() {
    let log =
        "{\"type\":\"turn.started\"}\n{\"type\":\"turn.failed\",\"error\":{\"message\":\"x\"}}\n";
    assert!(!codex_turn(log).expect("parses").in_progress());
}

#[test]
fn a_cut_off_last_line_is_ignored_and_any_other_bad_line_is_an_error() {
    let cut = format!("{IN_PROGRESS}{{\"type\":\"item.compl");
    assert_eq!(
        codex_turn(&cut).expect("parses"),
        codex_turn(IN_PROGRESS).expect("parses")
    );
    let bad = "{\"type\":\"turn.started\"}\nnot json\n{\"type\":\"turn.completed\"}\n";
    assert!(codex_turn(bad).expect_err("rejects").starts_with("line 2:"));
    assert_eq!(codex_turn("").expect("parses"), Turn::default());
}

// --- /proc ----------------------------------------------------------------

#[test]
fn processes_exclude_pid_1_the_runner_zombies_and_kernel_threads() {
    let f = Fixture::new("procs");
    f.process(1, &["/sbin/init"], "init", 'S');
    f.process(20, &["runner"], "runner", 'S');
    f.process(30, &["sleep", "120"], "sleep", 'S');
    f.process(31, &["defunct"], "defunct", 'Z');
    f.process(32, &[], "kthreadd", 'S');
    f.process(33, &["/usr/bin/odd"], "a) (b", 'R');
    f.write("proc/self", b"not a pid");
    let found = attempt_processes(&f.path("proc"), &BTreeSet::from([20])).expect("scans");
    let pids: Vec<u32> = found.iter().map(|p| p.pid).collect();
    assert_eq!(pids, vec![30, 33]);
    assert_eq!(found[0].argv, vec!["sleep".to_owned(), "120".to_owned()]);
}

#[test]
fn an_unreadable_proc_is_an_error() {
    let f = Fixture::new("noproc");
    assert!(attempt_processes(&f.path("missing"), &BTreeSet::new()).is_err());
}

#[test]
fn only_established_connections_to_the_port_count() {
    let f = Fixture::new("tcp");
    f.tcp(&[
        (8080, "0A"),
        (8080, "01"),
        (8080, "01"),
        (80, "01"),
        (8080, "06"),
    ]);
    let table = std::fs::read_to_string(f.path("proc/net/tcp")).expect("table");
    assert_eq!(established_connections(&table, 8080), 2);
    assert_eq!(established_connections(&table, 80), 1);
    assert_eq!(established_connections(&table, 9090), 0);
    let v6 = "  sl  local_address                         remote_address                        st\n   0: 00000000000000000000000000000000:1F90 00000000000000000000000001000000:C350 01 0\n";
    assert_eq!(established_connections(v6, 8080), 1);
}

// --- check: each control, switched on and off ----------------------------

#[test]
fn a_quiet_sandbox_is_quiescent() {
    let (_f, inputs) = Fixture::quiescent("quiet");
    let report = check(&inputs);
    assert_eq!(report.state, State::Quiescent, "{:?}", report.reasons);
    assert!(report.reasons.is_empty());
    assert!(report.quiescent());
}

#[test]
fn an_open_turn_with_codex_alive_is_busy() {
    let (f, inputs) = Fixture::quiescent("open-turn");
    f.write("work/run/events.jsonl", IN_PROGRESS.as_bytes());
    f.process(40, &["codex", "exec", "--json"], "codex", 'S');
    let report = check(&inputs);
    assert_eq!(report.state, State::Busy);
    assert!(report.reasons.contains(&Reason::TurnOpen {
        items: vec!["command_execution".into()]
    }));
}

#[test]
fn an_open_turn_with_no_codex_process_is_lost() {
    let (f, inputs) = Fixture::quiescent("lost");
    f.write("work/run/events.jsonl", IN_PROGRESS.as_bytes());
    assert_eq!(check(&inputs).state, State::Lost);
    // A Codex helper counts as Codex, by its program's file name.
    f.process(
        41,
        &["/work/codex-home/tmp/arg0/codex-linux-sandbox", "--"],
        "codex-linux-san",
        'S',
    );
    assert_eq!(check(&inputs).state, State::Busy);
}

#[test]
fn a_missing_log_is_no_turn_yet() {
    let (f, inputs) = Fixture::quiescent("no-log");
    std::fs::remove_file(f.path("work/run/events.jsonl")).expect("remove");
    assert_eq!(check(&inputs).state, State::Quiescent);
}

#[test]
fn a_leftover_background_process_is_busy_until_it_ends() {
    let (f, inputs) = Fixture::quiescent("leftover");
    // Orphaned by `setsid -f`, so its parent is PID 1; still the attempt's.
    f.process(50, &["sleep", "300"], "sleep", 'S');
    let report = check(&inputs);
    assert_eq!(report.state, State::Busy);
    assert_eq!(report.reasons, vec![Reason::Processes { count: 1 }]);
    std::fs::remove_dir_all(f.path("proc/50")).expect("end it");
    assert_eq!(check(&inputs).state, State::Quiescent);
}

#[test]
fn an_open_exec_server_session_is_busy() {
    let (f, mut inputs) = Fixture::quiescent("session");
    f.tcp(&[(8080, "0A"), (8080, "01")]);
    let report = check(&inputs);
    assert_eq!(report.reasons, vec![Reason::ExecSessions { count: 1 }]);
    assert_eq!(report.exec_sessions, 1);
    inputs.exec_server_port = None;
    assert_eq!(check(&inputs).state, State::Quiescent);
}

#[test]
fn codex_home_off_the_durable_directory_is_busy() {
    let (f, mut inputs) = Fixture::quiescent("codex-home");
    // A sibling whose name only starts with the durable directory's.
    inputs.codex_home = Some(f.path("work2/codex-home"));
    assert!(matches!(
        check(&inputs).reasons.as_slice(),
        [Reason::CodexHomeNotDurable { .. }]
    ));
    inputs.codex_home = Some(f.path("work/codex-home"));
    assert_eq!(check(&inputs).state, State::Quiescent);
}

#[test]
fn a_check_that_cannot_be_made_is_busy_never_quiescent_or_lost() {
    let (f, mut inputs) = Fixture::quiescent("fail-closed");
    f.write("work/run/events.jsonl", IN_PROGRESS.as_bytes());
    inputs.proc_root = f.path("missing");
    let report = check(&inputs);
    // The log says a turn is open, but with no process scan there is no
    // proof Codex is gone, so not Lost either.
    assert_eq!(report.state, State::Busy);
    assert!(
        report
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::CheckFailed { check, .. } if check == "processes"))
    );

    let (f, inputs) = Fixture::quiescent("bad-log");
    f.write(
        "work/run/events.jsonl",
        b"{\"type\":\"turn.started\"}\n{oops\n",
    );
    assert_eq!(check(&inputs).state, State::Busy);
}

// --- the worktree ---------------------------------------------------------

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args([
            "-c",
            "user.email=test@example.com",
            "-c",
            "user.name=test",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

fn repo(f: &Fixture) -> PathBuf {
    let dir = f.path("work/task");
    std::fs::create_dir_all(&dir).expect("dir");
    git(&dir, &["init", "-q"]);
    std::fs::write(dir.join("a.txt"), "a\n").expect("write");
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "a"]);
    dir
}

#[test]
fn uncommitted_work_is_busy_until_committed() {
    let (f, mut inputs) = Fixture::quiescent("uncommitted");
    let dir = repo(&f);
    inputs.worktree = Some(dir.clone());
    inputs.require_pushed = false;
    assert_eq!(check(&inputs).state, State::Quiescent);
    std::fs::write(dir.join("marker"), "untracked\n").expect("write");
    std::fs::write(dir.join("a.txt"), "changed\n").expect("write");
    assert_eq!(
        check(&inputs).reasons,
        vec![Reason::Uncommitted { changes: 2 }]
    );
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "b"]);
    assert_eq!(check(&inputs).state, State::Quiescent);
}

#[test]
fn unpushed_commits_are_busy_when_pushes_are_required() {
    let (f, mut inputs) = Fixture::quiescent("unpushed");
    let dir = repo(&f);
    inputs.worktree = Some(dir.clone());
    assert_eq!(check(&inputs).reasons, vec![Reason::NoUpstream]);

    let remote = f.path("remote.git");
    git(
        &f.root,
        &["init", "-q", "--bare", remote.to_str().expect("utf-8")],
    );
    git(
        &dir,
        &["remote", "add", "origin", remote.to_str().expect("utf-8")],
    );
    git(&dir, &["push", "-q", "-u", "origin", "main"]);
    assert_eq!(check(&inputs).state, State::Quiescent);

    std::fs::write(dir.join("b.txt"), "b\n").expect("write");
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "b"]);
    assert_eq!(check(&inputs).reasons, vec![Reason::Unpushed { ahead: 1 }]);
    git(&dir, &["push", "-q"]);
    assert_eq!(check(&inputs).state, State::Quiescent);
}

#[test]
fn the_worktree_check_runs_nothing_the_worktree_configures() {
    let f = Fixture::new("fsmonitor");
    let dir = repo(&f);
    let hook = f.path("fsmonitor.sh");
    let ran = f.path("fsmonitor.ran");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", ran.display()),
    )
    .expect("hook");
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("chmod");
    git(
        &dir,
        &["config", "core.fsmonitor", hook.to_str().expect("utf-8")],
    );

    // The control matters: a plain `git status` runs the repository's hook.
    git(&dir, &["status", "--porcelain"]);
    assert!(ran.exists(), "plain git status should run core.fsmonitor");
    std::fs::remove_file(&ran).expect("reset");

    worktree("git", &dir).expect("status");
    assert!(
        !ran.exists(),
        "the quiescence check ran the worktree's fsmonitor"
    );
}

#[test]
fn a_directory_that_is_not_a_repository_is_a_failed_check() {
    let (f, mut inputs) = Fixture::quiescent("not-a-repo");
    std::fs::create_dir_all(f.path("work/task")).expect("dir");
    inputs.worktree = Some(f.path("work/task"));
    // Not git's work tree: git looks upward and finds no repository. (A
    // temporary directory inside some other repository would mislead this.)
    let report = check(&inputs);
    assert!(
        report
            .reasons
            .iter()
            .any(|r| matches!(r, Reason::CheckFailed { check, .. } if check == "the worktree"))
    );
}

#[test]
fn status_lines_parse() {
    assert_eq!(
        parse_status("## main...origin/main [ahead 2, behind 1]\n M a\n?? b\n"),
        Worktree {
            changes: 2,
            upstream: true,
            ahead: 2
        }
    );
    assert_eq!(
        parse_status("## No commits yet on main\n"),
        Worktree {
            changes: 0,
            upstream: false,
            ahead: 0
        }
    );
    assert!(!parse_status("## main\n").upstream);
}

// --- idle time and the wire format ----------------------------------------

#[test]
fn idle_time_counts_from_quiescence_and_restarts_after_a_break() {
    let t0 = Instant::now();
    let at = |s: u64| t0 + Duration::from_secs(s);
    let mut idle = Idle::default();
    assert_eq!(idle.observe(true, at(0)), Duration::ZERO);
    assert_eq!(idle.observe(true, at(5)), Duration::from_secs(5));
    assert_eq!(idle.observe(false, at(6)), Duration::ZERO);
    assert_eq!(idle.observe(true, at(7)), Duration::ZERO);
    assert_eq!(idle.observe(true, at(10)), Duration::from_secs(3));

    let (_f, inputs) = Fixture::quiescent("stamp");
    let mut report = check(&inputs);
    idle.stamp(&mut report, at(12));
    assert_eq!(report.quiescent_for_ms, 5_000);
}

#[test]
fn the_report_round_trips_as_json() {
    let report = Report {
        state: State::Busy,
        reasons: vec![
            Reason::TurnOpen {
                items: vec!["command_execution".into()],
            },
            Reason::Processes { count: 2 },
        ],
        turn: Some(Turn {
            open: true,
            open_items: vec!["command_execution".into()],
        }),
        processes: vec![Process {
            pid: 30,
            argv: vec!["sleep".into(), "120".into()],
        }],
        exec_sessions: 0,
        quiescent_for_ms: 0,
    };
    let json = serde_json::to_value(&report).expect("serializes");
    assert_eq!(json["state"], "busy");
    assert_eq!(
        json["reasons"][1],
        serde_json::json!({"kind": "processes", "count": 2})
    );
    let back: Report = serde_json::from_value(json).expect("deserializes");
    assert_eq!(back, report);
    assert_eq!(
        report.reasons[0].to_string(),
        "a Codex turn is open (command_execution in progress)"
    );
}
