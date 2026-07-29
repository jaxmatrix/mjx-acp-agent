//! Which agents we can run, and the command line to run each one.
//!
//! Two sources, in priority order:
//!
//! 1. `[[agents]]` entries in `mjx.toml` — an explicit command, which wins so
//!    an already-installed binary can be used instead of paying `npx`.
//! 2. The ACP agent registry, a JSON document listing every published ACP
//!    agent and how to obtain it.
//!
//! Ported from Zed's `crates/project/src/agent_registry_store.rs` and
//! `agent_server_store.rs`, with the `Task`/`Entity` machinery replaced by
//! plain async and the binary-archive downloader left out (see
//! [`Availability::NeedsManualInstall`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

mod registry;

pub use registry::{BinaryDistribution, Distribution, NpxDistribution, RegistryAgent, RegistryDocument};

/// The default ACP agent registry.
pub const DEFAULT_REGISTRY_URL: &str =
    "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

/// A command line that starts an ACP agent speaking JSON-RPC on stdio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCommand {
    /// Program to execute. Resolved against `PATH` unless it contains a slash.
    pub program: String,
    /// Arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment, merged over the server's own.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl AgentCommand {
    /// The command as a display vector, for the inspector and logs.
    pub fn display(&self) -> Vec<String> {
        std::iter::once(self.program.clone())
            .chain(self.args.iter().cloned())
            .collect()
    }
}

/// Whether an agent can actually be started on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Availability {
    /// The program exists and we can run it now.
    Ready,
    /// The program isn't on `PATH`. For `npx` agents this means Node is
    /// missing; for local overrides, the configured command is wrong.
    #[serde(rename_all = "camelCase")]
    MissingProgram {
        /// The program we looked for.
        program: String,
    },
    /// The registry offers this agent only as a downloadable binary archive,
    /// which we don't fetch. Install it yourself and add an `[[agents]]` entry
    /// pointing at it.
    NeedsManualInstall,
}

impl Availability {
    /// Whether the agent can be started.
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// One agent, as offered to the browser's agent picker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEntry {
    /// Stable id, e.g. `claude-acp`. This is what `/ws?agent=` takes.
    pub id: String,
    /// Display name.
    pub name: String,
    /// One-line description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Icon URL from the registry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Whether it can be started.
    pub availability: Availability,
    /// The resolved command, shown in the UI so it's obvious what will run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// True when this came from `mjx.toml` rather than the registry.
    pub is_local_override: bool,
}

/// A locally configured agent, from an `[[agents]]` block in `mjx.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentOverride {
    /// Stable id. Shadows a registry agent with the same id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// One-line description.
    #[serde(default)]
    pub description: Option<String>,
    /// Program to run.
    pub command: String,
    /// Arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Everything we know how to run.
#[derive(Debug, Default)]
pub struct Catalog {
    overrides: Vec<AgentOverride>,
    registry: Option<RegistryDocument>,
    /// Directory paths are resolved against, so a relative `command` in
    /// `mjx.toml` means "relative to the project", not "relative to wherever
    /// the server happened to be started".
    base_dir: PathBuf,
}

impl Catalog {
    /// Builds a catalog from local overrides plus an optional registry.
    pub fn new(
        base_dir: impl Into<PathBuf>,
        overrides: Vec<AgentOverride>,
        registry: Option<RegistryDocument>,
    ) -> Self {
        Self {
            overrides,
            registry,
            base_dir: base_dir.into(),
        }
    }

