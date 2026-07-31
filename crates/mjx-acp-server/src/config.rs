//! `mjx.toml`.
//!
//! Every field has a working default, so the file is optional and a partial
//! file is fine. Paths in it are resolved relative to the file's own directory,
//! not the process's working directory, so `./scripts/demo.sh` behaves the same
//! wherever it's run from.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use mjx_agent_catalog::{AgentOverride, DEFAULT_REGISTRY_URL};
use serde::Deserialize;

/// The parsed configuration, with paths already made absolute.
#[derive(Debug, Clone)]
pub struct Config {
    /// Directory the config lives in; the base for every relative path.
    pub base_dir: PathBuf,
    /// Address to bind.
    pub bind: SocketAddr,
    /// Directories the filesystem capability is confined to, and the working
    /// directories offered in the agent picker.
    pub workspace_roots: Vec<PathBuf>,
    /// Where to fetch the agent registry.
    pub registry_url: String,
    /// Where the registry is cached.
    pub cache_dir: PathBuf,
    /// Locally configured agents, which shadow registry entries.
    pub agents: Vec<AgentOverride>,
    /// MCP servers offered to every agent this server starts.
    pub mcp_servers: Vec<McpServerConfig>,
    /// How agents get their own credentials, in the order they are tried.
    #[allow(dead_code, reason = "read by the auth provider registry")]
    pub auth_providers: Vec<AuthProviderConfig>,
    /// How long an agent keeps running with no browser attached, before it is
    /// reaped. Zero turns resuming off: an agent then dies with its socket.
    pub resume_ttl: Duration,
}

/// One MCP server, resolved: paths made absolute and every `*_from` indirection
/// looked up in the environment.
///
/// Deliberately not an `acp::McpServer`: an `Acp`-transport entry keeps its
/// command *here*, because this server spawns it and the agent never learns how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    /// The name the agent sees, and the key the merge deduplicates on.
    pub name: String,
    /// How the server is reached, and by whom.
    pub kind: McpServerKind,
    /// Why this server cannot be offered at all, if the config could not be
    /// completed. Kept rather than dropped so the sidebar can say *why* a
    /// configured server is missing; a silently shorter list is a support call.
    pub unavailable: Option<String>,
}

/// The four transports, carrying what each one needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerKind {
    /// A command the *agent* spawns. Every ACP agent must support this.
    Stdio(McpLaunch),
    /// An HTTP endpoint the agent connects to.
    Http(McpEndpoint),
    /// An SSE endpoint the agent connects to.
    Sse(McpEndpoint),
    /// A command *this* server spawns, reached by the agent over ACP itself.
    /// The command, its environment and therefore its credentials stay here.
    Acp(McpLaunch),
}

/// A command to run, with the environment it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpLaunch {
    pub command: PathBuf,
    pub args: Vec<String>,
    /// Resolved values, secrets included. Never send this to the browser.
    pub env: Vec<(String, String)>,
}

/// A URL to connect to, with the headers it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpEndpoint {
    pub url: String,
    /// Resolved values, secrets included. Never send this to the browser.
    pub headers: Vec<(String, String)>,
}

impl McpServerConfig {
    /// The wire spelling of the transport, which is also what the sidebar shows.
    pub fn transport(&self) -> &'static str {
        match self.kind {
            McpServerKind::Stdio(_) => "stdio",
            McpServerKind::Http(_) => "http",
            McpServerKind::Sse(_) => "sse",
            McpServerKind::Acp(_) => "acp",
        }
    }

    /// What this server points at, for display: the command or the URL. Never a
    /// value from `env` or `headers`.
    #[allow(dead_code, reason = "read by the sidebar's agent info")]
    pub fn target(&self) -> String {
        match &self.kind {
            McpServerKind::Stdio(launch) | McpServerKind::Acp(launch) => {
                let mut parts = vec![launch.command.display().to_string()];
                parts.extend(launch.args.iter().cloned());
                parts.join(" ")
            }
            McpServerKind::Http(endpoint) | McpServerKind::Sse(endpoint) => endpoint.url.clone(),
        }
    }

    /// The names — not the values — of the environment variables and headers
    /// this server carries, so the UI can show that a credential is in play
    /// without showing the credential.
    #[allow(dead_code, reason = "read by the sidebar's agent info")]
    pub fn secret_names(&self) -> Vec<String> {
        match &self.kind {
            McpServerKind::Stdio(launch) | McpServerKind::Acp(launch) => {
                launch.env.iter().map(|(name, _)| name.clone()).collect()
            }
            McpServerKind::Http(endpoint) | McpServerKind::Sse(endpoint) => endpoint
                .headers
                .iter()
                .map(|(name, _)| name.clone())
                .collect(),
        }
    }
}

