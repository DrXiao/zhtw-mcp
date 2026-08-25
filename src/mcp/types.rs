// MCP JSON-RPC 2.0 types for stdio transport.
//
// Covers the MCP protocol: initialize (with capability negotiation),
// tools/list, tools/call, resources/list, resources/read, prompts/list,
// prompts/get.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// JSON-RPC base types

/// A JSON-RPC 2.0 request ID (integer, string, or null).
///
/// JSON-RPC 2.0 allows `"id": null` in requests. A request with an explicit
/// null id is NOT a notification (notifications omit the id field entirely).
/// Error responses for requests with null id must include `"id": null`.
#[derive(Debug, Clone)]
pub enum RequestId {
    Int(i64),
    Str(String),
    /// Explicit `"id": null` in the request. Distinct from absent id
    /// (which indicates a notification).
    Null,
}

impl Serialize for RequestId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            RequestId::Int(i) => serializer.serialize_i64(*i),
            RequestId::Str(s) => serializer.serialize_str(s),
            RequestId::Null => serializer.serialize_unit(),
        }
    }
}

impl<'de> Deserialize<'de> for RequestId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v = Value::deserialize(deserializer)?;
        match &v {
            Value::Null => Ok(RequestId::Null),
            Value::Number(n) => n
                .as_i64()
                .map(RequestId::Int)
                .ok_or_else(|| serde::de::Error::custom("id number must be an integer")),
            Value::String(s) => Ok(RequestId::Str(s.clone())),
            _ => Err(serde::de::Error::custom(
                "id must be a string, integer, or null",
            )),
        }
    }
}

/// Incoming JSON-RPC request (method call or notification).
///
/// When `id` is `None`, this is a notification (no response expected).
/// When `id` is `Some(RequestId::Null)`, the client sent `"id": null`
/// and still expects a response.
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(default, deserialize_with = "deserialize_request_id")]
    pub id: Option<RequestId>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// Deserialize the `id` field so that `"id": null` becomes
/// `Some(RequestId::Null)` rather than `None` (which serde's default
/// `Option<T>` handling would produce).  An absent field still yields
/// `None` via `#[serde(default)]`.
fn deserialize_request_id<'de, D>(deserializer: D) -> Result<Option<RequestId>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    RequestId::deserialize(deserializer).map(Some)
}

/// Outgoing JSON-RPC response.
///
/// The `id` field is always serialized per JSON-RPC 2.0: `None` produces
/// `"id": null` (required for error responses when the request id is
/// unknown).
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: Option<RequestId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn success(id: Option<RequestId>, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Option<RequestId>, code: i64, message: String) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message,
                data: None,
            }),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

// MCP protocol types

// Transport error types

/// Structured transport error for distinguishing failure modes in the
/// dispatch loop. Maps to specific JSON-RPC error codes:
///   - Parse → -32700 (PARSE_ERROR)
///   - InvalidRequest → -32600 (INVALID_REQUEST)
///   - PeerResponse → no reply, and passed to the SDK to correlate
#[derive(Debug)]
pub enum TransportError {
    /// Input is not valid JSON (malformed syntax).
    Parse(serde_json::Error),
    /// Valid JSON but not a valid JSON-RPC request (missing method, wrong
    /// version, response-shaped message with id, etc.).  Carries the
    /// extracted request id (if any) for error response correlation.
    InvalidRequest(Option<RequestId>, String),
    /// Response-shaped message (has result/error, no method): a reply to a
    /// request this server sent, not a request to serve.
    ///
    /// Not an error so much as a different kind of message. It gets no reply
    /// of its own, per JSON-RPC 2.0 ("The Server MUST NOT reply to a
    /// Response"), but it is not discarded either: the caller hands it to the
    /// SDK, which owns the ids it has outstanding and is the only thing that
    /// can match a reply to its request.
    PeerResponse,
}

impl TransportError {
    /// JSON-RPC error code for this transport error, if applicable.
    /// Returns None for PeerResponse, which is not answered at all.
    pub fn error_code(&self) -> Option<i64> {
        match self {
            TransportError::PeerResponse => None,
            TransportError::Parse(_) => Some(PARSE_ERROR),
            TransportError::InvalidRequest(..) => Some(INVALID_REQUEST),
        }
    }

