//! `mjx.toml`.
//!
//! Every field has a working default, so the file is optional and a partial
//! file is fine. Paths in it are resolved relative to the file's own directory,
//! not the process's working directory, so `./scripts/demo.sh` behaves the same
//! wherever it's run from.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

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
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    bind: Option<String>,
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

        Ok(Self {
            bind,
            workspace_roots,
            registry_url: raw
                .registry
                .url
                .unwrap_or_else(|| DEFAULT_REGISTRY_URL.to_string()),
            cache_dir: absolute(
                &base_dir,
                raw.registry.cache_dir.as_deref().unwrap_or(".mjx-cache"),
            ),
            agents: raw.agents,
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
    fn a_bad_bind_address_is_an_error_rather_than_a_silent_default() {
        let err = parse("[server]\nbind = \"not-an-address\"").unwrap_err();
        assert!(err.to_string().contains("server.bind"), "{err}");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        // A typo in a config file should say so, not be silently ignored.
        assert!(toml::from_str::<RawConfig>("[server]\nport = 80").is_err());
        assert!(toml::from_str::<RawConfig>("[nonsense]\nx = 1").is_err());
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