/// One `[[auth_providers]]` entry, resolved: every `env_from` indirection
/// looked up in this server's environment.
///
/// The values are here, secrets included. This is the type the browser must
/// never be given — the same rule [`McpServerConfig`] follows, and for the same
/// reason: the server holding credentials is precisely why the browser does not
/// have to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthProviderConfig {
    /// How this provider is named in logs and in the auth panel.
    pub name: String,
    /// Which built-in does the work: `env` or `terminal`.
    pub kind: AuthProviderKind,
    /// Agent ids this applies to. Empty means every agent.
    pub agents: Vec<String>,
    /// Auth method ids this applies to. Empty means whichever it can handle.
    pub methods: Vec<String>,
    /// Resolved values, secrets included. Never send this to the browser.
    pub env: Vec<(String, String)>,
    /// Why this provider cannot do anything, if it cannot. Kept registered
    /// rather than dropped, so the panel can say *which* variable is missing
    /// instead of the list quietly being shorter.
    pub unavailable: Option<String>,
}

/// Which built-in provider an entry configures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthProviderKind {
    /// Supplies environment variables, at spawn time.
    Env,
    /// Runs the agent's own binary as an interactive login.
    Terminal,
}

#[allow(dead_code, reason = "read by the auth provider registry")]
impl AuthProviderConfig {
    /// The wire spelling of the kind, which is also what the panel shows.
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            AuthProviderKind::Env => "env",
            AuthProviderKind::Terminal => "terminal",
        }
    }

    /// The names — not the values — of the variables this provider carries.
    pub fn secret_names(&self) -> Vec<String> {
        self.env.iter().map(|(name, _)| name.clone()).collect()
    }

    /// Whether this entry applies to `agent_id`.
    ///
    /// No `agents` key means every agent, which is the useful default: an
    /// operator setting one API key usually means it for whatever they run.
    pub fn covers_agent(&self, agent_id: &str) -> bool {
        self.agents.is_empty() || self.agents.iter().any(|id| id == agent_id)
    }

    /// Whether this entry applies to the auth method `method_id`.
    pub fn covers_method(&self, method_id: &str) -> bool {
        self.methods.is_empty() || self.methods.iter().any(|id| id == method_id)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    server: RawServer,
    #[serde(default)]
    workspace: RawWorkspace,
    #[serde(default)]
    registry: RawRegistry,
    #[serde(default)]
    agents: Vec<AgentOverride>,
    #[serde(default)]
    mcp_servers: Vec<RawMcpServer>,
    #[serde(default)]
    auth_providers: Vec<RawAuthProvider>,
}

/// An `[[auth_providers]]` entry as written.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAuthProvider {
    name: String,
    /// `env` or `terminal`.
    kind: String,
    /// Agent ids this applies to. Absent means all of them.
    #[serde(default)]
    agents: Vec<String>,
    /// Auth method ids this applies to. Absent means whichever it can handle.
    #[serde(default)]
    methods: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Variable name → the environment variable to read it from, so a
    /// credential need not be written into a file that gets committed.
    #[serde(default)]
    env_from: BTreeMap<String, String>,
}

