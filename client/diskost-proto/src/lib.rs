//! Meeting protocol shared by the desktop, mobile, and command-line clients.
//!
//! The C++ server speaks this JSON shape over a WebSocket. Field names are the
//! contract; see `docs/protocol.md`.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

/// The peer with the greater id creates the WebRTC offer.
pub fn is_offerer(self_id: &str, peer_id: &str) -> bool {
    self_id > peer_id
}

pub fn format_clock(unix_ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(unix_ms as i64)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "--:--:--".to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Participant {
    pub id: String,
    pub display_name: String,
    #[serde(default = "default_audio")]
    pub audio: bool,
    #[serde(default)]
    pub video: bool,
    #[serde(default)]
    pub screen: bool,
}

fn default_audio() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomSnapshot {
    pub room_id: String,
    pub room_name: String,
    pub self_id: String,
    pub participants: Vec<Participant>,
    #[serde(default)]
    pub ice_servers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    CreateRoom {
        req: u64,
        name: String,
        display_name: String,
        #[serde(default)]
        passcode: String,
    },
    JoinRoom {
        req: u64,
        room_id: String,
        display_name: String,
        #[serde(default)]
        passcode: String,
    },
    Leave {
        req: u64,
    },
    Chat {
        req: u64,
        text: String,
    },
    Signal {
        req: u64,
        to: String,
        payload: serde_json::Value,
    },
    MediaState {
        req: u64,
        audio: bool,
        video: bool,
        screen: bool,
    },
    Ping {
        req: u64,
        nonce: u64,
    },
}

impl ClientMessage {
    pub fn req(&self) -> u64 {
        match self {
            Self::CreateRoom { req, .. }
            | Self::JoinRoom { req, .. }
            | Self::Leave { req }
            | Self::Chat { req, .. }
            | Self::Signal { req, .. }
            | Self::MediaState { req, .. }
            | Self::Ping { req, .. } => *req,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    Joined {
        #[serde(default)]
        req: u64,
        room_id: String,
        room_name: String,
        self_id: String,
        #[serde(default)]
        created: bool,
        participants: Vec<Participant>,
        #[serde(default)]
        ice_servers: Vec<String>,
    },
    ParticipantJoined {
        participant: Participant,
    },
    ParticipantLeft {
        participant_id: String,
    },
    ChatMessage {
        from: String,
        display_name: String,
        text: String,
        unix_ms: u64,
    },
    Signal {
        from: String,
        payload: serde_json::Value,
    },
    MediaState {
        participant_id: String,
        audio: bool,
        video: bool,
        screen: bool,
    },
    Left {
        #[serde(default)]
        req: u64,
    },
    Pong {
        #[serde(default)]
        req: u64,
        nonce: u64,
    },
    Error {
        #[serde(default)]
        req: u64,
        code: String,
        message: String,
    },
}

impl ServerEvent {
    pub fn request_id(&self) -> u64 {
        match self {
            Self::Joined { req, .. } | Self::Left { req } | Self::Pong { req, .. } | Self::Error { req, .. } => {
                *req
            }
            _ => 0,
        }
    }

    /// Direct replies that complete a waiting client command.
    pub fn is_terminal_reply(&self) -> bool {
        matches!(self, Self::Joined { .. } | Self::Left { .. } | Self::Error { .. })
    }

    pub fn snapshot(&self) -> Option<RoomSnapshot> {
        match self {
            Self::Joined {
                room_id,
                room_name,
                self_id,
                participants,
                ice_servers,
                ..
            } => Some(RoomSnapshot {
                room_id: room_id.clone(),
                room_name: room_name.clone(),
                self_id: self_id.clone(),
                participants: participants.clone(),
                ice_servers: ice_servers.clone(),
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greater_id_offers() {
        assert!(is_offerer("p_b", "p_a"));
        assert!(!is_offerer("p_a", "p_b"));
        assert!(!is_offerer("p_a", "p_a"));
    }

    #[test]
    fn client_messages_use_the_server_field_names() {
        let message = ClientMessage::Chat {
            req: 7,
            text: "hi".into(),
        };
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["type"], "chat");
        assert_eq!(value["req"], 7);
        assert_eq!(value["text"], "hi");
    }

    #[test]
    fn parses_the_server_joined_shape() {
        let raw = r#"{
            "type":"joined",
            "req":1,
            "room_id":"AB12CD34",
            "room_name":"Daily",
            "self_id":"p0123456789",
            "created":true,
            "participants":[{
                "id":"p0123456789",
                "display_name":"Alice",
                "audio":true,
                "video":false,
                "screen":false
            }],
            "ice_servers":["stun:stun.l.google.com:19302"]
        }"#;
        let event: ServerEvent = serde_json::from_str(raw).unwrap();
        let room = event.snapshot().unwrap();
        assert_eq!(room.room_id, "AB12CD34");
        assert_eq!(room.participants.len(), 1);
        assert_eq!(event.request_id(), 1);
        assert!(event.is_terminal_reply());
    }
}
