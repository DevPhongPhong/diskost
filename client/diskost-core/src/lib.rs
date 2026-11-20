//! Async client for the Diskost signaling server.
//!
//! One [`MeetingClient`] can be cloned freely. Clones share the socket.
//! The last clone aborts the read loop.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use diskost_proto::{ClientMessage, RoomSnapshot, ServerEvent};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("socket: {0}")]
    Socket(String),
    #[error("{0}")]
    Server(String),
    #[error("timed out waiting for the server")]
    Timeout,
    #[error("connection closed")]
    Closed,
    #[error("another request is already in flight")]
    Busy,
    #[error("{0}")]
    Protocol(String),
}

pub type Result<T> = std::result::Result<T, ClientError>;

enum Outbound {
    Text(String),
    Pong(Vec<u8>),
}

struct Pending {
    req: u64,
    tx: oneshot::Sender<std::result::Result<ServerEvent, String>>,
}

struct Shared {
    out_tx: mpsc::UnboundedSender<Outbound>,
    events: broadcast::Sender<ServerEvent>,
    waiter: Mutex<Option<Pending>>,
    room: Mutex<Option<RoomSnapshot>>,
    next_req: AtomicU64,
    alive: AtomicBool,
}

struct Tasks {
    handles: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Drop for Tasks {
    fn drop(&mut self) {
        if let Ok(handles) = self.handles.lock() {
            for handle in handles.iter() {
                handle.abort();
            }
        }
    }
}

#[derive(Clone)]
pub struct MeetingClient {
    shared: Arc<Shared>,
    /// Aborts the socket tasks when the last client clone is dropped.
    _tasks: Arc<Tasks>,
}

impl MeetingClient {
    pub async fn connect(url: &str) -> Result<Self> {
        let (socket, _) = connect_async(url)
            .await
            .map_err(|err| ClientError::Socket(err.to_string()))?;
        let (mut write, mut read) = socket.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(256);
        let shared = Arc::new(Shared {
            out_tx,
            events,
            waiter: Mutex::new(None),
            room: Mutex::new(None),
            next_req: AtomicU64::new(1),
            alive: AtomicBool::new(true),
        });

        let writer_flag = Arc::clone(&shared);
        let writer = tokio::spawn(async move {
            while let Some(outbound) = out_rx.recv().await {
                let sent = match outbound {
                    Outbound::Text(text) => write.send(Message::Text(text.into())).await,
                    Outbound::Pong(data) => write.send(Message::Pong(data.into())).await,
                };
                if sent.is_err() {
                    writer_flag.alive.store(false, Ordering::SeqCst);
                    break;
                }
            }
        });

        let reader_shared = Arc::clone(&shared);
        let reader_tx = shared.out_tx.clone();
        let reader = tokio::spawn(async move {
            loop {
                match read.next().await {
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = reader_tx.send(Outbound::Pong(payload.to_vec()));
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(err)) => {
                        log::warn!("socket read: {err}");
                        break;
                    }
                    Some(Ok(other)) => {
                        if let Ok(text) = other.to_text() {
                            on_text(&reader_shared, text);
                        }
                    }
                }
            }
            reader_shared.alive.store(false, Ordering::SeqCst);
            let event = ServerEvent::Error {
                req: 0,
                code: "disconnected".into(),
                message: "connection closed".into(),
            };
            fail_waiter(&reader_shared, "connection closed");
            let _ = reader_shared.events.send(event);
        });

        let tasks = Arc::new(Tasks {
            handles: Mutex::new(vec![writer, reader]),
        });
        log::info!("connected to {url}");
        Ok(Self { shared, _tasks: tasks })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.shared.events.subscribe()
    }

