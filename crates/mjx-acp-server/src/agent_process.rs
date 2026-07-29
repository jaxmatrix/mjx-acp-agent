//! Starting and stopping an ACP agent subprocess.
//!
//! The agent speaks newline-delimited JSON-RPC on stdin/stdout and writes
//! diagnostics to stderr. Modelled on Zed's `AcpConnection::stdio`
//! (`reference/zed-acp/agent_servers/src/acp.rs`), including its one-second
//! grace period before killing the child.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use mjx_agent_catalog::AgentCommand;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

/// How long a child gets to exit after its stdin closes, before we kill it.
/// Same value Zed's SDK uses.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// A running agent subprocess, split into the pieces the relay drives.
pub struct AgentProcess {
    /// Kept alive so the process can be waited on and killed.
    pub handle: AgentHandle,
    /// The agent's stdin. Frames from the browser go here.
    pub stdin: ChildStdin,
    /// The agent's stdout, one JSON-RPC frame per line.
    pub stdout: Lines<BufReader<ChildStdout>>,
    /// The agent's stderr, one diagnostic per line.
    pub stderr: Lines<BufReader<ChildStderr>>,
}

/// The process itself, once the relay has taken ownership of its streams.
pub struct AgentHandle {
    child: Child,
}

impl AgentProcess {
    /// Starts `command` in `cwd`.
    pub fn spawn(command: &AgentCommand, cwd: &Path) -> Result<Self> {
        let mut process = Command::new(&command.program);
        process
            .args(&command.args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The agent must not inherit the terminal's process group, or a
            // Ctrl-C aimed at the server would kill agents mid-turn.
            .kill_on_drop(true);

        // The agent inherits our environment so it can find its own
        // credentials the same way it would from a shell — this is why
        // `claude-acp` needs no configuration here.
        for (name, value) in &command.env {
            process.env(name, value);
        }

        let mut child = process.spawn().with_context(|| {
            format!(
                "could not start `{}` in {}",
                command.display().join(" "),
                cwd.display()
            )
        })?;

        let stdin = child.stdin.take().context("child stdin was not piped")?;
        let stdout = child.stdout.take().context("child stdout was not piped")?;
        let stderr = child.stderr.take().context("child stderr was not piped")?;

        Ok(Self {
            handle: AgentHandle { child },
            stdin,
            stdout: BufReader::new(stdout).lines(),
            stderr: BufReader::new(stderr).lines(),
        })
    }

    /// Ends the process, closing stdin first. Convenience for callers that
    /// still own every part; the relay splits it up and uses
    /// [`AgentHandle::shutdown`] instead.
    #[allow(
        dead_code,
        reason = "used by tests and by callers that never split the process"
    )]
    pub async fn shutdown(self) {
        drop(self.stdin);
        self.handle.shutdown().await;
    }
}

impl AgentHandle {
    /// Waits for the process to exit, then kills it if it overstays.
    ///
    /// The caller must have dropped the child's stdin first — that is the
    /// signal most agents shut down on. Agents flush state on the way out
    /// (`claude-acp` persists the session), so the grace period is worth
    /// waiting for rather than killing immediately.
    pub async fn shutdown(mut self) {
        match tokio::time::timeout(SHUTDOWN_GRACE, self.child.wait()).await {
            Ok(Ok(status)) => tracing::debug!(?status, "agent exited"),
            Ok(Err(err)) => tracing::warn!(%err, "could not wait for the agent"),
            Err(_) => {
                tracing::warn!("agent did not exit within the grace period; killing it");
                let _ = self.child.kill().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn command(program: &str, args: &[&str]) -> AgentCommand {
        AgentCommand {
            program: program.into(),
            args: args.iter().map(|s| (*s).into()).collect(),
            env: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn spawns_and_talks_over_stdio() {
        let mut agent = AgentProcess::spawn(&command("cat", &[]), Path::new(".")).unwrap();

        use tokio::io::AsyncWriteExt;
        agent.stdin.write_all(b"hello\n").await.unwrap();
        agent.stdin.flush().await.unwrap();

        assert_eq!(agent.stdout.next_line().await.unwrap().unwrap(), "hello");
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn stderr_is_captured_separately_from_stdout() {
        let agent = AgentProcess::spawn(
            &command("sh", &["-c", "echo out; echo err >&2"]),
            Path::new("."),
        )
        .unwrap();
        let mut agent = agent;

        assert_eq!(agent.stdout.next_line().await.unwrap().unwrap(), "out");
        assert_eq!(agent.stderr.next_line().await.unwrap().unwrap(), "err");
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn the_environment_is_passed_through() {
        let mut command = command("sh", &["-c", "echo $MJX_TEST_VAR"]);
        command.env.insert("MJX_TEST_VAR".into(), "visible".into());

        let mut agent = AgentProcess::spawn(&command, Path::new(".")).unwrap();
        assert_eq!(agent.stdout.next_line().await.unwrap().unwrap(), "visible");
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn the_working_directory_is_honoured() {
        let temp = std::env::temp_dir().canonicalize().unwrap();
        let mut agent = AgentProcess::spawn(&command("pwd", &[]), &temp).unwrap();
        let reported = agent.stdout.next_line().await.unwrap().unwrap();
        assert_eq!(Path::new(&reported), temp);
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn a_missing_program_fails_with_the_command_in_the_message() {
        let result = AgentProcess::spawn(
            &command("mjx-definitely-not-installed", &[]),
            Path::new("."),
        );
        let Err(err) = result else {
            panic!("spawning a nonexistent program should fail");
        };
        let message = format!("{err:#}");
        assert!(
            message.contains("mjx-definitely-not-installed"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn a_child_that_ignores_stdin_closing_is_killed() {
        // `sleep` never reads stdin, so closing it changes nothing and the
        // grace period has to expire.
        let agent = AgentProcess::spawn(&command("sleep", &["60"]), Path::new(".")).unwrap();
        let started = std::time::Instant::now();
        agent.shutdown().await;
        let elapsed = started.elapsed();

        assert!(
            elapsed >= SHUTDOWN_GRACE,
            "did not wait out the grace period"
        );
        assert!(
            elapsed < SHUTDOWN_GRACE + Duration::from_secs(5),
            "took {elapsed:?}, so the kill did not land"
        );
    }
}
