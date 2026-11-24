//! Pluggable subprocess execution and rate-limit sleep traits for offline-verifiable CLI transports.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use kp_model::BoxFuture;

use crate::{ConnectError, Result};

/// Captured output from a local CLI invocation (`lark-cli` or `gh`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutput {
    pub status_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Abstract subprocess executor so CLI transports can be verified 100% offline in tests.
pub trait CommandRunner: Send + Sync {
    fn run<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
    ) -> BoxFuture<'a, Result<CommandOutput>> {
        self.run_in_dir(program, args, None)
    }

    fn run_in_dir<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
        cwd: Option<&'a Path>,
    ) -> BoxFuture<'a, Result<CommandOutput>>;
}

/// Production command runner spawning non-interactive subprocesses (`stdin = null`).
#[derive(Clone, Copy, Debug, Default)]
pub struct StdCommandRunner;

impl CommandRunner for StdCommandRunner {
    fn run_in_dir<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
        cwd: Option<&'a Path>,
    ) -> BoxFuture<'a, Result<CommandOutput>> {
        let program_owned = program.to_owned();
        let args_owned = args.to_vec();
        let cwd_owned = cwd.map(Path::to_path_buf);
        Box::pin(async move {
            let joined = tokio::task::spawn_blocking(move || {
                let mut cmd = Command::new(&program_owned);
                cmd.args(&args_owned)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                if let Some(dir) = &cwd_owned {
                    cmd.current_dir(dir);
                }
                cmd.output()
            })
            .await
            .map_err(|err| {
                ConnectError::Transport(format!("subprocess task join failed: {err}"))
            })?;
            let output = joined?;
            Ok(CommandOutput {
                status_code: output.status.code().unwrap_or(-1),
                stdout: output.stdout,
                stderr: output.stderr,
            })
        })
    }
}

/// Abstract async timer so rate-limit cooldowns run deterministically without real wall-clock sleeps in tests.
pub trait Sleeper: Send + Sync {
    fn sleep_ms(&self, duration_ms: u64) -> BoxFuture<'_, ()>;
}

/// Production sleeper backed by `tokio::time::sleep`.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioSleeper;

impl Sleeper for TokioSleeper {
    fn sleep_ms(&self, duration_ms: u64) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(duration_ms)).await;
        })
    }
}

/// Deterministic zero-delay sleeper recording every requested backoff duration in milliseconds.
#[derive(Debug, Default)]
pub struct RecordingSleeper {
    sleeps_ms: Mutex<Vec<u64>>,
}

impl RecordingSleeper {
    pub fn recorded_sleeps(&self) -> Result<Vec<u64>> {
        let guard = self
            .sleeps_ms
            .lock()
            .map_err(|_| ConnectError::Transport("poisoned RecordingSleeper lock".into()))?;
        Ok(guard.clone())
    }
}

impl Sleeper for RecordingSleeper {
    fn sleep_ms(&self, duration_ms: u64) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if let Ok(mut guard) = self.sleeps_ms.lock() {
                guard.push(duration_ms);
            }
        })
    }
}

/// `(program, args, cwd)` recorded by [`MockCommandRunner`].
pub type RecordedInvocationWithCwd = (String, Vec<String>, Option<PathBuf>);

type MockStepHandler =
    Box<dyn FnOnce(&str, &[String], Option<&Path>) -> Result<CommandOutput> + Send>;

/// Deterministic scripted command runner for offline unit and integration tests.
///
/// Enforces real CLI subcommand and flag schemas for `lark-cli` and `gh` on every invocation so
/// nonexistent flags or write verbs fail immediately in unit tests just like the real binaries.
#[derive(Default)]
pub struct MockCommandRunner {
    invocations: Mutex<Vec<RecordedInvocationWithCwd>>,
    responses: Mutex<VecDeque<MockStepHandler>>,
}

impl MockCommandRunner {
    pub fn push_json_ok(&self, body: &serde_json::Value) -> Result<()> {
        let bytes = serde_json::to_vec(body)?;
        self.push_output(Ok(CommandOutput {
            status_code: 0,
            stdout: bytes,
            stderr: Vec::new(),
        }))
    }

    pub fn push_output(&self, response: Result<CommandOutput>) -> Result<()> {
        self.push_handler(move |_, _, _| response)
    }

    pub fn push_handler<F>(&self, handler: F) -> Result<()>
    where
        F: FnOnce(&str, &[String], Option<&Path>) -> Result<CommandOutput> + Send + 'static,
    {
        let mut guard = self
            .responses
            .lock()
            .map_err(|_| ConnectError::Transport("poisoned MockCommandRunner lock".into()))?;
        guard.push_back(Box::new(handler));
        Ok(())
    }

    pub fn invocations(&self) -> Result<Vec<(String, Vec<String>)>> {
        let guard = self
            .invocations
            .lock()
            .map_err(|_| ConnectError::Transport("poisoned MockCommandRunner lock".into()))?;
        Ok(guard
            .iter()
            .map(|(prog, args, _)| (prog.clone(), args.clone()))
            .collect())
    }

    pub fn invocations_with_cwd(&self) -> Result<Vec<RecordedInvocationWithCwd>> {
        let guard = self
            .invocations
            .lock()
            .map_err(|_| ConnectError::Transport("poisoned MockCommandRunner lock".into()))?;
        Ok(guard.clone())
    }
}

fn validate_mock_cli_invocation(program: &str, args: &[String]) -> Result<()> {
    let bin_name = Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(program);
    if bin_name == "lark-cli" {
        crate::feishu::validate_lark_cli_readonly_args(args)?;
    } else if bin_name == "gh" {
        crate::github::validate_gh_cli_readonly_args(args)?;
    }
    Ok(())
}

impl CommandRunner for MockCommandRunner {
    fn run_in_dir<'a>(
        &'a self,
        program: &'a str,
        args: &'a [String],
        cwd: Option<&'a Path>,
    ) -> BoxFuture<'a, Result<CommandOutput>> {
        Box::pin(async move {
            validate_mock_cli_invocation(program, args)?;
            {
                let mut inv = self.invocations.lock().map_err(|_| {
                    ConnectError::Transport("poisoned MockCommandRunner lock".into())
                })?;
                inv.push((
                    program.to_owned(),
                    args.to_vec(),
                    cwd.map(Path::to_path_buf),
                ));
            }
            let handler = {
                let mut resp = self.responses.lock().map_err(|_| {
                    ConnectError::Transport("poisoned MockCommandRunner lock".into())
                })?;
                resp.pop_front()
            };
            match handler {
                Some(step) => step(program, args, cwd),
                None => Err(ConnectError::Transport(format!(
                    "MockCommandRunner queue exhausted for `{program} {}`",
                    args.join(" ")
                ))),
            }
        })
    }
}