/// An `[[mcp_servers]]` entry as written. Which fields are allowed depends on
/// `transport`, which serde cannot express here without losing
/// `deny_unknown_fields`, so the combinations are checked in [`mcp_server`].
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMcpServer {
    name: String,
    transport: Option<String>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Variable name → the environment variable to read it from, so a token
    /// need not be written into a file that gets committed.
    #[serde(default)]
    env_from: BTreeMap<String, String>,
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    /// Header name → the environment variable to read it from.
    #[serde(default)]
    headers_from: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    bind: Option<String>,
    resume_ttl_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkspace {
    #[serde(default)]
    roots: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRegistry {
    url: Option<String>,
    cache_dir: Option<String>,
}

/// The address we bind when nothing says otherwise. Loopback on purpose — see
/// SECURITY.md.
pub const DEFAULT_BIND: &str = "127.0.0.1:4321";

/// How long an abandoned agent lives by default.
///
/// Long enough to survive a reload, a network blip and a glance at another tab;
/// short enough that a tab someone closed and forgot does not hold a subprocess
/// and its terminals for the rest of the day.
pub const DEFAULT_RESUME_TTL: Duration = Duration::from_secs(300);

impl Config {
    /// Loads `path`, or returns defaults if it doesn't exist.
    pub fn load(path: &Path) -> Result<Self> {
        let base_dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from("."));

        let raw: RawConfig = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("could not parse {}", path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(path = %path.display(), "no config file; using defaults");
                RawConfig::default()
            }
            Err(err) => {
                return Err(err).with_context(|| format!("could not read {}", path.display()));
            }
        };

        Self::from_raw(raw, base_dir)
    }

    fn from_raw(raw: RawConfig, base_dir: PathBuf) -> Result<Self> {
        let bind = raw.server.bind.as_deref().unwrap_or(DEFAULT_BIND);
        let bind: SocketAddr = bind
            .parse()
            .with_context(|| format!("`server.bind` is not an address: {bind}"))?;

        let mut workspace_roots: Vec<PathBuf> = raw
            .workspace
            .roots
            .iter()
            .map(|root| absolute(&base_dir, root))
            .collect();
        // A viewer with nowhere to look is useless; fall back to the directory
        // the config came from.
        if workspace_roots.is_empty() {
            workspace_roots.push(base_dir.clone());
        }

        let mut mcp_servers: Vec<McpServerConfig> = Vec::with_capacity(raw.mcp_servers.len());
        for (index, raw_server) in raw.mcp_servers.into_iter().enumerate() {
            let server = mcp_server(raw_server, &base_dir)
                .with_context(|| format!("`mcp_servers[{index}]` is not usable"))?;
            // The merge deduplicates by name, so two entries with one name means
            // one of them would never be offered. Say so instead.
            if mcp_servers.iter().any(|other| other.name == server.name) {
                anyhow::bail!(
                    "two `mcp_servers` entries are both named `{}`; names must be unique",
                    server.name
                );
            }
            if let Some(reason) = &server.unavailable {
                tracing::warn!(server = %server.name, %reason, "an MCP server cannot be offered");
            }
            mcp_servers.push(server);
        }

        let mut auth_providers: Vec<AuthProviderConfig> =
            Vec::with_capacity(raw.auth_providers.len());
        for (index, raw_provider) in raw.auth_providers.into_iter().enumerate() {
            let provider = auth_provider(raw_provider)
                .with_context(|| format!("`auth_providers[{index}]` is not usable"))?;
            // Order is dispatch order, so two entries with one name is an
            // ambiguity in the logs and the panel rather than in the behaviour —
            // but it is still not what the author meant.
            if auth_providers
                .iter()
                .any(|other| other.name == provider.name)
            {
                anyhow::bail!(
                    "two `auth_providers` entries are both named `{}`; names must be unique",
                    provider.name
                );
            }
            if let Some(reason) = &provider.unavailable {
                tracing::warn!(provider = %provider.name, %reason, "an auth provider is not usable");
            }
            auth_providers.push(provider);
        }

        Ok(Self {
            bind,
            workspace_roots,
            mcp_servers,
            auth_providers,
            registry_url: raw
                .registry
                .url
                .unwrap_or_else(|| DEFAULT_REGISTRY_URL.to_string()),
            cache_dir: absolute(
                &base_dir,
                raw.registry.cache_dir.as_deref().unwrap_or(".mjx-cache"),
            ),
            agents: raw.agents,
            resume_ttl: raw
                .server
                .resume_ttl_secs
                .map_or(DEFAULT_RESUME_TTL, Duration::from_secs),
            base_dir,
        })
    }

    /// The working directory to use when the request doesn't name one.
    pub fn default_cwd(&self) -> &Path {
        &self.workspace_roots[0]
    }

    /// Whether `candidate` is inside one of the workspace roots.
    ///
    /// Both sides are canonicalized first, so `../` and symlinks can't be used
    /// to step outside a root.
    pub fn is_within_roots(&self, candidate: &Path) -> bool {
        let Ok(candidate) = candidate.canonicalize() else {
            return false;
        };
        self.workspace_roots.iter().any(|root| {
            root.canonicalize()
                .is_ok_and(|root| candidate.starts_with(root))
        })
    }
}

