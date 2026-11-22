# Diskost

Online meetings for small rooms. A C++ server keeps the room together. The Rust project in `client/` is the only client, and the same app runs on Windows, macOS, and Linux.

The window is created by winit and painted by egui through glow (OpenGL). It does not embed a WebView, WKWebView, WebView2, HTML renderer, or an Electron, Tauri, or Capacitor shell. Network, room state, and storage sit in `diskost-core` and `diskost-proto`, which are tested without opening a window.

The server is a signaling server: rooms, passcodes, presence, chat, microphone/camera/screen flags, and WebRTC offer/answer/ICE relay. It does not forward audio or video. Each pair of people opens its own peer connection (a mesh). That fits the default cap of 16 people. A larger call would need a selective forwarding unit later.

```text
Windows / macOS / Linux
     diskost-desktop
            |
     Rust core (diskost-core)
            |
      WebSocket JSON
            |
    diskost-server (C++)
```

Signaling uses `ws://`. WebRTC media, once the peers connect, is encrypted with DTLS-SRTP. Put a TLS reverse proxy in front of the server before exposing it beyond a trusted network.

The message shapes are in [docs/protocol.md](docs/protocol.md).

## Server

Requires CMake and a C++17 compiler. No third-party libraries.

```bash
cmake -S server -B server/build -DCMAKE_BUILD_TYPE=Release
cmake --build server/build --config Release
ctest --test-dir server/build -C Release --output-on-failure
./server/build/diskost-server --port 7880
```

On Windows, if `g++` is the MinGW compiler from MSYS2, `scripts/build-server.ps1` configures the MinGW generator for you.

```text
diskost-server [--host 0.0.0.0] [--port 7880]
               [--max-participants 16] [--stun stun:host:3478]
               [--udp-port 7881]
               [--no-stun] [--verbose]
```

- WebSocket: `ws://127.0.0.1:7880/ws`
- Health: `http://127.0.0.1:7880/health`

## Client (Windows, macOS, Linux)

Requires Rust 1.81 or newer.

```bash
cd client
cargo run -p diskost-desktop
cargo run -p diskost-cli -- --user Alice create --name Daily
cargo run -p diskost-cli -- --user Bob join --room AB12CD34
```

`diskost-desktop` is the same binary on Windows, macOS, and Linux. Rendering is immediate-mode egui on an OpenGL context (`eframe` glow). It opens a WebRTC data channel to every other person so you can see the peer link come up (`linked`). Microphone, camera, and screen toggles update presence for the room. Encoded media frames attach to the same peer connection in `client/diskost-media`. A second path in `client/diskost-stream` records a copy: the desktop **Device test** panel opens the display, microphone, speaker, and webcam on this machine. **Show screen** paints the latest display frame in the window. **Test mic** plays the microphone through the selected speaker. Both stay live and report `live` / `stalled` or `hearing` / `no signal`.

That recording does not replace the WebRTC call. After you join a room, turning on Mic or Screen also compresses the raw samples, encrypts them with a process-local key (ChaCha20-Poly1305, not stored in the repo), and sends datagrams to the server on UDP port 7881 (`--udp-port`). The server keeps the ciphertext. It does not decode it.

The capture build needs development headers that are not part of the runtime libraries. On Linux:

```bash
sudo apt install libasound2-dev libv4l-dev libxcb-randr0-dev libxcb-shm0-dev libdbus-1-dev libclang-dev pkg-config
```

If `link.exe` is not installed (no Visual Studio Build Tools), use the GNU host toolchain and put MinGW `g++` on `PATH`:

```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;C:\msys64\mingw64\bin;" + $env:PATH
cd client
cargo +stable-x86_64-pc-windows-gnu build -p diskost-desktop -p diskost-cli
cargo +stable-x86_64-pc-windows-gnu run -p diskost-desktop
```

On Linux, the glow build needs native windowing libraries:

```bash
sudo apt install libxkbcommon-dev libgl1-mesa-dev libegl1-mesa-dev libwayland-dev libx11-dev libssl-dev pkg-config
```

Terminal commands while you are in a room: `/audio`, `/video`, `/screen`, `/peers`, `/leave`. Any other line is chat.

## Layout

| Path | Role |
| --- | --- |
| `server/` | C++ room and signaling server |
| `client/diskost-proto` | JSON messages |
| `client/diskost-core` | Async WebSocket client |
| `client/diskost-media` | Desktop WebRTC mesh (webrtc 0.17) |
| `client/diskost-stream` | Device capture, compress, encrypt, UDP recording |
| `client/diskost-cli` | Terminal client |
| `client/diskost-desktop` | Windows, macOS, and Linux UI (winit + egui + OpenGL) |

## What a call does

1. Alice creates a room and gets a short code. An optional passcode is checked on join.
2. Bob joins. Both clients receive the roster and the STUN URL list.
3. The client with the greater participant id creates the WebRTC offer. The other answers. ICE candidates are relayed one by one.
4. Chat and mute flags go through the server, so they work even when a peer link is still connecting.
5. If Mic or Screen is on, the desktop also sends a compressed encrypted copy of that media to the server over UDP. WebRTC remains the live call.
