//! Native desktop client. The window is created by winit and painted by egui
//! through glow (OpenGL). There is no WebView and no HTML/CSS/JS.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use diskost_core::MeetingClient;
use eframe::egui;
use diskost_media::LinkUpdate;
use diskost_proto::{format_clock, Participant, RoomSnapshot, ServerEvent};
use tokio::sync::{broadcast, oneshot};

fn main() -> eframe::Result<()> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([860.0, 560.0])
            .with_title("Diskost"),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "Diskost",
        options,
        Box::new(|cc| Ok(Box::new(MeetingApp::new(cc)))),
    )
}

struct ChatLine {
    when: String,
    author: String,
    text: String,
}

struct LiveSession {
    client: MeetingClient,
    events: broadcast::Receiver<ServerEvent>,
    stop: Option<oneshot::Sender<()>>,
    room: RoomSnapshot,
}

enum JoinOutcome {
    Ready(LiveSession),
    Failed(String),
}

struct MeetingApp {
    runtime: tokio::runtime::Runtime,
    server_url: String,
    display_name: String,
    room_name: String,
    room_id_input: String,
    passcode: String,
    status: String,
    session: Option<LiveSession>,
    pending: Option<Receiver<JoinOutcome>>,
    links: Receiver<LinkUpdate>,
    link_tx: std::sync::mpsc::Sender<LinkUpdate>,
    link_state: HashMap<String, String>,
    chat: Vec<ChatLine>,
    draft: String,
    audio: bool,
    video: bool,
    screen: bool,
}

impl MeetingApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let (link_tx, links) = mpsc::channel();
        Self {
            runtime: tokio::runtime::Runtime::new().expect("tokio runtime"),
            server_url: "ws://127.0.0.1:7880/ws".into(),
            display_name: String::new(),
            room_name: "Meeting".into(),
            room_id_input: String::new(),
            passcode: String::new(),
            status: "Enter a display name and create or join a room.".into(),
            session: None,
            pending: None,
            links,
            link_tx,
            link_state: HashMap::new(),
            chat: Vec::new(),
            draft: String::new(),
            audio: true,
            video: false,
            screen: false,
        }
    }

    fn start(&mut self, ctx: &egui::Context, create: bool) {
        if self.pending.is_some() {
            return;
        }
        let name = self.display_name.trim().to_owned();
        if name.is_empty() {
            self.status = "Display name is required.".into();
            return;
        }
        if !create && self.room_id_input.trim().is_empty() {
            self.status = "Room code is required to join.".into();
            return;
        }
        self.status = "Connecting…".into();
        let (tx, rx) = mpsc::channel();
        self.pending = Some(rx);
        let url = self.server_url.trim().to_owned();
        let room_name = self.room_name.trim().to_owned();
        let room_id = self.room_id_input.trim().to_owned();
        let passcode = self.passcode.clone();
        let link_tx = self.link_tx.clone();
        let ctx = ctx.clone();
        self.runtime.spawn(async move {
            let outcome = connect_session(url, name, room_name, room_id, passcode, create, link_tx).await;
            let _ = tx.send(outcome);
            ctx.request_repaint();
        });
    }

    fn leave(&mut self) {
        if let Some(session) = self.session.take() {
            if let Some(stop) = session.stop {
                let _ = stop.send(());
            }
            let client = session.client;
            self.runtime.spawn(async move {
                let _ = client.leave().await;
            });
        }
        self.chat.clear();
        self.link_state.clear();
        self.draft.clear();
        self.audio = true;
        self.video = false;
        self.screen = false;
        self.status = "Left the meeting.".into();
    }

    fn push_media(&self) {
        let Some(session) = &self.session else { return };
        let client = session.client.clone();
        let audio = self.audio;
        let video = self.video;
        let screen = self.screen;
        self.runtime.spawn(async move {
            if let Err(err) = client.set_media(audio, video, screen) {
                log::warn!("media state: {err}");
            }
        });
    }

    fn send_chat(&mut self) {
        let text = self.draft.trim().to_owned();
        if text.is_empty() {
            return;
        }
        let Some(session) = &self.session else { return };
        let client = session.client.clone();
        self.draft.clear();
        self.runtime.spawn(async move {
            if let Err(err) = client.chat(&text) {
                log::warn!("chat: {err}");
            }
        });
    }

    fn poll(&mut self) {
        if let Some(pending) = &self.pending {
            match pending.try_recv() {
                Ok(JoinOutcome::Ready(session)) => {
                    self.status = format!("In {} · code {}", session.room.room_name, session.room.room_id);
                    self.session = Some(session);
                    self.pending = None;
                }
                Ok(JoinOutcome::Failed(message)) => {
                    self.status = message;
                    self.pending = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.status = "The connect task stopped.".into();
                    self.pending = None;
                }
            }
        }
        while let Ok(update) = self.links.try_recv() {
            let keep_linked = self.link_state.get(&update.peer_id).map(String::as_str) == Some("linked")
                && update.state != "failed"
                && update.state != "closed"
                && update.state != "disconnected";
            if !keep_linked {
                self.link_state.insert(update.peer_id, update.state);
            }
        }
        let Some(session) = &mut self.session else { return };
        loop {
            match session.events.try_recv() {
                Ok(event) => apply_event(session, &mut self.chat, &mut self.status, event),
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Closed) => {
                    self.status = "Disconnected from the server.".into();
                    break;
                }
            }
        }
    }
}

