//! How an agent gets its own credentials.
//!
//! Distinct from anything about who may use this viewer. An ACP agent that needs
//! authenticating says so in `initialize`, by advertising `authMethods`, and
//! then answers `session/new` with `-32000` until one of them has been used.
//! Most agents in the registry work this way; `claude-acp` is the exception, and
//! only because it reuses a CLI that is already signed in.
//!
//! Rather than special-case each agent's login, this is a seam. An
//! [`AuthProvider`] is asked what it can do about one advertised method, and the
//! first one with an answer is the one that acts. Supporting a new credential
//! source is a new impl and a `[[auth_providers]]` block, not a change to the
//! relay.
//!
//! # Providers compute; the caller applies
//!
//! Nothing here runs a process, reads a file or touches the network. A provider
//! that wants a login run says so, in [`Outcome::RunLogin`], and the server
//! starts it. That keeps the decisions — which provider, which method, what the
//! operator must do — testable without any I/O at all, and keeps the *policy*
//! about what may be spawned in one place rather than in every provider.
//!
//! # What may travel to the browser
//!
//! Values never do. Providers hold resolved credentials; what comes out of this
//! module for the UI is [`mjx_acp_core::ext::AuthMethodInfo`], which has nowhere
//! to put one. The names, whether each is set, and why each provider passed are
//! the whole of it.

use mjx_acp_core::{acp, ext};
use mjx_agent_catalog::AgentCommand;

pub mod env_var;

pub use env_var::EnvVarProvider;

/// One thing that knows how to satisfy some auth methods.
///
/// Every hook but [`AuthProvider::name`] has a default, and each default
/// *refuses*. A provider that forgets to implement `satisfy` declines every
/// method rather than appearing to have authenticated something, because a
/// silent yes is the failure that cannot be detected from the outside.
pub trait AuthProvider: Send + Sync + 'static {
    /// How this provider is named in logs and in the auth panel.
    fn name(&self) -> &str;

    /// Environment to give the agent process, resolved before it is spawned.
    ///
    /// Separate from [`AuthProvider::satisfy`] because of *when* it runs. The
    /// agent inherits its environment at spawn, which is before `initialize` and
    /// therefore before anything knows what the agent's auth methods are; this
    /// hook can only key on the agent id. A `-32000` arriving later cannot be
    /// answered by setting a variable on a process that is already running.
    fn environment(&self, agent_id: &str) -> Vec<(String, String)> {
        let _ = agent_id;
        Vec::new()
    }

    /// What this provider can do about one method the agent advertised.
    ///
    /// The default declines, and says so in a way an operator can read. See
    /// [`Outcome`] for what each answer means and when to give it.
    fn satisfy(&self, ctx: &AuthContext<'_>) -> Outcome {
        let _ = ctx;
        Outcome::declined("this provider does not handle any auth method")
    }

    /// Why this provider could not configure itself, if it could not.
    ///
    /// Load-time and static, unlike the per-attempt reason in
    /// [`Outcome::Declined`]. A provider whose `env_from` variable is unset stays
    /// registered and reports it here, so the panel can name the variable rather
    /// than the list quietly being shorter.
    fn unavailable(&self) -> Option<String> {
        None
    }
}

/// One method the agent advertised, and what is known about the agent.
pub struct AuthContext<'a> {
    /// Catalog id, e.g. `claude-acp`.
    pub agent_id: &'a str,
    /// What this server ran to start the agent. A terminal login runs this same
    /// program with extra arguments — the caller never chooses a program.
    pub agent_command: &'a AgentCommand,
    /// The method being considered.
    pub method: &'a acp::AuthMethod,
}

/// What a provider can say about a method.
///
/// The distinctions are load-bearing, so they are spelled out rather than
/// collapsed into a `Result`:
///
/// * [`Outcome::Declined`] means *not mine, or mine but unconfigured*. The
///   registry tries the next provider and keeps the reason.
/// * [`Outcome::NeedsUser`] means *mine, and I can go no further alone*. The
///   registry stops; the panel shows the instructions.
/// * [`Outcome::RunLogin`] means *mine, and here is what to run*.
/// * [`Outcome::Authenticated`] means the agent really is authenticated now.
///   Never say this on a guess: an agent that answers `-32000` while a provider
///   claims success leaves the user with no explanation at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The agent has been authenticated by this method.
    Authenticated {
        /// The method that did it.
        method_id: String,
    },
    /// The operator has to do something. This is what the panel renders.
    NeedsUser(Instructions),
    /// Start this login, then send `authenticate` if it exits cleanly.
    RunLogin(LoginCommand),
    /// Not this provider's problem, for a reason worth reading.
    Declined {
        /// Written for an operator, not for a log.
        reason: String,
    },
}

