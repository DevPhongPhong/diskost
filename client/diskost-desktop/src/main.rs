//! Native desktop client. The window is created by winit and painted by egui
//! through glow (OpenGL). There is no WebView and no HTML/CSS/JS.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use diskost_core::MeetingClient;
use diskost_media::LinkUpdate;
use diskost_proto::{format_clock, Participant, RoomSnapshot, ServerEvent};
use diskost_stream::{
    now_ms, DeviceInfo, DeviceKind, DeviceManager, DeviceSession, MediaSample, RecordingPipeline, SourceKind,
    Stage, UplinkConfig,
};
use eframe::egui;
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
    hardware: DeviceSession,
    devices_ready: bool,
    device_list: Vec<DeviceInfo>,
    sel_display: String,
    sel_mic: String,
    sel_speaker: String,
    sel_webcam: String,
    screen_test: bool,
    mic_test: bool,
    screen_texture: Option<egui::TextureHandle>,
    last_loud: Option<Instant>,
    recording: Option<RecordingPipeline>,
    record_flags: Option<(bool, bool, bool)>,
    record_key: [u8; 32],
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
            hardware: DeviceSession::new(diskost_stream::DataStream::new()),
            devices_ready: false,
            device_list: Vec::new(),
            sel_display: String::new(),
            sel_mic: String::new(),
            sel_speaker: String::new(),
            sel_webcam: String::new(),
            screen_test: false,
            mic_test: false,
            screen_texture: None,
            last_loud: None,
            recording: None,
            record_flags: None,
            record_key: diskost_stream::random_key(),
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
        self.stop_recording();
        self.stop_device_tests();
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
                    self.stop_device_tests();
                    self.status = format!("In {} · code {}", session.room.room_name, session.room.room_id);
                    self.session = Some(session);
                    self.pending = None;
                    self.sync_recording();
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
        if self.screen_test || self.mic_test {
            ctx.request_repaint_after(Duration::from_millis(33));
        } else if self.pending.is_some() || self.session.is_some() {
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
            ui.add_space(16.0);
            ui.separator();
            self.device_test(ui, ctx);
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
                    self.sync_recording();
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

    fn refresh_devices(&mut self) {
        self.device_list = DeviceManager::refresh();
        self.devices_ready = true;
        let display = self.sel_display.clone();
        let mic = self.sel_mic.clone();
        let speaker = self.sel_speaker.clone();
        let webcam = self.sel_webcam.clone();
        self.sel_display = kept(&self.device_list, &display, DeviceKind::Display);
        self.sel_mic = kept(&self.device_list, &mic, DeviceKind::Mic);
        self.sel_speaker = kept(&self.device_list, &speaker, DeviceKind::Speaker);
        self.sel_webcam = kept(&self.device_list, &webcam, DeviceKind::Webcam);
    }

    fn stop_device_tests(&mut self) {
        self.hardware.stop();
        self.screen_test = false;
        self.mic_test = false;
        self.screen_texture = None;
        self.last_loud = None;
    }

    fn toggle_screen_test(&mut self) {
        if self.screen_test {
            self.hardware.stop_display();
            self.screen_test = false;
            self.screen_texture = None;
            return;
        }
        if !self.devices_ready {
            self.refresh_devices();
        }
        match self.hardware.open_display(&self.sel_display) {
            Ok(()) => self.screen_test = true,
            Err(err) => self.status = err.to_string(),
        }
    }

    fn toggle_mic_test(&mut self) {
        if self.mic_test {
            self.hardware.stop_mic_monitor();
            self.hardware.stop_mic();
            self.hardware.stop_speaker();
            self.mic_test = false;
            self.last_loud = None;
            return;
        }
        if !self.devices_ready {
            self.refresh_devices();
        }
        if let Err(err) = self.hardware.open_speaker(&self.sel_speaker) {
            self.status = err.to_string();
            return;
        }
        if let Err(err) = self.hardware.open_mic(&self.sel_mic) {
            self.hardware.stop_speaker();
            self.status = err.to_string();
            return;
        }
        if let Err(err) = self.hardware.start_mic_monitor() {
            self.hardware.stop_mic();
            self.hardware.stop_speaker();
            self.status = err.to_string();
            return;
        }
        self.mic_test = true;
        self.last_loud = None;
    }

    fn sync_recording(&mut self) {
        let joined = self.session.as_ref().map(|session| {
            (session.room.room_id.clone(), session.room.self_id.clone())
        });
        let Some((room_id, participant_id)) = joined else {
            self.stop_recording();
            return;
        };
        if !(self.audio || self.screen) {
            self.stop_recording();
            return;
        }
        let flags = (self.audio, self.video, self.screen);
        if self.record_flags == Some(flags) && self.recording.is_some() {
            return;
        }
        if !self.devices_ready {
            self.refresh_devices();
        }
        let remote = match diskost_stream::udp_target(&self.server_url, 7881) {
            Ok(addr) => addr,
            Err(err) => {
                self.stop_recording();
                self.status = err;
                return;
            }
        };
        let mic_id = if self.audio { self.chosen(DeviceKind::Mic) } else { None };
        let display_id = if self.screen { self.chosen(DeviceKind::Display) } else { None };
        let webcam_id = if self.video { self.chosen(DeviceKind::Webcam) } else { None };
        if self.audio && mic_id.is_none() {
            self.status = "No microphone found for the recording.".into();
        }
        if self.screen && display_id.is_none() {
            self.status = "No display found for the recording.".into();
        }
        self.stop_recording();
        match RecordingPipeline::start(UplinkConfig {
            remote,
            room_id,
            participant_id,
            key: self.record_key,
            mic_id,
            display_id,
            webcam_id,
        }) {
            Ok(pipeline) => {
                self.recording = Some(pipeline);
                self.record_flags = Some(flags);
            }
            Err(err) => self.status = format!("recording: {err}"),
        }
    }

    fn chosen(&self, kind: DeviceKind) -> Option<String> {
        let selected = match kind {
            DeviceKind::Mic => &self.sel_mic,
            DeviceKind::Speaker => &self.sel_speaker,
            DeviceKind::Webcam => &self.sel_webcam,
            DeviceKind::Display => &self.sel_display,
        };
        if !selected.is_empty() {
            return Some(selected.clone());
        }
        DeviceManager::default_id(kind)
    }

    fn stop_recording(&mut self) {
        if let Some(mut pipeline) = self.recording.take() {
            pipeline.stop();
        }
        self.record_flags = None;
    }

    fn device_test(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if !self.devices_ready {
            self.refresh_devices();
        }
        ui.heading("Device test");
        ui.label("Live check on this machine. It does not join a room.");
        ui.horizontal(|ui| {
            if ui.button("Refresh").clicked() {
                self.refresh_devices();
            }
        });
        device_combo(ui, "display", &mut self.sel_display, &self.device_list, DeviceKind::Display);
        device_combo(ui, "mic", &mut self.sel_mic, &self.device_list, DeviceKind::Mic);
        device_combo(ui, "speaker", &mut self.sel_speaker, &self.device_list, DeviceKind::Speaker);
        device_combo(ui, "webcam", &mut self.sel_webcam, &self.device_list, DeviceKind::Webcam);
        ui.horizontal(|ui| {
            let screen_label = if self.screen_test { "Stop screen" } else { "Show screen" };
            if ui.button(screen_label).clicked() {
                self.toggle_screen_test();
            }
            let mic_label = if self.mic_test { "Stop mic" } else { "Test mic" };
            if ui.button(mic_label).clicked() {
                self.toggle_mic_test();
            }
        });
        if self.screen_test {
            self.paint_screen(ui, ctx);
        }
        if self.mic_test {
            self.paint_mic(ui);
        }
    }

    fn paint_screen(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let frame = self.hardware.stream().latest(SourceKind::Display, Stage::Raw);
        let Some(frame) = frame else {
            ui.label("waiting for the first frame");
            return;
        };
        let age = now_ms().saturating_sub(frame.timestamp_ms);
        let state = if age < 500 { "live" } else { "stalled" };
        if let Some(image) = preview_image(&frame) {
            if let Some(texture) = &mut self.screen_texture {
                texture.set(image, egui::TextureOptions::LINEAR);
            } else {
                self.screen_texture = Some(ctx.load_texture("display-test", image, egui::TextureOptions::LINEAR));
            }
        }
        if let Some(texture) = &self.screen_texture {
            ui.add(egui::Image::from_texture(texture).max_width(ui.available_width()));
        }
        ui.label(format!(
            "{}x{} · {} frames · {age} ms · {state}",
            frame.width,
            frame.height,
            frame.seq.saturating_add(1)
        ));
    }

    fn paint_mic(&mut self, ui: &mut egui::Ui) {
        let sample = self.hardware.stream().latest(SourceKind::AudioIn, Stage::Raw);
        let mut peak = 0i32;
        let mut buffers = 0u64;
        let mut in_rate = self.hardware.mic_rate();
        let mut fresh = false;
        if let Some(sample) = sample {
            peak = pcm_peak(&sample.bytes);
            buffers = sample.seq.saturating_add(1);
            in_rate = sample.sample_rate;
            fresh = now_ms().saturating_sub(sample.timestamp_ms) < 500;
        }
        if fresh && peak > 500 {
            self.last_loud = Some(Instant::now());
        }
        let hearing = self
            .last_loud
            .map(|when| when.elapsed() < Duration::from_secs(2))
            .unwrap_or(false);
        let state = if hearing { "hearing" } else { "no signal" };
        ui.add(egui::ProgressBar::new(peak as f32 / 32767.0).show_percentage());
        ui.label(format!(
            "{in_rate} → {} Hz · {buffers} buffers · underrun {} · {state}",
            self.hardware.speaker_rate(),
            self.hardware.underruns()
        ));
    }
}

