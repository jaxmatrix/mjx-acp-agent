//! The provider behind `kind = "env"`: credentials the agent reads from its
//! environment.
//!
//! This is how most registry agents authenticate. The agent advertises an
//! `env_var` method naming the variables it wants, and the client's job is to
//! have set them before the agent started.
//!
//! Which is the awkward part, and the reason this provider has two halves. The
//! agent inherits its environment at spawn, long before `initialize` says what
//! it wants; by the time a `-32000` arrives the process is running with whatever
//! it was given. So [`EnvVarProvider::environment`] contributes at spawn from
//! configuration alone, and `satisfy` — which runs after, and does know what the
//! agent asked for — can only ever report.

use mjx_acp_core::acp;

use crate::{AuthContext, AuthProvider, Instructions, Outcome, secret};

/// Looks a variable up in the environment the server was started in.
///
/// Injected rather than calling [`std::env::var`] directly, so tests do not have
/// to mutate a process-wide environment they share with every other test in the
/// binary.
pub type Lookup = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Supplies environment variables to an agent, and explains the ones it cannot.
pub struct EnvVarProvider {
    name: String,
    /// Resolved values, secrets included. Never send these to the browser.
    env: Vec<(String, String)>,
    /// Agent ids this applies to. Empty means all of them.
    agents: Vec<String>,
    /// Method ids this applies to. Empty means any `env_var` method.
    methods: Vec<String>,
    unavailable: Option<String>,
    lookup: Lookup,
}

