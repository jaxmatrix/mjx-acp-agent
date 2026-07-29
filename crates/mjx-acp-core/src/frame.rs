//! A lossless JSON-RPC 2.0 frame.
//!
//! ACP frames a message per line of newline-delimited JSON, both over a
//! subprocess's stdio and (in this project) over a WebSocket text frame. This
//! module parses just enough structure to route a frame — is it a request, a
//! response, or a notification, and which method — while keeping the payload
//! opaque so forwarding never mangles it.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// A JSON-RPC request identifier.
///
/// The spec allows a string, a number, or null. Null only shows up on error
/// responses to requests the peer couldn't parse an id out of, but it does show
/// up, so it gets a variant rather than a parse failure.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// `"id": null`.
    Null,
    /// `"id": 7`.
    Number(i64),
    /// `"id": "abc"`.
    String(String),
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("null"),
            Self::Number(n) => write!(f, "{n}"),
            Self::String(s) => f.write_str(s),
        }
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Error code. ACP defines `-32000` (auth required) and `-32002`
    /// (resource not found) on top of the standard JSON-RPC codes.
    pub code: i64,
    /// Human-readable description.
    pub message: String,
    /// Optional structured detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Box<RawValue>>,
}

impl JsonRpcError {
    /// `-32601 Method not found`.
    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("method not found: {method}"),
            data: None,
        }
    }

    /// `-32602 Invalid params`.
    pub fn invalid_params(detail: impl fmt::Display) -> Self {
        Self {
            code: -32602,
            message: format!("invalid params: {detail}"),
            data: None,
        }
    }

    /// `-32603 Internal error`.
    pub fn internal(detail: impl fmt::Display) -> Self {
        Self {
            code: -32603,
            message: detail.to_string(),
            data: None,
        }
    }
}

/// The two things a response can carry.
#[derive(Debug, Clone)]
pub enum ResponsePayload {
    /// A successful result.
    Result(Box<RawValue>),
    /// A failure.
    Error(JsonRpcError),
}

/// A single JSON-RPC message.
#[derive(Debug, Clone)]
pub enum Frame {
    /// A call expecting a response.
    Request {
        /// Correlates the eventual response.
        id: RequestId,
        /// e.g. `session/prompt`.
        method: String,
        /// Opaque payload.
        params: Option<Box<RawValue>>,
    },
    /// A reply to a [`Frame::Request`].
    Response {
        /// The id of the request being answered.
        id: RequestId,
        /// Result or error.
        payload: ResponsePayload,
    },
    /// A one-way message.
    Notification {
        /// e.g. `session/update`.
        method: String,
        /// Opaque payload.
        params: Option<Box<RawValue>>,
    },
}

/// Why a line could not be understood as a JSON-RPC frame.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// The line wasn't valid JSON.
    #[error("not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// Valid JSON, but not a shape JSON-RPC defines — e.g. a batch array, or an
    /// object with neither `method` nor `result`/`error`.
    #[error("not a JSON-RPC message: {0}")]
    Shape(&'static str),
}

/// Every field we might see, so parsing is one pass and unknown fields are
/// tolerated rather than fatal.
#[derive(Deserialize)]
struct Wire {
    #[serde(default)]
    id: Option<RequestId>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Box<RawValue>>,
    /// Doubly optional so an explicit `"result": null` is distinguishable from
    /// no `result` key at all. `null` is a perfectly good success value — a
    /// method that returns nothing answers with it — and collapsing the two
    /// would turn every such response into "neither a call nor a reply".
    #[serde(default, deserialize_with = "present_or_absent")]
    result: Option<Option<Box<RawValue>>>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

/// Distinguishes a missing key from one explicitly set to null.
fn present_or_absent<'de, D>(deserializer: D) -> Result<Option<Option<Box<RawValue>>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<Box<RawValue>>::deserialize(deserializer).map(Some)
}

/// The JSON literal `null`, for a response whose result was explicitly null.
fn null_value() -> Box<RawValue> {
    RawValue::from_string("null".to_owned()).expect("`null` is valid JSON")
}

