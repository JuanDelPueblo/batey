//! HTTP and WebSocket adapters for agent-level authentication.
//!
//! Every handler parses the request, calls `HubService`, and maps
//! `ServiceError` to a status code. The rules live in the service and in the
//! authentication coordinator.
//!
//! A request body never names an executable, an argument, a working
//! directory, or an environment value. The only inputs are an agent id, a
//! method id, and a flow id, so these routes cannot become a remote shell.
use super::hub::{hub, Result};
use super::AppState;
use crate::auth::{
    AgentAuthRefreshView, AgentAuthView, ProtocolAuthFlowView, ProtocolElicitationView, PtyWindow,
    TerminalAuthFlow, TerminalAuthFlowView,
};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    Json,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;

/// What the browser may send on a flow socket.
///
/// Keystrokes and a window size, and nothing else.
///
/// The message is strict: an unknown field or an unknown type makes it
/// invalid. A tagged enum cannot express that with serde, so each variant is
/// a plain struct with `deny_unknown_fields`, and the tag is checked first.
/// Nothing in a message can therefore name a command, arguments, a working
/// directory, or an environment value.
#[derive(Debug)]
enum FlowClientMessage {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FlowInputMessage {
    data: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FlowResizeMessage {
    cols: u16,
    rows: u16,
}

impl<'de> Deserialize<'de> for FlowClientMessage {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("a flow message must be a JSON object"))?;
        let kind = object.get("type").and_then(|v| v.as_str()).ok_or_else(|| {
            serde::de::Error::custom("a flow message needs a string 'type' field")
        })?;
        let mut fields = object.clone();
        fields.remove("type");
        let fields = serde_json::Value::Object(fields);
        match kind {
            "input" => serde_json::from_value::<FlowInputMessage>(fields)
                .map(|message| Self::Input { data: message.data })
                .map_err(serde::de::Error::custom),
            "resize" => serde_json::from_value::<FlowResizeMessage>(fields)
                .map(|message| Self::Resize {
                    cols: message.cols,
                    rows: message.rows,
                })
                .map_err(serde::de::Error::custom),
            other => Err(serde::de::Error::custom(format!(
                "unknown flow message type '{other}'"
            ))),
        }
    }
}

