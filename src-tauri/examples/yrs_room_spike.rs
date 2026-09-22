//! Phase 0 spike: minimal y-websocket room hosting on axum + yrs.
//!
//! Speaks the y-protocol (sync step1/step2/update + awareness) that
//! y-websocket clients use — same wire protocol as the platform's Python
//! implementation (mnemosyne-platform/app/hocuspocus/protocol.py).
//!
//! Note: yrs::sync::Awareness is !Send (Rc-based observers), so the server
//! holds the Doc directly and tracks awareness as merged AwarenessUpdate
//! entries (plain data, Send) — sufficient for room hosting, where the
//! server is a relay + late-joiner answerer, not an awareness participant.
//!
//! Run:    cargo run --example yrs_room_spike
//! Verify: pnpm exec tsx scripts/yrs-room-spike-client.mts   (in frontend/)
//!
//! This is the seed of the Phase 2 cell room host; auth (bearer subprotocol)
//! and persistence are intentionally out of scope for the spike.

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};
use yrs::sync::{AwarenessUpdate, Message, SyncMessage};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, ReadTxn, Transact, Update};

struct Room {
    doc: Mutex<Doc>,
    /// Latest awareness entry per client id, merged by clock.
    awareness: Mutex<HashMap<yrs::block::ClientID, yrs::sync::awareness::AwarenessUpdateEntry>>,
    tx: broadcast::Sender<Vec<u8>>,
}

#[derive(Default)]
struct Rooms(Mutex<HashMap<String, Arc<Room>>>);

impl Rooms {
    async fn get_or_create(&self, name: &str) -> Arc<Room> {
        let mut map = self.0.lock().await;
        map.entry(name.to_string())
            .or_insert_with(|| {
                let (tx, _) = broadcast::channel(256);
                Arc::new(Room {
                    doc: Mutex::new(Doc::new()),
                    awareness: Mutex::new(HashMap::new()),
                    tx,
                })
            })
            .clone()
    }
}

impl Room {
    async fn merge_awareness(&self, update: &AwarenessUpdate) {
        let mut state = self.awareness.lock().await;
        for (client, entry) in &update.clients {
            match state.get(client) {
                Some(existing) if existing.clock >= entry.clock => {}
                _ => {
                    state.insert(*client, entry.clone());
                }
            }
        }
    }

    async fn full_awareness(&self) -> Option<AwarenessUpdate> {
        let state = self.awareness.lock().await;
        if state.is_empty() {
            return None;
        }
        Some(AwarenessUpdate {
            clients: state.clone().into_iter().collect(),
        })
    }
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("SPIKE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8787);
    let app = Router::new()
        .route("/room/{name}", get(ws_handler))
        .with_state(Arc::new(Rooms::default()));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind spike port");
    println!("yrs room spike listening on ws://127.0.0.1:{port}/room/{{name}}");
    axum::serve(listener, app).await.expect("serve");
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Path(name): Path<String>,
    State(rooms): State<Arc<Rooms>>,
) -> Response {
    let room = rooms.get_or_create(&name).await;
    ws.on_upgrade(move |socket| handle_socket(socket, room))
}

async fn handle_socket(socket: WebSocket, room: Arc<Room>) {
    let (mut sink, mut stream) = socket.split();
    let mut rx = room.tx.subscribe();

    // Server initiates sync: step1 with our state vector, then current awareness.
    {
        let doc = room.doc.lock().await;
        let sv = doc.transact().state_vector();
        let step1 = Message::Sync(SyncMessage::SyncStep1(sv)).encode_v1();
        if sink.send(WsMessage::binary(step1)).await.is_err() {
            return;
        }
    }
    if let Some(update) = room.full_awareness().await {
        let msg = Message::Awareness(update).encode_v1();
        let _ = sink.send(WsMessage::binary(msg)).await;
    }

    // Channel for direct (non-broadcast) replies to this client.
    let (direct_tx, mut direct_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

    let forward = tokio::spawn(async move {
        loop {
            tokio::select! {
                broadcasted = rx.recv() => {
                    let Ok(payload) = broadcasted else { break };
                    if sink.send(WsMessage::binary(payload)).await.is_err() {
                        break;
                    }
                }
                direct = direct_rx.recv() => {
                    let Some(payload) = direct else { break };
                    if sink.send(WsMessage::binary(payload)).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    while let Some(Ok(frame)) = stream.next().await {
        let data = match frame {
            WsMessage::Binary(data) => data,
            WsMessage::Close(_) => break,
            _ => continue,
        };
        let Ok(message) = Message::decode_v1(&data) else {
            continue;
        };
        match message {
            Message::Sync(SyncMessage::SyncStep1(sv)) => {
                let doc = room.doc.lock().await;
                let update = doc.transact().encode_state_as_update_v1(&sv);
                drop(doc);
                let reply = Message::Sync(SyncMessage::SyncStep2(update)).encode_v1();
                let _ = direct_tx.send(reply).await;
            }
            Message::Sync(SyncMessage::SyncStep2(update))
            | Message::Sync(SyncMessage::Update(update)) => {
                let applied = {
                    let doc = room.doc.lock().await;
                    let mut txn = doc.transact_mut();
                    Update::decode_v1(&update)
                        .ok()
                        .and_then(|decoded| txn.apply_update(decoded).ok())
                        .is_some()
                };
                if applied {
                    let rebroadcast =
                        Message::Sync(SyncMessage::Update(update.clone())).encode_v1();
                    let _ = room.tx.send(rebroadcast);
                }
            }
            Message::Awareness(update) => {
                room.merge_awareness(&update).await;
                let rebroadcast = Message::Awareness(update).encode_v1();
                let _ = room.tx.send(rebroadcast);
            }
            Message::AwarenessQuery => {
                if let Some(update) = room.full_awareness().await {
                    let _ = direct_tx.send(Message::Awareness(update).encode_v1()).await;
                }
            }
            Message::Auth(_) | Message::Custom(..) => {}
        }
    }

    forward.abort();
}
