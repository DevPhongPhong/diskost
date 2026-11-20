use std::env;
use std::io::{self, Write};

use diskost_core::MeetingClient;
use diskost_media::LinkUpdate;
use diskost_proto::{format_clock, ServerEvent};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{broadcast, oneshot};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if let Err(err) = run() {
        eprintln!("diskost: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_usage();
        return Ok(());
    }
    let mut server = "ws://127.0.0.1:7880/ws".to_owned();
    let mut user = String::new();
    let mut passcode = String::new();
    let mut command = String::new();
    let mut room_name = "Meeting".to_owned();
    let mut room_id = String::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let value = |flag: &str| -> Result<String, String> {
            args.get(index + 1)
                .cloned()
                .ok_or_else(|| format!("missing value for {flag}"))
        };
        match arg.as_str() {
            "--server" => {
                server = value("--server")?;
                index += 2;
            }
            "--user" => {
                user = value("--user")?;
                index += 2;
            }
            "--pass" => {
                passcode = value("--pass")?;
                index += 2;
            }
            "create" => {
                command = "create".into();
                index += 1;
            }
            "join" => {
                command = "join".into();
                index += 1;
            }
            "--name" => {
                room_name = value("--name")?;
                index += 2;
            }
            "--room" => {
                room_id = value("--room")?;
                index += 2;
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if user.is_empty() {
        return Err("pass --user <display name>".into());
    }
    if command.is_empty() {
        return Err("pass create or join".into());
    }

    let runtime = tokio::runtime::Runtime::new().map_err(|err| err.to_string())?;
    runtime.block_on(session(server, user, passcode, command, room_name, room_id))
}

async fn session(
    server: String,
    user: String,
    passcode: String,
    command: String,
    room_name: String,
    room_id: String,
) -> Result<(), String> {
    if command != "create" && room_id.is_empty() {
        return Err("join requires --room <id>".into());
    }
    let client = MeetingClient::connect(&server)
        .await
        .map_err(|err| err.to_string())?;
    let mut events = client.subscribe();
    let mesh_events = client.subscribe();
    let (stop_tx, stop_rx) = oneshot::channel();
    let (link_tx, mut link_rx) = tokio::sync::mpsc::unbounded_channel::<LinkUpdate>();
    let mesh_client = client.clone();
    tokio::spawn(async move {
        let (bridge_tx, bridge_rx) = std::sync::mpsc::channel();
        let pump = tokio::task::spawn_blocking(move || {
            while let Ok(update) = bridge_rx.recv() {
                if link_tx.send(update).is_err() {
                    break;
                }
            }
        });
        if let Err(err) = diskost_media::run(mesh_client, mesh_events, bridge_tx, stop_rx).await {
            log::error!("media mesh: {err}");
        }
        pump.abort();
    });

    let room = if command == "create" {
        client
            .create_room(&room_name, &user, &passcode)
            .await
            .map_err(|err| err.to_string())?
    } else {
        client
            .join_room(&room_id, &user, &passcode)
            .await
            .map_err(|err| err.to_string())?
    };
    println!("in room {} ({})", room.room_id, room.room_name);
    println!("you are {} ({})", room.self_id, user);
    println!("commands: /leave  /audio  /video  /screen  /peers");
    let mut audio = true;
    let mut video = false;
    let mut screen = false;
    let mut links: Vec<(String, String)> = Vec::new();

    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let line = line.map_err(|err| err.to_string())?.unwrap_or_default();
                if line == "/leave" {
                    break;
                } else if line == "/audio" {
                    audio = !audio;
                    client.set_media(audio, video, screen).map_err(|err| err.to_string())?;
                    println!("microphone {}", on_off(audio));
                } else if line == "/video" {
                    video = !video;
                    client.set_media(audio, video, screen).map_err(|err| err.to_string())?;
                    println!("camera {}", on_off(video));
                } else if line == "/screen" {
                    screen = !screen;
                    client.set_media(audio, video, screen).map_err(|err| err.to_string())?;
                    println!("screen {}", on_off(screen));
                } else if line == "/peers" {
                    if links.is_empty() {
                        println!("no peer links yet");
                    }
                    for (peer, state) in &links {
                        println!("  {peer}  {state}");
                    }
                } else if !line.is_empty() {
                    client.chat(&line).map_err(|err| err.to_string())?;
                }
            }
            event = events.recv() => {
                match event {
                    Ok(event) => print_event(&event),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            update = link_rx.recv() => {
                let Some(update) = update else { continue };
                println!("* link {} {}", update.peer_id, update.state);
                if let Some(slot) = links.iter_mut().find(|(id, _)| id == &update.peer_id) {
                    slot.1 = update.state;
                } else {
                    links.push((update.peer_id, update.state));
                }
            }
        }
    }
    let _ = client.leave().await;
    let _ = stop_tx.send(());
    let _ = io::stdout().flush();
    Ok(())
}

fn print_event(event: &ServerEvent) {
    match event {
        ServerEvent::ChatMessage {
            display_name,
            text,
            unix_ms,
            ..
        } => println!("[{}] {display_name}: {text}", format_clock(*unix_ms)),
        ServerEvent::ParticipantJoined { participant } => {
            println!("* {} joined", participant.display_name);
        }
        ServerEvent::ParticipantLeft { participant_id } => {
            println!("* {participant_id} left");
        }
        ServerEvent::MediaState {
            participant_id,
            audio,
            video,
            screen,
        } => println!(
            "* {participant_id} mic {} camera {} screen {}",
            on_off(*audio),
            on_off(*video),
            on_off(*screen)
        ),
        ServerEvent::Error { code, message, .. } => eprintln!("! {code}: {message}"),
        _ => {}
    }
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn print_usage() {
    println!(
        "\
diskost — terminal meeting client

  diskost --user Alice create [--name \"Daily\"] [--pass secret] [--server ws://127.0.0.1:7880/ws]
  diskost --user Bob join --room AB12CD34 [--pass secret] [--server ws://127.0.0.1:7880/ws]

Type a line to chat. /leave exits. /audio /video /screen toggle presence.
The client also opens a WebRTC data channel to each peer."
    );
}