impl Outcome {
    /// A decline carrying `reason`.
    pub fn declined(reason: impl Into<String>) -> Self {
        Self::Declined {
            reason: reason.into(),
        }
    }

    /// Whether the registry should stop here.
    fn is_answer(&self) -> bool {
        !matches!(self, Self::Declined { .. })
    }
}

/// What the operator must do, and what is missing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Instructions {
    /// One or two sentences, in the imperative.
    pub summary: String,
    /// The variables this method needs, and whether each is set. Never a value.
    pub secrets: Vec<ext::AuthSecret>,
    /// Documentation the agent pointed at.
    pub link: Option<String>,
}

impl Instructions {
    /// Instructions that say only `summary`.
    pub fn say(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            ..Self::default()
        }
    }
}

/// A login to run in an interactive terminal.
///
/// The program is always the agent's own, from the catalog. `AuthMethodTerminal`
/// carries *arguments to the agent binary*, not a command line, which is what
/// makes this safe to start on an operator's behalf: nothing outside this server
/// chooses what runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginCommand {
    /// The method this login is for, so the caller knows what to `authenticate`
    /// with when it succeeds.
    pub method_id: String,
    /// The agent's own program.
    pub program: String,
    /// Its own arguments, plus the ones the auth method asked for.
    pub args: Vec<String>,
    /// The environment the auth method asked for.
    pub env: Vec<(String, String)>,
}

/// Every provider, in the order they are tried.
#[derive(Default)]
pub struct AuthRegistry {
    providers: Vec<Box<dyn AuthProvider>>,
}

/// What the registry made of one method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// Which provider answered, if a configured one did.
    pub provider: Option<String>,
    /// Its answer.
    pub outcome: Outcome,
    /// Every provider that passed, and why, in the order they were asked.
    pub declines: Vec<ext::AuthDecline>,
}

impl AuthRegistry {
    /// A registry that asks `providers` in order.
    pub fn new(providers: Vec<Box<dyn AuthProvider>>) -> Self {
        Self { providers }
    }

    /// The environment every provider wants an agent started with.
    ///
    /// Later providers win a collision, matching the "order is dispatch order"
    /// rule everywhere else: a more specific entry written after a general one
    /// is how an operator overrides it.
    pub fn environment(&self, agent_id: &str) -> Vec<(String, String)> {
        let mut merged: Vec<(String, String)> = Vec::new();
        for provider in &self.providers {
            for (name, value) in provider.environment(agent_id) {
                match merged.iter_mut().find(|(existing, _)| *existing == name) {
                    Some(slot) => slot.1 = value,
                    None => merged.push((name, value)),
                }
            }
        }
        merged
    }

    /// Asks each provider in turn what it can do about `ctx.method`.
    ///
    /// The fold ends in an answer built from the method itself, so a viewer with
    /// nothing configured still tells the user what the agent asked for instead
    /// of showing them a bare protocol error. That is deliberately *not* a
    /// catch-all provider: one that claimed every method would make "the first
    /// provider that can handle it wins" stop being a rule.
    pub fn resolve(&self, ctx: &AuthContext<'_>) -> Resolution {
        let mut declines = Vec::new();
        for provider in &self.providers {
            // A provider that could not configure itself is asked nothing. Its
            // reason is more useful than whatever it would say without its
            // configuration.
            if let Some(reason) = provider.unavailable() {
                declines.push(ext::AuthDecline {
                    provider: provider.name().to_owned(),
                    reason,
                });
                continue;
            }
            let outcome = provider.satisfy(ctx);
            if outcome.is_answer() {
                return Resolution {
                    provider: Some(provider.name().to_owned()),
                    outcome,
                    declines,
                };
            }
            if let Outcome::Declined { reason } = outcome {
                declines.push(ext::AuthDecline {
                    provider: provider.name().to_owned(),
                    reason,
                });
            }
        }

        Resolution {
            provider: None,
            outcome: Outcome::NeedsUser(unconfigured(ctx.method)),
            declines,
        }
    }