fn apply_event(session: &mut LiveSession, chat: &mut Vec<ChatLine>, status: &mut String, event: ServerEvent) {
    match event {
        ServerEvent::ParticipantJoined { participant } => {
            if !session.room.participants.iter().any(|person| person.id == participant.id) {
                session.room.participants.push(participant);
            }
        }
        ServerEvent::ParticipantLeft { participant_id } => {
            session.room.participants.retain(|person| person.id != participant_id);
        }
        ServerEvent::ChatMessage {
            display_name,
            text,
            unix_ms,
            ..
        } => chat.push(ChatLine {
            when: format_clock(unix_ms),
            author: display_name,
            text,
        }),
        ServerEvent::MediaState {
            participant_id,
            audio,
            video,
            screen,
        } => {
            if let Some(person) = session
                .room
                .participants
                .iter_mut()
                .find(|person| person.id == participant_id)
            {
                person.audio = audio;
                person.video = video;
                person.screen = screen;
            }
        }
        ServerEvent::Error { message, code, req } if req == 0 => {
            *status = format!("{code}: {message}");
        }
        _ => {}
    }
}

async fn connect_session(
    url: String,
    name: String,
    room_name: String,
    room_id: String,
    passcode: String,
    create: bool,
    link_tx: std::sync::mpsc::Sender<LinkUpdate>,
) -> JoinOutcome {
    let client = match MeetingClient::connect(&url).await {
        Ok(client) => client,
        Err(err) => return JoinOutcome::Failed(err.to_string()),
    };
    let events = client.subscribe();
    let mesh_events = client.subscribe();
    let (stop_tx, stop_rx) = oneshot::channel();
    let mesh_client = client.clone();
    tokio::spawn(async move {
        if let Err(err) = diskost_media::run(mesh_client, mesh_events, link_tx, stop_rx).await {
            log::error!("media mesh: {err}");
        }
    });
    let joined = if create {
        client.create_room(&room_name, &name, &passcode).await
    } else {
        client.join_room(&room_id, &name, &passcode).await
    };
    match joined {
        Ok(room) => JoinOutcome::Ready(LiveSession {
            client,
            events,
            stop: Some(stop_tx),
            room,
        }),
        Err(err) => {
            let _ = stop_tx.send(());
            JoinOutcome::Failed(err.to_string())
        }
    }
}

impl eframe::App for MeetingApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        if self.pending.is_some() || self.session.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if self.session.is_none() {
            self.lobby(ctx);
        } else {
            self.meeting(ctx);
        }
    }
}

