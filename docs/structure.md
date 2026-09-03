# Codebase Structure

Two crates: `tain-core` holds everything that is not a window, and `tain-cli`
builds the `tain` binary — a GUI application that also runs as a Windows service
and a per-desktop agent.

Most of `tain-core` is Windows-only and gated as such. The parts that are not —
protocol, passcodes, sessions, connection codes, the adaptive controller,
discovery — build anywhere, which is what keeps them testable without a GPU.

## `tain-core`

### Networking and identity

| Module | What it does |
|--------|--------------|
| `network.rs` | `HostServer` and `GuestClient`. The accept loop, the handshake, the streams, the adaptive control loop, and the refusal of any session a relay would have to carry. The largest module here by a distance. |
| `protocol.rs` | Every message that crosses the wire, and the framing helpers. |
| `connection_code.rs` | Self-contained codes: type byte, secret, endpoint id, and relay, in one base58check string. |
| `passcode.rs` | Master and guest secrets, validation, and the approval state that goes with them. |
| `session.rs` | The live session registry: ids, per-session grants, cleanup. |
| `discovery.rs` | mDNS and mainline-DHT address lookup, and the record of which branch found a host. |
| `dns_cache.rs` | Relay addresses remembered from last time, so a filtered link can still reach them. |

### Capture

| Module | What it does |
|--------|--------------|
| `screen_capture.rs` | The capture trait and the pixel formats everything else agrees on. |
| `capture_dxgi.rs` | DXGI Desktop Duplication — the real capture path on Windows. |
| `capture_shared.rs` | One duplication shared by every session, because Windows only grants one. |
| `capture_nv12.rs` | BGRA to NV12 on the GPU, so the encoder is handed what it wants. |
| `capture_hdr.rs` | Tone-mapping an HDR desktop back into range, by shader or video processor. |
| `capture_follow.rs` | Keeps producing frames when the desktop changes underneath — lock screen, UAC prompt. |
| `agent_pipe.rs` | Frames from an agent running on another desktop, over a named pipe. |
| `wallpaper.rs`, `preview.rs` | A small blurred picture of a machine, so its card is recognisable later. |

### Video

| Module | What it does |
|--------|--------------|
| `encoder.rs` | Codec selection and the `VideoEncoder` trait. Decides what this machine can actually encode. |
| `codec_mf.rs` | Hardware H.264, H.265 and AV1 through Media Foundation. |
| `codec_h264.rs` | Software H.264 through OpenH264, and the fallback decoder. |
| `codec_delta.rs` | Lossless delta + zstd, for pixel-exact work on a fast link. |
| `codec_mf_decode.rs` | Media Foundation decode. |
| `decode_ffmpeg.rs` | Hardware decode through libavcodec's D3D11VA path — the guest's fast route. |
| `decode_rgba.rs` | NV12 to RGBA on the GPU, so a decoded frame never has to touch the CPU. |
| `h264_bitstream.rs` | Just enough bitstream parsing to drive a DXVA decoder. |
| `ffmpeg_runtime.rs` | Finds ffmpeg, or fetches it. |
| `adaptive.rs` | Measures the pipeline and decides what to give up when it cannot keep up. |

### Audio, input, and the service

| Module | What it does |
|--------|--------------|
| `audio_capture.rs` | System audio via WASAPI loopback. |
| `audio_codec.rs` | Opus, both directions. |
| `audio_output.rs`, `audio_render.rs` | The guest's speakers, fed from a jitter buffer that grows when the link keeps breaking it. |
| `cursor.rs` | The host's pointer sent as data, so it moves at the guest's frame rate rather than the stream's. |
| `input_inject.rs` | `SendInput` on Windows; log-only stubs elsewhere. |
| `service_control.rs`, `pipe_security.rs` | What the application tells the service, and who is allowed to say it. |

## `tain-cli`

| Module | What it does |
|--------|--------------|
| `main.rs` | Entry point. Reads bare `std::env::args` rather than clap — the binary has a handful of modes, not a command surface. |
| `service.rs` | Registering, controlling and running Tain as a Windows service. |
| `agent.rs` | The per-desktop agent process. |
| `console.rs` | Attaching a console to a GUI-subsystem binary. |
| `logging.rs`, `migrate.rs` | Where the log goes, and carrying an older installation across the rename. |
| `host.rs`, `guest.rs` | Headless host and guest paths. Not reached from `main.rs` today. |

### `gui/`

| Module | What it does |
|--------|--------------|
| `app.rs` | The application: tabs, host panel, guest panel, the whole main window. |
| `session.rs` | One guest session and everything the UI needs to drive it. |
| `session_window.rs` | A separate window holding one tab per live session, and the toolbar over the video. |
| `network_bridge.rs` | The guest's async half: connect, receive, decode, forward input, report telemetry. |
| `progress.rs` | How far a connection has got, drawn as the path it took. |
| `gl_interop.rs` | Drawing a decoded Direct3D texture straight from OpenGL, with no readback. |
| `immersive.rs` | Treating the session window as if it were the host's machine. |
| `keymap.rs`, `shortcuts.rs` | egui input to protocol key events, and the rebindable session controls. |
| `connections.rs`, `secrets.rs` | Remembered hosts, with their codes in the OS credential store. |
| `settings.rs`, `settings_panel.rs` | Persisted settings and the window that edits them. |
| `notify.rs` | Telling the person at the keyboard that somebody has connected. |
| `theme.rs`, `icon.rs`, `preview.rs`, `frametime.rs` | Palette and drawing helpers, the window icon, host thumbnails, and where a frame of UI time went. |

## Dependencies

### Always linked

| Crate | Why |
|-------|-----|
| `iroh` 1.x | P2P QUIC, hole punching, relays. Never bypassed for raw sockets. |
| `iroh-mdns-address-lookup`, `iroh-mainline-address-lookup` | Finding a peer without DNS — see `discovery.rs`. |
| `n0-future` | The stream types those lookups are written against. |
| `serde` + `bincode` **1.x** | The wire format. Not bincode 2, whose API differs. |
| `tokio` 1.x | Async runtime, required by iroh. |
| `openh264` 0.9 | Software H.264, built from source — **needs `nasm`**, see the README. |
| `zstd` | The lossless path. |
| `bs58` (check), `rand`, `hex` | Connection codes and passcodes. |
| `anyhow`, `thiserror`, `tracing` | Errors and logging. |
| `windows`, `windows-core` 0.58 | DXGI, D3D11, Media Foundation, WASAPI, `SendInput`. Windows only. |
| `egui` / `eframe` / `egui_extras` 0.29 (glow) | The interface. `tain-cli` only. |
| `keyring` 4 | Connection codes in the OS credential store, never in a file. |
| `arboard` 3 | Clipboard, for copy and paste of codes. |

### Optional

**ffmpeg** is found or fetched by `tain-core/build.rs`, which sets the
`db_ffmpeg` cfg only when the libavcodec shim can actually be compiled. Without
it the crate still builds and still decodes — it just loses the hardware
D3D11VA path. Anything gated on `db_ffmpeg` is that path or the audio codec.

## Where to look first

| Question | Start at |
|----------|----------|
| How does a connection get made? | `network.rs`, `GuestClient::connect_reporting` |
| Why did it refuse to connect? | `network.rs`, `await_direct_path` and `DialStep` |
| How was the host found? | `discovery.rs` |
| Why is the picture bad? | `adaptive.rs`, and the tally lines in `network.rs` |
| What crosses the wire? | `protocol.rs` |
| What does the guest see? | `gui/session_window.rs`, then `gui/network_bridge.rs` |