impl EnvVarProvider {
    /// A provider named `name` contributing `env`.
    pub fn new(name: impl Into<String>, env: Vec<(String, String)>) -> Self {
        Self {
            name: name.into(),
            env,
            agents: Vec::new(),
            methods: Vec::new(),
            unavailable: None,
            lookup: Box::new(|name| std::env::var(name).ok()),
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
    ///
    /// Named apart from the trait's `unavailable`, which reads it back: a
    /// builder that shadowed the accessor would be one call away from silently
    /// setting nothing.
    pub fn when_unavailable(mut self, reason: Option<String>) -> Self {
        self.unavailable = reason;
        self
    }

    /// Replaces the environment lookup. For tests.
    pub fn looking_up(mut self, lookup: Lookup) -> Self {
        self.lookup = lookup;
        self
    }

    fn covers(&self, ctx: &AuthContext<'_>, method_id: &str) -> bool {
        (self.agents.is_empty() || self.agents.iter().any(|id| id == ctx.agent_id))
            && (self.methods.is_empty() || self.methods.iter().any(|id| id == method_id))
    }

    /// Whether `name` will reach the agent: either this provider supplies it, or
    /// the server's own environment has it and the agent inherited it.
    fn have(&self, name: &str) -> bool {
        self.env.iter().any(|(set, _)| set == name) || (self.lookup)(name).is_some()
    }
}

impl AuthProvider for EnvVarProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn environment(&self, agent_id: &str) -> Vec<(String, String)> {
        if !self.agents.is_empty() && !self.agents.iter().any(|id| id == agent_id) {
            return Vec::new();
        }
        self.env.clone()
    }

    fn satisfy(&self, ctx: &AuthContext<'_>) -> Outcome {
        let acp::AuthMethod::EnvVar(method) = ctx.method else {
            return Outcome::declined("not an environment-variable method");
        };
        let method_id = method.id.0.to_string();
        if !self.covers(ctx, &method_id) {
            return Outcome::declined(format!(
                "configured for other agents or methods, not `{}`/`{method_id}`",
                ctx.agent_id
            ));
        }

        let secrets: Vec<_> = method
            .vars
            .iter()
            .map(|var| secret(var, self.have(&var.name)))
            .collect();
        let missing: Vec<&str> = secrets
            .iter()
            .filter(|s| !s.present && !s.optional)
            .map(|s| s.name.as_str())
            .collect();

        let summary = if missing.is_empty() {
            // Every variable is set and the agent still refused. Reporting
            // success here would be the worst thing this provider could do: the
            // agent would go on answering -32000 while the panel claimed it was
            // authenticated, and nothing on screen would explain it. What is
            // actually known is that the value is wrong, and that is what to say.
            format!(
                "{} is set, but the agent refused it. The value may be wrong, expired, or for a \
                 different account. Correct it and reconnect.",
                names(&secrets)
            )
        } else {
            format!(
                "Set {} in the environment the server runs in, or add it to this provider's \
                 `env`/`env_from`, then reconnect. An agent reads its credentials when it starts, \
                 so a variable set now reaches it only on the next connection.",
                missing.join(" and "),
            )
        };

        Outcome::NeedsUser(Instructions {
            summary,
            secrets,
            link: method.link.clone(),
        })
    }

    fn unavailable(&self) -> Option<String> {
        self.unavailable.clone()
    }
}

/// The required variables of a method, for a sentence.
fn names(secrets: &[mjx_acp_core::ext::AuthSecret]) -> String {
    let names: Vec<&str> = secrets
        .iter()
        .filter(|s| !s.optional)
        .map(|s| s.name.as_str())
        .collect();
    match names.split_last() {
        None => "Everything it asked for".to_owned(),
        Some((last, [])) => (*last).to_owned(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mjx_agent_catalog::AgentCommand;

    fn method(vars: Vec<acp::AuthEnvVar>) -> acp::AuthMethod {
        acp::AuthMethod::EnvVar(
            acp::AuthMethodEnvVar::new("api-key", "API key", vars)
                .link("https://example.test/keys".to_owned()),
        )
    }

    fn command() -> AgentCommand {
        AgentCommand {
            program: "the-agent".into(),
            args: Vec::new(),
            env: Default::default(),
        }
    }

    fn context<'a>(m: &'a acp::AuthMethod, c: &'a AgentCommand) -> AuthContext<'a> {
        AuthContext {
            agent_id: "test-agent",
            agent_command: c,
            method: m,
        }
    }

    /// An environment holding exactly `set`.
    fn holding(set: &'static [&'static str]) -> Lookup {
        Box::new(move |name| set.contains(&name).then(|| "value".to_owned()))
    }

    fn nothing() -> Lookup {
        Box::new(|_| None)
    }

    #[test]
    fn a_missing_variable_is_named_along_with_where_to_get_it() {
        let provider = EnvVarProvider::new("keys", Vec::new()).looking_up(nothing());
        let m = method(vec![
            acp::AuthEnvVar::new("OPENAI_API_KEY"),
            acp::AuthEnvVar::new("OPENAI_ORG").optional(true),
        ]);
        let c = command();

        let Outcome::NeedsUser(instructions) = provider.satisfy(&context(&m, &c)) else {
            panic!("a missing variable must produce instructions");
        };
        assert!(
            instructions.summary.contains("OPENAI_API_KEY"),
            "{instructions:?}"
        );
        // The optional one is reported but not demanded: insisting on it would
        // send the operator looking for something the agent said it can do
        // without.
        assert!(
            !instructions.summary.contains("OPENAI_ORG"),
            "{instructions:?}"
        );
        assert_eq!(
            instructions.link.as_deref(),
            Some("https://example.test/keys")
        );
        assert_eq!(instructions.secrets.len(), 2);
        assert!(!instructions.secrets[0].present);
    }

    #[test]
    fn a_variable_that_is_set_but_refused_is_reported_not_claimed_as_success() {
        // The fail-loud case. The agent has all of it and said no anyway, which
        // means the value is wrong — and answering `Authenticated` would leave
        // the panel claiming success while the agent went on refusing.
        let provider =
            EnvVarProvider::new("keys", Vec::new()).looking_up(holding(&["OPENAI_API_KEY"]));
        let m = method(vec![acp::AuthEnvVar::new("OPENAI_API_KEY")]);
        let c = command();

        let outcome = provider.satisfy(&context(&m, &c));
        assert!(
            !matches!(outcome, Outcome::Authenticated { .. }),
            "a provider must never claim an authentication it did not perform"
        );
        let Outcome::NeedsUser(instructions) = outcome else {
            panic!("expected instructions, got something else");
        };
        assert!(instructions.summary.contains("refused"), "{instructions:?}");
        assert!(instructions.secrets[0].present);
    }

    #[test]
    fn a_value_this_provider_supplies_counts_as_present() {
        // It will be in the agent's environment on the next connection even
        // though it is not in the server's own.
        let provider = EnvVarProvider::new(
            "keys",
            vec![("OPENAI_API_KEY".into(), "sk-not-real".into())],
        )
        .looking_up(nothing());
        let m = method(vec![acp::AuthEnvVar::new("OPENAI_API_KEY")]);
        let c = command();

        let Outcome::NeedsUser(instructions) = provider.satisfy(&context(&m, &c)) else {
            panic!("expected instructions");
        };
        assert!(instructions.secrets[0].present);
        assert!(instructions.summary.contains("refused"), "{instructions:?}");
    }

    #[test]
    fn a_method_of_another_shape_is_declined_rather_than_guessed_at() {
        let provider = EnvVarProvider::new("keys", Vec::new()).looking_up(nothing());
        let m = acp::AuthMethod::Terminal(acp::AuthMethodTerminal::new("login", "Log in"));
        let c = command();
        assert!(matches!(
            provider.satisfy(&context(&m, &c)),
            Outcome::Declined { .. }
        ));
    }

    #[test]
    fn the_filters_narrow_both_what_is_answered_and_what_is_contributed() {
        let provider = EnvVarProvider::new("keys", vec![("KEY".into(), "v".into())])
            .for_agents(vec!["claude-acp".into()])
            .looking_up(nothing());

        // The environment hook honours the same filter as `satisfy`. Otherwise
        // an operator narrowing a provider to one agent would still be handing
        // its key to every other one.
        assert!(provider.environment("claude-acp").len() == 1);
        assert!(provider.environment("gemini").is_empty());

        let m = method(vec![acp::AuthEnvVar::new("KEY")]);
        let c = command();
        let Outcome::Declined { reason } = provider.satisfy(&context(&m, &c)) else {
            panic!("a provider narrowed to another agent must decline");
        };
        assert!(reason.contains("test-agent"), "{reason}");
    }

    #[test]
    fn a_provider_that_could_not_configure_itself_says_so() {
        let provider = EnvVarProvider::new("keys", Vec::new())
            .when_unavailable(Some("`OPENAI_API_KEY` is not set".into()));
        assert!(AuthProvider::unavailable(&provider).is_some_and(|r| r.contains("OPENAI_API_KEY")));
    }
}
