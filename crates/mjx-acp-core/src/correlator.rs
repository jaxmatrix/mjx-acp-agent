//! Pairs JSON-RPC responses back to the method they answer.
//!
//! A JSON-RPC response carries only an id, never a method name, so anything
//! that wants to label a response — the protocol inspector, the relay's tracing
//! — has to remember what each outstanding request asked for.
//!
//! Ported from Zed's `crates/acp_tools/src/acp_tools.rs`, which keeps the same
//! pair of maps (`incoming_request_methods` / `outgoing_request_methods`) for
//! the same reason.

use std::collections::HashMap;
use std::sync::Arc;

use crate::frame::{Frame, RequestId};

/// Which way a frame is travelling through the relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Browser to agent. Carries the agent-side methods (`session/prompt`, ...)
    /// and the browser's responses to agent-side requests.
    ClientToAgent,
    /// Agent to browser. Carries the client-side methods (`session/update`,
    /// `session/request_permission`, ...) and the agent's responses.
    AgentToClient,
}

impl Direction {
    /// The direction a response to a request sent this way will travel.
    pub fn reply(self) -> Self {
        match self {
            Self::ClientToAgent => Self::AgentToClient,
            Self::AgentToClient => Self::ClientToAgent,
        }
    }
}

/// Remembers in-flight requests so responses can be labelled with a method.
///
/// Two maps, not one: both peers number their requests independently starting
/// from 1, so id `1` from the client and id `1` from the agent are different
/// requests that may be outstanding simultaneously.
#[derive(Debug, Default)]
pub struct MethodCorrelator {
    from_client: HashMap<RequestId, Arc<str>>,
    from_agent: HashMap<RequestId, Arc<str>>,
}

impl MethodCorrelator {
    /// A correlator with nothing in flight.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a frame travelling in `direction`.
    ///
    /// For a request, remembers its method. For a response, forgets and returns
    /// the method it answers — `None` if we never saw the request, which
    /// happens for messages exchanged before the correlator was attached.
    /// Notifications return `None`.
    pub fn observe(&mut self, direction: Direction, frame: &Frame) -> Option<Arc<str>> {
        match frame {
            Frame::Request { id, method, .. } => {
                self.pending_mut(direction)
                    .insert(id.clone(), Arc::from(method.as_str()));
                None
            }
            // A response travelling in `direction` answers a request that
            // travelled the other way.
            Frame::Response { id, .. } => self.pending_mut(direction.reply()).remove(id),
            Frame::Notification { .. } => None,
        }
    }

    /// The method of an in-flight request, without forgetting it.
    pub fn peek(&self, direction: Direction, id: &RequestId) -> Option<Arc<str>> {
        self.pending(direction).get(id).cloned()
    }

    /// Number of requests still awaiting a response, both directions.
    pub fn in_flight(&self) -> usize {
        self.from_client.len() + self.from_agent.len()
    }

    fn pending(&self, direction: Direction) -> &HashMap<RequestId, Arc<str>> {
        match direction {
            Direction::ClientToAgent => &self.from_client,
            Direction::AgentToClient => &self.from_agent,
        }
    }

    fn pending_mut(&mut self, direction: Direction) -> &mut HashMap<RequestId, Arc<str>> {
        match direction {
            Direction::ClientToAgent => &mut self.from_client,
            Direction::AgentToClient => &mut self.from_agent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: i64, method: &str) -> Frame {
        Frame::Request {
            id: RequestId::Number(id),
            method: method.into(),
            params: None,
        }
    }

    fn response(id: i64) -> Frame {
        Frame::result(RequestId::Number(id), &serde_json::json!({})).unwrap()
    }

    #[test]
    fn pairs_a_response_with_its_request() {
        let mut c = MethodCorrelator::new();
        assert!(
            c.observe(Direction::ClientToAgent, &request(1, "session/prompt"))
                .is_none()
        );
        assert_eq!(c.in_flight(), 1);

        let method = c.observe(Direction::AgentToClient, &response(1)).unwrap();
        assert_eq!(&*method, "session/prompt");
        assert_eq!(c.in_flight(), 0, "a paired request is forgotten");
    }

    #[test]
    fn the_two_directions_do_not_collide() {
        // Both peers number from 1 independently. This is the whole reason
        // there are two maps.
        let mut c = MethodCorrelator::new();
        c.observe(Direction::ClientToAgent, &request(1, "session/prompt"));
        c.observe(Direction::AgentToClient, &request(1, "fs/read_text_file"));
        assert_eq!(c.in_flight(), 2);

        // The client answering the agent's request/1.
        let method = c.observe(Direction::ClientToAgent, &response(1)).unwrap();
        assert_eq!(&*method, "fs/read_text_file");

        // The agent answering the client's request/1.
        let method = c.observe(Direction::AgentToClient, &response(1)).unwrap();
        assert_eq!(&*method, "session/prompt");
    }

    #[test]
    fn an_unknown_response_is_not_an_error() {
        // Happens when the inspector attaches to a connection mid-flight.
        let mut c = MethodCorrelator::new();
        assert!(c.observe(Direction::AgentToClient, &response(99)).is_none());
    }

    #[test]
    fn notifications_are_not_tracked() {
        let mut c = MethodCorrelator::new();
        let note = Frame::Notification {
            method: "session/update".into(),
            params: None,
        };
        assert!(c.observe(Direction::AgentToClient, &note).is_none());
        assert_eq!(c.in_flight(), 0);
    }

    #[test]
    fn peek_does_not_forget() {
        let mut c = MethodCorrelator::new();
        c.observe(Direction::ClientToAgent, &request(4, "session/new"));
        let id = RequestId::Number(4);
        assert_eq!(
            &*c.peek(Direction::ClientToAgent, &id).unwrap(),
            "session/new"
        );
        assert_eq!(c.in_flight(), 1);
    }
}