    /// Fetches the registry, falling back to a cached copy.
    ///
    /// A missing registry is not an error: the local `[[agents]]` entries —
    /// including the mock agent — still work offline, which is the whole point
    /// of shipping one.
    pub async fn fetch_registry(url: &str, cache_dir: &Path) -> Option<RegistryDocument> {
        let cache_file = cache_dir.join("registry.json");

        match fetch(url).await {
            Ok(document) => {
                if let Err(err) = write_cache(&cache_file, &document).await {
                    tracing::warn!(%err, "could not cache the agent registry");
                }
                return Some(document);
            }
            Err(err) => tracing::warn!(%err, url, "could not fetch the agent registry"),
        }

        match tokio::fs::read_to_string(&cache_file).await {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(document) => {
                    tracing::info!("using the cached agent registry");
                    Some(document)
                }
                Err(err) => {
                    tracing::warn!(%err, "the cached agent registry is unreadable");
                    None
                }
            },
            Err(_) => None,
        }
    }

    /// Every agent, local overrides first, then the registry, deduplicated by
    /// id. Ready agents sort ahead of unavailable ones so the picker leads with
    /// what actually works.
    pub fn entries(&self) -> Vec<AgentEntry> {
        let mut entries: Vec<AgentEntry> = Vec::new();

        for over in &self.overrides {
            let command = self.override_command(over);
            entries.push(AgentEntry {
                id: over.id.clone(),
                name: over.name.clone(),
                description: over.description.clone(),
                icon: None,
                availability: availability_of(&command),
                command: Some(command.display()),
                is_local_override: true,
            });
        }

        for agent in self.registry.iter().flat_map(|r| &r.agents) {
            if entries.iter().any(|e| e.id == agent.id) {
                continue;
            }
            let command = agent.distribution.to_command();
            entries.push(AgentEntry {
                id: agent.id.clone(),
                name: agent.name.clone(),
                description: agent.description.clone(),
                icon: agent.icon.clone(),
                availability: match &command {
                    Some(command) => availability_of(command),
                    None => Availability::NeedsManualInstall,
                },
                command: command.as_ref().map(AgentCommand::display),
                is_local_override: false,
            });
        }

        // Runnable first, then the ones configured in `mjx.toml` — someone who
        // named an agent explicitly wants it ahead of thirty from a registry.
        //
        // A stable sort, so within each group the original order survives:
        // `mjx.toml`'s order for the configured ones, and the registry's own
        // (already alphabetical) order for the rest.
        entries.sort_by_key(|e| (!e.availability.is_ready(), !e.is_local_override));
        entries
    }

    /// The command for an agent id, or `None` if we don't know it or can't run
    /// it.
    pub fn resolve(&self, id: &str) -> Option<AgentCommand> {
        if let Some(over) = self.overrides.iter().find(|o| o.id == id) {
            return Some(self.override_command(over));
        }
        self.registry
            .as_ref()?
            .agents
            .iter()
            .find(|a| a.id == id)?
            .distribution
            .to_command()
    }

    fn override_command(&self, over: &AgentOverride) -> AgentCommand {
        AgentCommand {
            program: self.resolve_program(&over.command),
            args: over.args.clone(),
            env: over.env.clone(),
        }
    }

    /// Turns a relative path like `target/debug/mjx-mock-agent` into an
    /// absolute one. A bare name like `kilo` is left alone for `PATH` lookup.
    fn resolve_program(&self, program: &str) -> String {
        if program.contains('/') || program.contains(std::path::MAIN_SEPARATOR) {
            self.base_dir.join(program).display().to_string()
        } else {
            program.to_string()
        }
    }
}

fn availability_of(command: &AgentCommand) -> Availability {
    if program_exists(&command.program) {
        Availability::Ready
    } else {
        Availability::MissingProgram {
            program: command.program.clone(),
        }
    }
}

/// Whether a program can be executed: either an existing path, or a name found
/// on `PATH`.
fn program_exists(program: &str) -> bool {
    let path = Path::new(program);
    if path.is_absolute() || program.contains('/') {
        return path.is_file();
    }
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let candidate = dir.join(program);
                // `.cmd` covers npm shims on Windows.
                candidate.is_file() || candidate.with_extension("cmd").is_file()
            })
        })
        .unwrap_or(false)
}

