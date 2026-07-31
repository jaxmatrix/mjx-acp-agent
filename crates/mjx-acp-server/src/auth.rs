//! Builds the auth provider registry from `mjx.toml`.
//!
//! The one place `[[auth_providers]]` entries become [`AuthProvider`]s. Kept
//! apart from the providers themselves because this is where the *server's*
//! knowledge lives: `mjx-agent-auth` is deliberately ignorant of how an operator
//! spells its configuration.

use std::sync::Arc;

use mjx_agent_auth::{AuthProvider, AuthRegistry, EnvVarProvider, TerminalLoginProvider};

use crate::config::{AuthProviderConfig, AuthProviderKind};

/// One provider per configured entry, in the order they were written.
///
/// Order is dispatch order, so the file's order is a decision the operator made
/// and this must not reorder it.
pub fn registry(configured: &[AuthProviderConfig]) -> Arc<AuthRegistry> {
    let providers: Vec<Box<dyn AuthProvider>> = configured.iter().map(provider).collect();
    Arc::new(AuthRegistry::new(providers))
}

fn provider(config: &AuthProviderConfig) -> Box<dyn AuthProvider> {
    match config.kind {
        AuthProviderKind::Env => Box::new(
            EnvVarProvider::new(config.name.clone(), config.env.clone())
                .for_agents(config.agents.clone())
                .for_methods(config.methods.clone())
                .when_unavailable(config.unavailable.clone()),
        ),
        AuthProviderKind::Terminal => Box::new(
            TerminalLoginProvider::new(config.name.clone())
                .for_agents(config.agents.clone())
                .for_methods(config.methods.clone())
                .when_unavailable(config.unavailable.clone()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mjx_acp_core::acp;
    use mjx_agent_auth::{AuthContext, Outcome};
    use mjx_agent_catalog::AgentCommand;

    fn config(name: &str, kind: AuthProviderKind) -> AuthProviderConfig {
        AuthProviderConfig {
            name: name.into(),
            kind,
            agents: Vec::new(),
            methods: Vec::new(),
            env: Vec::new(),
            unavailable: None,
        }
    }

    fn command() -> AgentCommand {
        AgentCommand {
            program: "the-agent".into(),
            args: Vec::new(),
            env: Default::default(),
        }
    }

    #[test]
    fn each_kind_becomes_the_provider_that_handles_it() {
        let registry = registry(&[
            config("keys", AuthProviderKind::Env),
            config("interactive", AuthProviderKind::Terminal),
        ]);
        let command = command();

        let terminal = acp::AuthMethod::Terminal(acp::AuthMethodTerminal::new("login", "Log in"));
        let resolved = registry.resolve(&AuthContext {
            agent_id: "test",
            agent_command: &command,
            method: &terminal,
        });
        // The env provider is asked first and passes, which is what makes the
        // order in the file meaningful — and its reason is kept.
        assert_eq!(resolved.provider.as_deref(), Some("interactive"));
        assert!(matches!(resolved.outcome, Outcome::RunLogin(_)));
        assert_eq!(resolved.declines[0].provider, "keys");
    }

    #[test]
    fn an_unusable_entry_stays_registered_and_reports_why() {
        // Dropping it would leave the panel with a shorter list and no
        // explanation, which is the support call this avoids.
        let mut broken = config("keys", AuthProviderKind::Env);
        broken.unavailable = Some("`OPENAI_API_KEY` is not set".into());
        let registry = registry(&[broken]);
        let command = command();

        let method = acp::AuthMethod::EnvVar(acp::AuthMethodEnvVar::new(
            "api-key",
            "API key",
            vec![acp::AuthEnvVar::new("OPENAI_API_KEY")],
        ));
        let resolved = registry.resolve(&AuthContext {
            agent_id: "test",
            agent_command: &command,
            method: &method,
        });
        assert_eq!(resolved.declines.len(), 1);
        assert!(resolved.declines[0].reason.contains("OPENAI_API_KEY"));
    }

    #[test]
    fn nothing_configured_is_a_registry_that_still_answers() {
        let registry = registry(&[]);
        let command = command();
        let method = acp::AuthMethod::Agent(acp::AuthMethodAgent::new("own", "Sign in"));
        assert!(matches!(
            registry
                .resolve(&AuthContext {
                    agent_id: "test",
                    agent_command: &command,
                    method: &method,
                })
                .outcome,
            Outcome::NeedsUser(_)
        ));
    }
}
