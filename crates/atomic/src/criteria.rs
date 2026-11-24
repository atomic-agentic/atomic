// SPDX-License-Identifier: Apache-2.0

//! Bounded executable criteria for a trusted, frozen workflow declaration.
//! A caller records each result in its own durable sibling step before it
//! starts a reviewer. Imported issue prose never enters this executor.

use atomic_core::workflow::{Criterion, parse_criteria};
use atomic_exec::{Command, ExecError, ExecutionBackend};
use atomic_schema::expr::{self, Shape};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

/// A result to checkpoint before an independent reviewer can start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriterionOutcome {
    /// Whether the criterion was met.
    pub passed: bool,
    /// Bounded diagnostic for an author repair round, never trusted policy.
    pub detail: String,
}

/// A criterion could not be evaluated. A lost runner outcome must be parked;
/// it cannot be interpreted as a passing check or silently repeated.
#[derive(Debug, thiserror::Error)]
pub enum CriterionError {
    /// The expression failed to evaluate or did not yield a boolean.
    #[error("criterion CEL: {0}")]
    Cel(String),
    /// The bounded runner refused or lost the command.
    #[error("criterion runner: {0}")]
    Runner(#[from] ExecError),
    /// The command could not be constructed from a validated declaration.
    #[error("criterion declaration: {0}")]
    Declaration(String),
}

impl CriterionError {
    /// Whether the command may have run although its result was not returned.
    pub fn outcome_unknown(&self) -> bool {
        matches!(self, Self::Runner(error) if error.outcome_unknown())
    }
}

/// Evaluate one frozen criterion. The caller supplies a scratch copy as `cwd`
/// and a production backend confined by the execution boundary. This method
/// blocks; async callers use a blocking worker. It launches the declared
/// program directly, without adding a shell.
pub fn evaluate(
    criterion: &Criterion,
    backend: &dyn ExecutionBackend,
    cwd: &Path,
    cel_values: &[(&str, &Value, &Shape<'_>)],
) -> Result<CriterionOutcome, CriterionError> {
    // A deserialized or hand-built Criterion is not proof that the trusted
    // workflow loader approved it. Recheck the same bounds at the runner
    // boundary so future graph-item inputs cannot bypass declaration bounds.
    let value = serde_json::to_value(criterion)
        .map_err(|error| CriterionError::Declaration(error.to_string()))?;
    parse_criteria(&serde_json::json!([value])).map_err(CriterionError::Declaration)?;
    match criterion {
        Criterion::Text { .. } => Ok(CriterionOutcome {
            passed: true,
            detail: "text criterion is for human or reviewer judgment".into(),
        }),
        Criterion::Cel { expr: source } => {
            let value = expr::evaluate(source, cel_values).map_err(CriterionError::Cel)?;
            let Some(passed) = value.as_bool() else {
                return Err(CriterionError::Cel(
                    "expression must return a boolean".into(),
                ));
            };
            Ok(CriterionOutcome {
                passed,
                detail: if passed {
                    "CEL passed"
                } else {
                    "CEL returned false"
                }
                .into(),
            })
        }
        Criterion::Test { argv, timeout_ms } => {
            let (program, args) = argv
                .split_first()
                .ok_or_else(|| CriterionError::Declaration("test argv is empty".into()))?;
            let command = confined_command(program, args.to_vec(), cwd, *timeout_ms, vec![])?;
            let completed = backend.run(&command)?;
            Ok(CriterionOutcome {
                passed: completed.success(),
                detail: format!(
                    "test exited {:?}: {}",
                    completed.code,
                    bounded_message(&completed.stderr)
                ),
            })
        }
        Criterion::Http {
            method,
            url,
            allowed_host,
            status,
            timeout_ms,
        } => {
            // `-q` must be curl's first argument: it ignores a user's .curlrc.
            // No redirects are followed, and the sandbox grants exactly one
            // declared host. An unavailable curl fails the criterion closed.
            let mut args = vec![
                "-q".into(),
                "--silent".into(),
                "--show-error".into(),
                "--max-redirs".into(),
                "0".into(),
                "--proto".into(),
                "=http,https".into(),
                "--output".into(),
                "/dev/null".into(),
                "--write-out".into(),
                "%{http_code}".into(),
            ];
            if method == "HEAD" {
                args.push("--head".into());
            }
            args.push(url.clone());
            let command =
                confined_command("curl", args, cwd, *timeout_ms, vec![allowed_host.clone()])?;
            let completed = backend.run(&command)?;
            let actual = std::str::from_utf8(&completed.stdout)
                .ok()
                .and_then(|text| text.parse::<u16>().ok());
            let passed = completed.success() && actual == Some(*status);
            Ok(CriterionOutcome {
                passed,
                detail: format!(
                    "HTTP status {} (expected {status})",
                    actual.map_or("unknown".into(), |n| n.to_string())
                ),
            })
        }
    }
}

fn confined_command(
    program: &str,
    args: Vec<String>,
    cwd: &Path,
    timeout_ms: u64,
    network: Vec<String>,
) -> Result<Command, CriterionError> {
    if !(1..=300_000).contains(&timeout_ms) || !cwd.is_dir() {
        return Err(CriterionError::Declaration(
            "invalid timeout or scratch directory".into(),
        ));
    }
    Ok(Command {
        program: program.to_owned(),
        args,
        cwd: cwd.to_owned(),
        env: Vec::new(),
        stdin: Vec::new(),
        time_limit: Duration::from_millis(timeout_ms),
        output_limit: 64 * 1024,
        cpu_seconds: Some(60),
        data_bytes: Some(256 * 1024 * 1024),
        writable: vec![cwd.to_owned()],
        read_only: Vec::new(),
        network,
        denied: Vec::new(),
        secret_env: Default::default(),
    })
}

fn bounded_message(bytes: &[u8]) -> String {
    let end = bytes.len().min(4096);
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_exec::{Completed, Open, Statement, Workspace};
    use std::sync::Mutex;

    struct Fake {
        commands: Mutex<Vec<Command>>,
        result: Result<Completed, ExecError>,
    }

    impl ExecutionBackend for Fake {
        fn connect(&self) -> Result<Statement, ExecError> {
            Ok(Statement::default())
        }
        fn open(&self, _: &Open) -> Result<Workspace, ExecError> {
            panic!("not used")
        }
        fn run(&self, command: &Command) -> Result<Completed, ExecError> {
            self.commands
                .lock()
                .expect("test command log lock")
                .push(command.clone());
            self.result.clone()
        }
        fn close(&self, _: &Workspace) -> Result<(), ExecError> {
            panic!("not used")
        }
    }

    fn fake(result: Result<Completed, ExecError>) -> Fake {
        Fake {
            commands: Mutex::new(Vec::new()),
            result,
        }
    }

    #[test]
    fn test_and_http_use_bounded_runner_commands_without_shell_or_redirects() {
        let cwd = std::env::temp_dir();
        let backend = fake(Ok(Completed {
            code: Some(0),
            stdout: b"204".to_vec(),
            stderr: Vec::new(),
        }));
        let checked = evaluate(
            &Criterion::Test {
                argv: vec!["cargo".into(), "test".into(), "--locked".into()],
                timeout_ms: 5000,
            },
            &backend,
            &cwd,
            &[],
        )
        .expect("test criterion executes");
        assert!(checked.passed);
        let probed = evaluate(
            &Criterion::Http {
                method: "GET".into(),
                url: "https://health.example.test/ready".into(),
                allowed_host: "health.example.test".into(),
                status: 204,
                timeout_ms: 2000,
            },
            &backend,
            &cwd,
            &[],
        )
        .expect("HTTP criterion executes");
        assert!(probed.passed);
        let commands = backend.commands.lock().expect("test command log lock");
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].program, "cargo");
        assert_eq!(commands[0].args, ["test", "--locked"]);
        assert!(commands[0].network.is_empty());
        assert_eq!(commands[0].time_limit, Duration::from_secs(5));
        assert_eq!(commands[0].output_limit, 64 * 1024);
        assert_eq!(commands[1].program, "curl");
        assert_eq!(commands[1].args.first().map(String::as_str), Some("-q"));
        assert!(
            commands[1]
                .args
                .windows(2)
                .any(|pair| pair == ["--max-redirs", "0"])
        );
        assert_eq!(commands[1].network, ["health.example.test"]);
        assert_eq!(commands[1].time_limit, Duration::from_secs(2));
    }

    #[test]
    fn runner_unavailable_never_passes_a_criterion() {
        let backend = fake(Err(ExecError::Start {
            program: "curl".into(),
            message: "unavailable".into(),
        }));
        let result = evaluate(
            &Criterion::Http {
                method: "HEAD".into(),
                url: "https://health.example.test/ready".into(),
                allowed_host: "health.example.test".into(),
                status: 200,
                timeout_ms: 2000,
            },
            &backend,
            &std::env::temp_dir(),
            &[],
        );
        assert!(matches!(
            result,
            Err(CriterionError::Runner(ExecError::Start { .. }))
        ));
    }

    #[test]
    fn a_hand_built_shell_criterion_is_rejected_before_runner_call() {
        let backend = fake(Ok(Completed {
            code: Some(0),
            stdout: vec![],
            stderr: vec![],
        }));
        let result = evaluate(
            &Criterion::Test {
                argv: vec!["/bin/sh".into(), "-c".into(), "true".into()],
                timeout_ms: 1000,
            },
            &backend,
            &std::env::temp_dir(),
            &[],
        );
        assert!(matches!(result, Err(CriterionError::Declaration(_))));
        assert!(
            backend
                .commands
                .lock()
                .expect("test command log lock")
                .is_empty()
        );
    }

    #[test]
    fn cel_must_return_a_true_boolean() {
        let backend = fake(Ok(Completed {
            code: Some(0),
            stdout: vec![],
            stderr: vec![],
        }));
        let candidate = serde_json::json!({"output":{"passed":true}});
        let schema = serde_json::json!({"type":"object","properties":{"output":{"type":"object","properties":{"passed":{"type":"boolean"}}}}});
        let values = [("candidate", &candidate, &Shape::Schema(&schema))];
        assert!(
            evaluate(
                &Criterion::Cel {
                    expr: "candidate.output.passed".into()
                },
                &backend,
                &std::env::temp_dir(),
                &values
            )
            .expect("true CEL criterion evaluates")
            .passed
        );
        assert!(
            !evaluate(
                &Criterion::Cel {
                    expr: "!candidate.output.passed".into()
                },
                &backend,
                &std::env::temp_dir(),
                &values
            )
            .expect("false CEL criterion evaluates")
            .passed
        );
        assert!(matches!(
            evaluate(
                &Criterion::Cel {
                    expr: "candidate.output".into()
                },
                &backend,
                &std::env::temp_dir(),
                &values
            ),
            Err(CriterionError::Cel(_))
        ));
        assert!(
            backend
                .commands
                .lock()
                .expect("test command log lock")
                .is_empty()
        );
    }
}