async fn fetch(url: &str) -> anyhow::Result<RegistryDocument> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    Ok(client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

async fn write_cache(path: &Path, document: &RegistryDocument) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, serde_json::to_vec_pretty(document)?).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog_with_registry(json: &str) -> Catalog {
        Catalog::new("/base", vec![], Some(serde_json::from_str(json).unwrap()))
    }

    fn an_override(id: &str, command: &str, args: &[&str]) -> AgentOverride {
        AgentOverride {
            id: id.into(),
            name: id.into(),
            description: None,
            command: command.into(),
            args: args.iter().map(|s| (*s).into()).collect(),
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn a_local_override_shadows_the_registry() {
        let mut catalog = catalog_with_registry(
            r#"{"version":"1.0.0","agents":[
                {"id":"kilo","name":"Kilo","distribution":{"npx":{"package":"@kilocode/cli@7.4.16","args":["acp"]}}}
            ]}"#,
        );
        catalog.overrides.push(an_override("kilo", "kilo", &["acp"]));

        let entries = catalog.entries();
        let kilo: Vec<_> = entries.iter().filter(|e| e.id == "kilo").collect();
        assert_eq!(kilo.len(), 1, "the registry entry must not be listed twice");
        assert!(kilo[0].is_local_override);
        assert_eq!(
            catalog.resolve("kilo").unwrap().program,
            "kilo",
            "the override's command wins over npx"
        );
    }

    #[test]
    fn relative_override_paths_resolve_against_the_project() {
        let catalog = Catalog::new(
            "/project",
            vec![an_override("mock", "target/debug/mjx-mock-agent", &[])],
            None,
        );
        assert_eq!(
            catalog.resolve("mock").unwrap().program,
            "/project/target/debug/mjx-mock-agent"
        );
    }

    #[test]
    fn bare_program_names_are_left_for_path_lookup() {
        let catalog = Catalog::new("/project", vec![an_override("k", "kilo", &[])], None);
        assert_eq!(catalog.resolve("k").unwrap().program, "kilo");
    }

    #[test]
    fn binary_only_agents_report_that_they_need_installing() {
        let catalog = catalog_with_registry(
            r#"{"version":"1.0.0","agents":[
                {"id":"opencode","name":"OpenCode","distribution":{"binary":{
                    "linux-x86_64":{"archive":"https://example.com/a.tar.gz","cmd":"./opencode","args":["acp"]}
                }}}
            ]}"#,
        );
        let entry = &catalog.entries()[0];
        assert_eq!(entry.availability, Availability::NeedsManualInstall);
        assert!(entry.command.is_none());
        assert!(catalog.resolve("opencode").is_none());
    }

    #[test]
    fn unknown_agents_do_not_resolve() {
        assert!(Catalog::default().resolve("nope").is_none());
    }

    #[test]
    fn ready_agents_sort_ahead_of_unavailable_ones() {
        let catalog = catalog_with_registry(
            r#"{"version":"1.0.0","agents":[
                {"id":"zzz","name":"Zzz","distribution":{"npx":{"package":"z@1.0.0"}}},
                {"id":"needs-install","name":"Aaa","distribution":{"binary":{}}}
            ]}"#,
        );
        let entries = catalog.entries();
        // "Aaa" sorts before "Zzz" alphabetically, but it can't run, so the
        // runnable one leads.
        assert_eq!(entries[0].id, "zzz");
        assert_eq!(entries[1].id, "needs-install");
    }

    #[test]
    fn configured_agents_lead_the_registry() {
        // The demo hinges on the mock agent being the obvious thing to click,
        // not the thirtieth entry in an alphabetical list.
        let mut catalog = catalog_with_registry(
            r#"{"version":"1.0.0","agents":[
                {"id":"aaa","name":"Aaa","distribution":{"npx":{"package":"a@1.0.0"}}}
            ]}"#,
        );
        catalog.overrides.push(an_override("zzz-mock", "npx", &[]));
        catalog.overrides.push(an_override("second", "npx", &[]));

        let entries = catalog.entries();
        assert_eq!(entries[0].id, "zzz-mock", "the configured agents must lead");
        // ...and keep the order they were written in, rather than being
        // alphabetized into a different one.
        assert_eq!(entries[1].id, "second");
        assert_eq!(entries[2].id, "aaa");
    }

    #[test]
    fn a_missing_program_is_reported_rather_than_hidden() {
        let catalog = Catalog::new(
            "/project",
            vec![an_override("gone", "/definitely/not/here", &[])],
            None,
        );
        assert_eq!(
            catalog.entries()[0].availability,
            Availability::MissingProgram {
                program: "/definitely/not/here".into()
            }
        );
        // Still resolvable: the server reports a clear error when the spawn
        // fails rather than pretending the agent doesn't exist.
        assert!(catalog.resolve("gone").is_some());
    }
}