impl Frame {
    /// Parses one line of newline-delimited JSON-RPC.
    pub fn parse(line: &str) -> Result<Self, FrameError> {
        let wire: Wire = serde_json::from_str(line)?;

        // `method` is what separates a call from a reply, so branch on it first
        // rather than on every combination of the four fields at once.
        if let Some(method) = wire.method {
            if wire.result.is_some() || wire.error.is_some() {
                return Err(FrameError::Shape(
                    "message has both a method and a result/error",
                ));
            }
            // A request has an id, a notification does not. An explicit
            // `"id": null` alongside a method is malformed but harmless, so
            // treat it as a notification rather than refuse to forward it.
            return Ok(match wire.id {
                Some(RequestId::Null) | None => Self::Notification {
                    method,
                    params: wire.params,
                },
                Some(id) => Self::Request {
                    id,
                    method,
                    params: wire.params,
                },
            });
        }

        // `Option<RequestId>` collapses an explicit `"id": null` into `None`,
        // and null is exactly what JSON-RPC uses on an error response to a
        // request it couldn't parse an id out of. Same meaning either way.
        let id = wire.id.unwrap_or(RequestId::Null);
        match (wire.result, wire.error) {
            (Some(result), None) => Ok(Self::Response {
                id,
                payload: ResponsePayload::Result(result.unwrap_or_else(null_value)),
            }),
            (None, Some(error)) => Ok(Self::Response {
                id,
                payload: ResponsePayload::Error(error),
            }),
            (Some(_), Some(_)) => Err(FrameError::Shape("reply has both result and error")),
            (None, None) => Err(FrameError::Shape("neither a call nor a reply")),
        }
    }

    /// The method name, for requests and notifications.
    pub fn method(&self) -> Option<&str> {
        match self {
            Self::Request { method, .. } | Self::Notification { method, .. } => Some(method),
            Self::Response { .. } => None,
        }
    }

    /// The request id, for requests and responses.
    pub fn id(&self) -> Option<&RequestId> {
        match self {
            Self::Request { id, .. } | Self::Response { id, .. } => Some(id),
            Self::Notification { .. } => None,
        }
    }

    /// The payload of a request or notification.
    pub fn params(&self) -> Option<&RawValue> {
        match self {
            Self::Request { params, .. } | Self::Notification { params, .. } => params.as_deref(),
            Self::Response { .. } => None,
        }
    }

    /// Deserializes the payload of a request or notification into a typed
    /// message. Returns `Ok(None)` when there is no payload at all.
    pub fn params_as<T: serde::de::DeserializeOwned>(
        &self,
    ) -> Result<Option<T>, serde_json::Error> {
        self.params()
            .map(|raw| serde_json::from_str(raw.get()))
            .transpose()
    }

    /// Builds a successful response to the given request id.
    pub fn result(id: RequestId, value: &impl Serialize) -> Result<Self, serde_json::Error> {
        Ok(Self::Response {
            id,
            payload: ResponsePayload::Result(serde_json::value::to_raw_value(value)?),
        })
    }

    /// Builds an error response to the given request id.
    pub fn error(id: RequestId, error: JsonRpcError) -> Self {
        Self::Response {
            id,
            payload: ResponsePayload::Error(error),
        }
    }

