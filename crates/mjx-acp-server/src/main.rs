//! A WebSocket transport for the Agent Client Protocol.
//!
//! ACP is only ever spoken over a subprocess's stdio, and a browser can't spawn
//! a subprocess. This server closes that gap: it accepts an ACP connection over
//! a WebSocket, starts the requested agent, and relays between them.
//!
//! There is no authentication, deliberately, so the demo works with no setup.
//! See SECURITY.md; the loopback default below is the compensating control.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use clap::Parser;
use futures::StreamExt;
use mjx_acp_core::ext;
use mjx_agent_catalog::Catalog;
use serde::{Deserialize, Serialize};

mod agent_process;
mod config;
mod id_bridge;
mod mcp;
mod relay;
mod sessions;
mod workspace_interceptor;

use agent_process::AgentProcess;
use config::Config;
use workspace_interceptor::WorkspaceInterceptor;

#[derive(Parser, Debug)]
#[command(
    name = "mjx-acp-server",
    about = "A web transport for the Agent Client Protocol"
)]
struct Args {
    /// Path to the configuration file.
    #[arg(long, default_value = "mjx.toml")]
    config: PathBuf,

    /// Override the configured bind address.
    #[arg(long)]
    bind: Option<String>,

    /// Directory to serve the web app from. Defaults to `web/dist` next to the
    /// config file.
    #[arg(long)]
    web_dir: Option<PathBuf>,

    /// Allow binding a non-loopback address. This server has no
    /// authentication: anyone who can reach the port can run commands as you.
    #[arg(long)]
    i_know_this_is_unauthenticated: bool,
}

/// Shared across every request.
struct AppState {
    config: Config,
    catalog: Catalog,
    connections: Connections,
}

/// Agents that outlive their sockets, keyed by the id a browser passes back as
/// `?resume=`.
#[derive(Default)]
struct Connections(std::sync::Mutex<HashMap<String, Arc<Pooled>>>);

/// One agent, still running, waiting for a browser.
struct Pooled {
    /// What it was started for. A resume asking for a different agent or a
    /// different directory is not this connection, and must not be answered by
    /// it — an id is a handle to one conversation, not to the pool.
    agent_id: String,
    cwd: PathBuf,
    connection: Arc<relay::Connection<WorkspaceInterceptor>>,
    /// When the last socket went away; `None` while one is attached.
    idle_since: std::sync::Mutex<Option<Instant>>,
}

impl Pooled {
    /// How long this has had nobody watching it.
    fn idle_for(&self) -> Option<Duration> {
        self.idle_since
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .map(|since| since.elapsed())
    }

    fn mark_attached(&self) {
        *self
            .idle_since
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    fn mark_idle(&self) {
        *self
            .idle_since
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
    }
}

impl AppState {
    /// Ends every pooled agent. Called on the way out.
    async fn shutdown_all(&self) {
        let pooled = self.connections.take_all();
        if pooled.is_empty() {
            return;
        }
        tracing::info!(
            agents = pooled.len(),
            "stopping agents that outlived their sockets"
        );
        for pooled in pooled {
            pooled.connection.shutdown().await;
        }
    }
}

impl Connections {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Pooled>>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The pooled agent `id` names, if it is the one being asked for.
    ///
    /// A mismatched agent or directory is treated as no match rather than as an
    /// error, for the same reason an unknown id is: the browser gets a fresh
    /// agent, which is what it wanted, instead of a failed page load.
    fn resume(&self, id: &str, agent_id: &str, cwd: &Path) -> Option<Arc<Pooled>> {
        let pooled = self.lock().get(id).cloned()?;
        (pooled.agent_id == agent_id && pooled.cwd == cwd).then_some(pooled)
    }

    fn insert(&self, id: String, pooled: Arc<Pooled>) {
        self.lock().insert(id, pooled);
    }

    fn remove(&self, id: &str) -> Option<Arc<Pooled>> {
        self.lock().remove(id)
    }

