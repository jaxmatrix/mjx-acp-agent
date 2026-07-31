//! The ACP agent registry document.
//!
//! Shape mirrors `https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json`,
//! and the port target is Zed's `crates/project/src/agent_registry_store.rs`.
//! Unknown fields are ignored rather than rejected: the registry gains entries
//! and keys faster than we can track it, and one unfamiliar agent must not take
//! the whole catalog down.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::AgentCommand;

/// The whole registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryDocument {
    /// Schema version of the document itself.
    #[serde(default)]
    pub version: String,
    /// Every published agent.
    #[serde(default)]
    pub agents: Vec<RegistryAgent>,
}

/// One published agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryAgent {
    /// Stable id, e.g. `claude-acp`.
    pub id: String,
    /// Display name.
    pub name: String,
    /// One-line description.
    #[serde(default)]
    pub description: Option<String>,
    /// The agent's own version, distinct from the registry's.
    #[serde(default)]
    pub version: Option<String>,
    /// Icon URL.
    #[serde(default)]
    pub icon: Option<String>,
    /// How to obtain it.
    #[serde(default)]
    pub distribution: Distribution,
}

/// The ways an agent can be obtained. An agent may offer several; we prefer the
/// package-manager forms because they need no download-and-extract step of our
/// own.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Distribution {
    /// Run via `npx`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npx: Option<PackageDistribution>,
    /// Run via `uvx`, for agents published to PyPI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uvx: Option<PackageDistribution>,
    /// Prebuilt binaries, keyed by `os-arch` (e.g. `linux-x86_64`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub binary: BTreeMap<String, BinaryDistribution>,
}

/// A package that starts an ACP agent, run through `npx` or `uvx`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageDistribution {
    /// Package spec. npm uses `name@version`; PyPI uses either `name==version`
    /// or `name@version`.
    pub package: String,
    /// Arguments after the package name.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment the agent needs.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Retained for readers coming from the registry's own naming.
pub type NpxDistribution = PackageDistribution;

/// A downloadable prebuilt binary. Recorded for the UI's benefit; we don't
/// fetch or extract archives.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryDistribution {
    /// Archive URL.
    pub archive: String,
    /// Command inside the extracted archive.
    pub cmd: String,
    /// Arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment the agent needs.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Expected archive digest.
    #[serde(default)]
    pub sha256: Option<String>,
}

impl Distribution {
    /// The command to start this agent, if we can start it without downloading
    /// anything.
    ///
    /// `npx` wins over `uvx` when an agent offers both, only because Node is
    /// more likely to already be present than `uv`.
    pub fn to_command(&self) -> Option<AgentCommand> {
        if let Some(npx) = &self.npx {
            let mut args = vec!["-y".to_string(), bounded_npm_package_spec(&npx.package)];
            args.extend(npx.args.iter().cloned());
            return Some(AgentCommand {
                program: "npx".into(),
                args,
                env: npx.env.clone(),
            });
        }
        if let Some(uvx) = &self.uvx {
            // Python specs are passed through as written: `==0.9.26` is already
            // an exact pin, and npm's hyphen-range syntax means nothing here.
            let mut args = vec![uvx.package.clone()];
            args.extend(uvx.args.iter().cloned());
            return Some(AgentCommand {
                program: "uvx".into(),
                args,
                env: uvx.env.clone(),
            });
        }
        None
    }

    /// The binary entry for the machine we're running on, if any.
    pub fn binary_for_this_host(&self) -> Option<&BinaryDistribution> {
        self.binary.get(&host_target())
    }
}

/// The registry's `os-arch` key for this machine.
fn host_target() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    // The registry happens to use Rust's own arch spelling, so ARCH passes
    // through; only the OS name differs.
    format!("{os}-{}", std::env::consts::ARCH)
}

/// Turns `pkg@1.2.3` into the npm range `pkg@0.0.0 - 1.2.3`, i.e. "at most
/// 1.2.3".
///
/// Ported from Zed's `bounded_npm_package_spec`. An upper bound rather than an
/// exact pin lets npm reuse a version it already has instead of installing on
/// every launch, while still refusing to silently jump to a newer agent whose
/// arguments may have changed.
///
/// Zed uses the hyphen-range form rather than `<=1.2.3` because on Windows npm
/// is a batch file and an unquoted `<` reaching cmd.exe is read as input
/// redirection (zed-industries/zed#55921). We keep the same form for the same
/// reason.
fn bounded_npm_package_spec(package_spec: &str) -> String {
    let Some((package_name, version)) = package_spec.rsplit_once('@') else {
        return package_spec.to_string();
    };
    if package_name.is_empty() || !is_semver(version) {
        return package_spec.to_string();
    }
    format!("{package_name}@0.0.0 - {version}")
}