    pub fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::SeqCst)
    }

    pub fn room(&self) -> Option<RoomSnapshot> {
        lock(&self.shared.room).clone()
    }

    pub async fn create_room(
        &self,
        name: &str,
        display_name: &str,
        passcode: &str,
    ) -> Result<RoomSnapshot> {
        let event = self
            .roundtrip(ClientMessage::CreateRoom {
                req: self.alloc_req(),
                name: name.to_owned(),
                display_name: display_name.to_owned(),
                passcode: passcode.to_owned(),
            })
            .await?;
        event
            .snapshot()
            .ok_or_else(|| ClientError::Protocol(format!("expected joined, got {event:?}")))
    }

    pub async fn join_room(
        &self,
        room_id: &str,
        display_name: &str,
        passcode: &str,
    ) -> Result<RoomSnapshot> {
        let event = self
            .roundtrip(ClientMessage::JoinRoom {
                req: self.alloc_req(),
                room_id: room_id.to_owned(),
                display_name: display_name.to_owned(),
                passcode: passcode.to_owned(),
            })
            .await?;
        event
            .snapshot()
            .ok_or_else(|| ClientError::Protocol(format!("expected joined, got {event:?}")))
    }

    pub async fn leave(&self) -> Result<()> {
        let event = self
            .roundtrip(ClientMessage::Leave { req: self.alloc_req() })
            .await?;
        match event {
            ServerEvent::Left { .. } => Ok(()),
            ServerEvent::Error { code, message, .. } => Err(ClientError::Server(format!("{code}: {message}"))),
            other => Err(ClientError::Protocol(format!("expected left, got {other:?}"))),
        }
    }

    pub fn chat(&self, text: &str) -> Result<()> {
        self.enqueue(&ClientMessage::Chat {
            req: self.alloc_req(),
            text: text.to_owned(),
        })
    }

    pub fn set_media(&self, audio: bool, video: bool, screen: bool) -> Result<()> {
        self.enqueue(&ClientMessage::MediaState {
            req: self.alloc_req(),
            audio,
            video,
            screen,
        })
    }

    pub fn signal(&self, to: &str, payload: serde_json::Value) -> Result<()> {
        self.enqueue(&ClientMessage::Signal {
            req: self.alloc_req(),
            to: to.to_owned(),
            payload,
        })
    }

    fn alloc_req(&self) -> u64 {
        self.shared.next_req.fetch_add(1, Ordering::Relaxed)
    }

    fn enqueue(&self, message: &ClientMessage) -> Result<()> {
        if !self.is_alive() {
            return Err(ClientError::Closed);
        }
        let text = serde_json::to_string(message).map_err(|err| ClientError::Protocol(err.to_string()))?;
        self.shared
            .out_tx
            .send(Outbound::Text(text))
            .map_err(|_| ClientError::Closed)
    }

    async fn roundtrip(&self, message: ClientMessage) -> Result<ServerEvent> {
        let req = message.req();
        let (tx, rx) = oneshot::channel();
        {
            let mut slot = lock(&self.shared.waiter);
            if slot.is_some() {
                return Err(ClientError::Busy);
            }
            *slot = Some(Pending { req, tx });
        }
        if let Err(err) = self.enqueue(&message) {
            clear_waiter(&self.shared, req);
            return Err(err);
        }
        match timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(Ok(event))) => Ok(event),
            Ok(Ok(Err(text))) => Err(ClientError::Server(text)),
            Ok(Err(_)) => Err(ClientError::Closed),
            Err(_) => {
                clear_waiter(&self.shared, req);
                Err(ClientError::Timeout)
            }
        }
    }
}

fn on_text(shared: &Shared, text: &str) {
    let event = match serde_json::from_str::<ServerEvent>(text) {
        Ok(event) => event,
        Err(err) => {
            log::warn!("ignored server message: {err}");
            return;
        }
    };
    apply_room(&shared.room, &event);
    if event.is_terminal_reply() {
        let req = event.request_id();
        let mut slot = lock(&shared.waiter);
        if slot.as_ref().map(|pending| pending.req) == Some(req) && req != 0 {
            if let Some(pending) = slot.take() {
                let result = match &event {
                    ServerEvent::Error { code, message, .. } => Err(format!("{code}: {message}")),
                    other => Ok(other.clone()),
                };
                let _ = pending.tx.send(result);
            }
        }
    }
    let _ = shared.events.send(event);
}

fn apply_room(slot: &Mutex<Option<RoomSnapshot>>, event: &ServerEvent) {
    let mut room = lock(slot);
    match event {
        ServerEvent::Joined { .. } => *room = event.snapshot(),
        ServerEvent::ParticipantJoined { participant } => {
            if let Some(room) = room.as_mut() {
                if !room.participants.iter().any(|person| person.id == participant.id) {
                    room.participants.push(participant.clone());
                }
            }
        }
        ServerEvent::ParticipantLeft { participant_id } => {
            if let Some(room) = room.as_mut() {
                room.participants.retain(|person| &person.id != participant_id);
            }
        }
        ServerEvent::MediaState {
            participant_id,
            audio,
            video,
            screen,
        } => {
            if let Some(room) = room.as_mut() {
                if let Some(person) = room
                    .participants
                    .iter_mut()
                    .find(|person| &person.id == participant_id)
                {
                    person.audio = *audio;
                    person.video = *video;
                    person.screen = *screen;
                }
            }
        }
        ServerEvent::Left { .. } => *room = None,
        _ => {}
    }
}

fn fail_waiter(shared: &Shared, message: &str) {
    if let Some(pending) = lock(&shared.waiter).take() {
        let _ = pending.tx.send(Err(message.to_owned()));
    }
}

fn clear_waiter(shared: &Shared, req: u64) {
    let mut slot = lock(&shared.waiter);
    if slot.as_ref().map(|pending| pending.req) == Some(req) {
        *slot = None;
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}
