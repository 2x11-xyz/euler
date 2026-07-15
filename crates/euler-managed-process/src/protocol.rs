use serde_json::{json, Map, Value};

#[derive(Debug)]
pub(crate) enum IncomingMessage {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: Value,
        body: ResponseBody,
    },
}

#[derive(Debug)]
pub(crate) enum ResponseBody {
    Result(Value),
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProtocolError {
    InvalidMessage,
}

pub(crate) fn decode_message(bytes: &[u8]) -> Result<IncomingMessage, ProtocolError> {
    let value =
        serde_json::from_slice::<Value>(bytes).map_err(|_| ProtocolError::InvalidMessage)?;
    let object = value.as_object().ok_or(ProtocolError::InvalidMessage)?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(ProtocolError::InvalidMessage);
    }
    if let Some(method) = object.get("method") {
        let method = method
            .as_str()
            .filter(|method| !method.is_empty())
            .ok_or(ProtocolError::InvalidMessage)?
            .to_owned();
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        return match object.get("id") {
            Some(id) if valid_id(id) => Ok(IncomingMessage::Request {
                id: id.clone(),
                method,
                params,
            }),
            None => Ok(IncomingMessage::Notification { method, params }),
            _ => Err(ProtocolError::InvalidMessage),
        };
    }

    let id = object
        .get("id")
        .filter(|id| valid_id(id))
        .ok_or(ProtocolError::InvalidMessage)?
        .clone();
    match (object.get("result"), object.get("error")) {
        (Some(result), None) => Ok(IncomingMessage::Response {
            id,
            body: ResponseBody::Result(result.clone()),
        }),
        (None, Some(error)) if error.is_object() => Ok(IncomingMessage::Response {
            id,
            body: ResponseBody::Error,
        }),
        _ => Err(ProtocolError::InvalidMessage),
    }
}

pub(crate) fn request(id: Value, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

pub(crate) fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

pub(crate) fn result_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub(crate) fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    })
}

pub(crate) fn object(value: Value) -> Result<Map<String, Value>, ProtocolError> {
    value
        .as_object()
        .cloned()
        .ok_or(ProtocolError::InvalidMessage)
}

fn valid_id(value: &Value) -> bool {
    value.is_string() || value.is_number()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_request_notification_and_response() {
        let request =
            decode_message(br#"{"jsonrpc":"2.0","id":"a","method":"x"}"#).expect("request");
        assert!(matches!(request, IncomingMessage::Request { .. }));
        let notification =
            decode_message(br#"{"jsonrpc":"2.0","method":"x"}"#).expect("notification");
        assert!(matches!(notification, IncomingMessage::Notification { .. }));
        let response =
            decode_message(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#).expect("response");
        assert!(matches!(response, IncomingMessage::Response { .. }));
    }

    #[test]
    fn rejects_ambiguous_or_unbounded_id_shapes() {
        for value in [
            json!({"jsonrpc": "2.0", "id": null, "method": "x"}),
            json!({"jsonrpc": "2.0", "id": {}, "method": "x"}),
            json!({"jsonrpc": "2.0", "id": "x", "result": {}, "error": {}}),
            json!({"jsonrpc": "1.0", "id": "x", "result": {}}),
        ] {
            assert!(decode_message(value.to_string().as_bytes()).is_err());
        }
    }
}
