//! The terminal capability, backed by a real PTY.
//!
//! A pipe would be simpler, but most commands worth watching — test runners,
//! build tools — change their behaviour when stdout isn't a TTY: no colour, no
//! progress, sometimes different output entirely. A PTY means the browser sees
//! what a terminal would show.
//!
//! Output accounting follows Zed's `acp_thread::TerminalOutput`
//! (`reference/zed-acp/acp_thread/src/terminal.rs`): a byte budget, oldest
//! output discarded first, and a `truncated` flag once anything has been lost.

use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use tokio::sync::watch;

use crate::{WorkspaceError, WorkspaceEvent};

/// What a terminal's process ended with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExitStatus {
    /// Exit code, absent if the process was signalled.
    pub exit_code: Option<i32>,
    /// Signal name, absent on a normal exit.
    pub signal: Option<String>,
}

/// The bytes a terminal has produced, within its budget.
#[derive(Debug)]
struct Output {
    buffer: VecDeque<u8>,
    limit: usize,
    truncated: bool,
}

impl Output {
    fn new(limit: usize) -> Self {
        Self {
            buffer: VecDeque::new(),
            limit,
            truncated: false,
        }
    }

    /// Appends `chunk`, discarding the oldest bytes if that exceeds the budget.
    fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend(chunk);
        while self.buffer.len() > self.limit {
            self.buffer.pop_front();
            self.truncated = true;
        }
    }

    /// The retained output as text.
    ///
    /// Dropping bytes off the front can leave a partial UTF-8 sequence, so the
    /// lossy decode is deliberate — the protocol wants a string, and the
    /// alternative is failing a read because of a character we already threw
    /// away.
    fn text(&self) -> String {
        let bytes: Vec<u8> = self.buffer.iter().copied().collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// One running (or finished) terminal.
struct Terminal {
    output: Arc<Mutex<Output>>,
    exit: watch::Receiver<Option<ExitStatus>>,
    /// Kills the process. `None` once it has been used or the process ended.
    killer: Mutex<Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
    /// Kept alive so the PTY isn't closed while the process still runs.
    ///
    /// Behind a mutex only to make `Terminal` `Sync`: `MasterPty` is `Send`
    /// but not `Sync`, and the workspace is shared across connection tasks.
    _master: Mutex<Box<dyn portable_pty::MasterPty + Send>>,
}

/// Every terminal belonging to one connection.
#[derive(Default)]
pub struct Terminals {
    terminals: Mutex<std::collections::HashMap<String, Arc<Terminal>>>,
    next_id: std::sync::atomic::AtomicU64,
}

/// The default output budget when the agent doesn't ask for one. Matches the
/// size Zed uses.
const DEFAULT_OUTPUT_LIMIT: usize = 1024 * 1024;

impl Terminals {
    /// Starts `command` in `cwd` and begins streaming its output.
    ///
    /// `events` receives an incremental chunk per read, so the browser can show
    /// output as it appears rather than only when the process exits.
    pub fn create(
        &self,
        command: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: PathBuf,
        output_byte_limit: Option<usize>,
        events: tokio::sync::mpsc::UnboundedSender<WorkspaceEvent>,
    ) -> Result<String, WorkspaceError> {
        let id = format!(
            "term-{}",
            self.next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );

        let pty = NativePtySystem::default()
            .openpty(PtySize {
                rows: 24,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| WorkspaceError::Terminal(err.to_string()))?;

        let mut builder = CommandBuilder::new(command);
        builder.args(args);
        builder.cwd(&cwd);
        for (name, value) in env {
            builder.env(name, value);
        }

        let mut child = pty
            .slave
            .spawn_command(builder)
            .map_err(|err| WorkspaceError::Terminal(format!("could not run `{command}`: {err}")))?;

        // The slave handle must be dropped or the reader below never sees EOF:
        // this process would still be holding the other end open after the
        // child exits.
        drop(pty.slave);

        let mut reader = pty
            .master
            .try_clone_reader()
            .map_err(|err| WorkspaceError::Terminal(err.to_string()))?;

        let output = Arc::new(Mutex::new(Output::new(
            output_byte_limit.unwrap_or(DEFAULT_OUTPUT_LIMIT).max(1),
        )));
        let killer = child.clone_killer();
        let (exit_tx, exit_rx) = watch::channel(None);

        // Announce before the reader starts. A fast command can produce output
        // in the microseconds between spawning and returning, and a client that
        // received output for a terminal it had never heard of would have
        // nowhere to put it.
        let _ = events.send(WorkspaceEvent::TerminalCreated {
            terminal_id: id.clone(),
            command: command.to_string(),
            args: args.to_vec(),
            cwd: cwd.display().to_string(),
        });

        // portable-pty is a blocking API, so the reader and the waiter each get
        // a blocking thread rather than being polled.
        {
            let output = output.clone();
            let events = events.clone();
            let id = id.clone();
            std::thread::spawn(move || {
                let mut buffer = [0u8; 8192];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let chunk = &buffer[..n];
                            let truncated = {
                                let mut output = output.lock().unwrap_or_else(|e| e.into_inner());
                                output.push(chunk);
                                output.truncated
                            };
                            let event = WorkspaceEvent::TerminalOutput {
                                terminal_id: id.clone(),
                                chunk: chunk.to_vec(),
                                truncated,
                            };
                            if events.send(event).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }

        {
            let id = id.clone();
            std::thread::spawn(move || {
                let status = match child.wait() {
                    Ok(status) => exit_status(&status),
                    Err(err) => ExitStatus {
                        exit_code: None,
                        signal: Some(err.to_string()),
                    },
                };
                let _ = events.send(WorkspaceEvent::TerminalExit {
                    terminal_id: id,
                    status: status.clone(),
                });
                let _ = exit_tx.send(Some(status));
            });
        }

        self.terminals.lock().unwrap_or_else(|e| e.into_inner()).insert(
            id.clone(),
            Arc::new(Terminal {
                output,
                exit: exit_rx,
                killer: Mutex::new(Some(killer)),
                _master: Mutex::new(pty.master),
            }),
        );

        Ok(id)
    }

    /// The output so far, whether the process has exited, and whether anything
    /// was discarded.
    pub fn output(&self, id: &str) -> Result<(String, bool, Option<ExitStatus>), WorkspaceError> {
        let terminal = self.get(id)?;
        let (text, truncated) = {
            let output = terminal.output.lock().unwrap_or_else(|e| e.into_inner());
            (output.text(), output.truncated)
        };
        Ok((text, truncated, terminal.exit.borrow().clone()))
    }

    /// Waits for the process to exit, returning immediately if it already has.
    pub async fn wait_for_exit(&self, id: &str) -> Result<ExitStatus, WorkspaceError> {
        let mut exit = self.get(id)?.exit.clone();
        loop {
            if let Some(status) = exit.borrow().clone() {
                return Ok(status);
            }
            if exit.changed().await.is_err() {
                // The waiter thread went away without reporting; treat it as an
                // unknown exit rather than hanging the agent forever.
                return Ok(ExitStatus::default());
            }
        }
    }

    /// Signals the process. Killing an already-dead terminal is not an error.
    pub fn kill(&self, id: &str) -> Result<(), WorkspaceError> {
        let terminal = self.get(id)?;
        let mut killer = terminal.killer.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut killer) = killer.take() {
            let _ = killer.kill();
        }
        Ok(())
    }

    /// Drops the terminal. Its output is no longer readable.
    ///
    /// The process is killed first: an agent that releases a terminal without
    /// waiting for it would otherwise leave a build running with nobody
    /// watching.
    pub fn release(&self, id: &str) -> Result<(), WorkspaceError> {
        let _ = self.kill(id);
        self.terminals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        Ok(())
    }

    /// Kills everything. Called when the connection ends.
    pub fn release_all(&self) {
        let ids: Vec<String> = self
            .terminals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        for id in ids {
            let _ = self.release(&id);
        }
    }

    fn get(&self, id: &str) -> Result<Arc<Terminal>, WorkspaceError> {
        self.terminals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| WorkspaceError::NoSuchTerminal(id.to_string()))
    }
}

/// Splits a process exit into the protocol's code/signal pair.
fn exit_status(status: &portable_pty::ExitStatus) -> ExitStatus {
    // portable-pty reports a signal death as a non-zero code on Unix, so the
    // code is always what we have to go on.
    ExitStatus {
        exit_code: i32::try_from(status.exit_code()).ok(),
        signal: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn channel() -> (
        tokio::sync::mpsc::UnboundedSender<WorkspaceEvent>,
        tokio::sync::mpsc::UnboundedReceiver<WorkspaceEvent>,
    ) {
        tokio::sync::mpsc::unbounded_channel()
    }

    fn cwd() -> PathBuf {
        std::env::temp_dir().canonicalize().unwrap()
    }

    /// Waits until a terminal's retained output contains `needle`.
    ///
    /// A process exiting does not mean its output has been read: the reader is
    /// a separate thread, and asserting on `output()` the instant `wait_for_exit`
    /// returns is a race.
    async fn output_containing(terminals: &Terminals, id: &str, needle: &str) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let (output, _, _) = terminals.output(id).unwrap();
            if output.contains(needle) {
                return output;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{needle:?} never appeared; got {output:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn runs_a_command_and_collects_its_output() {
        let terminals = Terminals::default();
        let (tx, _rx) = channel();
        let id = terminals
            .create("echo", &["hello".into()], &[], cwd(), None, tx)
            .unwrap();

        let status = terminals.wait_for_exit(&id).await.unwrap();
        assert_eq!(status.exit_code, Some(0));

        output_containing(&terminals, &id, "hello").await;
        let (_, truncated, exit) = terminals.output(&id).unwrap();
        assert!(!truncated);
        assert_eq!(exit.unwrap().exit_code, Some(0));
    }

    #[tokio::test]
    async fn output_is_streamed_incrementally_not_only_at_exit() {
        let terminals = Terminals::default();
        let (tx, mut rx) = channel();
        let id = terminals
            .create("echo", &["streamed".into()], &[], cwd(), None, tx)
            .unwrap();

        let mut streamed = Vec::new();
        let mut saw_exit = false;

        // Read until both have arrived rather than stopping at the exit event.
        // The reader and the waiter are independent threads, so nothing orders
        // the last output chunk before the exit — a test that assumed
        // otherwise would pass or fail depending on scheduling.
        let collected = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(event) = rx.recv().await {
                match event {
                    WorkspaceEvent::TerminalOutput { chunk, .. } => streamed.extend(chunk),
                    WorkspaceEvent::TerminalExit { .. } => saw_exit = true,
                    _ => {}
                }
                if saw_exit && !streamed.is_empty() {
                    return;
                }
            }
        })
        .await;

        assert!(collected.is_ok(), "timed out; saw_exit={saw_exit}");
        assert!(saw_exit, "no exit event");
        assert!(
            String::from_utf8_lossy(&streamed).contains("streamed"),
            "output never arrived as chunks: {:?}",
            String::from_utf8_lossy(&streamed)
        );
        let _ = id;
    }

    #[tokio::test]
    async fn a_failing_command_reports_its_exit_code() {
        let terminals = Terminals::default();
        let (tx, _rx) = channel();
        let id = terminals
            .create("sh", &["-c".into(), "exit 3".into()], &[], cwd(), None, tx)
            .unwrap();
        assert_eq!(terminals.wait_for_exit(&id).await.unwrap().exit_code, Some(3));
    }

    #[tokio::test]
    async fn the_environment_and_working_directory_are_honoured() {
        let terminals = Terminals::default();
        let (tx, _rx) = channel();
        let id = terminals
            .create(
                "sh",
                &["-c".into(), "echo $MJX_TERM_TEST; pwd".into()],
                &[("MJX_TERM_TEST".into(), "set".into())],
                cwd(),
                None,
                tx,
            )
            .unwrap();

        terminals.wait_for_exit(&id).await.unwrap();
        let output = output_containing(&terminals, &id, &cwd().display().to_string()).await;
        assert!(output.contains("set"), "env not honoured: {output:?}");
    }

    #[tokio::test]
    async fn output_beyond_the_byte_limit_is_truncated_from_the_front() {
        let terminals = Terminals::default();
        let (tx, _rx) = channel();
        // Far more output than the 64-byte budget allows.
        let id = terminals
            .create(
                "sh",
                &["-c".into(), "for i in $(seq 1 500); do echo line$i; done".into()],
                &[],
                cwd(),
                Some(64),
                tx,
            )
            .unwrap();

        terminals.wait_for_exit(&id).await.unwrap();
        // The reader thread may still be draining after the process exits.
        output_containing(&terminals, &id, "line500").await;

        let (output, truncated, _) = terminals.output(&id).unwrap();
        assert!(truncated, "the truncation flag was never set");
        assert!(output.len() <= 64, "kept {} bytes of a 64 budget", output.len());
        // The *end* is what survives: recent output is the useful part.
        assert!(output.contains("line500"), "{output:?}");
        assert!(!output.contains("line1\n"), "{output:?}");
    }

    #[tokio::test]
    async fn a_long_running_process_can_be_killed() {
        let terminals = Terminals::default();
        let (tx, _rx) = channel();
        let id = terminals
            .create("sleep", &["60".into()], &[], cwd(), None, tx)
            .unwrap();

        terminals.kill(&id).unwrap();
        let status = tokio::time::timeout(Duration::from_secs(10), terminals.wait_for_exit(&id))
            .await
            .expect("kill did not take effect")
            .unwrap();
        assert_ne!(status.exit_code, Some(0), "a killed process exited cleanly?");

        // Killing twice is not an error.
        assert!(terminals.kill(&id).is_ok());
    }

    #[tokio::test]
    async fn releasing_forgets_the_terminal() {
        let terminals = Terminals::default();
        let (tx, _rx) = channel();
        let id = terminals
            .create("echo", &["x".into()], &[], cwd(), None, tx)
            .unwrap();

        terminals.wait_for_exit(&id).await.unwrap();
        terminals.release(&id).unwrap();

        assert!(matches!(
            terminals.output(&id),
            Err(WorkspaceError::NoSuchTerminal(_))
        ));
    }

    #[tokio::test]
    async fn an_unknown_terminal_id_is_an_error_not_a_panic() {
        let terminals = Terminals::default();
        assert!(matches!(
            terminals.output("nope"),
            Err(WorkspaceError::NoSuchTerminal(_))
        ));
        assert!(terminals.wait_for_exit("nope").await.is_err());
    }

    #[tokio::test]
    async fn a_command_that_does_not_exist_fails_with_its_name() {
        let terminals = Terminals::default();
        let (tx, _rx) = channel();
        let result = terminals.create("mjx-no-such-command", &[], &[], cwd(), None, tx);
        let Err(err) = result else {
            panic!("spawning a nonexistent command should fail");
        };
        assert!(err.to_string().contains("mjx-no-such-command"), "{err}");
    }

    #[test]
    fn the_output_buffer_keeps_the_tail_within_budget() {
        let mut output = Output::new(5);
        output.push(b"abc");
        assert!(!output.truncated);
        assert_eq!(output.text(), "abc");

        output.push(b"defg");
        assert!(output.truncated);
        assert_eq!(output.text(), "cdefg", "the newest bytes must survive");
    }

    #[test]
    fn a_partial_character_left_by_truncation_does_not_fail_the_read() {
        // Dropping bytes off the front can cut a multi-byte character in half.
        let mut output = Output::new(3);
        output.push("é".as_bytes()); // two bytes
        output.push(b"xy");
        assert!(output.truncated);
        // Lossy rather than an error: the byte is already gone.
        assert!(output.text().ends_with("xy"));
    }
}
