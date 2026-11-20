//! Small-room WebRTC mesh.
//!
//! Each pair of participants opens one peer connection. The participant with
//! the greater id creates the offer, so the two sides never glare. Audio and
//! video presence is carried by the signaling server; this mesh links a
//! `diskost` data channel so desktop peers can see that media connectivity
//! actually came up. Encoded microphone or camera frames can be attached to
//! the same [`RTCPeerConnection`](webrtc::peer_connection::RTCPeerConnection).

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::Arc;

use diskost_core::MeetingClient;
use diskost_proto::{is_offerer, ServerEvent};
use serde_json::{json, Value};
use tokio::sync::{broadcast, oneshot};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

pub struct LinkUpdate {
    pub peer_id: String,
    pub state: String,
}

struct Peer {
    pc: Arc<RTCPeerConnection>,
    remote_ready: bool,
    pending_ice: Vec<RTCIceCandidateInit>,
}

pub async fn run(
    client: MeetingClient,
    mut events: broadcast::Receiver<ServerEvent>,
    status_tx: Sender<LinkUpdate>,
    mut stop: oneshot::Receiver<()>,
) -> Result<(), String> {
    let api = build_api().await?;
    let mut mesh = Mesh {
        api,
        client,
        status_tx,
        self_id: String::new(),
        ice_servers: Vec::new(),
        peers: HashMap::new(),
        early_ice: HashMap::new(),
    };

    loop {
        tokio::select! {
            _ = &mut stop => break,
            incoming = events.recv() => {
                match incoming {
                    Ok(event) => mesh.on_event(event).await,
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        log::warn!("mesh skipped {skipped} signaling events");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    for (_, peer) in mesh.peers.drain() {
        let _ = peer.pc.close().await;
    }
    Ok(())
}

struct Mesh {
    api: webrtc::api::API,
    client: MeetingClient,
    status_tx: Sender<LinkUpdate>,
    self_id: String,
    ice_servers: Vec<String>,
    peers: HashMap<String, Peer>,
    early_ice: HashMap<String, Vec<RTCIceCandidateInit>>,
}

impl Mesh {
    async fn on_event(&mut self, event: ServerEvent) {
        match event {
            ServerEvent::Joined {
                self_id,
                participants,
                ice_servers,
                ..
            } => {
                if !self.self_id.is_empty() {
                    return;
                }
                self.self_id = self_id;
                self.ice_servers = ice_servers;
                let peers: Vec<String> = participants
                    .into_iter()
                    .map(|person| person.id)
                    .filter(|id| id != &self.self_id)
                    .collect();
                for peer_id in peers {
                    self.ensure_offer(&peer_id).await;
                }
            }
            ServerEvent::ParticipantJoined { participant } => {
                self.ensure_offer(&participant.id).await;
            }
            ServerEvent::ParticipantLeft { participant_id } => {
                self.early_ice.remove(&participant_id);
                if let Some(peer) = self.peers.remove(&participant_id) {
                    let _ = peer.pc.close().await;
                }
                self.report(&participant_id, "closed");
            }
            ServerEvent::Signal { from, payload } => self.on_signal(&from, &payload).await,
            _ => {}
        }
    }

    async fn ensure_offer(&mut self, peer_id: &str) {
        if self.self_id.is_empty() || peer_id == self.self_id {
            return;
        }
        if !is_offerer(&self.self_id, peer_id) || self.peers.contains_key(peer_id) {
            return;
        }
        if let Err(err) = self.start_offer(peer_id).await {
            log::warn!("offer to {peer_id}: {err}");
            self.report(peer_id, "failed");
        }
    }

    async fn start_offer(&mut self, peer_id: &str) -> Result<(), String> {
        self.report(peer_id, "connecting");
        let pc = self.new_peer(peer_id).await?;
        let dc = pc
            .create_data_channel("diskost", None)
            .await
            .map_err(|err| err.to_string())?;
        watch_channel(dc, peer_id.to_owned(), self.status_tx.clone());
        self.insert_peer(peer_id, Arc::clone(&pc));

        let offer = pc.create_offer(None).await.map_err(|err| err.to_string())?;
        pc.set_local_description(offer)
            .await
            .map_err(|err| err.to_string())?;
        let local = pc
            .local_description()
            .await
            .ok_or_else(|| "local description missing".to_owned())?;
        self.client
            .signal(peer_id, json!({ "kind": "offer", "sdp": local.sdp }))
            .map_err(|err| err.to_string())?;
        Ok(())
    }

    async fn on_signal(&mut self, from: &str, payload: &Value) {
        let kind = payload.get("kind").and_then(Value::as_str).unwrap_or("");
        let result = match kind {
            "offer" => self.accept_offer(from, payload).await,
            "answer" => self.accept_answer(from, payload).await,
            "ice" => {
                self.add_ice(from, ice_init(payload)).await;
                Ok(())
            }
            other => {
                log::debug!("ignore signal kind {other}");
                Ok(())
            }
        };
        if let Err(err) = result {
            log::warn!("signal from {from}: {err}");
            self.report(from, "failed");
        }
    }

    async fn accept_offer(&mut self, peer_id: &str, payload: &Value) -> Result<(), String> {
        if is_offerer(&self.self_id, peer_id) {
            log::warn!("ignore glare offer from {peer_id}");
            return Ok(());
        }
        self.report(peer_id, "connecting");
        let pc = if let Some(peer) = self.peers.get(peer_id) {
            Arc::clone(&peer.pc)
        } else {
            let pc = self.new_peer(peer_id).await?;
            self.insert_peer(peer_id, Arc::clone(&pc));
            pc
        };
        let sdp = payload.get("sdp").and_then(Value::as_str).unwrap_or("");
        let offer = session_description("offer", sdp)?;
        pc.set_remote_description(offer)
            .await
            .map_err(|err| err.to_string())?;
        self.flush_ice(peer_id).await;
        let answer = pc.create_answer(None).await.map_err(|err| err.to_string())?;
        pc.set_local_description(answer)
            .await
            .map_err(|err| err.to_string())?;
        let local = pc
            .local_description()
            .await
            .ok_or_else(|| "local description missing".to_owned())?;
        self.client
            .signal(peer_id, json!({ "kind": "answer", "sdp": local.sdp }))
            .map_err(|err| err.to_string())?;
        Ok(())
    }

    async fn accept_answer(&mut self, peer_id: &str, payload: &Value) -> Result<(), String> {
        let pc = self
            .peers
            .get(peer_id)
            .map(|peer| Arc::clone(&peer.pc))
            .ok_or_else(|| format!("answer from unknown peer {peer_id}"))?;
        let sdp = payload.get("sdp").and_then(Value::as_str).unwrap_or("");
        let answer = session_description("answer", sdp)?;
        pc.set_remote_description(answer)
            .await
            .map_err(|err| err.to_string())?;
        self.flush_ice(peer_id).await;
        Ok(())
    }

    async fn add_ice(&mut self, peer_id: &str, init: RTCIceCandidateInit) {
        let ready = if let Some(peer) = self.peers.get_mut(peer_id) {
            if peer.remote_ready {
                Some(Arc::clone(&peer.pc))
            } else {
                peer.pending_ice.push(init);
                return;
            }
        } else {
            self.early_ice.entry(peer_id.to_owned()).or_default().push(init);
            return;
        };
        if let Some(pc) = ready {
            if let Err(err) = pc.add_ice_candidate(init).await {
                log::warn!("ice from {peer_id}: {err}");
            }
        }
    }

    async fn flush_ice(&mut self, peer_id: &str) {
        let Some((pc, queued)) = self.peers.get_mut(peer_id).map(|peer| {
            peer.remote_ready = true;
            (Arc::clone(&peer.pc), std::mem::take(&mut peer.pending_ice))
        }) else {
            return;
        };
        for init in queued {
            if let Err(err) = pc.add_ice_candidate(init).await {
                log::warn!("queued ice from {peer_id}: {err}");
            }
        }
    }

    async fn new_peer(&self, peer_id: &str) -> Result<Arc<RTCPeerConnection>, String> {
        let pc = self
            .api
            .new_peer_connection(self.configuration())
            .await
            .map_err(|err| err.to_string())?;
        let pc = Arc::new(pc);
        let client = self.client.clone();
        let remote = peer_id.to_owned();
        pc.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
            let client = client.clone();
            let remote = remote.clone();
            Box::pin(async move {
                let Some(candidate) = candidate else {
                    return;
                };
                let Ok(init) = candidate.to_json() else {
                    return;
                };
                let payload = json!({
                    "kind": "ice",
                    "candidate": init.candidate,
                    "sdp_mid": init.sdp_mid,
                    "sdp_mline_index": init.sdp_mline_index,
                });
                if let Err(err) = client.signal(&remote, payload) {
                    log::warn!("send ice: {err}");
                }
            })
        }));

        let status = self.status_tx.clone();
        let remote = peer_id.to_owned();
        pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
            let status = status.clone();
            let remote = remote.clone();
            let label = state.to_string();
            Box::pin(async move {
                let _ = status.send(LinkUpdate {
                    peer_id: remote,
                    state: label,
                });
            })
        }));

        let status = self.status_tx.clone();
        let remote = peer_id.to_owned();
        pc.on_data_channel(Box::new(move |channel: Arc<RTCDataChannel>| {
            watch_channel(channel, remote.clone(), status.clone());
            Box::pin(async {})
        }));
        Ok(pc)
    }

    fn insert_peer(&mut self, peer_id: &str, pc: Arc<RTCPeerConnection>) {
        let pending = self.early_ice.remove(peer_id).unwrap_or_default();
        self.peers.insert(
            peer_id.to_owned(),
            Peer {
                pc,
                remote_ready: false,
                pending_ice: pending,
            },
        );
    }

    fn configuration(&self) -> RTCConfiguration {
        let urls = if self.ice_servers.is_empty() {
            vec!["stun:stun.l.google.com:19302".to_owned()]
        } else {
            self.ice_servers.clone()
        };
        RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn report(&self, peer_id: &str, state: &str) {
        let _ = self.status_tx.send(LinkUpdate {
            peer_id: peer_id.to_owned(),
            state: state.to_owned(),
        });
    }
}