    /// Takes everything, for shutdown.
    fn take_all(&self) -> Vec<Arc<Pooled>> {
        self.lock().drain().map(|(_, pooled)| pooled).collect()
    }

    /// Takes every connection nobody came back to within `ttl`.
    ///
    /// Removed under the lock and shut down outside it: `shutdown` waits on a
    /// subprocess, and holding a `std::sync::Mutex` across an await would be a
    /// deadlock waiting to happen.
    fn take_expired(&self, ttl: Duration) -> Vec<(String, Arc<Pooled>)> {
        let mut connections = self.lock();
        let expired: Vec<String> = connections
            .iter()
            .filter(|(_, pooled)| {
                pooled.connection.is_finished() || pooled.idle_for().is_some_and(|idle| idle >= ttl)
            })
            .map(|(id, _)| id.clone())
            .collect();
        expired
            .into_iter()
            .filter_map(|id| connections.remove(&id).map(|pooled| (id, pooled)))
            .collect()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MJX_LOG")
                .unwrap_or_else(|_| "mjx_acp_server=info,warn".into()),
        )
        .init();

    let args = Args::parse();
    let mut config = Config::load(&args.config)?;

    if let Some(bind) = &args.bind {
        config.bind = bind
            .parse()
            .with_context(|| format!("--bind is not an address: {bind}"))?;
    }

    let ip: IpAddr = config.bind.ip();
    if !ip.is_loopback() && !args.i_know_this_is_unauthenticated {
        bail!(
            "refusing to bind {ip}: this server has no authentication, so anyone who can reach \
             the port can read your files and run commands as you. Bind a loopback address, or \
             pass --i-know-this-is-unauthenticated if you have put auth in front of it. \
             See SECURITY.md."
        );
    }

    let registry = Catalog::fetch_registry(&config.registry_url, &config.cache_dir).await;
    let catalog = Catalog::new(&config.base_dir, config.agents.clone(), registry);

    let entries = catalog.entries();
    tracing::info!(
        agents = entries.len(),
        ready = entries.iter().filter(|e| e.availability.is_ready()).count(),
        "agent catalog loaded"
    );
    for root in &config.workspace_roots {
        tracing::info!(root = %root.display(), "workspace root");
    }

    let web_dir = args
        .web_dir
        .unwrap_or_else(|| config.base_dir.join("web/dist"));
    let bind = config.bind;
    let state = Arc::new(AppState {
        config,
        catalog,
        connections: Connections::default(),
    });
    tokio::spawn(reap_idle_connections(state.clone()));

    let mut app = Router::new()
        .route("/api/agents", get(list_agents))
        .route("/api/workspaces", get(list_workspaces))
        .route("/api/files", get(list_files))
        .route("/api/connections", get(list_connections))
        .route("/ws", get(websocket))
        .with_state(state.clone());

    if web_dir.is_dir() {
        // `index.html` is the fallback so a client-side route still resolves.
        app = app.fallback_service(tower_http::services::ServeDir::new(&web_dir).fallback(
            tower_http::services::ServeFile::new(web_dir.join("index.html")),
        ));
        tracing::info!(dir = %web_dir.display(), "serving the web app");
    } else {
        tracing::warn!(
            dir = %web_dir.display(),
            "no built web app; run `npm run build` in web/, or `npm run dev` for hot reload"
        );
    }

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("could not bind {bind}"))?;
    // The bound address, not the requested one: with port 0 the OS picks, and
    // the actual port is the only useful thing to report.
    let bound = listener.local_addr().unwrap_or(bind);
    tracing::info!("listening on http://{bound}");

    axum::serve(listener, app)
        .with_graceful_shutdown(interrupted())
        .await?;

    // Agents outlive their sockets now, so at any moment there may be several
    // running with nobody watching them. They have to be ended here: a
    // subprocess whose parent has gone is reparented to init and keeps whatever
    // terminals it started, which is exactly the orphan this feature is
    // supposed to avoid rather than create.
    state.shutdown_all().await;
    Ok(())
}