    /// How the browser should show one method.
    pub fn describe(&self, ctx: &AuthContext<'_>, satisfied: bool) -> ext::AuthMethodInfo {
        let resolution = self.resolve(ctx);
        let instructions = match &resolution.outcome {
            Outcome::NeedsUser(instructions) => Some(instructions.clone()),
            _ => None,
        };
        ext::AuthMethodInfo {
            id: method_id(ctx.method),
            name: ctx.method.name().to_owned(),
            description: ctx.method.description().map(str::to_owned),
            kind: kind_of(ctx.method).to_owned(),
            provider: resolution.provider,
            instructions: instructions.as_ref().map(|i| i.summary.clone()),
            link: instructions
                .as_ref()
                .and_then(|i| i.link.clone())
                .or_else(|| link_of(ctx.method)),
            secrets: instructions.map(|i| i.secrets).unwrap_or_default(),
            declines: resolution.declines,
            satisfied,
        }
    }
}

/// What to tell the operator when no provider claimed a method.
///
/// Built from what the agent itself said, which is the whole point: even with
/// nothing configured, the panel can name the variables an `env_var` method
/// wants and link the docs it pointed at.
fn unconfigured(method: &acp::AuthMethod) -> Instructions {
    match method {
        acp::AuthMethod::EnvVar(env) => Instructions {
            summary: format!(
                "This agent wants {}. Set {} in the environment the server runs in, or add an \
                 `[[auth_providers]]` entry, then reconnect.",
                method.name(),
                names_of(&env.vars),
            ),
            secrets: env.vars.iter().map(|var| secret(var, false)).collect(),
            link: env.link.clone(),
        },
        acp::AuthMethod::Terminal(terminal) => Instructions::say(format!(
            "This agent offers an interactive login ({}). Add an `[[auth_providers]]` entry with \
             `kind = \"terminal\"` to run it from here, or run the agent with {} on the server \
             host yourself and then reconnect.",
            method.name(),
            if terminal.args.is_empty() {
                "its login arguments".to_owned()
            } else {
                format!("`{}`", terminal.args.join(" "))
            },
        )),
        // An `agent` method is the agent's own business, and the protocol gives
        // us nothing to do but ask it. Saying so is still better than a bare
        // JSON-RPC error, which is what this replaces.
        _ => Instructions::say(format!(
            "This agent handles \"{}\" itself. Try it, and follow whatever it asks for.",
            method.name()
        )),
    }
}

