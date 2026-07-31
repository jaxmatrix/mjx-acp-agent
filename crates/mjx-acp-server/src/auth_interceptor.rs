//! Turns an agent's refusal into something the user can act on.
//!
//! An agent that wants authenticating advertises `authMethods` in `initialize`
//! and then answers `session/new` with `-32000` until one has been used. Both
//! halves used to be thrown away: the methods were never read, and the error
//! reached the browser as `Connection failed: {"code":-32000,...}`. Thirty-odd
//! agents in the picker looked broken for no stated reason.
//!
//! This is the connection's memory of that exchange, and the thing that turns it
//! into an [`ext::AuthState`] the panel can render.
//!
//! # Why the refusal is rewritten rather than retried
//!
//! It would be tidier to authenticate and re-send `session/new` transparently,
//! and it cannot be done from here. An interceptor only ever sees *agent-space*
//! request ids — the relay rebinds before anything observes a frame — so it
//! could not answer the browser's original request afterwards, and the id map it
//! would need is private to the relay for good reasons. Rewriting the error
//! keeps id correlation entirely inside the relay, and the browser re-issues
//! `session/new` itself once it has authenticated. It is an ordinary ACP client;
//! letting it act like one is simpler than teaching the server to impersonate it.
//!
//! A transparent retry would also park the browser's `session/new` for the
//! length of a device-code login, with no progress and no way to cancel, because
//! ACP has no "still working" for a request in flight.

use std::sync::Arc;
use std::sync::Mutex;

use mjx_acp_core::{
    Direction, Frame, JsonRpcError, MethodCorrelator, ResponsePayload, acp, ext, frame, method,
};
use mjx_agent_auth::{AuthContext, AuthRegistry};
use mjx_agent_catalog::AgentCommand;

use crate::relay::{Disposition, Interceptor, Outbox};

/// Reads `authMethods`, and explains `-32000`.
pub struct AuthInterceptor {
    agent_id: String,
    agent_command: AgentCommand,
    registry: Arc<AuthRegistry>,
    state: Mutex<State>,
}

/// What this connection knows about authenticating its agent.
///
/// Per *agent*, not per socket: the interceptor is built once when the agent
/// starts and outlives every browser that attaches to it. That is why
/// `_mjx/auth/state` is a request the browser makes rather than a notification
/// this sends — the refusal may have arrived while a different browser was
/// attached, or none at all, and a notification sent then is simply gone.
#[derive(Default)]
struct State {
    /// What the agent advertised, verbatim, parsed once from `initialize`.
    methods: Vec<acp::AuthMethod>,
    /// Whether the agent has refused something for want of authentication.
    required: bool,
    /// Whether one of the methods has since succeeded.
    authenticated: bool,
    /// The method that authenticated it, so the panel can mark which one.
    satisfied_by: Option<String>,
    /// The ACP method the agent refused, if it refused one.
    refused_method: Option<String>,
    /// Labels responses with the method they answer, since a JSON-RPC response
    /// carries only an id. The relay keeps one of these too; this is a second
    /// because an interceptor is not given the relay's.
    correlator: MethodCorrelator,
}