/// Turns one `[[auth_providers]]` entry into a resolved [`AuthProviderConfig`].
///
/// The same split [`mcp_server`] makes: an unusable *shape* is an error, because
/// the file does not say what its author thought; a missing `env_from` variable
/// is not, because one absent credential should not stop the server starting for
/// every other agent. The entry stays, marked unavailable and naming the
/// variable, so the panel can say what to set rather than showing a shorter list
/// with no explanation.
fn auth_provider(raw: RawAuthProvider) -> Result<AuthProviderConfig> {
    if raw.name.trim().is_empty() {
        anyhow::bail!("`name` is required and cannot be blank");
    }
    let kind = match raw.kind.as_str() {
        "env" => AuthProviderKind::Env,
        "terminal" => AuthProviderKind::Terminal,
        other => anyhow::bail!("`kind` must be `env` or `terminal`, not `{other}`"),
    };

    // A terminal login runs the agent's own binary, so it has nothing to read
    // out of the environment on the operator's behalf. Silently ignoring the
    // keys would leave an operator believing a credential was being passed.
    if kind == AuthProviderKind::Terminal && !(raw.env.is_empty() && raw.env_from.is_empty()) {
        anyhow::bail!(
            "a `terminal` provider takes no `env` or `env_from`: it runs the agent's own \
             binary, and the environment for that comes from the auth method the agent \
             advertised"
        );
    }

    let mut unavailable = None;
    let env = resolve_secrets(raw.env, raw.env_from, "env", &mut unavailable)?;

    // An `env` provider with nothing to contribute would decline every method
    // for a reason nobody could act on. Say so at load time instead.
    if kind == AuthProviderKind::Env && env.is_empty() && unavailable.is_none() {
        anyhow::bail!("an `env` provider needs at least one `env` or `env_from` entry");
    }

    Ok(AuthProviderConfig {
        name: raw.name,
        kind,
        agents: raw.agents,
        methods: raw.methods,
        env,
        unavailable,
    })
}

/// Turns one `[[mcp_servers]]` entry into a resolved [`McpServerConfig`].
///
/// Wrong *combinations* are errors — a `url` on a stdio server means the file
/// does not say what its author thought it said. A missing `env_from` variable
/// is not: the entry is kept, marked unavailable, and reported.
fn mcp_server(raw: RawMcpServer, base_dir: &Path) -> Result<McpServerConfig> {
    if raw.name.trim().is_empty() {
        anyhow::bail!("`name` is required and cannot be blank");
    }
    let transport = raw.transport.as_deref().unwrap_or("stdio");

    // Checked before anything is built, because the point is to catch a key that
    // would be *ignored* — and every ignored key is one the other transport
    // would have read, which means the file does not say what its author thought.
    let stray: Vec<(&str, bool)> = match transport {
        "stdio" | "acp" => vec![
            ("url", raw.url.is_some()),
            ("headers", !raw.headers.is_empty()),
            ("headers_from", !raw.headers_from.is_empty()),
        ],
        _ => vec![
            ("command", raw.command.is_some()),
            ("args", !raw.args.is_empty()),
            ("env", !raw.env.is_empty()),
            ("env_from", !raw.env_from.is_empty()),
        ],
    };
    if let Some((key, _)) = stray.iter().find(|(_, present)| *present) {
        anyhow::bail!("`{key}` means nothing for transport `{transport}`");
    }

    let mut unavailable = None;

    let kind = match transport {
        "stdio" | "acp" => {
            let command = raw
                .command
                .as_deref()
                .with_context(|| format!("transport `{transport}` needs a `command` to run"))?;
            let env = resolve_secrets(raw.env, raw.env_from, "env", &mut unavailable)?;
            let launch = McpLaunch {
                command: command_path(base_dir, command),
                args: raw.args,
                env,
            };
            if transport == "stdio" {
                McpServerKind::Stdio(launch)
            } else {
                McpServerKind::Acp(launch)
            }
        }
        "http" | "sse" => {
            let url = raw
                .url
                .clone()
                .with_context(|| format!("transport `{transport}` needs a `url`"))?;
            let headers =
                resolve_secrets(raw.headers, raw.headers_from, "headers", &mut unavailable)?;
            let endpoint = McpEndpoint { url, headers };
            if transport == "http" {
                McpServerKind::Http(endpoint)
            } else {
                McpServerKind::Sse(endpoint)
            }
        }
        other => {
            anyhow::bail!("`transport` is `{other}`; it must be one of stdio, http, sse or acp")
        }
    };

    Ok(McpServerConfig {
        name: raw.name,
        kind,
        unavailable,
    })
}