async fn build_api() -> Result<webrtc::api::API, String> {
    let mut media = MediaEngine::default();
    media
        .register_default_codecs()
        .map_err(|err| err.to_string())?;
    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media).map_err(|err| err.to_string())?;
    Ok(APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .build())
}

fn session_description(kind: &str, sdp: &str) -> Result<RTCSessionDescription, String> {
    serde_json::from_value(json!({ "type": kind, "sdp": sdp })).map_err(|err| err.to_string())
}

fn ice_init(payload: &Value) -> RTCIceCandidateInit {
    RTCIceCandidateInit {
        candidate: payload
            .get("candidate")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        sdp_mid: payload
            .get("sdp_mid")
            .and_then(Value::as_str)
            .map(str::to_owned),
        sdp_mline_index: payload
            .get("sdp_mline_index")
            .and_then(Value::as_u64)
            .map(|index| index as u16),
        ..Default::default()
    }
}

fn watch_channel(channel: Arc<RTCDataChannel>, peer_id: String, status: Sender<LinkUpdate>) {
    let opened = Arc::clone(&channel);
    let open_peer = peer_id.clone();
    let open_status = status;
    channel.on_open(Box::new(move || {
        let opened = Arc::clone(&opened);
        let open_peer = open_peer.clone();
        let open_status = open_status.clone();
        Box::pin(async move {
            if let Err(err) = opened.send_text("hello").await {
                log::warn!("data channel hello: {err}");
            }
            let _ = open_status.send(LinkUpdate {
                peer_id: open_peer,
                state: "linked".into(),
            });
        })
    }));
    channel.on_message(Box::new(move |message: DataChannelMessage| {
        log::debug!(
            "data channel from {peer_id}: {}",
            String::from_utf8_lossy(&message.data)
        );
        Box::pin(async {})
    }));
}
