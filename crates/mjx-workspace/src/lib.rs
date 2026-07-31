//! The ACP client capabilities a browser cannot provide.
//!
//! The browser is the ACP client, but the workspace lives on the server, so
//! `fs/*` and `terminal/*` are answered here instead of being forwarded. Every
//! operation also emits a [`WorkspaceEvent`] so the UI can still show what
//! happened — a live terminal, a diff of a file the agent rewrote — even though
//! the requests never reached it.

use std::path::PathBuf;

pub mod fs;
pub mod terminal;

pub use terminal::{ExitStatus, TerminalSpec, Terminals};

/// Why an operation could not be performed.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// The path resolved outside every workspace root.
    #[error("{0} is outside the workspace")]
    OutsideWorkspace(PathBuf),
    /// ACP requires absolute paths.
    #[error("{0} is not an absolute path")]
    NotAbsolute(PathBuf),
    /// No such file or directory.
    #[error("{0} does not exist")]
    NotFound(PathBuf),
    /// The filesystem refused.
    #[error("{0}: {1}")]
    Io(PathBuf, String),
    /// A terminal could not be started or driven.
    #[error("{0}")]
    Terminal(String),
    /// No terminal with that id, or it was already released.
    #[error("no such terminal: {0}")]
    NoSuchTerminal(String),
    /// The terminal exists but takes no input, because the agent owns it.
    #[error("terminal {0} does not take input")]
    NotInteractive(String),
}

impl WorkspaceError {
    /// The JSON-RPC error code to answer the agent with.
    ///
    /// A refusal must be distinguishable from a missing file: an agent told
    /// "not found" for a file it can see would keep retrying, whereas
    /// `-32602` tells it the request itself was wrong.
    pub fn code(&self) -> i64 {
        match self {
            // -32002 is ACP's "resource not found".
            Self::NotFound(_) | Self::NoSuchTerminal(_) => -32002,
            // Refused, not absent: the terminal is right there, and asking
            // again will not change the answer.
            Self::OutsideWorkspace(_) | Self::NotAbsolute(_) | Self::NotInteractive(_) => -32602,
            Self::Io(..) | Self::Terminal(_) => -32603,
        }
    }
}

/// Something the workspace did, mirrored to the browser as an `_mjx/*`
/// notification.
#[derive(Debug, Clone)]
pub enum WorkspaceEvent {
    /// A terminal started.
    TerminalCreated {
        /// Matches the `terminalId` the agent references in tool call content.
        terminal_id: String,
        /// Program name.
        command: String,
        /// Arguments.
        args: Vec<String>,
        /// Working directory.
        cwd: String,
    },
    /// New bytes from a terminal, since the last event.
    TerminalOutput {
        /// Which terminal.
        terminal_id: String,
        /// Raw bytes: escape sequences and partial UTF-8 included, because
        /// that is what a terminal emulator needs.
        chunk: Vec<u8>,
        /// Whether output has been discarded to stay within the byte budget.
        truncated: bool,
    },
    /// A terminal's process exited.
    TerminalExit {
        /// Which terminal.
        terminal_id: String,
        /// How it ended.
        status: ExitStatus,
    },
    /// A file was rewritten, with its before and after.
    FileWritten {
        /// Absolute path.
        path: String,
        /// Contents before; `None` if the file was created.
        old_text: Option<String>,
        /// Contents after.
        new_text: String,
    },
}

/// One connection's view of the workspace.
pub struct Workspace {
    roots: Vec<PathBuf>,
    /// Working directory for terminals that don't name one.
    cwd: PathBuf,
    terminals: Terminals,
    events: tokio::sync::mpsc::UnboundedSender<WorkspaceEvent>,
}

impl Workspace {
    /// Builds a workspace confined to `roots`.
    pub fn new(
        roots: Vec<PathBuf>,
        cwd: PathBuf,
        events: tokio::sync::mpsc::UnboundedSender<WorkspaceEvent>,
    ) -> Self {
        Self {
            roots,
            cwd,
            terminals: Terminals::default(),
            events,
        }
    }