/// Merges literal values with the ones named indirectly through the
/// environment, refusing to guess when both define the same name.
///
/// A variable that is not set marks the server unavailable rather than failing
/// the whole config: one missing token should not stop the server starting for
/// every other agent.
fn resolve_secrets(
    literal: BTreeMap<String, String>,
    from_env: BTreeMap<String, String>,
    field: &str,
    unavailable: &mut Option<String>,
) -> Result<Vec<(String, String)>> {
    let mut resolved: Vec<(String, String)> = literal.into_iter().collect();
    for (name, variable) in from_env {
        if resolved.iter().any(|(existing, _)| existing == &name) {
            anyhow::bail!("`{name}` is set in both `{field}` and `{field}_from`");
        }
        match std::env::var(&variable) {
            Ok(value) => resolved.push((name, value)),
            Err(_) => {
                unavailable.get_or_insert(format!(
                    "`{variable}` is not set in this server's environment"
                ));
            }
        }
    }
    resolved.sort_by(|(a, _), (b, _)| a.cmp(b));
    Ok(resolved)
}

/// Resolves an MCP server's command.
///
/// Unlike every other path in this file, a bare name is left alone: most MCP
/// servers are run as `npx` or `uvx`, and `<base_dir>/npx` cannot be executed.
/// Anything that looks like a path — absolute, or with a separator, or starting
/// with a dot — is resolved against the config file as usual.
fn command_path(base: &Path, command: &str) -> PathBuf {
    if command.contains('/') || command.starts_with('.') {
        absolute(base, command)
    } else {
        PathBuf::from(command)
    }
}