    /// Human-readable error message for JSON-RPC error responses.
    pub fn error_message(&self) -> String {
        match self {
            TransportError::PeerResponse => "response to a server request".into(),
            TransportError::Parse(e) => format!("parse error: {e}"),
            TransportError::InvalidRequest(_, msg) => format!("invalid request: {msg}"),
        }
    }
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Parse(e) => write!(f, "JSON parse: {e}"),
            TransportError::InvalidRequest(_, msg) => write!(f, "invalid request: {msg}"),
            TransportError::PeerResponse => write!(f, "response to a server request"),
        }
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TransportError::Parse(e) => Some(e),
            _ => None,
        }
    }
}

impl TransportError {
    /// Build a JSON-RPC error response for this transport error, if one
    /// should be sent.  Returns None for PeerResponse.
    ///
    /// For InvalidRequest, the carried id (extracted during parsing) is
    /// used so the client can correlate the error with its original request.
    /// The `fallback_id` is used for Parse, where the input never parsed far
    /// enough to yield an id.
    pub fn into_response(self, fallback_id: Option<RequestId>) -> Option<JsonRpcResponse> {
        let code = self.error_code()?;
        let id = match &self {
            TransportError::InvalidRequest(carried_id, _) => carried_id.clone(),
            _ => fallback_id,
        };
        let message = self.error_message();
        Some(JsonRpcResponse::error(id, code, message))
    }
}

/// Parse a raw JSON line into a validated JsonRpcRequest.
///
/// Returns TransportError variants that preserve the distinction between
/// malformed JSON (Parse → -32700) and valid-JSON-but-invalid-JSON-RPC
/// (InvalidRequest → -32600).
pub fn parse_jsonrpc_line(line: &str) -> Result<JsonRpcRequest, TransportError> {
    // Step 1: parse as generic JSON.
    //
    // Everything goes through Value.  Deserializing straight into
    // JsonRpcRequest is measurably faster but not equivalent: serde's
    // ignore-unknown-field path skips strings without decoding them, so a lone
    // UTF-16 surrogate escape in a field this struct does not name parses fine
    // there and fails here.  The server must not act on a line its own JSON
    // parser rejects, and a few hundred nanoseconds do not buy that.
    let obj: serde_json::Value = serde_json::from_str(line).map_err(TransportError::Parse)?;

    // Step 2: a JSON-RPC message is an object.  Arrays (batches, which MCP
    // does not support, and positional forms) and bare scalars are invalid
    // requests.  No id can be recovered from them, so none is echoed.
    //
    // This check is what keeps serde from building a JsonRpcRequest out of a
    // positional array in step 5: `["2.0",1,"ping",{}]` deserializes into the
    // struct just as happily as an object does.
    if !obj.is_object() {
        return Err(TransportError::InvalidRequest(
            None,
            "request must be a JSON object".into(),
        ));
    }

    // Step 3: extract id once for error correlation across all branches.
    // id_present is true when the JSON has an "id" key (even if null or
    // an unparseable type like boolean/array).  raw_id is the parsed id
    // when it's a valid string, integer, or null.
    let id_value = obj.get("id");
    let raw_id: Option<RequestId> = id_value.and_then(|v| serde_json::from_value(v.clone()).ok());
    let id_present = id_value.is_some();

    // Step 4: handle messages without a method field.
    if obj.get("method").is_none() {
        let is_response = obj.get("result").is_some() || obj.get("error").is_some();
        if is_response {
            // Response-shaped (has result/error, no method): silently discard.
            // JSON-RPC 2.0: "The Server MUST NOT reply to a Response."
            // Covers late sampling responses (with id) and orphaned responses
            // (without id).
            return Err(TransportError::PeerResponse);
        }
        if id_present {
            // Has id, no method, not response-shaped — genuinely invalid.
            return Err(TransportError::InvalidRequest(
                raw_id,
                "message has id but no method".into(),
            ));
        }
        // No id, no method, no result/error — genuinely invalid request
        // (e.g. `{}` or `{"foo":"bar"}`).
        return Err(TransportError::InvalidRequest(
            None,
            "object has no method, result, or error field".into(),
        ));
    }

    // Step 5: convert to typed request.
    let req: JsonRpcRequest = serde_json::from_value(obj)
        .map_err(|e| TransportError::InvalidRequest(raw_id.clone(), e.to_string()))?;

    // Step 6: validate JSON-RPC version.
    if req.jsonrpc != JSONRPC_VERSION {
        return Err(TransportError::InvalidRequest(
            raw_id,
            format!(
                "expected jsonrpc \"{JSONRPC_VERSION}\", got \"{}\"",
                req.jsonrpc
            ),
        ));
    }

    Ok(req)
}

