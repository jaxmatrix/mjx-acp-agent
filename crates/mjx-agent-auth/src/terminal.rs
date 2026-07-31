//! The provider behind `kind = "terminal"`: an interactive login the operator
//! types into.
//!
//! `AuthMethodTerminal` is deliberately narrow, and that narrowness is what
//! makes running one safe. It carries *arguments to the agent binary*, not a
//! command line — the program comes from the catalog, on this side. So the worst
//! an agent can ask for is to be run with different flags, and neither the agent
//! nor the browser can name a program.
//!
//! This provider only describes the login. Starting it, streaming it and sending
//! `authenticate` when it exits cleanly are the server's, so the policy about
//! what may be spawned lives in one place rather than in every provider.

use mjx_acp_core::acp;

use crate::{AuthContext, AuthProvider, LoginCommand, Outcome};

/// Turns a `terminal` auth method into the login to run for it.
pub struct TerminalLoginProvider {
    name: String,
    /// Agent ids this applies to. Empty means all of them.
    agents: Vec<String>,
    /// Method ids this applies to. Empty means any `terminal` method.
    methods: Vec<String>,
    unavailable: Option<String>,
}

impl TerminalLoginProvider {
    /// A provider named `name`.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            agents: Vec::new(),
            methods: Vec::new(),
            unavailable: None,
        }
    }

    /// Narrows this provider to `agents`. Empty leaves it applying to all.
    pub fn for_agents(mut self, agents: Vec<String>) -> Self {
        self.agents = agents;
        self
    }

    /// Narrows this provider to `methods`. Empty leaves it applying to any.
    pub fn for_methods(mut self, methods: Vec<String>) -> Self {
        self.methods = methods;
        self
    }

    /// Marks it unusable, with the reason the panel should show.
    pub fn when_unavailable(mut self, reason: Option<String>) -> Self {
        self.unavailable = reason;
        self
    }
}

impl AuthProvider for TerminalLoginProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn satisfy(&self, ctx: &AuthContext<'_>) -> Outcome {
        let acp::AuthMethod::Terminal(method) = ctx.method else {
            return Outcome::declined("not a terminal login method");
        };
        let method_id = method.id.0.to_string();

        let covered = (self.agents.is_empty() || self.agents.iter().any(|id| id == ctx.agent_id))
            && (self.methods.is_empty() || self.methods.contains(&method_id));
        if !covered {
            return Outcome::declined(format!(
                "configured for other agents or methods, not `{}`/`{method_id}`",
                ctx.agent_id
            ));
        }

        // The agent's own arguments first, then the method's. A login is a
        // different *mode* of the same binary — `the-agent acp` becomes
        // `the-agent acp --login` — and putting ours first would mean an agent
        // whose subcommand comes first never saw it.
        let mut args = ctx.agent_command.args.clone();
        args.extend(method.args.iter().cloned());

        // The agent's configured environment, then the method's. This is the
        // login of the agent we are already running, so it should see the same
        // environment that agent does.
        let mut env: Vec<(String, String)> = ctx
            .agent_command
            .env
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        for (name, value) in &method.env {
            match env.iter_mut().find(|(existing, _)| existing == name) {
                Some(slot) => slot.1 = value.clone(),
                None => env.push((name.clone(), value.clone())),
            }
        }
        // `HashMap` iteration order is not stable, and a command line that
        // differs run to run is one nobody can compare against a log.
        env.sort_by(|(a, _), (b, _)| a.cmp(b));

        Outcome::RunLogin(LoginCommand {
            method_id,
            program: ctx.agent_command.program.clone(),
            args,
            env,
        })
    }

    fn unavailable(&self) -> Option<String> {
        self.unavailable.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mjx_agent_catalog::AgentCommand;
    use std::collections::BTreeMap;

    fn method() -> acp::AuthMethod {
        acp::AuthMethod::Terminal(
            acp::AuthMethodTerminal::new("login", "Log in")
                .args(vec!["--login".to_owned()])
                .env(
                    [("AGENT_LOGIN".to_owned(), "1".to_owned())]
                        .into_iter()
                        .collect(),
                ),
        )
    }

    fn command() -> AgentCommand {
        AgentCommand {
            program: "the-agent".into(),
            args: vec!["acp".into()],
            env: BTreeMap::from([("AGENT_HOME".to_string(), "/opt/agent".to_string())]),
        }
    }

    fn context<'a>(m: &'a acp::AuthMethod, c: &'a AgentCommand) -> AuthContext<'a> {
        AuthContext {
            agent_id: "test-agent",
            agent_command: c,
            method: m,
        }
    }

    #[test]
    fn the_login_runs_the_agents_own_binary_with_the_arguments_it_asked_for() {
        // The security property, stated as a test. The program comes from the
        // catalog and nowhere else; all the agent contributes is flags.
        let m = method();
        let c = command();
        let Outcome::RunLogin(login) =
            TerminalLoginProvider::new("interactive").satisfy(&context(&m, &c))
        else {
            panic!("a terminal method must produce a login to run");
        };

        assert_eq!(login.program, "the-agent");
        // The agent's own arguments come first: a login is a different mode of
        // the same binary, and an agent whose subcommand leads would otherwise
        // never see it.
        assert_eq!(login.args, ["acp", "--login"]);
        assert_eq!(login.method_id, "login");
        assert_eq!(
            login.env,
            [
                ("AGENT_HOME".to_string(), "/opt/agent".to_string()),
                ("AGENT_LOGIN".to_string(), "1".to_string()),
            ],
            "the agent's environment, then the method's, in a stable order"
        );
    }

    #[test]
    fn the_method_wins_a_collision_with_the_agents_own_environment() {
        let m = acp::AuthMethod::Terminal(
            acp::AuthMethodTerminal::new("login", "Log in").env(
                [("AGENT_HOME".to_owned(), "/tmp/login".to_owned())]
                    .into_iter()
                    .collect(),
            ),
        );
        let c = command();
        let Outcome::RunLogin(login) =
            TerminalLoginProvider::new("interactive").satisfy(&context(&m, &c))
        else {
            panic!("expected a login");
        };
        assert_eq!(
            login.env,
            [("AGENT_HOME".to_string(), "/tmp/login".to_string())]
        );
    }

    #[test]
    fn a_method_of_another_shape_is_declined_rather_than_guessed_at() {
        let m = acp::AuthMethod::EnvVar(acp::AuthMethodEnvVar::new("k", "Key", Vec::new()));
        let c = command();
        assert!(matches!(
            TerminalLoginProvider::new("interactive").satisfy(&context(&m, &c)),
            Outcome::Declined { .. }
        ));
    }

    #[test]
    fn a_provider_narrowed_to_another_agent_declines_and_says_which() {
        let m = method();
        let c = command();
        let Outcome::Declined { reason } = TerminalLoginProvider::new("interactive")
            .for_agents(vec!["claude-acp".into()])
            .satisfy(&context(&m, &c))
        else {
            panic!("expected a decline");
        };
        assert!(reason.contains("test-agent"), "{reason}");
    }
}