impl AuthInterceptor {
    /// An interceptor for `agent_id`, started with `agent_command`.
    pub fn new(agent_id: String, agent_command: AgentCommand, registry: Arc<AuthRegistry>) -> Self {
        Self {
            agent_id,
            agent_command,
            registry,
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        // A panic while one connection's auth state is locked must not poison
        // it for the rest of that connection's life.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// What the browser should be shown right now.
    pub fn auth_state(&self) -> ext::AuthState {
        let state = self.state();
        ext::AuthState {
            required: state.required,
            authenticated: state.authenticated,
            methods: state
                .methods
                .iter()
                .map(|method| {
                    let satisfied = state.satisfied_by.as_deref()
                        == Some(mjx_agent_auth::method_id(method).as_str());
                    self.registry.describe(
                        &AuthContext {
                            agent_id: &self.agent_id,
                            agent_command: &self.agent_command,
                            method,
                        },
                        satisfied,
                    )
                })
                .collect(),
            refused_method: state.refused_method.clone(),
        }
    }

    /// Reads `authMethods` out of an `initialize` result.
    ///
    /// Everything off the wire is untrusted, so a malformed entry is skipped
    /// rather than failing the parse: an agent that advertises one method we
    /// cannot read and two we can should still get the two.
    fn record_methods(&self, result: &serde_json::value::RawValue) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(result.get()) else {
            return;
        };
        let Some(advertised) = value.get("authMethods").and_then(|m| m.as_array()) else {
            return;
        };
        let methods: Vec<acp::AuthMethod> = advertised
            .iter()
            .filter_map(|method| match serde_json::from_value(method.clone()) {
                Ok(method) => Some(method),
                Err(err) => {
                    tracing::warn!(%err, "skipping an auth method we could not read");
                    None
                }
            })
            .collect();
        if !methods.is_empty() {
            tracing::info!(
                agent = %self.agent_id,
                count = methods.len(),
                "the agent offers authentication methods"
            );
        }
        self.state().methods = methods;
    }

    /// Answers a `-32000` with something the panel can render.
    ///
    /// Returns the rewritten error, or `None` if this is not an auth refusal.
    fn explain(&self, error: &JsonRpcError, refused: Option<&str>) -> Option<JsonRpcError> {
        if !error.is_auth_required() {
            return None;
        }
        {
            let mut state = self.state();
            state.required = true;
            state.refused_method = refused.map(str::to_owned);
        }

        let auth = self.auth_state();
        let data = match serde_json::value::to_raw_value(&auth) {
            Ok(data) => Some(data),
            Err(err) => {
                // The error is still worth sending without its detail: the
                // browser also hears `_mjx/auth/required`, and a bare -32000 is
                // no worse than what it used to get.
                tracing::error!(%err, "could not attach the auth detail to a -32000");
                None
            }
        };

        Some(JsonRpcError {
            code: frame::AUTH_REQUIRED,
            // The agent's own message is kept. It is the only part written by
            // the thing that actually refused.
            message: error.message.clone(),
            data,
        })
    }
}

impl Interceptor for AuthInterceptor {
    fn on_client_frame(&self, frame: &Frame, _outbox: &Outbox) -> Disposition {
        self.state()
            .correlator
            .observe(Direction::ClientToAgent, frame);

        // Declare that this client can run an interactive login. An agent is
        // entitled to withhold its `terminal` methods otherwise — the schema
        // says it "may include" them when the client declares this — so without
        // it the login the operator could actually complete is never offered.
        match merge_auth_capability(frame) {
            Some(rewritten) => Disposition::Rewrite(rewritten),
            None => Disposition::Forward,
        }
    }

    fn on_agent_frame(&self, frame: &Frame, outbox: &Outbox) -> Disposition {
        let answered = self
            .state()
            .correlator
            .observe(Direction::AgentToClient, frame);

        let Frame::Response { id, payload } = frame else {
            return Disposition::Forward;
        };

        match payload {
            ResponsePayload::Result(result) => {
                if answered.as_deref() == Some(method::agent::INITIALIZE) {
                    self.record_methods(result);
                }
                Disposition::Forward
            }
            // Any method can be refused, not just `session/new`: a token that
            // expires mid-conversation answers a `session/prompt`, and code that
            // only watched session creation would let that through raw.
            ResponsePayload::Error(error) => {
                let Some(explained) = self.explain(error, answered.as_deref()) else {
                    return Disposition::Forward;
                };
                // Beside the error, not instead of it. The error belongs to the
                // one request that provoked it; the refusal belongs to the
                // connection, and the panel that renders it outlives the
                // request.
                outbox.notify_browser(ext::AUTH_REQUIRED, &self.auth_state());
                Disposition::Rewrite(Frame::Response {
                    id: id.clone(),
                    payload: ResponsePayload::Error(explained),
                })
            }
        }
    }

