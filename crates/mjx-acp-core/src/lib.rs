//! Shared ACP plumbing: JSON-RPC frames, method names, and the request-id to
//! method-name correlation the inspector needs.
//!
//! This crate deliberately works on *raw* JSON-RPC frames rather than typed
//! messages. The server's job is to relay ACP between a browser and an agent
//! subprocess, and a relay that can only forward messages it understands is a
//! relay that breaks the moment either side speaks a newer protocol version.
//! Frames carry their payload as [`serde_json::value::RawValue`], so anything
//! we don't touch survives the round trip byte for byte.
//!
//! Typed access is still one step away: [`acp`] re-exports the protocol schema,
//! and [`Frame::params_as`] deserializes a payload when we actually need it.

pub mod correlator;
pub mod ext;
pub mod frame;
pub mod method;

pub use correlator::{Direction, MethodCorrelator};
// `AUTH_REQUIRED` is deliberately *not* re-exported here: `ext` has a method
// name by that spelling, and the two reading alike at the crate root would be a
// trap. Say `frame::AUTH_REQUIRED` for the code and `ext::AUTH_REQUIRED` for
// the notification.
pub use frame::{Frame, FrameError, JsonRpcError, RequestId, ResponsePayload};
pub use method::Side;

/// The ACP protocol schema, re-exported under the same name Zed uses so code
/// ported from `../zed` reads the same (`use agent_client_protocol::schema::v1 as acp`).
pub use agent_client_protocol::schema::v1 as acp;

/// The wire protocol version we speak. The `agent-client-protocol` crate is at
/// 2.0.0 but implements wire protocol v1; the two numbers are unrelated.
pub const PROTOCOL_VERSION: u16 = 1;