/// Resolves when the process is asked to stop.
///
/// SIGKILL cannot be caught, so nothing here saves an agent from `kill -9` on
/// the server. Ctrl-C and `systemctl stop` are what actually happen.
async fn interrupted() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => {
                tracing::warn!(%err, "could not listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    tracing::info!("shutting down");
}

async fn list_agents(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    axum::Json(state.catalog.entries())
}

/// One directory an agent may be pointed at.
#[derive(Serialize)]
struct Workspace {
    path: String,
    name: String,
}

async fn list_workspaces(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let roots: Vec<Workspace> = state
        .config
        .workspace_roots
        .iter()
        .map(|root| Workspace {
            path: root.display().to_string(),
            name: root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.display().to_string()),
        })
        .collect();
    axum::Json(roots)
}

/// What the composer asks for when it offers `@`-mention candidates.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FilesQuery {
    /// One configured root. Omitted means every root.
    root: Option<String>,
    /// A case-insensitive substring of the path relative to its root.
    #[serde(default)]
    q: String,
    limit: Option<usize>,
}

/// One mention candidate.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FileEntry {
    root: String,
    path: String,
    rel_path: String,
    name: String,
    is_dir: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Files {
    entries: Vec<FileEntry>,
    truncated: bool,
}

/// How many candidates a caller may ask for, and how many it gets by default.
const FILES_LIMIT_MAX: usize = 500;
const FILES_LIMIT_DEFAULT: usize = 200;

/// Lists workspace filenames so the browser can offer them as mentions.
///
/// Names only, never contents, and always through the same jail `fs/*` uses —
/// see SECURITY.md, which says plainly what this exposes.
async fn list_files(
    State(state): State<Arc<AppState>>,
    Query(query): Query<FilesQuery>,
) -> Response {
    let root = query.root.as_ref().map(PathBuf::from);
    let limit = query.limit.unwrap_or(FILES_LIMIT_DEFAULT).min(FILES_LIMIT_MAX);

    let listing = match mjx_workspace::fs::list_within(
        &state.config.workspace_roots,
        root.as_deref(),
        &query.q,
        limit,
    ) {
        Ok(listing) => listing,
        Err(err) => {
            // A root outside the workspace is the caller's mistake, not ours.
            return (StatusCode::BAD_REQUEST, err.to_string()).into_response();
        }
    };

    let entries = listing
        .entries
        .into_iter()
        .map(|entry| FileEntry {
            rel_path: entry
                .path
                .strip_prefix(&entry.root)
                .unwrap_or(&entry.path)
                .to_string_lossy()
                .into_owned(),
            name: entry
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            root: entry.root.display().to_string(),
            path: entry.path.display().to_string(),
            is_dir: entry.is_dir,
        })
        .collect();

    axum::Json(Files {
        entries,
        truncated: listing.truncated,
    })
    .into_response()
}

/// The first subprotocol the client offered, if any.
fn requested_subprotocol(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::SEC_WEBSOCKET_PROTOCOL)?
        .to_str()
        .ok()?
        .split(',')
        .map(str::trim)
        .find(|protocol| !protocol.is_empty())
        .map(str::to_owned)
}

/// Query string of `/ws`.
#[derive(Debug, Deserialize)]
struct Connect {
    /// Catalog id of the agent to start.
    agent: String,
    /// Working directory for the session. Must be inside a workspace root.
    #[serde(default)]
    cwd: Option<String>,
    /// A connection id from a previous socket, to rejoin the agent it started
    /// rather than start another. Unknown or expired ids are not an error: the
    /// browser gets a fresh agent and is told so.
    #[serde(default)]
    resume: Option<String>,
}

