# Codebase Structure

Two crates: `tain-core` holds everything that is not a window, and `tain-cli`
builds the `tain` binary — a GUI application that also runs as a Windows service,
the unattended host that service starts, and a per-desktop agent. Beside them,
`i18n/` holds the string catalogues and `third_party/` the virtual display
driver, and both are compiled into the binary.

Most of `tain-core` is Windows-only and gated as such. The parts that are not —
protocol, passcodes, sessions, connection codes, the adaptive controller,
discovery, the string catalogues — build anywhere, which is what keeps them
testable without a GPU.

## `tain-core`

### Networking and identity

| Module | What it does |
|--------|--------------|
| `network.rs` | `HostServer` and `GuestClient`. The accept loop, the handshake, who has to be approved, the streams, the adaptive control loop, and the refusal of any session a relay would have to carry. Also the identity this machine dials with, derived from the one it hosts on. The largest module here by a distance. |
| `protocol.rs` | Every message that crosses the wire, the framing helpers, and the reason codes a session ends with. |
| `connection_code.rs` | Self-contained codes: type byte, secret, endpoint id, and relay, in one base58check string. |
| `passcode.rs` | Master and guest secrets, and validating them. Approval deliberately does not live here: a guest code is shared, so approval keyed on the secret would approve everyone holding it. |
| `session.rs` | The live session registry: ids, per-session grants, which displays or windows each session may see, cleanup. |
| `discovery.rs` | mDNS and mainline-DHT address lookup, and the record of which branch found a host. |
| `dns_cache.rs` | Relay addresses remembered from last time, so a filtered link can still reach them. |

### Capture

| Module | What it does |
|--------|--------------|
| `screen_capture.rs` | The capture trait, the pixel formats everything else agrees on, the host's list of displays, and what a session is pointed at (`DisplayTarget`). |
| `capture_dxgi.rs` | DXGI Desktop Duplication — the first choice on Windows: no capture border, the pointer reported separately, and a frame the moment the desktop changes. |
| `capture_wgc.rs` | Windows.Graphics.Capture, which asks the compositor for frames. The fallback where duplication will not start — typically a laptop whose desktop is drawn by the other graphics chip — and the only way to capture a single window. |
| `capture_shared.rs` | One capture per display, shared by every session, because Windows grants a process one duplication of an output. Also where duplication is tried first and the compositor second. |
| `capture_wall.rs` | Every display composed into one picture laid out as the host has them, or several shared windows tiled into a grid. One stream rather than one per display, with a thread per display because each one blocks. |
| `capture_nv12.rs` | BGRA to NV12 on the GPU, so the encoder is handed what it wants. |
| `capture_hdr.rs` | Tone-mapping an HDR desktop back into range, by shader or video processor. |
| `capture_follow.rs` | Keeps producing frames when the desktop changes underneath — lock screen, UAC prompt — by taking them from the secure desktop's agent until the input comes back. |
| `agent_pipe.rs` | Frames from an agent running on another desktop, over a named pipe. |
| `gpu_preference.rs` | Windows' per-application graphics preference. Today it only takes back the one an older Tain wrote for itself, which pinned it to the integrated chip and kept the encoder off the discrete one. |
| `wallpaper.rs`, `preview.rs` | A small blurred picture of a machine, so its card is recognisable later. |

### Displays and windows

| Module | What it does |
|--------|--------------|
| `window_capture.rs` | Which application windows can be shared, their thumbnails for the picker, and whether a shared one is showing, minimised or closed — states a monitor does not have. |
| `virtual_display.rs` | Putting a virtual display on the desktop, sizing it, and taking it off again. Works with any driver that has presented a monitor, and installs nothing: that is `tain-cli`'s `display_driver.rs`. |

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
| `input_inject.rs` | `SendInput` and touch injection on Windows, log-only stubs elsewhere, and `Deferring`, which holds remote input back while somebody at the host is using it — releases excepted, so no key is left stuck down. |
| `local_input.rs` | Noticing the person at the host. A low-level hook rather than `GetLastInputInfo`, because only the hook can tell their input from the guest's injected events. |
| `service_control.rs`, `pipe_security.rs` | What the application tells the service — the identity, codes and settings to host with, and whether the application is hosting itself — and who is allowed to say it. |

### Language, and tests

| Module | What it does |
|--------|--------------|
| `i18n.rs` | Every sentence the interface shows, from the catalogues in `i18n/` that `tain-core/build.rs` compiles in, in the language the user chose or the system's. A missing key falls back to English. Log output is never translated, because a log is what gets sent to whoever is diagnosing. |
| `watchdog.rs` | Test-only. A time limit for tests that call into a graphics driver, so a stall inside Media Foundation aborts the run with a name rather than hanging it. |

## `tain-cli`

| Module | What it does |
|--------|--------------|
| `main.rs` | Entry point. Reads bare `std::env::args` rather than clap — the binary has a handful of modes, not a command surface. Every mode but the GUI is entered by a flag that something else passes: the service control manager, the service, or the app relaunching itself elevated. |
| `service.rs` | Registering, controlling and running Tain as a Windows service, with the firewall rule its own copy of the executable needs. Session zero has no desktop, so the service does no work there itself: it supervises the agents and the unattended host. |
| `unattended_host.rs` | The host the service starts on the console desktop while the application is closed. It runs the application's own hosting path with the owner's settings, takes trusted devices from the credential store rather than the settings file, and has nobody to approve anything. |
| `agent.rs` | The per-desktop agent process. One per desktop that matters, so the secure desktop the lock screen and UAC prompts run on can be captured and clicked. It also removes a virtual display left behind by a Tain that was killed. |
| `display_driver.rs` | Installing and removing the vendored virtual display driver, elevated, and the driver's control pipe, which is how a monitor is made to exist. |
| `console.rs` | Attaching a console to a GUI-subsystem binary. |
| `logging.rs`, `migrate.rs` | Where the log goes, and carrying an older installation across the rename. |
| `host.rs`, `guest.rs` | Headless host and guest paths. Not compiled in: `main.rs` declares no module for them. |