/// Resolves `path` against `base` unless it is already absolute.
fn absolute(base: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_text: &str) -> Result<Config> {
        Config::from_raw(
            toml::from_str(toml_text).unwrap(),
            PathBuf::from("/project"),
        )
    }

    #[test]
    fn auth_providers_are_read_in_the_order_they_are_written() {
        // Order is dispatch order: the first provider that can handle a method
        // is the one that does, so the file's order is a decision.
        let config = parse(
            r#"
            [[auth_providers]]
            name = "anthropic"
            kind = "env"
            agents = ["claude-acp"]
            methods = ["api-key"]
            env = { ANTHROPIC_API_KEY = "sk-not-a-real-key" }

            [[auth_providers]]
            name = "interactive"
            kind = "terminal"
            "#,
        )
        .unwrap();

        let [first, second] = &config.auth_providers[..] else {
            panic!("{:?}", config.auth_providers);
        };
        assert_eq!(first.name, "anthropic");
        assert_eq!(first.kind_name(), "env");
        assert!(first.covers_agent("claude-acp"));
        assert!(!first.covers_agent("gemini"));
        assert!(first.covers_method("api-key"));
        assert!(!first.covers_method("oauth"));
        // Names, never values. This is what the panel is given.
        assert_eq!(first.secret_names(), ["ANTHROPIC_API_KEY"]);
        assert!(first.unavailable.is_none());

        // No filters means everything, which is the useful default: an operator
        // who configures one login usually means it for whatever they run.
        assert_eq!(second.kind_name(), "terminal");
        assert!(second.covers_agent("anything"));
        assert!(second.covers_method("anything"));
    }

    #[test]
    fn an_auth_credential_is_read_from_the_environment_and_a_missing_one_is_reported() {
        // Unique names: tests share a process, and so an environment.
        // SAFETY: single-threaded here, and the names are not read elsewhere.
        unsafe { std::env::set_var("MJX_TEST_AUTH_KEY", "sk-hh") };

        let config = parse(
            r#"
            [[auth_providers]]
            name = "present"
            kind = "env"
            env_from = { OPENAI_API_KEY = "MJX_TEST_AUTH_KEY" }

            [[auth_providers]]
            name = "absent"
            kind = "env"
            env_from = { OPENAI_API_KEY = "MJX_TEST_AUTH_NOT_SET" }
            "#,
        )
        .unwrap();

        assert_eq!(
            config.auth_providers[0].env,
            [("OPENAI_API_KEY".to_string(), "sk-hh".to_string())]
        );

        // Kept and marked, not dropped and not fatal. One missing credential
        // must not stop the server starting for every other agent, and the
        // panel has to be able to say *which* variable to set.
        let reason = config.auth_providers[1]
            .unavailable
            .as_deref()
            .expect("a missing variable must be reported");
        assert!(reason.contains("MJX_TEST_AUTH_NOT_SET"), "{reason}");
    }

    #[test]
    fn an_auth_provider_that_could_never_work_is_refused_at_load() {
        // Each of these is a file that does not say what its author thought.
        // Better to fail at startup, where somebody is looking, than to decline
        // every method later for a reason nobody can act on.
        let unusable = [
            // Nothing to contribute.
            r#"[[auth_providers]]
               name = "empty"
               kind = "env""#,
            // A terminal login runs the agent's own binary; these would be
            // silently ignored, and the operator would believe otherwise.
            r#"[[auth_providers]]
               name = "confused"
               kind = "terminal"
               env = { TOKEN = "x" }"#,
            // Not a provider this server has.
            r#"[[auth_providers]]
               name = "oauth"
               kind = "oauth""#,
            r#"[[auth_providers]]
               name = ""
               kind = "terminal""#,
            // Two entries with one name is an ambiguity in the logs and the
            // panel, even though dispatch would still be deterministic.
            r#"[[auth_providers]]
               name = "twice"
               kind = "terminal"
               [[auth_providers]]
               name = "twice"
               kind = "terminal""#,
        ];
        for text in unusable {
            assert!(parse(text).is_err(), "should have been refused:\n{text}");
        }

        // A key nothing reads is refused too, which is what
        // `deny_unknown_fields` buys: a misspelled `agents` would otherwise
        // quietly widen the provider to every agent. Checked against `RawConfig`
        // because this one fails in the TOML parse, before `from_raw` runs.
        assert!(
            toml::from_str::<RawConfig>(
                "[[auth_providers]]\nname = \"typo\"\nkind = \"terminal\"\nagent = [\"c\"]"
            )
            .is_err()
        );
    }

    #[test]
    fn an_empty_config_is_valid_and_binds_loopback() {
        let config = parse("").unwrap();
        assert_eq!(config.bind.to_string(), DEFAULT_BIND);
        assert!(config.bind.ip().is_loopback());
        assert_eq!(config.workspace_roots, [PathBuf::from("/project")]);
        assert_eq!(config.registry_url, DEFAULT_REGISTRY_URL);
    }

    #[test]
    fn relative_paths_resolve_against_the_config_file() {
        let config = parse(
            r#"
            [workspace]
            roots = ["demo/workspace", "/absolute/elsewhere"]
            [registry]
            cache_dir = ".mjx-cache"
            "#,
        )
        .unwrap();
        assert_eq!(
            config.workspace_roots,
            [
                PathBuf::from("/project/demo/workspace"),
                PathBuf::from("/absolute/elsewhere")
            ]
        );
        assert_eq!(config.cache_dir, PathBuf::from("/project/.mjx-cache"));
    }

    #[test]
    fn agent_overrides_are_read() {
        let config = parse(
            r#"
            [[agents]]
            id = "mock"
            name = "Mock Agent"
            command = "target/debug/mjx-mock-agent"

            [[agents]]
            id = "kilo"
            name = "Kilo"
            command = "kilo"
            args = ["acp"]
            "#,
        )
        .unwrap();
        assert_eq!(config.agents.len(), 2);
        assert_eq!(config.agents[1].args, ["acp"]);
    }

    #[test]
    fn every_mcp_transport_is_read() {
        let config = parse(
            r#"
            [[mcp_servers]]
            name = "git"
            command = "npx"
            args = ["-y", "@modelcontextprotocol/server-git"]
            env = { GIT_DIR = ".git" }

            [[mcp_servers]]
            name = "docs"
            transport = "http"
            url = "https://example.com/mcp"
            headers = { Authorization = "Bearer t" }

            [[mcp_servers]]
            name = "feed"
            transport = "sse"
            url = "https://example.com/sse"

            [[mcp_servers]]
            name = "private"
            transport = "acp"
            command = "./scripts/private-mcp"
            "#,
        )
        .unwrap();

        let transports: Vec<&str> = config.mcp_servers.iter().map(|s| s.transport()).collect();
        assert_eq!(transports, ["stdio", "http", "sse", "acp"]);
        assert!(config.mcp_servers.iter().all(|s| s.unavailable.is_none()));

        let McpServerKind::Stdio(git) = &config.mcp_servers[0].kind else {
            panic!(
                "the first entry should be stdio: {:?}",
                config.mcp_servers[0]
            );
        };
        // A bare command stays bare, so PATH can find it.
        assert_eq!(git.command, PathBuf::from("npx"));
        assert_eq!(git.args, ["-y", "@modelcontextprotocol/server-git"]);
        assert_eq!(git.env, [("GIT_DIR".to_string(), ".git".to_string())]);

        // A command that looks like a path is resolved against the config file,
        // exactly like every other path here.
        let McpServerKind::Acp(private) = &config.mcp_servers[3].kind else {
            panic!("the last entry should be acp: {:?}", config.mcp_servers[3]);
        };
        assert_eq!(
            private.command,
            PathBuf::from("/project/./scripts/private-mcp")
        );
    }

    #[test]
    fn a_secret_is_read_from_the_environment_and_a_missing_one_is_reported() {
        // Unique names: tests share a process, and so an environment.
        // SAFETY: single-threaded here, and the names are not read elsewhere.
        unsafe { std::env::set_var("MJX_TEST_MCP_TOKEN", "sh-hh") };

        let config = parse(
            r#"
            [[mcp_servers]]
            name = "present"
            transport = "http"
            url = "https://example.com/mcp"
            headers_from = { Authorization = "MJX_TEST_MCP_TOKEN" }

            [[mcp_servers]]
            name = "absent"
            command = "npx"
            env_from = { TOKEN = "MJX_TEST_MCP_NOT_SET" }
            "#,
        )
        .unwrap();

        let McpServerKind::Http(present) = &config.mcp_servers[0].kind else {
            panic!("{:?}", config.mcp_servers[0]);
        };
        assert_eq!(
            present.headers,
            [("Authorization".to_string(), "sh-hh".to_string())]
        );
        assert!(config.mcp_servers[0].unavailable.is_none());

        // Kept, not dropped: the sidebar has to be able to say why it is missing.
        let absent = &config.mcp_servers[1];
        let reason = absent.unavailable.as_deref().unwrap_or("");
        assert!(reason.contains("MJX_TEST_MCP_NOT_SET"), "{reason}");

        unsafe { std::env::remove_var("MJX_TEST_MCP_TOKEN") };
    }

    #[test]
    fn a_key_the_transport_cannot_use_is_an_error() {
        let cases = [
            // A url on a spawned server: one of the two was meant to go.
            (
                "url",
                r#"name = "x""#,
                r#"command = "npx""#,
                r#"url = "http://h""#,
            ),
            (
                "command",
                r#"name = "x""#,
                r#"transport = "http""#,
                "command = \"npx\"\nurl = \"http://h\"",
            ),
        ];
        for (key, name, transport, stray) in cases {
            let text = format!("[[mcp_servers]]\n{name}\n{transport}\n{stray}\n");
            let err = parse(&text).unwrap_err();
            assert!(
                format!("{err:#}").contains(key),
                "`{key}` should be reported: {err:#}"
            );
        }

        // A transport with nothing to reach, and a transport we have never heard
        // of, are both errors rather than a server that quietly never appears.
        for text in [
            "[[mcp_servers]]\nname = \"x\"\n",
            "[[mcp_servers]]\nname = \"x\"\ntransport = \"http\"\n",
            "[[mcp_servers]]\nname = \"x\"\ntransport = \"telepathy\"\ncommand = \"npx\"\n",
            // A blank name has nothing to deduplicate on and nothing to show.
            "[[mcp_servers]]\nname = \"\"\ncommand = \"npx\"\n",
        ] {
            assert!(parse(text).is_err(), "should have been refused: {text}");
        }
    }

    #[test]
    fn two_servers_with_one_name_are_refused() {
        // The merge deduplicates by name, so the second would never be offered.
        let err = parse(
            r#"
            [[mcp_servers]]
            name = "git"
            command = "a"
            [[mcp_servers]]
            name = "git"
            command = "b"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("git"), "{err:#}");
    }

    #[test]
    fn a_bad_bind_address_is_an_error_rather_than_a_silent_default() {
        let err = parse("[server]\nbind = \"not-an-address\"").unwrap_err();
        assert!(err.to_string().contains("server.bind"), "{err}");
    }

    #[test]
    fn the_resume_window_defaults_to_five_minutes_and_can_be_turned_off() {
        assert_eq!(parse("").unwrap().resume_ttl, DEFAULT_RESUME_TTL);
        assert_eq!(
            parse("[server]\nresume_ttl_secs = 30").unwrap().resume_ttl,
            Duration::from_secs(30)
        );
        // Zero is the escape hatch back to an agent that dies with its socket.
        assert_eq!(
            parse("[server]\nresume_ttl_secs = 0").unwrap().resume_ttl,
            Duration::ZERO
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        // A typo in a config file should say so, not be silently ignored.
        assert!(toml::from_str::<RawConfig>("[server]\nport = 80").is_err());
        assert!(toml::from_str::<RawConfig>("[nonsense]\nx = 1").is_err());
        assert!(
            toml::from_str::<RawConfig>("[[mcp_servers]]\nname = \"x\"\ncmd = \"npx\"").is_err()
        );
    }

    #[test]
    fn the_missing_file_case_yields_defaults() {
        let config = Config::load(Path::new("/definitely/not/here/mjx.toml")).unwrap();
        assert_eq!(config.bind.to_string(), DEFAULT_BIND);
    }

    #[test]
    fn root_containment_resolves_traversal_and_symlinks() {
        let temp = std::env::temp_dir().canonicalize().unwrap();
        let root = temp.join("mjx-config-test-root");
        let outside = temp.join("mjx-config-test-outside");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("nested/in.txt"), "x").unwrap();
        std::fs::write(outside.join("out.txt"), "x").unwrap();

        let config = Config {
            base_dir: temp.clone(),
            bind: DEFAULT_BIND.parse().unwrap(),
            workspace_roots: vec![root.clone()],
            registry_url: String::new(),
            cache_dir: temp.clone(),
            agents: vec![],
            mcp_servers: vec![],
            auth_providers: vec![],
            resume_ttl: DEFAULT_RESUME_TTL,
        };

        assert!(config.is_within_roots(&root.join("nested/in.txt")));
        assert!(!config.is_within_roots(&outside.join("out.txt")));
        // The traversal resolves to a real file outside the root, and is
        // still rejected.
        assert!(!config.is_within_roots(&root.join("../mjx-config-test-outside/out.txt")));
        // A path that doesn't exist can't be proven safe, so it isn't allowed.
        assert!(!config.is_within_roots(&root.join("nope.txt")));

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