async fn websocket(
    ws: WebSocketUpgrade,
    Query(connect): Query<Connect>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Response {
    // Echo whatever subprotocol the client asked for.
    //
    // A browser refuses the handshake if it sent `Sec-WebSocket-Protocol` and
    // the server didn't answer with one — and the ACP SDK's browser path sends
    // the literal string "undefined", because `new WebSocket(url, undefined)`
    // stringifies its second argument. Node's `ws` is lenient about this, so
    // the failure only shows up in a real browser.
    //
    // Echoing rather than negotiating is deliberate: every subprotocol here
    // carries the same ACP framing, so there is nothing to negotiate, and
    // refusing would break a client that meant no harm.
    let ws = match requested_subprotocol(&headers) {
        Some(protocol) => ws.protocols([protocol]),
        None => ws,
    };

    // Everything that can fail is checked before the upgrade, so a bad request
    // gets a real HTTP status instead of a socket that opens and immediately
    // closes for no visible reason.
    let Some(command) = state.catalog.resolve(&connect.agent) else {
        return (
            StatusCode::NOT_FOUND,
            format!("unknown agent: {}", connect.agent),
        )
            .into_response();
    };

    let cwd = match &connect.cwd {
        Some(cwd) => {
            let path = PathBuf::from(cwd);
            if !state.config.is_within_roots(&path) {
                return (
                    StatusCode::FORBIDDEN,
                    format!("cwd is outside every workspace root: {cwd}"),
                )
                    .into_response();
            }
            path
        }
        None => state.config.default_cwd().to_path_buf(),
    };

    let agent_name = state
        .catalog
        .entries()
        .into_iter()
        .find(|e| e.id == connect.agent)
        .map_or_else(|| connect.agent.clone(), |e| e.name);

    // Rejoin the agent this browser was already talking to, if it is still
    // here. Everything about the connection — the subprocess, the thread, the
    // running terminals — is on the other side of this.
    let resumed = connect
        .resume
        .as_deref()
        .and_then(|id| state.connections.resume(id, &connect.agent, &cwd));

    let (id, pooled) = match resumed {
        Some(pooled) => {
            let id = connect.resume.clone().unwrap_or_default();
            tracing::info!(connection = %id, agent = %connect.agent, "rejoining a running agent");
            (id, pooled)
        }
        None => {
            let agent = match AgentProcess::spawn(&command, &cwd) {
                Ok(agent) => agent,
                Err(err) => {
                    tracing::error!(%err, agent = connect.agent, "could not start the agent");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("could not start {}: {err:#}", connect.agent),
                    )
                        .into_response();
                }
            };

            // Random rather than sequential, because it has to stay unique
            // across restarts of this server: a browser holding a stale id from
            // a previous run must not be handed somebody else's conversation
            // that happens to have been given the same number.
            let id = uuid::Uuid::new_v4().to_string();
            let info = ext::AgentInfo {
                agent_id: connect.agent.clone(),
                name: agent_name,
                command: command.display(),
                cwd: cwd.display().to_string(),
                connection_id: id.clone(),
                // Set per attachment by the relay, which is what knows whether
                // it answered the handshake from a recording.
                resumed: false,
            };

            // The jail is the session's cwd plus every configured root, so an
            // agent can read a shared library directory while writing only its
            // own project.
            let interceptor = Arc::new(WorkspaceInterceptor::new(
                state.config.workspace_roots.clone(),
                cwd.clone(),
            ));

            tracing::info!(connection = %id, agent = %info.agent_id, cwd = %info.cwd, "agent started");
            let pooled = Arc::new(Pooled {
                agent_id: connect.agent.clone(),
                cwd: cwd.clone(),
                connection: relay::start(interceptor, agent, info, state.config.mcp_servers.clone()),
                idle_since: std::sync::Mutex::new(Some(Instant::now())),
            });
            // With resuming turned off there is nothing to come back to, so the
            // connection is never registered and dies with its socket.
            if !state.config.resume_ttl.is_zero() {
                state.connections.insert(id.clone(), pooled.clone());
            }
            (id, pooled)
        }
    };

    ws.on_upgrade(move |socket| serve(socket, state, id, pooled))
}