### `gui/`

| Module | What it does |
|--------|--------------|
| `app.rs` | The application: tabs, host panel, guest panel, the whole main window. |
| `session.rs` | One guest session and everything the UI needs to drive it, pointer capture included. |
| `session_window.rs` | A separate window holding one tab per live session, and the toolbar over the video with its screen and window picker. |
| `network_bridge.rs` | The guest's async half: connect, receive, decode, forward input, report telemetry. |
| `progress.rs` | How far a connection has got, drawn as the path it took. |
| `gl_interop.rs` | Drawing a decoded Direct3D texture straight from OpenGL, with no readback. |
| `immersive.rs` | Treating the session window as if it were the host's machine. |
| `keymap.rs`, `shortcuts.rs` | egui input to protocol key events, and the rebindable session controls. |
| `connections.rs`, `secrets.rs` | Remembered hosts, with their codes in the OS credential store. |
| `settings.rs`, `settings_panel.rs` | Persisted settings and the window that edits them. |
| `notify.rs` | Telling the person at the keyboard that somebody has connected. |
| `theme.rs`, `icon.rs`, `preview.rs`, `frametime.rs` | Palette and drawing helpers, the window icon, host thumbnails, and where a frame of UI time went. |

## Other folders

| Folder | What it holds |
|--------|---------------|
| `i18n/` | One `.lang` file per language — English and Russian today — compiled into the binary by `tain-core/build.rs`. Adding a language is dropping a file in, with no list to join and no Rust to edit. The format is a key in brackets and then its text, because the text is full of the apostrophes and quotation marks every other format would make a translator escape. `i18n/README.md` is written for the person adding one. |
| `third_party/VirtualDisplayDriver/` | The virtual display driver, vendored unmodified with its licence and a provenance record, and compiled into the binary by `tain-cli/build.rs` so installing it works offline. Signed by its author, so nothing in the folder may be edited — an edit voids the signature. See `PROVENANCE.md`. |
| `third_party/ffmpeg/` | Where `scripts/fetch-ffmpeg.ps1` puts a shared ffmpeg build. Gitignored, for its size and its licence. |
| `tain-core/ffmpeg/` | The libavcodec shim, compiled only when ffmpeg is present. |

## Dependencies

### Always linked

| Crate | Why |
|-------|-----|
| `iroh` 1.x | P2P QUIC, hole punching, relays. Never bypassed for raw sockets. |
| `iroh-mdns-address-lookup`, `iroh-mainline-address-lookup` | Finding a peer without DNS — see `discovery.rs`. |
| `n0-future` | The stream types those lookups are written against. |
| `serde` + `bincode` **1.x** | The wire format. Not bincode 2, whose API differs. |
| `serde_json` | Settings, saved connections and the relay address cache, and the settings handed to the service. |
| `tokio` 1.x | Async runtime, required by iroh. |
| `openh264` 0.9 | Software H.264, built from source — **needs `nasm`**, see the README. |
| `zstd` | The lossless path. |
| `bs58` (check), `rand`, `hex` | Connection codes and passcodes. |
| `blake3` | Deriving the key this machine dials with from the one it hosts on, so the two are never one endpoint id. Already in the tree under iroh. |
| `anyhow`, `thiserror`, `tracing` | Errors and logging. |
| `windows`, `windows-core` 0.58 | DXGI, Windows.Graphics.Capture, D3D11, Media Foundation, WASAPI, `SendInput`, and in `tain-cli` the service, SetupAPI for the display driver, and the firewall rule. Windows only. |
| `egui` / `eframe` / `egui_extras` 0.29 (glow) | The interface. `tain-cli` only. |
| `keyring` 4 | Connection codes and this machine's identity key in the OS credential store, never in a file. |
| `arboard` 3 | Clipboard, for copy and paste of codes. |

### Optional

**ffmpeg** is found or fetched by `tain-core/build.rs`, which sets the
`db_ffmpeg` cfg only when the libavcodec shim can actually be compiled. Without
it the crate still builds and still decodes — it loses the hardware D3D11VA
path, and sound. Anything gated on `db_ffmpeg` is one of those two: libopus is
inside the same avcodec, so the Opus codec and the guest's playback come through
it too.

## Where to look first

| Question | Start at |
|----------|----------|
| How does a connection get made? | `network.rs`, `GuestClient::connect_reporting` |
| Why did it refuse to connect? | `network.rs`, `await_direct_path` and `DialStep` |
| How was the host found? | `discovery.rs` |
| Why is the picture bad? | `adaptive.rs`, and the tally lines in `network.rs` |
| Who has to approve a connection? | `network.rs`, `must_be_approved`, then `SessionConfig::trusted_admins` |
| Why is it capturing through the compositor? | `capture_shared.rs`, `start_capturing` |
| How is a window shared? | `window_capture.rs`, then `capture_wgc.rs` |
| What answers with the app closed? | `tain-cli/src/service.rs`, then `unattended_host.rs` |
| What crosses the wire? | `protocol.rs` |
| What does the guest see? | `gui/session_window.rs`, then `gui/network_bridge.rs` |
| Where does a sentence come from? | `i18n.rs`, and its key in `i18n/en.lang` |
