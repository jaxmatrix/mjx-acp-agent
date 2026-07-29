//! One id space for an agent that outlives its browsers.
//!
//! JSON-RPC ids are per-connection, and both peers number their requests
//! upwards from one. That is fine while a connection *is* one socket. It stops
//! being fine once an agent survives a reload: the browser that comes back
//! starts its numbering over while the agent still has the previous browser's
//! `session/prompt` in flight, so the agent's answer to the old request would
//! be delivered as the answer to the new one.
//!
//! So the relay stops passing the browser's ids through. It mints its own for
//! everything it forwards to the agent, and maps each answer back to the id the
//! browser used — or drops it, if that browser has gone.

use std::collections::HashMap;

use mjx_acp_core::RequestId;

/// Maps each attachment's request ids onto one continuous agent-side id space.
#[derive(Debug, Default)]
pub struct IdBridge {
    next: i64,
    /// Agent-side id to the browser-side id it was minted for. Only ever holds
    /// ids belonging to the browser currently attached, because
    /// [`IdBridge::reattach`] clears it.
    outstanding: HashMap<RequestId, RequestId>,
}

impl IdBridge {
    /// A bridge with nothing in flight.
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts a new attachment.
    ///
    /// Everything still outstanding belongs to a browser that has gone, so the
    /// agent's eventual answers have nobody to go to. Forgetting them here is
    /// what makes [`IdBridge::for_browser`] return `None` for those answers, and
    /// is also what keeps the map from growing across a long-lived agent's many
    /// reloads.
    pub fn reattach(&mut self) {
        self.outstanding.clear();
    }

    /// The id the agent should see for a request from the browser.
    ///
    /// Numeric, always: an id that round-trips through every agent matters more
    /// than one that encodes where it came from, and nothing says every agent
    /// handles string ids as well as it handles numbers.
    pub fn for_agent(&mut self, browser: RequestId) -> RequestId {
        self.next += 1;
        let agent = RequestId::Number(self.next);
        self.outstanding.insert(agent.clone(), browser);
        agent
    }

    /// The browser id an agent's response belongs to.
    ///
    /// `None` means the browser that asked is no longer attached, and the
    /// response must not be forwarded: it would answer a question the browser
    /// now watching never asked.
    pub fn for_browser(&mut self, agent: &RequestId) -> Option<RequestId> {
        self.outstanding.remove(agent)
    }

    /// How many requests are still awaiting an answer.
    #[allow(dead_code, reason = "used by tests")]
    pub fn in_flight(&self) -> usize {
        self.outstanding.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn number(n: i64) -> RequestId {
        RequestId::Number(n)
    }

    #[test]
    fn a_request_is_answered_with_the_id_the_browser_used() {
        let mut bridge = IdBridge::new();
        let agent = bridge.for_agent(number(1));
        assert_eq!(bridge.for_browser(&agent), Some(number(1)));
        assert_eq!(bridge.in_flight(), 0, "an answered request is forgotten");
    }

    #[test]
    fn two_browsers_that_both_start_at_one_do_not_collide() {
        // The whole reason this type exists. The first browser asks something
        // slow, goes away mid-answer, and the browser that replaces it opens
        // with its own request 1.
        let mut bridge = IdBridge::new();
        let first = bridge.for_agent(number(1));

        bridge.reattach();
        let second = bridge.for_agent(number(1));

        assert_ne!(first, second, "the agent saw one id for two requests");
        assert_eq!(bridge.for_browser(&second), Some(number(1)));
    }

    #[test]
    fn an_answer_owed_to_a_browser_that_left_is_not_deliverable() {
        let mut bridge = IdBridge::new();
        let agent = bridge.for_agent(number(1));

        bridge.reattach();

        assert_eq!(
            bridge.for_browser(&agent),
            None,
            "the departed browser's answer must not be handed to its replacement"
        );
    }

    #[test]
    fn reattaching_forgets_what_the_previous_browser_left_in_flight() {
        // Otherwise an agent that never answers would grow the map by one entry
        // per reload, for as long as it lives.
        let mut bridge = IdBridge::new();
        bridge.for_agent(number(1));
        bridge.for_agent(number(2));
        assert_eq!(bridge.in_flight(), 2);

        bridge.reattach();
        assert_eq!(bridge.in_flight(), 0);
    }

    #[test]
    fn a_string_id_is_translated_to_a_number_and_back() {
        // The browser SDK numbers its requests, but the protocol allows strings
        // and the relay must not be the thing that breaks a client that uses
        // them.
        let mut bridge = IdBridge::new();
        let browser = RequestId::String("abc".into());
        let agent = bridge.for_agent(browser.clone());

        assert!(matches!(agent, RequestId::Number(_)));
        assert_eq!(bridge.for_browser(&agent), Some(browser));
    }

    #[test]
    fn an_id_we_never_minted_belongs_to_nobody() {
        let mut bridge = IdBridge::new();
        assert_eq!(bridge.for_browser(&number(99)), None);
    }
}