impl MeetingApp {
    fn lobby(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(48.0);
                ui.heading("Diskost");
                ui.label("Small online meetings. The server relays the room; peers link directly.");
                ui.add_space(16.0);
            });
            egui::Grid::new("lobby")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Server");
                    ui.add(egui::TextEdit::singleline(&mut self.server_url).desired_width(420.0));
                    ui.end_row();
                    ui.label("Your name");
                    ui.add(egui::TextEdit::singleline(&mut self.display_name).desired_width(420.0));
                    ui.end_row();
                    ui.label("Room name");
                    ui.add(egui::TextEdit::singleline(&mut self.room_name).desired_width(420.0));
                    ui.end_row();
                    ui.label("Room code");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.room_id_input)
                            .desired_width(420.0)
                            .hint_text("only needed when joining"),
                    );
                    ui.end_row();
                    ui.label("Passcode");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.passcode)
                            .password(true)
                            .desired_width(420.0)
                            .hint_text("optional"),
                    );
                    ui.end_row();
                });
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let busy = self.pending.is_some();
                if ui.add_enabled(!busy, egui::Button::new("Create room")).clicked() {
                    self.start(ctx, true);
                }
                if ui.add_enabled(!busy, egui::Button::new("Join room")).clicked() {
                    self.start(ctx, false);
                }
            });
            ui.add_space(12.0);
            ui.label(&self.status);
        });
    }

    fn meeting(&mut self, ctx: &egui::Context) {
        let room_id = self.session.as_ref().map(|session| session.room.room_id.clone()).unwrap_or_default();
        let room_name = self
            .session
            .as_ref()
            .map(|session| session.room.room_name.clone())
            .unwrap_or_default();
        let self_id = self.session.as_ref().map(|session| session.room.self_id.clone()).unwrap_or_default();
        let participants = self
            .session
            .as_ref()
            .map(|session| session.room.participants.clone())
            .unwrap_or_default();

        egui::TopBottomPanel::top("bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading(&room_name);
                ui.label(format!("code {room_id}"));
                if ui.button("Copy code").clicked() {
                    ui.ctx().copy_text(room_id.clone());
                    self.status = "Room code copied.".into();
                }
                ui.separator();
                let mut audio = self.audio;
                let mut video = self.video;
                let mut screen = self.screen;
                let mut changed = false;
                changed |= ui.checkbox(&mut audio, "Mic").changed();
                changed |= ui.checkbox(&mut video, "Camera").changed();
                changed |= ui.checkbox(&mut screen, "Screen").changed();
                if changed {
                    self.audio = audio;
                    self.video = video;
                    self.screen = screen;
                    self.push_media();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Leave").clicked() {
                        self.leave();
                    }
                });
            });
            ui.label(&self.status);
        });

        if self.session.is_none() {
            return;
        }

        egui::SidePanel::left("roster")
            .resizable(true)
            .default_width(250.0)
            .show(ctx, |ui| {
                ui.heading("People");
                ui.separator();
                for person in &participants {
                    person_row(ui, person, person.id == self_id, self.link_state.get(&person.id));
                }
            });

        egui::TopBottomPanel::bottom("composer").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.draft)
                        .desired_width(ui.available_width() - 80.0)
                        .hint_text("Message the room"),
                );
                let submit = ui.button("Send").clicked()
                    || (response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)));
                if submit {
                    self.send_chat();
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                if self.chat.is_empty() {
                    ui.label("No messages yet.");
                }
                for line in &self.chat {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&line.when).weak());
                        ui.label(egui::RichText::new(&line.author).strong());
                    });
                    ui.label(&line.text);
                    ui.add_space(6.0);
                }
            });
        });
    }
}

fn person_row(ui: &mut egui::Ui, person: &Participant, is_self: bool, link: Option<&String>) {
    let state = link.map(String::as_str);
    let (color, label) = match state {
        Some("linked") => (egui::Color32::from_rgb(80, 200, 120), "linked"),
        Some(other @ ("failed" | "disconnected" | "closed")) => {
            (egui::Color32::from_rgb(220, 90, 90), other)
        }
        Some(other) => (egui::Color32::from_rgb(220, 180, 70), other),
        None if is_self => (egui::Color32::from_rgb(120, 170, 255), "you"),
        None => (egui::Color32::from_rgb(140, 140, 150), "waiting"),
    };
    ui.horizontal(|ui| {
        ui.colored_label(color, "●");
        let name = if is_self {
            format!("{} (you)", person.display_name)
        } else {
            person.display_name.clone()
        };
        ui.label(name);
        ui.weak(label);
    });
    let flags = format!(
        "mic {} · camera {} · screen {}",
        on_off(person.audio),
        on_off(person.video),
        on_off(person.screen)
    );
    ui.label(egui::RichText::new(flags).small().weak());
    ui.add_space(4.0);
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}