async fn serve(socket: WebSocket, state: Arc<AppState>, id: String, pooled: Arc<Pooled>) {
    let (sink, stream) = socket.split();

    // Only text frames carry protocol. A close or a transport error ends the
    // stream, which ends the connection; pings are handled by axum itself.
    let incoming = stream.filter_map(|message| async move {
        match message {
            Ok(Message::Text(text)) if !text.trim().is_empty() => Some(text.to_string()),
            Ok(Message::Close(_)) | Err(_) => None,
            Ok(_) => Some(String::new()),
        }
    });
    let incoming = incoming.filter(|line| {
        let keep = !line.is_empty();
        async move { keep }
    });

    let outgoing = futures::sink::unfold(sink, |mut sink, line: String| async move {
        use futures::SinkExt;
        sink.send(Message::Text(line.into())).await?;
        Ok::<_, axum::Error>(sink)
    });

    // Boxed rather than stack-pinned: the relay spawns tasks that own these,
    // so they have to be `'static` and owned, not borrows of a local.
    let incoming = Box::pin(incoming);
    let outgoing = Box::pin(outgoing);

    pooled.mark_attached();
    let detached = pooled.connection.attach(incoming, outgoing).await;
    pooled.mark_idle();
    tracing::info!(connection = %id, ?detached, "socket closed");

    // A browser leaving is not the agent leaving: it stays, and the reaper
    // deals with it if nobody comes back. An agent that has gone is different —
    // there is nothing left to resume, so retire the id now rather than let a
    // reload attach to a corpse and wait for a handshake that never arrives.
    if detached == relay::Detached::AgentGone
        && let Some(pooled) = state.connections.remove(&id)
    {
        pooled.connection.shutdown().await;
    }
}

/// Ends connections nobody came back to.
///
/// An agent that outlives its socket has to be reaped by something, or a closed
/// tab leaves a subprocess and its terminals running for as long as the server
/// does. The sweep is coarse on purpose: the TTL is minutes, so being a few
/// seconds late costs nothing.
async fn reap_idle_connections(state: Arc<AppState>) {
    let ttl = state.config.resume_ttl;
    if ttl.is_zero() {
        return;
    }
    let interval = ttl.min(Duration::from_secs(15)).max(Duration::from_secs(1));

    loop {
        tokio::time::sleep(interval).await;
        for (id, pooled) in state.connections.take_expired(ttl) {
            // Resolved before the macro: an `.await` inside `tracing!` holds a
            // non-Send temporary across it, which makes the whole future
            // non-Send and so unspawnable.
            let pid = pooled.connection.agent_pid().await;
            tracing::info!(
                connection = %id,
                agent = %pooled.agent_id,
                ?pid,
                "reaping a connection nobody came back to"
            );
            pooled.connection.shutdown().await;
        }
    }
}

/// One pooled agent, for `GET /api/connections`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionSummary {
    agent_id: String,
    cwd: String,
    attached: bool,
    /// Seconds since the last socket went away; absent while one is attached.
    #[serde(skip_serializing_if = "Option::is_none")]
    idle_seconds: Option<u64>,
    /// The agent's process id, so that a reaped agent can be shown to be gone
    /// rather than merely forgotten.
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
}

/// Lists the agents currently pooled.
///
/// Deliberately without their connection ids. A connection id is the capability
/// to speak to a running agent — reading its conversation, prompting it, and
/// through it reaching the workspace — and this endpoint has no authentication
/// in front of it. Handing those out here would be a wider hole than the one
/// SECURITY.md already describes.
async fn list_connections(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let pooled: Vec<Arc<Pooled>> = state.connections.lock().values().cloned().collect();

    let mut summaries = Vec::with_capacity(pooled.len());
    for pooled in pooled {
        summaries.push(ConnectionSummary {
            agent_id: pooled.agent_id.clone(),
            cwd: pooled.cwd.display().to_string(),
            attached: pooled.connection.is_attached(),
            idle_seconds: pooled.idle_for().map(|idle| idle.as_secs()),
            pid: pooled.connection.agent_pid().await,
        });
    }
    axum::Json(summaries)
}