/// `NAME`, `NAME and OTHER`, `NAME, OTHER and THIRD`.
fn names_of(vars: &[acp::AuthEnvVar]) -> String {
    let names: Vec<&str> = vars
        .iter()
        .filter(|var| !var.optional)
        .map(|var| var.name.as_str())
        .collect();
    match names.split_last() {
        None => "nothing in particular".to_owned(),
        Some((last, [])) => (*last).to_owned(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

/// One variable, as the browser is told about it. Never carries a value.
pub fn secret(var: &acp::AuthEnvVar, present: bool) -> ext::AuthSecret {
    ext::AuthSecret {
        name: var.name.clone(),
        label: var.label.clone(),
        present,
        optional: var.optional,
    }
}

/// The `methodId` to pass to `authenticate`.
pub fn method_id(method: &acp::AuthMethod) -> String {
    method.id().0.to_string()
}

/// Which shape a method is, spelled the way the browser expects it.
///
/// `_` rather than an exhaustive match: `acp::AuthMethod` is `#[non_exhaustive]`,
/// and a shape added upstream should read as the agent's own business rather
/// than fail to compile.
pub fn kind_of(method: &acp::AuthMethod) -> &'static str {
    match method {
        acp::AuthMethod::EnvVar(_) => "envVar",
        acp::AuthMethod::Terminal(_) => "terminal",
        _ => "agent",
    }
}

/// The docs link a method carries, if it carries one.
fn link_of(method: &acp::AuthMethod) -> Option<String> {
    match method {
        acp::AuthMethod::EnvVar(env) => env.link.clone(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_method() -> acp::AuthMethod {
        acp::AuthMethod::EnvVar(
            acp::AuthMethodEnvVar::new(
                "api-key",
                "API key",
                vec![
                    acp::AuthEnvVar::new("OPENAI_API_KEY"),
                    acp::AuthEnvVar::new("OPENAI_ORG").optional(true),
                ],
            )
            .link("https://example.test/keys".to_owned()),
        )
    }

    fn terminal_method() -> acp::AuthMethod {
        acp::AuthMethod::Terminal(
            acp::AuthMethodTerminal::new("login", "Log in").args(vec!["--login".to_owned()]),
        )
    }

    fn agent_method() -> acp::AuthMethod {
        acp::AuthMethod::Agent(acp::AuthMethodAgent::new("own", "Sign in"))
    }

    fn command() -> AgentCommand {
        AgentCommand {
            program: "the-agent".into(),
            args: vec!["acp".into()],
            env: Default::default(),
        }
    }

    fn context<'a>(method: &'a acp::AuthMethod, command: &'a AgentCommand) -> AuthContext<'a> {
        AuthContext {
            agent_id: "test-agent",
            agent_command: command,
            method,
        }
    }

    /// A provider that always gives the same answer.
    struct Fixed {
        name: &'static str,
        outcome: Outcome,
        unavailable: Option<String>,
        env: Vec<(String, String)>,
    }

    impl Fixed {
        fn saying(name: &'static str, outcome: Outcome) -> Self {
            Self {
                name,
                outcome,
                unavailable: None,
                env: Vec::new(),
            }
        }

        fn declining(name: &'static str) -> Self {
            Self::saying(name, Outcome::declined("not mine"))
        }

        fn broken(name: &'static str, reason: &str) -> Self {
            let mut provider = Self::declining(name);
            provider.unavailable = Some(reason.to_owned());
            provider
        }

        fn with_env(mut self, name: &str, value: &str) -> Self {
            self.env.push((name.to_owned(), value.to_owned()));
            self
        }

        fn boxed(self) -> Box<dyn AuthProvider> {
            Box::new(self)
        }
    }

    impl AuthProvider for Fixed {
        fn name(&self) -> &str {
            self.name
        }

        fn environment(&self, _agent_id: &str) -> Vec<(String, String)> {
            self.env.clone()
        }

        fn satisfy(&self, _ctx: &AuthContext<'_>) -> Outcome {
            self.outcome.clone()
        }

        fn unavailable(&self) -> Option<String> {
            self.unavailable.clone()
        }
    }

    /// A provider that implements nothing beyond its name.
    struct Forgetful;

    impl AuthProvider for Forgetful {
        fn name(&self) -> &str {
            "forgetful"
        }
    }

    #[test]
    fn a_provider_that_implements_nothing_declines() {
        // The default must refuse. A provider that appeared to have
        // authenticated something would leave the agent answering -32000 with
        // nothing on screen to explain it, which is the failure this whole
        // module exists to remove.
        let method = env_method();
        let command = command();
        let outcome = Forgetful.satisfy(&context(&method, &command));
        assert!(matches!(outcome, Outcome::Declined { .. }));
        assert!(!outcome.is_answer());
    }

    #[test]
    fn the_first_provider_with_an_answer_wins_and_the_rest_are_not_asked() {
        let registry = AuthRegistry::new(vec![
            Fixed::declining("first").boxed(),
            Fixed::saying(
                "second",
                Outcome::Authenticated {
                    method_id: "api-key".into(),
                },
            )
            .boxed(),
            Fixed::saying(
                "third",
                Outcome::Authenticated {
                    method_id: "never".into(),
                },
            )
            .boxed(),
        ]);
        let method = env_method();
        let command = command();

        let resolution = registry.resolve(&context(&method, &command));
        assert_eq!(resolution.provider.as_deref(), Some("second"));
        assert_eq!(
            resolution.outcome,
            Outcome::Authenticated {
                method_id: "api-key".into()
            }
        );
        // The one that passed is kept. Reducing the chain to "unsupported"
        // throws away the only thing that says what to fix.
        assert_eq!(resolution.declines.len(), 1);
        assert_eq!(resolution.declines[0].provider, "first");
    }

    #[test]
    fn a_provider_that_could_not_configure_itself_is_not_asked_but_is_reported() {
        let registry = AuthRegistry::new(vec![
            Fixed::broken("anthropic", "`ANTHROPIC_API_KEY` is not set").boxed(),
        ]);
        let method = env_method();
        let command = command();

        let resolution = registry.resolve(&context(&method, &command));
        assert_eq!(resolution.declines.len(), 1);
        assert!(resolution.declines[0].reason.contains("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn with_nothing_configured_the_method_still_explains_itself() {
        // The case that matters for the ~30 agents in the registry: no
        // providers at all, and the panel must still be better than a raw
        // JSON-RPC error.
        let registry = AuthRegistry::default();
        let command = command();

        let method = env_method();
        let Outcome::NeedsUser(instructions) =
            registry.resolve(&context(&method, &command)).outcome
        else {
            panic!("an unclaimed method must still say what to do");
        };
        assert!(
            instructions.summary.contains("OPENAI_API_KEY"),
            "{instructions:?}"
        );
        // The optional one is listed but not demanded.
        assert!(
            !instructions.summary.contains("OPENAI_ORG"),
            "{instructions:?}"
        );
        assert_eq!(instructions.secrets.len(), 2);
        assert!(instructions.secrets[1].optional);
        assert_eq!(
            instructions.link.as_deref(),
            Some("https://example.test/keys")
        );

        let method = terminal_method();
        let Outcome::NeedsUser(instructions) =
            registry.resolve(&context(&method, &command)).outcome
        else {
            panic!("an unclaimed terminal method must still say what to do");
        };
        // It names the arguments, so an operator can run the login by hand even
        // with no provider configured to run it from here.
        assert!(instructions.summary.contains("--login"), "{instructions:?}");

        let method = agent_method();
        assert!(matches!(
            registry.resolve(&context(&method, &command)).outcome,
            Outcome::NeedsUser(_)
        ));
    }

    #[test]
    fn a_described_method_carries_names_and_reasons_but_never_a_value() {
        let registry = AuthRegistry::new(vec![
            Fixed::broken("anthropic", "`ANTHROPIC_API_KEY` is not set").boxed(),
        ]);
        let method = env_method();
        let command = command();

        let info = registry.describe(&context(&method, &command), false);
        assert_eq!(info.id, "api-key");
        assert_eq!(info.kind, "envVar");
        assert!(!info.satisfied);
        assert_eq!(info.secrets[0].name, "OPENAI_API_KEY");
        assert!(!info.secrets[0].present);
        assert_eq!(info.declines[0].provider, "anthropic");
        assert_eq!(info.link.as_deref(), Some("https://example.test/keys"));
    }

    #[test]
    fn every_method_shape_has_a_kind_the_browser_understands() {
        assert_eq!(kind_of(&env_method()), "envVar");
        assert_eq!(kind_of(&terminal_method()), "terminal");
        assert_eq!(kind_of(&agent_method()), "agent");
    }

    #[test]
    fn the_environment_of_every_provider_is_merged_and_the_last_wins() {
        // Order is dispatch order throughout, so a specific entry written after
        // a general one is how an operator overrides it.
        let registry = AuthRegistry::new(vec![
            Fixed::declining("general")
                .with_env("OPENAI_API_KEY", "general")
                .with_env("OPENAI_ORG", "acme")
                .boxed(),
            Fixed::declining("specific")
                .with_env("OPENAI_API_KEY", "specific")
                .boxed(),
        ]);

        let env = registry.environment("test-agent");
        assert_eq!(
            env,
            [
                ("OPENAI_API_KEY".to_string(), "specific".to_string()),
                ("OPENAI_ORG".to_string(), "acme".to_string()),
            ]
        );
    }

    #[test]
    fn a_method_id_is_the_one_authenticate_takes() {
        assert_eq!(method_id(&env_method()), "api-key");
        // Round-trips through the request the agent will receive.
        let request = acp::AuthenticateRequest::new(method_id(&env_method()));
        assert_eq!(request.method_id.0.as_ref(), "api-key");
    }
}