// Protocol constants

pub const JSONRPC_VERSION: &str = "2.0";

// Standard JSON-RPC error codes.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const SERVER_NOT_INITIALIZED: i64 = -32002;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_id_int_roundtrip() {
        let id = RequestId::Int(42);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "42");
        let parsed: RequestId = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, RequestId::Int(42)));
    }

    #[test]
    fn request_id_string_roundtrip() {
        let id = RequestId::Str("req-abc".into());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"req-abc\"");
        let parsed: RequestId = serde_json::from_str(&json).unwrap();
        match parsed {
            RequestId::Str(s) => assert_eq!(s, "req-abc"),
            _ => panic!("expected string id"),
        }
    }

    #[test]
    fn request_id_negative_int() {
        let id = RequestId::Int(-1);
        let json = serde_json::to_string(&id).unwrap();
        let parsed: RequestId = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, RequestId::Int(-1)));
    }

    #[test]
    fn request_id_null_roundtrip() {
        let id = RequestId::Null;
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "null");
        let parsed: RequestId = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, RequestId::Null));
    }

    #[test]
    fn request_id_boolean_rejected() {
        let result = serde_json::from_str::<RequestId>("true");
        assert!(result.is_err());
    }

    #[test]
    fn request_id_array_rejected() {
        let result = serde_json::from_str::<RequestId>("[1,2]");
        assert!(result.is_err());
    }

    #[test]
    fn request_null_id_is_not_notification() {
        // "id": null is a request, not a notification.
        let line = r#"{"jsonrpc":"2.0","method":"ping","id":null}"#;
        let req = parse_jsonrpc_line(line).unwrap();
        assert!(req.id.is_some(), "null id must be Some(RequestId::Null)");
        assert!(matches!(req.id, Some(RequestId::Null)));
    }

    #[test]
    fn request_absent_id_is_notification() {
        let line = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let req = parse_jsonrpc_line(line).unwrap();
        assert!(req.id.is_none(), "absent id must be None (notification)");
    }

    // -- JsonRpcError serde --

    #[test]
    fn jsonrpc_error_with_data_roundtrip() {
        let err = JsonRpcError {
            code: INVALID_REQUEST,
            message: "bad request".into(),
            data: Some(json!({"field": "profile", "accepted": ["base", "strict"]})),
        };
        let json = serde_json::to_string(&err).unwrap();
        // Read the wire, not the struct. Parsing back through the same derive
        // that produced the JSON is self-consistent by construction: a stray
        // rename on a field would survive it, while these pin the key names
        // JSON-RPC 2.0 actually specifies.
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["code"], INVALID_REQUEST);
        assert_eq!(parsed["message"], "bad request");
        assert_eq!(parsed["data"]["field"], "profile");
    }

    #[test]
    fn jsonrpc_error_without_data_omits_field() {
        let err = JsonRpcError {
            code: PARSE_ERROR,
            message: "parse error".into(),
            data: None,
        };
        let json = serde_json::to_string(&err).unwrap();
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["code"], PARSE_ERROR);
        assert!(parsed.get("data").is_none());
    }

    // -- JsonRpcResponse serde --

    #[test]
    fn response_success_omits_error() {
        let resp = JsonRpcResponse::success(Some(RequestId::Int(1)), json!("ok"));
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["result"], "ok");
        assert!(parsed.get("error").is_none());
    }

    #[test]
    fn response_error_omits_result() {
        let resp = JsonRpcResponse::error(Some(RequestId::Int(1)), -32600, "bad".into());
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("result").is_none());
        assert_eq!(parsed["error"]["code"], -32600);
    }

    #[test]
    fn response_unknown_id_serializes_as_null() {
        // JSON-RPC 2.0: error responses with unknown id must include "id": null
        let resp = JsonRpcResponse::error(None, PARSE_ERROR, "err".into());
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("id").is_some(), "id field must be present");
        assert!(parsed["id"].is_null(), "unknown id must serialize as null");
    }

    // -- parse_jsonrpc_line --

    #[test]
    fn parse_valid_request() {
        let line = r#"{"jsonrpc":"2.0","method":"tools/list","id":1,"params":{}}"#;
        let req = parse_jsonrpc_line(line).unwrap();
        assert_eq!(req.method, "tools/list");
        assert!(matches!(req.id, Some(RequestId::Int(1))));
    }

    #[test]
    fn parse_malformed_json_returns_parse_error() {
        let line = "not json at all";
        let err = parse_jsonrpc_line(line).unwrap_err();
        assert!(matches!(err, TransportError::Parse(_)));
        assert_eq!(err.error_code(), Some(PARSE_ERROR));
    }

    #[test]
    fn parse_response_shaped_with_id_returns_stale() {
        // JSON-RPC 2.0: "The Server MUST NOT reply to a Response."
        // Response-shaped messages (has result/error, no method) are silently
        // discarded regardless of whether they carry an id.
        let line = r#"{"jsonrpc":"2.0","id":1,"result":"ok"}"#;
        let err = parse_jsonrpc_line(line).unwrap_err();
        assert!(matches!(err, TransportError::PeerResponse));
        assert_eq!(err.error_code(), None);
        assert!(err.into_response(None).is_none());
    }

    #[test]
    fn parse_response_shaped_without_id_returns_stale() {
        let line = r#"{"jsonrpc":"2.0","result":"stale"}"#;
        let err = parse_jsonrpc_line(line).unwrap_err();
        assert!(matches!(err, TransportError::PeerResponse));
        assert_eq!(err.error_code(), None);
        assert!(err.into_response(None).is_none());
    }

    #[test]
    fn parse_wrong_jsonrpc_version() {
        let line = r#"{"jsonrpc":"1.0","method":"test","id":1}"#;
        let err = parse_jsonrpc_line(line).unwrap_err();
        assert!(matches!(err, TransportError::InvalidRequest(..)));
    }

    #[test]
    fn parse_notification_no_id() {
        let line = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let req = parse_jsonrpc_line(line).unwrap();
        assert!(req.id.is_none());
        assert_eq!(req.method, "notifications/initialized");
    }

    #[test]
    fn parse_empty_object_returns_invalid_request() {
        let line = r#"{}"#;
        let err = parse_jsonrpc_line(line).unwrap_err();
        assert!(matches!(err, TransportError::InvalidRequest(..)));
        assert_eq!(err.error_code(), Some(INVALID_REQUEST));
    }

    #[test]
    fn parse_arbitrary_object_without_method_returns_invalid_request() {
        let line = r#"{"foo":"bar","baz":42}"#;
        let err = parse_jsonrpc_line(line).unwrap_err();
        assert!(matches!(err, TransportError::InvalidRequest(..)));
    }

    #[test]
    fn parse_invalid_request_carries_id() {
        // A message with id but no method and not response-shaped should
        // produce an error response that echoes the id back to the client.
        let line = r#"{"id":99,"jsonrpc":"2.0"}"#;
        let err = parse_jsonrpc_line(line).unwrap_err();
        assert!(matches!(err, TransportError::InvalidRequest(..)));
        let resp = err.into_response(None).expect("should produce response");
        match &resp.id {
            Some(RequestId::Int(99)) => {}
            other => panic!("expected id=99, got {other:?}"),
        }
    }

    #[test]
    fn parse_positional_array_returns_invalid_request() {
        // serde deserializes a struct from a positional sequence, so without
        // an explicit object check this parsed as a valid ping and the server
        // answered it.
        let line = r#"["2.0",1,"ping",{}]"#;
        let err = parse_jsonrpc_line(line).unwrap_err();
        assert!(matches!(err, TransportError::InvalidRequest(..)));
        assert_eq!(err.error_code(), Some(INVALID_REQUEST));
    }

    #[test]
    fn parse_batch_array_returns_invalid_request() {
        // MCP does not support JSON-RPC batching; 2025-06-18 removed it.
        let line = r#"[{"jsonrpc":"2.0","method":"ping","id":1}]"#;
        let err = parse_jsonrpc_line(line).unwrap_err();
        assert!(matches!(err, TransportError::InvalidRequest(..)));
        assert_eq!(err.error_code(), Some(INVALID_REQUEST));
    }

    #[test]
    fn parse_bare_scalar_returns_invalid_request() {
        // Valid JSON, not a JSON-RPC message.
        for line in [r#""ping""#, "42", "true", "null"] {
            let err = parse_jsonrpc_line(line).unwrap_err();
            assert!(
                matches!(err, TransportError::InvalidRequest(..)),
                "scalar {line} must be an invalid request"
            );
            assert_eq!(err.error_code(), Some(INVALID_REQUEST));
        }
    }

    #[test]
    fn parse_surrounding_whitespace() {
        let line = "  \t{\"jsonrpc\":\"2.0\",\"method\":\"ping\",\"id\":3}  \t ";
        let req = parse_jsonrpc_line(line).unwrap();
        assert_eq!(req.method, "ping");
        assert!(matches!(req.id, Some(RequestId::Int(3))));
    }

    #[test]
    fn parse_lone_surrogate_in_ignored_field_is_a_parse_error() {
        // Regression guard against reintroducing a
        // `from_str::<JsonRpcRequest>` shortcut: serde's ignore-unknown-field
        // path skips strings without decoding them, so this line parses as a
        // request there but not as a Value. Acting on it would mean executing
        // a message our own JSON parser rejects.
        let line = r#"{"jsonrpc":"2.0","method":"ping","id":1,"x":"\ud800"}"#;
        let err = parse_jsonrpc_line(line).unwrap_err();
        assert!(matches!(err, TransportError::Parse(_)));
        assert_eq!(err.error_code(), Some(PARSE_ERROR));
    }

    #[test]
    fn parse_duplicate_key_takes_the_last_value() {
        // Value applies last-key-wins, while serde's derived impl rejects
        // duplicate fields outright. Pinned because the two disagree.
        let line = r#"{"jsonrpc":"2.0","method":"ping","id":1,"id":2}"#;
        let req = parse_jsonrpc_line(line).unwrap();
        assert!(matches!(req.id, Some(RequestId::Int(2))));
    }

    #[test]
    fn parse_duplicate_key_with_invalid_last_value_is_rejected() {
        let line = r#"{"jsonrpc":"2.0","method":"ping","id":1,"id":true}"#;
        let err = parse_jsonrpc_line(line).unwrap_err();
        assert!(matches!(err, TransportError::InvalidRequest(..)));
        let resp = err.into_response(None).expect("should produce response");
        assert!(resp.id.is_none());
    }

    #[test]
    fn parse_non_object_error_response_has_null_id() {
        for line in [r#"["2.0",1,"ping",{}]"#, r#""ping""#] {
            let err = parse_jsonrpc_line(line).unwrap_err();
            let resp = err.into_response(None).expect("should produce response");
            assert!(resp.id.is_none(), "{line} must answer with a null id");
            let parsed: Value =
                serde_json::from_str(&serde_json::to_string(&resp).unwrap()).unwrap();
            assert!(parsed["id"].is_null());
        }
    }

    #[test]
    fn parse_wrong_version_carries_id() {
        let line = r#"{"jsonrpc":"1.0","method":"test","id":7}"#;
        let err = parse_jsonrpc_line(line).unwrap_err();
        let resp = err.into_response(None).expect("should produce response");
        match &resp.id {
            Some(RequestId::Int(7)) => {}
            other => panic!("expected id=7, got {other:?}"),
        }
    }

    // -- TransportError --
}