pub async fn agent_auth(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AgentAuthView>> {
    Ok(Json(hub(&s)?.agent_auth(&id).await?))
}

/// Explicit refresh: the only cache-refreshing action a client can trigger
/// besides an actual authentication or session lifecycle event.
pub async fn refresh_agent_auth(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AgentAuthRefreshView>> {
    Ok(Json(hub(&s)?.refresh_agent_auth(&id).await?))
}

pub async fn authenticate_agent(
    State(s): State<AppState>,
    Path((id, method_id)): Path<(String, String)>,
) -> Result<Json<AgentAuthView>> {
    Ok(Json(hub(&s)?.authenticate_agent(&id, &method_id).await?))
}

pub async fn logout_agent(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AgentAuthView>> {
    Ok(Json(hub(&s)?.logout_agent(&id).await?))
}

pub async fn start_terminal_auth(
    State(s): State<AppState>,
    Path((id, method_id)): Path<(String, String)>,
) -> Result<Json<TerminalAuthFlowView>> {
    Ok(Json(hub(&s)?.start_terminal_auth(&id, &method_id).await?))
}

pub async fn terminal_auth_flow(
    State(s): State<AppState>,
    Path(flow_id): Path<String>,
) -> Result<Json<TerminalAuthFlowView>> {
    Ok(Json(hub(&s)?.terminal_auth_flow(&flow_id)?))
}

pub async fn cancel_terminal_auth(
    State(s): State<AppState>,
    Path(flow_id): Path<String>,
) -> Result<Json<TerminalAuthFlowView>> {
    Ok(Json(hub(&s)?.cancel_terminal_auth(&flow_id)?))
}

/// Starts an asynchronous protocol flow for one `agent` method.
///
/// The response carries the opaque flow id at once. The browser polls the
/// flow and its request-scoped elicitations instead of blocking on one
/// long `authenticate` RPC.
pub async fn start_protocol_auth(
    State(s): State<AppState>,
    Path((id, method_id)): Path<(String, String)>,
) -> Result<Json<ProtocolAuthFlowView>> {
    Ok(Json(hub(&s)?.start_protocol_auth(&id, &method_id).await?))
}

pub async fn protocol_auth_flow(
    State(s): State<AppState>,
    Path(flow_id): Path<String>,
) -> Result<Json<ProtocolAuthFlowView>> {
    Ok(Json(hub(&s)?.protocol_auth_flow(&flow_id).await?))
}

pub async fn cancel_protocol_auth(
    State(s): State<AppState>,
    Path(flow_id): Path<String>,
) -> Result<Json<ProtocolAuthFlowView>> {
    Ok(Json(hub(&s)?.cancel_protocol_auth(&flow_id).await?))
}

pub async fn protocol_auth_elicitations(
    State(s): State<AppState>,
    Path(flow_id): Path<String>,
) -> Result<Json<Vec<ProtocolElicitationView>>> {
    Ok(Json(hub(&s)?.protocol_auth_elicitations(&flow_id).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolElicitationInput {
    pub action: String,
    pub content: Option<serde_json::Value>,
}

pub async fn respond_protocol_auth_elicitation(
    State(s): State<AppState>,
    Path((flow_id, eid)): Path<(String, String)>,
    Json(input): Json<ProtocolElicitationInput>,
) -> Result<Json<serde_json::Value>> {
    let ok = hub(&s)?
        .respond_protocol_auth_elicitation(&flow_id, &eid, &input.action, input.content)
        .await?;
    Ok(Json(serde_json::json!({ "success": ok })))
}

/// The live terminal socket of one flow.
///
/// The authentication middleware already ran on the upgrade request, so an
/// unauthenticated client never reaches this handler.
pub async fn terminal_auth_socket(
    ws: WebSocketUpgrade,
    State(s): State<AppState>,
    Path(flow_id): Path<String>,
) -> Result<axum::response::Response> {
    let flow = hub(&s)?.terminal_auth_socket(&flow_id)?;
    Ok(ws.on_upgrade(move |socket| drive_flow_socket(socket, flow)))
}

/// Serves one attached client for the life of its socket.
async fn drive_flow_socket(socket: WebSocket, flow: Arc<TerminalAuthFlow>) {
    let (mut sender, mut receiver) = socket.split();
    let attachment = flow.attach().await;
    let mut output = attachment.output;
    let mut status = attachment.status;

    // Send the retained tail first, so a client that attaches late sees the
    // current screen. The tail is bounded; older output is already gone.
    let mut decoder = Utf8Stream::default();
    if !attachment.scrollback.is_empty() {
        let text = decoder.push(&attachment.scrollback);
        if !text.is_empty() && send_output(&mut sender, &text).await.is_err() {
            flow.detach();
            return;
        }
    }
    if send_state(&mut sender, &flow).await.is_err() {
        flow.detach();
        return;
    }
    if flow.state().is_finished() {
        flow.detach();
        return;
    }

    loop {
        tokio::select! {
            chunk = output.recv() => match chunk {
                Ok(chunk) => {
                    let text = decoder.push(&chunk);
                    if !text.is_empty() && send_output(&mut sender, &text).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // The socket fell behind. Terminal output is a stream of
                    // screen updates, so resynchronize instead of replaying.
                    decoder = Utf8Stream::default();
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    let _ = send_state(&mut sender, &flow).await;
                    break;
                }
            },
            changed = status.changed() => {
                if changed.is_err() {
                    break;
                }
                if flow.state().is_finished() {
                    // The program may have written its last lines just before
                    // it ended. Deliver those, then report the outcome.
                    drain_output(&mut sender, &mut output, &mut decoder).await;
                    let _ = send_state(&mut sender, &flow).await;
                    break;
                }
                if send_state(&mut sender, &flow).await.is_err() {
                    break;
                }
            }
            incoming = receiver.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<FlowClientMessage>(&text) {
                        Ok(FlowClientMessage::Input { data }) => {
                            // Keystrokes go to the PTY and nowhere else.
                            let _ = flow.send_input(data.as_bytes());
                        }
                        Ok(FlowClientMessage::Resize { cols, rows }) => {
                            let _ = flow.resize(PtyWindow::clamped(cols, rows));
                        }
                        Err(_) => {}
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            }
        }
    }

    flow.detach();
}

/// How long a finished flow waits for the last output to arrive.
///
/// The reader and the exit watcher are separate, so the final lines can still
/// be in flight when the program ends. The wait is short and bounded.
const FINAL_OUTPUT_GRACE: std::time::Duration = std::time::Duration::from_millis(250);

/// Delivers whatever output is still in flight after the program ended.
async fn drain_output(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    output: &mut tokio::sync::broadcast::Receiver<Arc<Vec<u8>>>,
    decoder: &mut Utf8Stream,
) {
    let deadline = tokio::time::Instant::now() + FINAL_OUTPUT_GRACE;
    while let Ok(Ok(chunk)) = tokio::time::timeout_at(deadline, output.recv()).await {
        let text = decoder.push(&chunk);
        if !text.is_empty() && send_output(sender, &text).await.is_err() {
            return;
        }
    }
}

async fn send_output(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    text: &str,
) -> std::result::Result<(), axum::Error> {
    let payload = serde_json::json!({"type": "output", "data": text});
    sender.send(Message::Text(payload.to_string())).await
}

async fn send_state(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    flow: &TerminalAuthFlow,
) -> std::result::Result<(), axum::Error> {
    let view = flow.view();
    let payload = serde_json::json!({
        "type": "state",
        "flow_id": view.flow_id,
        "agent_id": view.agent_id,
        "method_id": view.method_id,
        "state": view.state,
        "exit_code": view.exit_code,
        "reason": view.reason,
    });
    sender.send(Message::Text(payload.to_string())).await
}

/// Decodes PTY bytes as UTF-8 across chunk boundaries.
///
/// A terminal writes whenever it wants, so one read can end in the middle of
/// a multi-byte character. The decoder holds that tail back until the next
/// chunk completes it.
#[derive(Default)]
struct Utf8Stream {
    pending: Vec<u8>,
}

/// The longest UTF-8 sequence. A tail longer than this is not a split
/// character, so the decoder stops holding it back.
const MAX_UTF8_SEQUENCE: usize = 4;

impl Utf8Stream {
    fn push(&mut self, chunk: &[u8]) -> String {
        self.pending.extend_from_slice(chunk);
        let mut text = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(valid) => {
                    text.push_str(valid);
                    self.pending.clear();
                    return text;
                }
                Err(error) => {
                    let valid_up_to = error.valid_up_to();
                    text.push_str(&String::from_utf8_lossy(&self.pending[..valid_up_to]));
                    match error.error_len() {
                        // Invalid bytes: report one replacement and continue.
                        Some(len) => {
                            self.pending = self.pending.split_off(valid_up_to + len);
                            text.push('\u{fffd}');
                        }
                        // An incomplete sequence waits for the next chunk,
                        // unless it is already too long to be one.
                        None => {
                            self.pending = self.pending.split_off(valid_up_to);
                            if self.pending.len() > MAX_UTF8_SEQUENCE {
                                text.push('\u{fffd}');
                                self.pending.clear();
                            }
                            return text;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_stream_joins_split_characters() {
        let mut stream = Utf8Stream::default();
        let bytes = "héllo".as_bytes();
        assert_eq!(stream.push(&bytes[..2]), "h");
        assert_eq!(stream.push(&bytes[2..]), "éllo");
    }

    #[test]
    fn utf8_stream_replaces_invalid_bytes() {
        let mut stream = Utf8Stream::default();
        assert_eq!(stream.push(&[0x61, 0xff, 0x62]), "a\u{fffd}b");
    }

    /// A long run of continuation bytes must not accumulate forever.
    #[test]
    fn utf8_stream_gives_up_on_an_oversized_tail() {
        let mut stream = Utf8Stream::default();
        let text = stream.push(&[0xf0, 0x9f, 0x98, 0x80, 0xf0, 0x9f]);
        assert_eq!(text, "\u{1f600}");
        assert_eq!(stream.pending.len(), 2);
        let text = stream.push(&[0x98]);
        assert_eq!(text, "");
        let text = stream.push(&[0x80]);
        assert_eq!(text, "\u{1f600}");
    }

    #[test]
    fn client_messages_accept_only_input_and_resize() {
        let input: FlowClientMessage =
            serde_json::from_str(r#"{"type":"input","data":"ok"}"#).unwrap();
        assert!(matches!(input, FlowClientMessage::Input { .. }));
        let resize: FlowClientMessage =
            serde_json::from_str(r#"{"type":"resize","cols":100,"rows":30}"#).unwrap();
        assert!(matches!(
            resize,
            FlowClientMessage::Resize {
                cols: 100,
                rows: 30
            }
        ));
    }

    /// The socket messages are strict. Any field beyond the ones a variant
    /// defines, and any unknown type, is a rejection. A message therefore can
    /// never grow a command, argument, path, or environment surface.
    #[test]
    fn client_messages_reject_unknown_fields_and_types() {
        let rejected = [
            r#"{"type":"exec","command":"sh"}"#,
            r#"{"type":"input"}"#,
            r#"{"type":"input","data":"ok","command":"sh"}"#,
            r#"{"type":"input","data":"ok","args":["-c","sh"]}"#,
            r#"{"type":"input","data":"ok","cwd":"/etc"}"#,
            r#"{"type":"input","data":"ok","env":{"PATH":"/no"}}"#,
            r#"{"type":"resize","cols":100,"rows":30,"cwd":"/etc"}"#,
            r#"{"type":"resize","cols":100,"rows":30,"command":"sh"}"#,
        ];
        for message in rejected {
            assert!(
                serde_json::from_str::<FlowClientMessage>(message).is_err(),
                "the flow socket accepted {message}"
            );
        }
    }
}