/// Whether `v` looks like `major.minor.patch`, with optional pre-release and
/// build metadata. Enough to tell a version from a dist-tag such as `latest`,
/// which is all the caller needs.
fn is_semver(v: &str) -> bool {
    let core = v.split_once(['-', '+']).map_or(v, |(core, _)| core);
    let mut parts = core.split('.');
    let valid =
        |p: Option<&str>| p.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    valid(parts.next()) && valid(parts.next()) && valid(parts.next()) && parts.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npx_distributions_become_a_runnable_command() {
        let dist: Distribution = serde_json::from_str(
            r#"{"npx":{"package":"@google/gemini-cli@0.53.0","args":["--acp"]}}"#,
        )
        .unwrap();
        let command = dist.to_command().unwrap();
        assert_eq!(command.program, "npx");
        assert_eq!(
            command.args,
            ["-y", "@google/gemini-cli@0.0.0 - 0.53.0", "--acp"]
        );
    }

    #[test]
    fn scoped_package_names_keep_their_leading_at() {
        // `rsplit_once('@')` must split on the version separator, not on the
        // `@` that starts an npm scope.
        assert_eq!(
            bounded_npm_package_spec("@agentclientprotocol/claude-agent-acp@0.63.0"),
            "@agentclientprotocol/claude-agent-acp@0.0.0 - 0.63.0"
        );
    }

    #[test]
    fn specs_without_a_real_version_are_left_alone() {
        // A dist-tag is not a version, and bounding it would produce nonsense.
        assert_eq!(bounded_npm_package_spec("cline@latest"), "cline@latest");
        assert_eq!(bounded_npm_package_spec("cline"), "cline");
        assert_eq!(bounded_npm_package_spec("@scope/pkg"), "@scope/pkg");
    }

    #[test]
    fn prerelease_versions_are_still_bounded() {
        assert_eq!(
            bounded_npm_package_spec("pkg@1.2.3-beta.1"),
            "pkg@0.0.0 - 1.2.3-beta.1"
        );
    }

    #[test]
    fn semver_detection() {
        assert!(is_semver("0.53.0"));
        assert!(is_semver("1.2.3-beta.1"));
        assert!(is_semver("1.2.3+build"));
        assert!(!is_semver("latest"));
        assert!(!is_semver("1.2"));
        assert!(!is_semver("1.2.3.4"));
        assert!(!is_semver("1.a.3"));
    }

    #[test]
    fn binary_only_distributions_have_no_command() {
        let dist: Distribution = serde_json::from_str(
            r#"{"binary":{"linux-x86_64":{"archive":"https://e/a.tar.gz","cmd":"./x","args":["acp"]}}}"#,
        )
        .unwrap();
        assert!(dist.to_command().is_none());
        // Still parsed, so the UI can say where to get it.
        assert!(dist.binary.contains_key("linux-x86_64"));
    }

    #[test]
    fn unknown_fields_and_agents_do_not_break_parsing() {
        // The real registry grows keys we've never seen. Tolerating them is the
        // difference between one odd agent and an empty picker.
        let doc: RegistryDocument = serde_json::from_str(
            r#"{"version":"1.0.0","futureKey":42,"agents":[
                {"id":"a","name":"A","repository":"https://x","authors":["y"],
                 "license":"MIT","website":"https://z","distribution":{"npx":{"package":"a@1.0.0"}}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(doc.agents.len(), 1);
        assert!(doc.agents[0].distribution.to_command().is_some());
    }

    #[test]
    fn an_agent_with_no_distribution_at_all_parses() {
        let doc: RegistryDocument =
            serde_json::from_str(r#"{"agents":[{"id":"a","name":"A"}]}"#).unwrap();
        assert!(doc.agents[0].distribution.to_command().is_none());
    }

    #[test]
    fn uvx_distributions_pass_their_spec_through_verbatim() {
        // PyPI specs use `==`, which npm's hyphen-range bounding would mangle.
        let dist: Distribution =
            serde_json::from_str(r#"{"uvx":{"package":"fast-agent-acp==0.9.26","args":["-x"]}}"#)
                .unwrap();
        let command = dist.to_command().unwrap();
        assert_eq!(command.program, "uvx");
        assert_eq!(command.args, ["fast-agent-acp==0.9.26", "-x"]);
    }

    #[test]
    fn npx_wins_when_an_agent_offers_both() {
        let dist: Distribution =
            serde_json::from_str(r#"{"npx":{"package":"a@1.0.0"},"uvx":{"package":"a==1.0.0"}}"#)
                .unwrap();
        assert_eq!(dist.to_command().unwrap().program, "npx");
    }

    /// A snapshot of the live registry. Parsing every real entry is the only
    /// way to know the shapes above match what is actually published; the
    /// hand-written cases above only prove we match our own assumptions.
    #[test]
    fn the_real_registry_parses_completely() {
        let doc: RegistryDocument =
            serde_json::from_str(include_str!("../../../fixtures/registry.json"))
                .expect("the published registry must parse");

        assert!(doc.agents.len() >= 30, "suspiciously few agents");

        let runnable = doc
            .agents
            .iter()
            .filter(|a| a.distribution.to_command().is_some())
            .count();
        assert!(
            runnable >= 20,
            "only {runnable} of {} agents resolved to a command",
            doc.agents.len()
        );

        // The four agents this project targets must all be present and,
        // except for binary-only Kilo, directly runnable.
        for id in ["claude-acp", "gemini", "codex-acp", "kilo"] {
            let agent = doc
                .agents
                .iter()
                .find(|a| a.id == id)
                .unwrap_or_else(|| panic!("{id} is missing from the registry"));
            assert!(
                agent.distribution.to_command().is_some(),
                "{id} did not resolve to a command"
            );
        }

        // Every resolved command must be one of the two runners we support.
        for agent in &doc.agents {
            if let Some(command) = agent.distribution.to_command() {
                assert!(
                    matches!(command.program.as_str(), "npx" | "uvx"),
                    "{} resolved to an unexpected runner: {}",
                    agent.id,
                    command.program
                );
                assert!(!command.args.is_empty(), "{} has no args", agent.id);
            }
        }
    }

    #[test]
    fn host_target_matches_the_registry_key_format() {
        let target = host_target();
        assert!(target.contains('-'), "{target}");
        assert!(!target.starts_with("macos"), "macOS must map to darwin");
    }
}