    /// Reads a file, honouring the protocol's 1-based `line` and `limit`.
    pub fn read_text_file(
        &self,
        path: &std::path::Path,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> Result<String, WorkspaceError> {
        fs::read_text_file(&self.roots, path, line, limit)
    }

    /// Writes a file and mirrors the change to the browser as a diff.
    pub fn write_text_file(
        &self,
        path: &std::path::Path,
        contents: &str,
    ) -> Result<(), WorkspaceError> {
        let old_text = fs::write_text_file(&self.roots, path, contents)?;
        let _ = self.events.send(WorkspaceEvent::FileWritten {
            path: path.display().to_string(),
            old_text,
            new_text: contents.to_string(),
        });
        Ok(())
    }

    /// Starts a terminal. `cwd` defaults to the session's working directory,
    /// and must be inside a workspace root when given.
    pub fn create_terminal(
        &self,
        command: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: Option<PathBuf>,
        output_byte_limit: Option<usize>,
    ) -> Result<String, WorkspaceError> {
        let cwd = match cwd {
            Some(cwd) => fs::resolve_within(&self.roots, &cwd, true)?,
            None => self.cwd.clone(),
        };

        // `Terminals::create` emits `TerminalCreated` itself, before it starts
        // reading, so no output can precede the announcement.
        //
        // Never interactive. This is the door the *agent* comes through, and it
        // owns the process on the other side.
        self.terminals.create(
            TerminalSpec {
                command,
                args,
                env,
                cwd,
                output_byte_limit,
                interactive: false,
            },
            self.events.clone(),
        )
    }

    /// Starts a terminal for a login flow, which the browser may type into.
    ///
    /// A separate door from [`Workspace::create_terminal`] rather than a flag on
    /// it, so the agent's path has no way to ask for a writable terminal even by
    /// mistake. What runs here is never the browser's choice: the caller builds
    /// the command from the agent's own binary and the arguments the agent
    /// advertised.
    pub fn create_login_terminal(
        &self,
        command: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<String, WorkspaceError> {
        self.terminals.create(
            TerminalSpec {
                command,
                args,
                env,
                cwd: self.cwd.clone(),
                output_byte_limit: None,
                interactive: true,
            },
            self.events.clone(),
        )
    }

    /// Writes to a login terminal's stdin. Refused on the agent's own terminals.
    pub fn write_terminal(&self, terminal_id: &str, bytes: &[u8]) -> Result<(), WorkspaceError> {
        self.terminals.write(terminal_id, bytes)
    }

    /// Tells a terminal how large the browser is showing it.
    pub fn resize_terminal(
        &self,
        terminal_id: &str,
        rows: u16,
        cols: u16,
    ) -> Result<(), WorkspaceError> {
        self.terminals.resize(terminal_id, rows, cols)
    }

    /// A terminal's retained output, whether it was truncated, and its exit
    /// status if it has finished.
    pub fn terminal_output(
        &self,
        terminal_id: &str,
    ) -> Result<(String, bool, Option<ExitStatus>), WorkspaceError> {
        self.terminals.output(terminal_id)
    }

    /// Waits for a terminal's process to exit.
    pub async fn wait_for_terminal_exit(
        &self,
        terminal_id: &str,
    ) -> Result<ExitStatus, WorkspaceError> {
        self.terminals.wait_for_exit(terminal_id).await
    }

    /// Signals a terminal's process.
    pub fn kill_terminal(&self, terminal_id: &str) -> Result<(), WorkspaceError> {
        self.terminals.kill(terminal_id)
    }

    /// Discards a terminal.
    pub fn release_terminal(&self, terminal_id: &str) -> Result<(), WorkspaceError> {
        self.terminals.release(terminal_id)
    }

    /// Kills every terminal. Called when the connection ends, so a closed
    /// browser tab doesn't leave a build running.
    pub fn release_all_terminals(&self) {
        self.terminals.release_all();
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        // A browser that closes its tab must not leave a build running.
        self.terminals.release_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (
        tempfile::TempDir,
        Workspace,
        tokio::sync::mpsc::UnboundedReceiver<WorkspaceEvent>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let workspace = Workspace::new(vec![root.clone()], root, tx);
        (dir, workspace, rx)
    }

    #[test]
    fn error_codes_distinguish_a_refusal_from_a_missing_file() {
        assert_eq!(WorkspaceError::NotFound("/x".into()).code(), -32002);
        assert_eq!(WorkspaceError::NoSuchTerminal("t".into()).code(), -32002);
        assert_eq!(WorkspaceError::OutsideWorkspace("/x".into()).code(), -32602);
        assert_eq!(WorkspaceError::NotAbsolute("x".into()).code(), -32602);
        assert_eq!(WorkspaceError::Terminal("boom".into()).code(), -32603);
    }

    #[tokio::test]
    async fn writing_a_file_emits_a_diff_for_the_browser() {
        let (dir, workspace, mut events) = workspace();
        let path = dir.path().canonicalize().unwrap().join("f.txt");

        workspace.write_text_file(&path, "first").unwrap();
        let WorkspaceEvent::FileWritten {
            old_text, new_text, ..
        } = events.try_recv().unwrap()
        else {
            panic!("expected a FileWritten event");
        };
        assert_eq!(old_text, None, "a created file has no previous contents");
        assert_eq!(new_text, "first");

        workspace.write_text_file(&path, "second").unwrap();
        let WorkspaceEvent::FileWritten {
            old_text, new_text, ..
        } = events.try_recv().unwrap()
        else {
            panic!("expected a FileWritten event");
        };
        assert_eq!(old_text.as_deref(), Some("first"));
        assert_eq!(new_text, "second");
    }

    #[tokio::test]
    async fn a_refused_write_emits_nothing() {
        let (_dir, workspace, mut events) = workspace();
        assert!(
            workspace
                .write_text_file(std::path::Path::new("/etc/mjx-should-not-exist"), "x")
                .is_err()
        );
        assert!(
            events.try_recv().is_err(),
            "a refused write must not be reported as if it happened"
        );
    }

    #[tokio::test]
    async fn creating_a_terminal_announces_it_before_any_output() {
        let (_dir, workspace, mut events) = workspace();
        let id = workspace
            .create_terminal("echo", &["hi".into()], &[], None, None)
            .unwrap();

        // The announcement has to come first, or the UI would receive output
        // for a terminal it has never heard of.
        let WorkspaceEvent::TerminalCreated {
            terminal_id,
            command,
            ..
        } = events.recv().await.unwrap()
        else {
            panic!("the first event must be TerminalCreated");
        };
        assert_eq!(terminal_id, id);
        assert_eq!(command, "echo");
    }

    #[tokio::test]
    async fn a_terminal_cwd_outside_the_roots_is_refused() {
        let (_dir, workspace, _events) = workspace();
        let err = workspace
            .create_terminal("echo", &[], &[], Some("/etc".into()), None)
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::OutsideWorkspace(_)), "{err}");
    }

    #[tokio::test]
    async fn only_a_login_terminal_can_be_typed_into() {
        // Two doors rather than a flag on one, so the agent's path has no way to
        // ask for a writable terminal even by mistake. This test is what says
        // the two doors really differ.
        let (_dir, workspace, _events) = workspace();

        let agents = workspace
            .create_terminal("sleep", &["60".into()], &[], None, None)
            .unwrap();
        let err = workspace.write_terminal(&agents, b"whoami\n").unwrap_err();
        assert!(matches!(err, WorkspaceError::NotInteractive(_)), "{err}");

        let login = workspace.create_login_terminal("cat", &[], &[]).unwrap();
        workspace.write_terminal(&login, b"ok\n").unwrap();

        // Size is not input, so it is allowed on both: the worst a wrong one
        // does is make output wrap badly, and the browser renders both.
        workspace.resize_terminal(&agents, 40, 100).unwrap();
        workspace.resize_terminal(&login, 40, 100).unwrap();
    }

    #[tokio::test]
    async fn dropping_the_workspace_releases_running_terminals() {
        let (_dir, workspace, _events) = workspace();
        let id = workspace
            .create_terminal("sleep", &["60".into()], &[], None, None)
            .unwrap();
        assert!(workspace.terminal_output(&id).is_ok());
        // The point is that Drop runs the release path without panicking and
        // does not leave the process behind.
        drop(workspace);
    }
}