    /// Builds a notification.
    pub fn notification(
        method: impl Into<String>,
        params: &impl Serialize,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self::Notification {
            method: method.into(),
            params: Some(serde_json::value::to_raw_value(params)?),
        })
    }

    /// Renders the frame as a single line of JSON, with no trailing newline.
    pub fn to_line(&self) -> String {
        // Built by hand rather than through a Serialize impl: `params`,
        // `result` and `data` are already-serialized JSON text, and going
        // through serde_json::to_string would be a parse-and-reprint of bytes
        // we want to pass through untouched.
        let mut out = String::from(r#"{"jsonrpc":"2.0""#);
        match self {
            Self::Request { id, method, params } => {
                out.push_str(r#","id":"#);
                push_json(&mut out, id);
                out.push_str(r#","method":"#);
                push_json(&mut out, method);
                if let Some(params) = params {
                    out.push_str(r#","params":"#);
                    out.push_str(params.get());
                }
            }
            Self::Notification { method, params } => {
                out.push_str(r#","method":"#);
                push_json(&mut out, method);
                if let Some(params) = params {
                    out.push_str(r#","params":"#);
                    out.push_str(params.get());
                }
            }
            Self::Response { id, payload } => {
                out.push_str(r#","id":"#);
                push_json(&mut out, id);
                match payload {
                    ResponsePayload::Result(result) => {
                        out.push_str(r#","result":"#);
                        out.push_str(result.get());
                    }
                    ResponsePayload::Error(error) => {
                        out.push_str(r#","error":"#);
                        push_json(&mut out, error);
                    }
                }
            }
        }
        out.push('}');
        out
    }
}

/// Appends `value` as JSON. The values passed here are plain data that cannot
/// fail to serialize, so a failure is a bug rather than a runtime condition.
fn push_json(out: &mut String, value: &impl Serialize) {
    match serde_json::to_string(value) {
        Ok(json) => out.push_str(&json),
        Err(err) => unreachable!("frame field failed to serialize: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(line: &str) -> String {
        Frame::parse(line).expect("parses").to_line()
    }

    #[test]
    fn parses_a_request() {
        let frame = Frame::parse(
            r#"{"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"sessionId":"s1"}}"#,
        )
        .unwrap();
        assert_eq!(frame.method(), Some("session/prompt"));
        assert_eq!(frame.id(), Some(&RequestId::Number(1)));
        assert!(matches!(frame, Frame::Request { .. }));
    }

    #[test]
    fn parses_a_notification() {
        let frame = Frame::parse(r#"{"jsonrpc":"2.0","method":"session/update","params":{"a":1}}"#)
            .unwrap();
        assert!(matches!(frame, Frame::Notification { .. }));
        assert_eq!(frame.id(), None);
    }

    #[test]
    fn parses_both_kinds_of_response() {
        let ok = Frame::parse(r#"{"jsonrpc":"2.0","id":"x","result":{"stopReason":"end_turn"}}"#)
            .unwrap();
        assert!(matches!(
            ok,
            Frame::Response {
                payload: ResponsePayload::Result(_),
                ..
            }
        ));

        let err =
            Frame::parse(r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"nope"}}"#)
                .unwrap();
        let Frame::Response {
            payload: ResponsePayload::Error(error),
            ..
        } = err
        else {
            panic!("expected an error response");
        };
        assert_eq!(error.code, -32601);
    }

    #[test]
    fn a_null_result_is_a_successful_response() {
        // `null` is what a method returning nothing answers with. Treating it
        // as "no result" would turn every such reply into a parse failure.
        let line = r#"{"jsonrpc":"2.0","id":2,"result":null}"#;
        let frame = Frame::parse(line).unwrap();
        assert!(matches!(
            frame,
            Frame::Response {
                payload: ResponsePayload::Result(_),
                ..
            }
        ));
        assert_eq!(round_trip(line), line);
    }

    #[test]
    fn null_id_is_a_valid_response_id() {
        let frame =
            Frame::parse(r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"bad"}}"#)
                .unwrap();
        assert_eq!(frame.id(), Some(&RequestId::Null));
    }

    #[test]
    fn payloads_survive_the_round_trip_byte_for_byte() {
        // Key order, unicode escapes and float formatting must all come out the
        // way the agent wrote them: the relay is not allowed to normalize.
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"z":1,"a":{"deep":[1.50,"é"]}}}"#;
        assert_eq!(round_trip(line), line);
    }

    #[test]
    fn notifications_without_params_round_trip() {
        let line = r#"{"jsonrpc":"2.0","method":"session/cancel"}"#;
        assert_eq!(round_trip(line), line);
    }

    #[test]
    fn rejects_shapes_that_are_not_json_rpc() {
        // A batch array. We refuse to classify it so the relay forwards it raw
        // rather than silently dropping it.
        assert!(matches!(
            Frame::parse("[]"),
            Err(FrameError::Json(_) | FrameError::Shape(_))
        ));
        assert!(matches!(Frame::parse("{}"), Err(FrameError::Shape(_))));
        assert!(Frame::parse("not json").is_err());
    }

    #[test]
    fn builds_responses_and_notifications() {
        let frame = Frame::result(RequestId::Number(3), &serde_json::json!({"ok": true})).unwrap();
        assert_eq!(
            frame.to_line(),
            r#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#
        );

        let frame = Frame::error(
            RequestId::String("a".into()),
            JsonRpcError::method_not_found("x"),
        );
        assert!(frame.to_line().contains(r#""code":-32601"#));
    }

    #[test]
    fn typed_params_deserialize() {
        #[derive(serde::Deserialize)]
        struct P {
            session_id: String,
        }
        let frame =
            Frame::parse(r#"{"jsonrpc":"2.0","method":"x","params":{"session_id":"s1"}}"#).unwrap();
        assert_eq!(frame.params_as::<P>().unwrap().unwrap().session_id, "s1");

        let frame = Frame::parse(r#"{"jsonrpc":"2.0","method":"x"}"#).unwrap();
        assert!(frame.params_as::<P>().unwrap().is_none());
    }
}
