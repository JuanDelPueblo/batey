use super::AppState;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Notify;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
enum ClientMessage {
    Subscribe {
        #[serde(default)]
        from_seq: u64,
    },
    Prompt {
        session_id: String,
        text: String,
    },
    Cancel {
        session_id: String,
    },
    PermissionResponse {
        session_id: String,
        id: String,
        option_id: String,
    },
}

pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (ws_sender, mut receiver) = socket.split();
    let ws_sender = Arc::new(tokio::sync::Mutex::new(ws_sender));

    let event_log = state.session_manager.event_log().clone();
    let mut event_rx = event_log.subscribe();
    let replay_ready = Arc::new(Notify::new());
    let replay_started = Arc::new(AtomicBool::new(false));
    let replay_seq = Arc::new(AtomicU64::new(0));

    let (action_tx, mut action_rx) = mpsc::channel::<ClientMessage>(32);

    // Send task: forward events to client
    let sender_clone = ws_sender.clone();
    let replay_ready_send = replay_ready.clone();
    let replay_started_send = replay_started.clone();
    let replay_seq_send = replay_seq.clone();
    let event_log_send = event_log.clone();
    let mut send_task = tokio::spawn(async move {
        loop {
            let notified = replay_ready_send.notified();
            if replay_started_send.load(Ordering::SeqCst) {
                break;
            }
            notified.await;
        }

        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    let last_replayed_seq = replay_seq_send.load(Ordering::SeqCst);
                    if event.seq <= last_replayed_seq {
                        continue;
                    }
                    replay_seq_send.store(event.seq, Ordering::SeqCst);
                    if let Ok(json) = serde_json::to_string(&event) {
                        let mut sender = sender_clone.lock().await;
                        if sender.send(Message::Text(json)).await.is_err() {
                            break;
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("WebSocket lagged by {} events", n);
                    let through = match event_log_send.high_watermark() {
                        Ok(seq) => seq,
                        Err(error) => {
                            let control = serde_json::json!({"type":"stream_error","code":"event_store_unavailable","error":error.to_string()});
                            let mut sender = sender_clone.lock().await;
                            let _ = sender.send(Message::Text(control.to_string())).await;
                            break;
                        }
                    };
                    let mut cursor = replay_seq_send.load(Ordering::SeqCst).saturating_add(1);
                    while cursor <= through {
                        let page = match event_log_send.replay_page(cursor, through, 512) {
                            Ok(page) => page,
                            Err(error) => {
                                let control = serde_json::json!({"type":"stream_error","code":"replay_failed","error":error.to_string()});
                                let mut sender = sender_clone.lock().await;
                                let _ = sender.send(Message::Text(control.to_string())).await;
                                return;
                            }
                        };
                        if page.is_empty() {
                            break;
                        }
                        for event in page {
                            cursor = event.seq.saturating_add(1);
                            replay_seq_send.store(event.seq, Ordering::SeqCst);
                            let mut sender = sender_clone.lock().await;
                            if sender
                                .send(Message::Text(serde_json::to_string(&event).unwrap()))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Recv task: parse client messages
    let action_tx_clone = action_tx.clone();
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            match msg {
                Message::Text(text) => {
                    if let Ok(cmd) = serde_json::from_str::<ClientMessage>(&text) {
                        if action_tx_clone.send(cmd).await.is_err() {
                            break;
                        }
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // Action task: handle client commands
    let sm = state.session_manager.clone();
    let sender_clone2 = ws_sender.clone();
    let replay_ready_action = replay_ready.clone();
    let replay_started_action = replay_started.clone();
    let replay_seq_action = replay_seq.clone();
    let mut action_task = tokio::spawn(async move {
        while let Some(cmd) = action_rx.recv().await {
            match cmd {
                ClientMessage::Subscribe { from_seq } => {
                    let mut sender = sender_clone2.lock().await;
                    let through = match event_log.high_watermark() {
                        Ok(seq) => seq,
                        Err(error) => {
                            let control = serde_json::json!({"type":"stream_error","code":"event_store_unavailable","error":error.to_string()});
                            let _ = sender.send(Message::Text(control.to_string())).await;
                            return;
                        }
                    };
                    // `from_seq == 0` is the fresh-browser handshake: capture
                    // the durable baseline and begin live delivery without
                    // replaying the global event table. A nonzero cursor is a
                    // reconnect and must catch up durably through this mark.
                    if from_seq > 0 {
                        let mut cursor = from_seq;
                        while cursor <= through {
                            let page = match event_log.replay_page(cursor, through, 512) {
                                Ok(page) => page,
                                Err(error) => {
                                    let control = serde_json::json!({"type":"stream_error","code":"replay_failed","error":error.to_string()});
                                    let _ = sender.send(Message::Text(control.to_string())).await;
                                    return;
                                }
                            };
                            if page.is_empty() {
                                break;
                            }
                            for event in page {
                                cursor = event.seq.saturating_add(1);
                                if sender
                                    .send(Message::Text(serde_json::to_string(&event).unwrap()))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                    }

                    replay_seq_action.store(through, Ordering::SeqCst);
                    let _ = sender
                        .send(Message::Text(
                            serde_json::json!({"type":"subscribed","through_seq":through})
                                .to_string(),
                        ))
                        .await;
                    replay_started_action.store(true, Ordering::SeqCst);
                    replay_ready_action.notify_waiters();
                }
                ClientMessage::PermissionResponse {
                    session_id,
                    id,
                    option_id,
                } => {
                    if let Some(session) = sm.get_by_id(&session_id).await {
                        session.respond_to_permission(&id, &option_id).await;
                    }
                }
                ClientMessage::Prompt { .. } | ClientMessage::Cancel { .. } => {
                    // Prompt/cancel via WebSocket is not supported; use REST API
                }
            }
        }
    });

    tokio::select! {
        _ = &mut send_task => {},
        _ = &mut recv_task => {},
        _ = &mut action_task => {},
    }
    send_task.abort();
    recv_task.abort();
    action_task.abort();
}