    fn on_extension_request(&self, frame: &Frame, outbox: &Outbox) -> bool {
        let Frame::Request { id, method, .. } = frame else {
            return false;
        };
        if method != ext::AUTH_STATE {
            return false;
        }
        let reply = Frame::result(id.clone(), &self.auth_state())
            .unwrap_or_else(|err| Frame::error(id.clone(), JsonRpcError::internal(err)));
        outbox.to_browser(&reply);
        true
    }
}

/// Adds `clientCapabilities.auth.terminal` to an `initialize` request.
///
/// A sibling of the relay's `merge_client_capabilities` rather than part of it:
/// that one declares what the *workspace* serves, and this declares what the
/// auth path can drive. Returns `None` for anything that is not `initialize`.
fn merge_auth_capability(frame: &Frame) -> Option<Frame> {
    let Frame::Request { id, method, params } = frame else {
        return None;
    };
    if method != method::agent::INITIALIZE {
        return None;
    }

    let mut value: serde_json::Value = match params {
        Some(params) => serde_json::from_str(params.get()).ok()?,
        None => serde_json::json!({}),
    };
    let capabilities = value
        .get_mut("clientCapabilities")
        .filter(|c| c.is_object())
        .map(|c| c.as_object_mut().expect("checked above"));
    let capabilities = match capabilities {
        Some(capabilities) => capabilities,
        None => {
            value["clientCapabilities"] = serde_json::json!({});
            value["clientCapabilities"]
                .as_object_mut()
                .expect("just inserted an object")
        }
    };
    // A plain bool: `AuthCapabilities.terminal` is a `bool` in this version of
    // the schema, unlike the object-shaped capabilities beside it.
    capabilities.insert("auth".to_owned(), serde_json::json!({ "terminal": true }));

    Some(Frame::Request {
        id: id.clone(),
        method: method.clone(),
        params: Some(serde_json::value::to_raw_value(&value).ok()?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mjx_acp_core::RequestId;
    use serde_json::json;

    fn interceptor() -> AuthInterceptor {
        AuthInterceptor::new(
            "test-agent".into(),
            AgentCommand {
                program: "the-agent".into(),
                args: vec!["acp".into()],
                env: Default::default(),
            },
            Arc::new(AuthRegistry::default()),
        )
    }

    fn request(id: i64, method: &str, params: serde_json::Value) -> Frame {
        Frame::Request {
            id: RequestId::Number(id),
            method: method.into(),
            params: Some(serde_json::value::to_raw_value(&params).unwrap()),
        }
    }

    fn result(id: i64, value: serde_json::Value) -> Frame {
        Frame::Response {
            id: RequestId::Number(id),
            payload: ResponsePayload::Result(serde_json::value::to_raw_value(&value).unwrap()),
        }
    }

    fn auth_error(id: i64) -> Frame {
        Frame::error(
            RequestId::Number(id),
            JsonRpcError::auth_required("authenticate first"),
        )
    }

    /// Drives the handshake: `initialize` out, `authMethods` back.
    fn handshake(interceptor: &AuthInterceptor, outbox: &Outbox, methods: serde_json::Value) {
        interceptor.on_client_frame(&request(1, method::agent::INITIALIZE, json!({})), outbox);
        interceptor.on_agent_frame(&result(1, json!({ "authMethods": methods })), outbox);
    }

    fn env_method() -> serde_json::Value {
        json!({
            "type": "env_var",
            "id": "api-key",
            "name": "API key",
            "vars": [{ "name": "OPENAI_API_KEY" }],
            "link": "https://example.test/keys"
        })
    }

    #[test]
    fn initialize_declares_that_this_client_can_run_a_login() {
        // Without it a conformant agent withholds its `terminal` methods, and
        // the one login the operator could actually complete is never offered.
        let (outbox, _rx) = Outbox::for_test();
        let out = interceptor()
            .on_client_frame(&request(1, method::agent::INITIALIZE, json!({})), &outbox);
        let Disposition::Rewrite(frame) = out else {
            panic!("initialize must be rewritten");
        };
        let params: serde_json::Value =
            serde_json::from_str(frame.params().unwrap().get()).unwrap();
        assert_eq!(params["clientCapabilities"]["auth"]["terminal"], true);
    }

    #[test]
    fn the_browsers_own_capabilities_survive_the_merge() {
        let (outbox, _rx) = Outbox::for_test();
        let out = interceptor().on_client_frame(
            &request(
                1,
                method::agent::INITIALIZE,
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": { "elicitation": { "form": {} } }
                }),
            ),
            &outbox,
        );
        let Disposition::Rewrite(frame) = out else {
            panic!("expected a rewrite");
        };
        let params: serde_json::Value =
            serde_json::from_str(frame.params().unwrap().get()).unwrap();
        assert_eq!(params["protocolVersion"], 1);
        assert!(params["clientCapabilities"]["elicitation"]["form"].is_object());
        assert_eq!(params["clientCapabilities"]["auth"]["terminal"], true);
    }

    #[test]
    fn only_initialize_is_rewritten() {
        let (outbox, _rx) = Outbox::for_test();
        assert!(matches!(
            interceptor().on_client_frame(
                &request(2, method::agent::SESSION_NEW, json!({ "cwd": "/w" })),
                &outbox
            ),
            Disposition::Forward
        ));
    }

    #[test]
    fn the_methods_the_agent_advertised_are_read_off_the_handshake() {
        let (outbox, _rx) = Outbox::for_test();
        let interceptor = interceptor();
        handshake(&interceptor, &outbox, json!([env_method()]));

        let state = interceptor.auth_state();
        assert_eq!(state.methods.len(), 1);
        assert_eq!(state.methods[0].id, "api-key");
        assert_eq!(state.methods[0].kind, "envVar");
        // Nothing has been refused yet, so nothing is required.
        assert!(!state.required);
        assert!(!state.authenticated);
    }

    #[test]
    fn a_method_we_cannot_read_does_not_cost_us_the_ones_we_can() {
        // Everything off the wire is untrusted. An agent that advertises one
        // malformed entry beside two good ones should still get the two.
        let (outbox, _rx) = Outbox::for_test();
        let interceptor = interceptor();
        handshake(
            &interceptor,
            &outbox,
            json!([
                { "id": "first", "name": "First" },
                { "type": "env_var", "id": "broken" },
                env_method(),
            ]),
        );

        let state = interceptor.auth_state();
        let ids: Vec<&str> = state.methods.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["first", "api-key"]);
    }

    #[test]
    fn a_refusal_is_rewritten_to_carry_what_the_agent_offered() {
        let (outbox, _rx) = Outbox::for_test();
        let interceptor = interceptor();
        handshake(&interceptor, &outbox, json!([env_method()]));

        interceptor.on_client_frame(
            &request(2, method::agent::SESSION_NEW, json!({ "cwd": "/w" })),
            &outbox,
        );
        let out = interceptor.on_agent_frame(&auth_error(2), &outbox);

        let Disposition::Rewrite(Frame::Response {
            payload: ResponsePayload::Error(error),
            ..
        }) = out
        else {
            panic!("a -32000 must be rewritten with its explanation");
        };
        assert!(error.is_auth_required());
        // The agent's own message survives: it is the only part written by the
        // thing that actually refused.
        assert_eq!(error.message, "authenticate first");

        let detail: ext::AuthState =
            serde_json::from_str(error.data.as_ref().unwrap().get()).unwrap();
        assert!(detail.required);
        assert_eq!(detail.refused_method.as_deref(), Some("session/new"));
        assert_eq!(detail.methods[0].id, "api-key");
        // With nothing configured the registry still says what to do, which is
        // the whole point: this replaces a raw JSON-RPC error.
        assert!(
            detail.methods[0]
                .instructions
                .as_deref()
                .is_some_and(|i| i.contains("OPENAI_API_KEY")),
            "{:?}",
            detail.methods[0]
        );
        // Names, never values.
        assert_eq!(detail.methods[0].secrets[0].name, "OPENAI_API_KEY");
        assert!(!detail.methods[0].secrets[0].present);
    }

    #[test]
    fn a_refusal_of_any_method_is_explained_not_just_a_new_session() {
        // A token that expires mid-conversation answers a `session/prompt`.
        let (outbox, _rx) = Outbox::for_test();
        let interceptor = interceptor();
        handshake(&interceptor, &outbox, json!([env_method()]));

        interceptor.on_client_frame(
            &request(
                7,
                method::agent::SESSION_PROMPT,
                json!({ "sessionId": "s1" }),
            ),
            &outbox,
        );
        assert!(matches!(
            interceptor.on_agent_frame(&auth_error(7), &outbox),
            Disposition::Rewrite(_)
        ));
        assert_eq!(
            interceptor.auth_state().refused_method.as_deref(),
            Some("session/prompt")
        );
    }

    #[test]
    fn every_other_error_is_left_alone() {
        // The relay's default is to forward what it cannot classify, and an
        // error between the agent and the browser is none of our business.
        let (outbox, _rx) = Outbox::for_test();
        let interceptor = interceptor();
        interceptor.on_client_frame(
            &request(3, method::agent::SESSION_NEW, json!({ "cwd": "/w" })),
            &outbox,
        );
        let out = interceptor.on_agent_frame(
            &Frame::error(RequestId::Number(3), JsonRpcError::internal("boom")),
            &outbox,
        );
        assert!(matches!(out, Disposition::Forward));
        assert!(!interceptor.auth_state().required);
    }

    #[test]
    fn the_state_can_be_asked_for_rather_than_only_announced() {
        // A pull, because the refusal may have arrived while a different
        // browser was attached — or none at all — and a notification sent then
        // is gone.
        let (outbox, _agent_rx, mut browser_rx) = Outbox::for_test_with_browser();
        let interceptor = interceptor();
        handshake(&interceptor, &outbox, json!([env_method()]));

        let answered =
            interceptor.on_extension_request(&request(9, ext::AUTH_STATE, json!({})), &outbox);
        assert!(answered);

        let line = browser_rx.try_recv().expect("an answer must be sent");
        let Ok(Frame::Response {
            payload: ResponsePayload::Result(result),
            id,
        }) = Frame::parse(&line)
        else {
            panic!("expected a result, got {line}");
        };
        // Answered on the browser's own id, untranslated. These never enter the
        // agent's id space.
        assert_eq!(id, RequestId::Number(9));
        let state: ext::AuthState = serde_json::from_str(result.get()).unwrap();
        assert_eq!(state.methods[0].id, "api-key");
    }

    #[test]
    fn an_extension_that_is_not_ours_is_left_for_somebody_else() {
        let (outbox, _rx) = Outbox::for_test();
        assert!(
            !interceptor()
                .on_extension_request(&request(9, ext::SESSION_REPLAY, json!({})), &outbox)
        );
    }
}