fn kept(devices: &[DeviceInfo], current: &str, kind: DeviceKind) -> String {
    if devices.iter().any(|device| device.kind == kind && device.id == current) {
        return current.to_owned();
    }
    DeviceManager::default_id(kind)
        .or_else(|| {
            devices
                .iter()
                .find(|device| device.kind == kind)
                .map(|device| device.id.clone())
        })
        .unwrap_or_default()
}

fn device_combo(ui: &mut egui::Ui, salt: &str, selected: &mut String, devices: &[DeviceInfo], kind: DeviceKind) {
    let label = match kind {
        DeviceKind::Display => "Display",
        DeviceKind::Mic => "Microphone",
        DeviceKind::Speaker => "Speaker",
        DeviceKind::Webcam => "Webcam",
    };
    let current = devices
        .iter()
        .find(|device| device.id == *selected)
        .map(|device| device.label.as_str())
        .unwrap_or("none");
    ui.horizontal(|ui| {
        ui.label(label);
        egui::ComboBox::from_id_salt(salt)
            .selected_text(current)
            .show_ui(ui, |ui| {
                for device in devices.iter().filter(|device| device.kind == kind) {
                    ui.selectable_value(selected, device.id.clone(), &device.label);
                }
            });
    });
}

fn preview_image(frame: &MediaSample) -> Option<egui::ColorImage> {
    let width = frame.width as usize;
    let height = frame.height as usize;
    if width == 0 || height == 0 || frame.bytes.len() < width * height * 4 {
        return None;
    }
    const MAX_WIDTH: usize = 960;
    if width <= MAX_WIDTH {
        return Some(egui::ColorImage::from_rgba_unmultiplied(
            [width, height],
            &frame.bytes[..width * height * 4],
        ));
    }
    let dst_width = MAX_WIDTH;
    let dst_height = (height * dst_width / width).max(1);
    let mut pixels = vec![0u8; dst_width * dst_height * 4];
    for y in 0..dst_height {
        let src_y = y * height / dst_height;
        for x in 0..dst_width {
            let src_x = x * width / dst_width;
            let src = (src_y * width + src_x) * 4;
            let dst = (y * dst_width + x) * 4;
            pixels[dst..dst + 4].copy_from_slice(&frame.bytes[src..src + 4]);
        }
    }
    Some(egui::ColorImage::from_rgba_unmultiplied([dst_width, dst_height], &pixels))
}

fn pcm_peak(bytes: &[u8]) -> i32 {
    bytes
        .chunks_exact(2)
        .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]) as i32)
        .map(|sample| sample.abs())
        .max()
        .unwrap_or(0)
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
