//! A WebSocket transport for the Agent Client Protocol.
//!
//! ACP is only ever spoken over a subprocess's stdio, and a browser can't spawn
//! a subprocess. This server closes that gap: it accepts an ACP connection over
//! a WebSocket, starts the requested agent, and relays between them.
//!
//! There is no authentication, deliberately, so the demo works with no setup.
//! See SECURITY.md; the loopback default below is the compensating control.

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

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
    let state = Arc::new(AppState { config, catalog });

    let mut app = Router::new()
        .route("/api/agents", get(list_agents))
        .route("/api/workspaces", get(list_workspaces))
        .route("/ws", get(websocket))
        .with_state(state);

    if web_dir.is_dir() {
        // `index.html` is the fallback so a client-side route still resolves.
        app = app.fallback_service(
            tower_http::services::ServeDir::new(&web_dir).fallback(
                tower_http::services::ServeFile::new(web_dir.join("index.html")),
            ),
        );
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

    axum::serve(listener, app).await?;
    Ok(())
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

    let info = ext::AgentInfo {
        agent_id: connect.agent.clone(),
        name: agent_name,
        command: command.display(),
        cwd: cwd.display().to_string(),
    };

    // The jail is the session's cwd plus every configured root, so an agent
    // can read a shared library directory while writing only its own project.
    let interceptor = Arc::new(WorkspaceInterceptor::new(
        state.config.workspace_roots.clone(),
        cwd,
    ));

    tracing::info!(agent = %info.agent_id, cwd = %info.cwd, "connection opened");
    ws.on_upgrade(move |socket| serve(socket, agent, interceptor, info))
}

async fn serve(
    socket: WebSocket,
    agent: AgentProcess,
    interceptor: Arc<WorkspaceInterceptor>,
    info: ext::AgentInfo,
) {
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

    relay::run(interceptor, agent, incoming, outgoing, info.clone()).await;

    tracing::info!(agent = %info.agent_id, "connection closed");
}
