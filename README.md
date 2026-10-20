# Tain

A peer-to-peer remote desktop application built in Rust, using [iroh](https://iroh.computer) for NAT-traversing P2P QUIC connections. Designed for wide hardware compatibility — from low-latency gaming to light desktop tasks.

Tain runs on Windows. The protocol, networking, passcodes and adaptive controller
build anywhere, but capture, encoding, audio and input injection are Windows-only
for now.

## Features

- **P2P via iroh 1.x** — QUIC with hole punching. A session runs on a **direct path or not at all**: a relay carries the handshake and never the video
- **Found without one server** — mDNS on the local network and the mainline DHT run alongside the public lookup service, so a peer is findable when DNS is blocked and instantly when it is in the same building
- **Two kinds of code** — An admin code is full control, approved once per device and trusted after that. A guest code is streamed **nothing** until the host allows it, every time it connects
- **Share exactly what you mean to** — Whole screens, a chosen few of them, every screen at once laid out as the host has them, or individual application windows with nothing behind them. The host can change what a guest sees without the guest reconnecting
- **Multi-user support** — Several guests on one host at once, each approved and limited independently
- **Hardware video** — H.264, H.265 and AV1 through Media Foundation, decoded on the GPU through libavcodec, with software H.264, lossless and raw paths behind them
- **Adapts to what it measures** — Frame rate, resolution and bitrate follow the pipeline and the link, and the session says which of them is holding it back
- **Audio and cursor** — System audio over Opus, minus any apps you exclude. The host's pointer is sent as data so it moves at the guest's frame rate, and can be captured as relative movement for games
- **Virtual displays** — A screen that exists only in software: for a machine with no monitor, or a guest who wants a screen at their own resolution
- **Unattended access** — An optional Windows service reaches the lock screen, UAC prompts, and a machine with nobody signed in
- **Saved connections** — Hosts you have connected to are remembered, with their codes in the OS credential store
- **Localised** — English and Russian. Adding a language is one file — see [i18n/README.md](i18n/README.md)

## Architecture

```text
┌──────────────────┐   iroh P2P QUIC, ALPN "tain/6"    ┌──────────────────┐
│       Host       │ ◄════════ direct path only ══════► │       Guest      │
│                  │                                     │                  │
│  DXGI / WGC      │  video stream (unidirectional)      │  GPU decode      │
│  capture         │ ─────────────────────────────────►  │  GPU RGBA        │
│  GPU NV12        │                                     │  OpenGL, no      │
│  HW encoder      │                                     │  readback        │
│                  │                                     │                  │
│  WASAPI + Opus   │  audio stream (unidirectional)      │  jitter buffer,  │
│                  │ ─────────────────────────────────►  │  playback        │
│                  │                                     │                  │
│  adaptive ctrl   │  control stream (bidirectional)     │                  │
│  passcodes       │ ◄═════════════════════════════════► │  input, cursor,  │
│  sessions        │    input · telemetry · cursor       │  session         │
└──────────────────┘                                     └──────────────────┘
```

Video and audio each go down a stream of their own, so a keyframe cannot queue in
front of a mouse movement. See [docs/architecture/overview.md](docs/architecture/overview.md).

### Protocol Messages

Every message is length-prefixed `bincode` 1.x. Frames and audio travel on
unidirectional streams of their own, each opened with a byte saying which it is.
Everything else shares the bidirectional control stream. The full shapes are in
`tain-core/src/protocol.rs`.

| Direction | Message | Purpose |
|-----------|---------|---------|
| Guest → Host | `Auth` | The whole connection code, plus the name the guest publishes, if any |
| Guest → Host | `Config` | The guest's window size, frame rate and bitrate ceiling, so the host does not send pixels nobody will see |
| Guest → Host | `Input` | Pointer, buttons, scroll, keys and touch, dropped by the host unless granted |
| Guest → Host | `Telemetry` | Decode and render time, frames decoded, round trip. This steers the adaptive controller |
| Guest → Host | `FrameAck` | A frame was decoded and shown, which bounds how far ahead the host may get |
| Guest → Host | `RequestKeyframe` | The decoder lost sync and needs a frame that stands alone |
| Guest → Host | `WatchDisplay` | Show another screen, every screen, or another of the shared windows |
| Guest → Host | `Probe` | Check a code without connecting: the "is it ready" check after a paste |
| Host → Guest | `SessionStart` | Session established: codec, host name, the displays and windows this guest may see |
| Host → Guest | `Frame` | Encoded video |
| Host → Guest | `CodecChanged`, `DisplayChanged` | Rebuild the decoder, because the stream is about to change shape |
| Host → Guest | `SharedWindow`, `SharedChanged` | A shared window was minimised or closed, or the host changed what this guest may see |
| Host → Guest | `GuestApprovalRequest` | Waiting for the host to allow the connection |
| Host → Guest | `ControlState` | The authoritative mouse and keyboard grants for this session |
| Host → Guest | `StreamStatus` | What the stream is running at and what is limiting it |
| Host → Guest | `Audio` | Opus packets of the host's system audio |
| Host → Guest | `Cursor` | Where the host's pointer is, what it looks like, and whether it is hidden |
| Host → Guest | `HostPreview` | A small blurred picture of the host, so its saved card is recognisable |
| Host → Guest | `ProbeResult` | The answer to `Probe` |
| Either | `SessionEnd` | A reason *code*, worded in their own language by whichever end displays it |
| Either | `Ping` / `Pong` | Keepalive, round trip, and the clock offset between the two machines |

### Approval

A guest code gets nothing on connecting — no screen, no input — until the host
allows it. The connection handler waits before it captures a single frame, so a
refused guest never sees a pixel.

Every outcome that is not an explicit allow denies: clicking deny, letting the
prompt time out (30 seconds by default), or a host with nobody to ask. That last
one matters most — an unattended machine must not share its screen because nobody
was there to say no. Putting the popup away without answering is not an answer
either: the guest keeps waiting and seeing nothing until someone decides or the
timeout runs out.

Guest approval is per *connection*. Reconnecting with the same code asks again.

**Admin codes are approved once per device.** The admin code is unrestricted
access to the machine, and codes get out: pasted into the wrong window, read off a
screen share, left in a clipboard manager. So the first connection from each
device has to be allowed at the host, and after that the device is trusted and
connects straight in. A device is identified by its iroh endpoint id, never by a
name or address, because a caller chooses both of those. Resetting the admin
passcode revokes the code *and* forgets every device it let in. Settings →
Security shows how many devices are trusted and can forget them all without a
reset.

A consequence worth knowing: a host with nobody to ask cannot be reached by a
device that has never been approved at it, even with the admin code.

### Permissions

Each guest's permissions are chosen in the popup that allows it:

- **Mouse + Touch** and **Keyboard**, granted independently. Both start unticked
  unless Settings → Security → *Offer by default* says otherwise.
- **Displays**: *All* (which includes monitors plugged in later) or any subset.
  With none chosen the guest connects and sees nothing.
- **Or individual windows**: application windows and nothing else. No desktop
  behind them, and nothing that happens to be on the same screen.

```
ControlGrants { mouse: true, keyboard: false }   ← pointer only; keystrokes dropped
ControlGrants { mouse: false, keyboard: false }  ← screen sharing, no control
```

Allowing with neither device is a real answer, not a refusal: the screen streams and
nothing is controllable.

The host filters every inbound `Input` event against the current grants, re-reading
them per event so a change applies mid-session without reconnecting. Whenever grants
change the host pushes a `ControlState` message, so the guest knows its real
permissions instead of inferring them from which events silently vanish.

An admin connection has nothing to narrow: it sees every screen and has the mouse
and keyboard.

**Remote input gives way to the person at the host.** When the host's own mouse or
keyboard is used, guest input is ignored for two seconds and then resumes on its
own (Settings → Security → *Interrupt guest input*, where zero turns it off).
Presses, movement and scrolls give way, but releases always go through, because a
dropped key-up is how a key gets stuck down on somebody else's machine.

### Connection Codes

```
MASTER-<base58check>   ← Admin: full control, once this device has been approved
GUEST-<base58check>    ← Nothing until the host allows the connection
```

A code is **self-contained**: a type byte, a 32-byte random secret, the host's
32-byte iroh endpoint ID, and the relay it can be reached through. That is why the
UI needs only a copy button on the host and a paste button on the guest. There is
no separate ticket to shuttle around, and nothing to look up before dialling. The
host authenticates on the secret inside the code and rejects codes addressed to a
different endpoint.

Base58**check** is deliberate: codes get pasted through chat clients, and a truncated
or mangled paste must fail loudly rather than producing a valid-looking code that
dials the wrong host.

The same code travels as a link, `tain://connect/MASTER-<base58check>`, with nothing
added. Tain registers the scheme for the signed-in user (no administrator needed),
so clicking a link starts Tain and connects. If Tain is already running, the new
process passes the link to it over a named pipe and exits. A second copy would host
on the same identity. The pipe only accepts the same user, and the sender checks the
pipe is served from its own session before writing a passcode into it. A link pasted
into *Paste passcode* works the same as the code.

### Relays

A relay is where two machines meet to hole punch. Tain uses n0's public relays, and
**Settings → Network** takes a list of your own `iroh-relay` servers, either beside
n0's or instead of them. Beside them, iroh picks whichever answers fastest as this
machine's home relay, and yours whenever n0's can't be reached. With *Use only these
relays*, n0's relays are never contacted. Either way a code carries the relay the
host is on, and iroh dials any relay an address names, so guests need no relay
settings of their own. If the host moves to another relay mid-run, the codes it
hands out from then on name the new one, with the same secrets. The list is read at
startup. The unattended host reads the
same list from the settings the application hands the service.

### Multi-User Support

Each guest connection gets its own iroh QUIC streams and session ID. The `SessionManager` tracks:
- Per-session input grants, and which displays or windows the guest may see
- Display configuration from each guest
- Approval status
- How each guest is connected: its address, round trip, what the host is sending
  it, and what it manages to decode. All of this shows when you hover over its card

The host can approve, narrow, or disconnect individual guests without affecting others.

## Project Structure

```
├── Cargo.toml              # Workspace root
├── README.md               # This file
├── CHANGELOG.md            # What changed in each version
├── LICENSE                 # GPL-3.0
├── tain-core/              # Everything that is not a window
│   ├── ffmpeg/             # The libavcodec shim, compiled when ffmpeg is present
│   └── src/
│       ├── network.rs      # HostServer + GuestClient: accept, handshake, streams, adaptive loop
│       ├── protocol.rs     # Every message that crosses the wire
│       ├── discovery.rs    # mDNS + mainline DHT, and which branch found a host
│       ├── adaptive.rs     # Measures the pipeline, decides what to give up
│       ├── capture_*.rs    # DXGI duplication, WGC, shared capture, NV12, HDR, the all-screens wall
│       ├── window_capture.rs  # Sharing individual application windows
│       ├── virtual_display.rs # Putting virtual displays on the desktop, sizing them, taking them off
│       ├── codec_*.rs      # Media Foundation, OpenH264, delta+zstd
│       ├── decode_*.rs     # libavcodec D3D11VA, NV12→RGBA on the GPU
│       ├── audio_*.rs      # WASAPI loopback, Opus, jitter buffer, playback
│       ├── local_input.rs  # Noticing the person at the host, so remote input gives way
│       ├── i18n.rs         # The compiled-in string catalogues
│       └── …               # passcodes, sessions, connection codes, cursor, input, service control
├── tain-cli/               # The `tain` binary: GUI, service, unattended host, and agent
│   └── src/
│       ├── main.rs         # Entry point; bare flags, no subcommands
│       ├── service.rs      # Register, control and run as a Windows service
│       ├── unattended_host.rs # The host the service starts while the app is closed
│       ├── agent.rs        # The per-desktop agent, for the lock screen and UAC
│       ├── display_driver.rs  # Installing the virtual display driver and talking to it
│       └── gui/            # app, session, session_window, network_bridge,
│                           # settings, immersive, gl_interop, theme, …
├── i18n/                   # One .lang file per language
├── licenses/               # Third-party licence texts, embedded and shown in Settings
├── scripts/                # Building ffmpeg, listing crate licences
├── third_party/ffmpeg/     # The slim ffmpeg build Tain embeds, with its headers
├── third_party/VirtualDisplayDriver/  # The vendored, signed driver package
└── docs/                   # Architecture, module map, and the decision log
```

The full list, with what each module is for, is in
[docs/structure.md](docs/structure.md). Why things are the way they are is in
[docs/decisions.md](docs/decisions.md).

## Building

### You need `nasm` first

Not optional, and not a normal build dependency you can skip. OpenH264 is
compiled from source, and without `nasm` it silently builds **without SIMD
assembly** — one line in the build log and no other sign. Measured cost on a
2560×1600 display: **1147 ms per frame instead of 67 ms.** The app appears to
work and is unusable, and the symptoms all point at the network.

```bash
winget install NASM.NASM
```

(`brew install nasm` on macOS, `apt install nasm` on Debian/Ubuntu.)

The winget package does not add itself to `PATH`. Either put `nasm` on `PATH`, or
copy `.cargo/config.toml.example` to `.cargo/config.toml` and point it at your
`nasm.exe`. That file is gitignored, because the path belongs to one machine.
`tain-core/build.rs` warns loudly if it cannot find `nasm`. **Do not ignore that
warning**: it is the difference between a working app and a broken-feeling one.
If you built before `nasm` was in place, run
`cargo clean -p openh264-sys2 -p tain-core` once it is: OpenH264 does not
rebuild on its own, so the plain-C build would stay.

```bash
cargo build
```

### ffmpeg is in the repository

Hardware decode and audio both go through ffmpeg. The build Tain uses is
committed in `third_party/ffmpeg`, so a clone has it and needs nothing
downloaded: avcodec, avutil and swresample, built with only the decoders and the
Opus encoder Tain calls, a few MB in all. `scripts/build-ffmpeg.sh` is how it was
built and how to rebuild it, and `third_party/ffmpeg/PROVENANCE.md` records
exactly what went in. The DLLs are carried inside the binary and unpacked on
first use.

Set `FFMPEG_DIR` to build against a different shared build. Without one at all
the build still succeeds and still decodes, through Media Foundation instead of
D3D11VA, but has no audio.

### Tests

```bash
cargo test --workspace
```

Tests that need real hardware or real network traffic are `#[ignore]`d and named,
so they can be run one at a time. **Do not run `-- --ignored` wholesale** unless
you mean to exercise encoders, decoders, display drivers and the public DHT all at
once. The end-to-end tests make a real iroh connection:

```bash
cargo test -p tain-core --test e2e_iroh -- --ignored
```

To check that every string the source asks for exists in the English catalogue:

```bash
cargo test -p tain-core --test i18n_keys -- --nocapture
```

### When the stream feels slow

Measure before theorising — the symptoms of a capture-bound stream and a
network-bound one are identical from the outside, and both make the bandwidth
graph look idle:

```bash
cargo test --release -p tain-core --test pipeline_timing -- --ignored --nocapture
```

It reports milliseconds per frame for capture, encode and decode at every
resolution step. Anything at or above 33 ms cannot hold 30 fps, and the bottleneck
is this machine rather than the link. Build it in release — a debug build's
numbers are dominated by unoptimised pixel loops.

### Dependencies

| Component | Dependency | Purpose |
|-----------|-----------|---------|
| Networking | `iroh` 1.x | P2P QUIC connections, NAT traversal (hole punching + relay) |
| Discovery | `iroh-mdns-address-lookup`, `iroh-mainline-address-lookup` | Finding a peer on the local network, or through the DHT, without DNS |
| Serialization | `serde` + `bincode` (v1.x) | Efficient binary protocol messages (NOT v2, which requires serde feature flag) |
| Screen capture | `windows` (DXGI, Windows.Graphics.Capture) | Desktop Duplication, with the compositor as the fallback; test pattern off Windows |
| Hardware video | `windows` (Media Foundation) | H.264, H.265 and AV1 encode on the GPU |
| Hardware decode | ffmpeg / libavcodec | D3D11VA decode. **Optional**: `build.rs` sets the `db_ffmpeg` cfg only when the shim compiles |
| Software video | `openh264` 0.9 | H.264 encode and decode, built from source — **needs `nasm`** |
| Audio | `windows` (WASAPI) + Opus | System audio capture and playback |
| Lossless codec | `zstd` | The delta+zstd path |
| Passcode encoding | `bs58` (check) | Base58check codes, so a mangled paste fails loudly |
| Secret storage | `keyring` 4 | Codes and identity in the OS credential store, never in a file |
| Interface | `egui` / `eframe` 0.29 (glow) | The application window |
| Virtual display | [Virtual Display Driver](third_party/VirtualDisplayDriver/PROVENANCE.md) (MIT) | Vendored, pinned by hash, and compiled into the binary |
| Async runtime | `tokio` 1.x | Required by iroh and used throughout |

### Releasing

A release is a pushed `v*` tag, and nothing is built locally.
[`.github/workflows/release.yml`](.github/workflows/release.yml) builds `tain.exe`
on GitHub's Windows runner and attaches it, with its SHA-256 beside it, to that
tag's release. If there is no release yet, it creates one.

1. Bump `version` under `[workspace.package]` in `Cargo.toml`. That is the number
   Settings shows, and the workflow refuses a tag that disagrees with it.
2. Make sure [CHANGELOG.md](CHANGELOG.md) has a section for that version, dated
   today. Every change adds its own entry as it is committed, under the version
   in `Cargo.toml` until that version is tagged, so the section should already
   cover everything since the last release.
3. Regenerate the crate licence list. It is compiled into the binary, and the
   workflow fails if it does not match `Cargo.lock`:

   ```powershell
   .\scripts\list-crate-licences.ps1
   ```

4. Commit `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md` and
   `licenses/rust-crates.txt` on `main`. The version is in `Cargo.lock` too, and
   the release builds with `--locked`.
5. Tag that commit and push the tag:

   ```bash
   git tag -a v0.4.0 -m "Tain 0.4.0"
   git push origin main v0.4.0
   ```

The job stops rather than ship an exe that only looks complete. It fails without
`nasm`, without ffmpeg in the checkout, with any file checked out differently
from how it was committed, or on any warning from `tain-core`'s build script. A
release published by hand before the tag keeps its notes, and only the exe on it
is replaced.

To try it without cutting a tag, run **Release** from the Actions tab with **Run
workflow**, on `main`. The exe comes back as a workflow artifact, kept for a
week, and no release is touched. A run on `main` is also the only kind that
saves the build cache, which is what the next tag's build starts from.

## Usage

```bash
cargo run
```

That launches the GUI, which is the only entry point you type. There are **no
subcommands**. `main.rs` reads a few bare flags rather than parsing a command
surface, and the other modes it compiles are started for you: the service and
unattended host by the service control manager, the agent by the service, and
the installers by the app relaunching itself with administrator rights.
`tain-cli/src/host.rs` and `guest.rs` still exist and are not compiled in.

Each run logs to `%APPDATA%\Tain\tain.log`. The service, the unattended host and
the agents log to `%ProgramData%\Tain`. Every log starts with the build that
wrote it.

### Debugging aids

The middle and footer sections are empty until a real session exists, which makes
their layout hard to iterate on. Debug builds accept an opt-in flag that fills them
with sample rows — it is compiled out of release builds entirely:

```bash
TAIN_DEBUG=demo cargo run
```

Flags are comma-separated. The ones that need sample data are compiled out of
release builds; the rest are deliberately not, because the build whose numbers
matter is the release one:

| Flag | Effect | Builds |
|------|--------|--------|
| `demo` | Fill the connection lists with sample rows | debug |
| `settings` | Open the settings window immediately | **all** |
| `approval` | Open the approval popup for the pending demo guest (with `demo`) | debug |
| `loopback` | Connect to our own host endpoint on startup, to exercise the video path on one machine | **all** |
| `frametime` | Report where a frame of UI time went, once a second | **all** |
| `links` | Register `tain://` links to this build. Debug builds otherwise leave them to the copy you actually use | debug |

`loopback` and `frametime` stay in release because measuring the pipeline needs
both ends running, and a debug build's per-pixel decode is slow for reasons that
say nothing about the shipped one — reporting debug numbers as if they described
the real thing has sent an investigation down a wrong turn once already.

```bash
TAIN_DEBUG=loopback,settings cargo run
```

A loopback session **disables input forwarding**, and says so in the session toolbar.
Injection is real on Windows, so forwarding the pointer to our own endpoint would
move the pointer we are reading from.

Other environment variables:

| Variable | Effect |
|----------|--------|
| `TAIN_CONSOLE=1` | Give a release build a console of its own, the same as `--console` |
| `TAIN_LOG_FILE` | Write the log somewhere other than the default |
| `TAIN_PROFILE` | Give a second instance on one machine its own identity, so two can connect to each other |
| `TAIN_CAPTURE=wgc` | Capture through the compositor even where Desktop Duplication works, to compare the two |

Release builds are linked as a GUI app, so no console window appears beside them.
Output is still available when you want it: run the binary from a terminal and it
attaches to that terminal, or pass `--console` to get a console of its own.

### The main window

The header holds both halves of the app:

- **Host:** *Admin passcode*, behind a cover you slide aside rather than click, so
  a stray click or a key repeat cannot copy it. The first time, a warning explains
  what the code grants. The cover closes again after the copy, on a reset, and by
  itself after twenty seconds. *Guest passcode* has no cover. Each button copies
  its code, and the link segment at its end copies it as a `tain://` link instead.
  Each code has a reset button beside it, and the guest code can reset itself when
  the last guest leaves.
- **Connect:** *Paste passcode* reads a code or a link from the clipboard and checks the host
  before you commit to anything. It reports the host as ready, ready but going to
  ask, expired, or not answering. The arrow then opens the session.

Below the header are **previous connections** (hosts this machine has dialled)
and **incoming connections** (guests connected to this one).

### Sessions

Sessions open in a **separate window**, one tab each, so several can run at once.
That window owns fullscreen — the main window can never be left stranded by
disconnecting from it, and there is no app header taking up space over the remote
screen. Closing the window ends every session in it; closing a tab ends just that one.

The toolbar over the video starts with what you send — the **pointer** and the
**keyboard**, each of which can be kept local — then what you receive: connection
**stats**, the **host's pointer**, and its **sound** with a volume control. The
screen picker, scaling, fullscreen and immersive mode are on the toolbar too.
Right-click the pointer button for aim sensitivity and pointer capture.

The **screen picker** offers whatever the host allowed. Screens are labelled by
where they sit (*Main*, *Left*, *Above*, *Virtual*, …), and there is also a view of
every screen at once, laid out as the host has them. A window share shows one
window at a time, or all of them tiled side by side so none can hide behind
another.

The **stats** distinguish `lat`, which runs from capture on the host to display
here, from `net`, which is the round trip on the link alone. A large gap between
them puts the delay in encoding or decoding rather than the network. Hovering
also shows the path (and warns when a private address means a VPN is carrying
it), what the host is sending, decode and draw time per frame, and the audio
buffer.

### Managing guests

A guest arriving opens an approval popup attached to its card, as wide as it.
Several can be open at once. The popup says what allowing will share and offers
the [permissions](#permissions) above, with two answers:

| Button | Effect |
|--------|--------|
| **Allow** | Start streaming what is ticked |
| **deny** | Refuse and disconnect — the guest never sees a frame |

Clicking the card again puts the popup away unanswered, and the guest keeps
waiting. An admin connection from a new device gets a different popup, *Allow full
control of this machine?*, because there is nothing to choose. Allowing it makes
the device trusted.

On an **established** connection the mouse and keyboard icons toggle directly, because
revoking should be instant. Clicking the card reopens the popup to change which
screens or windows are shared, which takes effect while the guest is watching. The
person icon swaps to the kick icon on hover, and kicking asks for confirmation.

If the guest published a name, the popup says so — the host cannot verify a name the
other machine chose for itself.

A card disappears as soon as its session ends, however it ended. The UI reconciles
against the server's live session list each frame rather than waiting for a
notification per event. A guest connecting raises a Windows notification, admin
connections included.

### Previous connections

Hosts you connect to appear as cards, with a blurred picture of the host's screen
so each is recognisable. The play triangle reconnects, the name is editable, and
the ✕ that appears on hover forgets the host *and* the code saved with it.

`connections.json` (beside `settings.json`) holds only which hosts you have
connected to and what you call them. The codes themselves are kept in the OS
credential store, and never in that file. An older version did write them there,
and loading it moves any it finds into the credential store.

If the host you connected to has a name set, the card adopts it. Renaming a card
yourself sticks — a host's published name never overwrites it.

### Session controls

Rebindable in Settings → Shortcuts. Defaults:

| Shortcut | Action |
|----------|--------|
| `Ctrl+Shift+Q` | Disconnect |
| `Ctrl+Shift+W` | Toggle fullscreen / windowed |
| `Ctrl+Shift+A` | Toggle the session toolbar |
| `Ctrl+Shift+E` | Toggle immersive mode |
| `Ctrl+Shift+C` | Capture the pointer, or release it |
| `Esc` | Leave fullscreen — always available, and never disconnects, because a reflexive press should not end a session |

These keys are consumed locally and never forwarded to the host, which is why the
settings panel warns when a binding has no modifier: a bare letter would be stolen
from the host mid-sentence. Keeping the defaults on one modifier pair bounds that
cost — `Ctrl+Shift` plus five letters is the whole hole in the keymap. Duplicate
bindings are flagged rather than blocked, so swapping two shortcuts doesn't get
stuck halfway.

### Immersive mode

Makes the session window behave like the host's own machine:

- **Every key goes to the host, Win and Alt included.** egui never reports the Win
  key — Windows routes it straight to the shell — so this installs a low-level
  keyboard hook that swallows it before the shell sees it.
- **The pointer stays inside the window**, so aiming at the far edge of a remote
  screen doesn't drop the cursor onto a local monitor.

It releases both the hook and the pointer when you toggle it off, disconnect, or
the window loses focus, and Windows tears them down automatically if the process
dies. `Ctrl+Alt+Del` is unaffected — Windows reserves it from low-level hooks
regardless.

### Pointer capture

A game looking around wants mouse *movement*, not a position, and a position has
nowhere left to go once the local cursor reaches the window border. Capture hides
this machine's cursor and sends movement instead. It switches on by itself when
the host is seen to hide its own pointer, which works on any capture backend. You
can also switch it on from the pointer button's right-click menu or with
`Ctrl+Shift+C`. Use the shortcut to release it: a captured cursor cannot click the
button that captured it. *Aim sensitivity*, in the same menu, applies only while
the pointer is captured.

### Scaling

**Fit** preserves aspect ratio and letterboxes the remote screen into the window.
**1:1** shows one remote pixel per screen pixel and auto-pans: move the pointer
into the outer 20% and the view slides that way, faster the closer you get to the
edge. The middle 60% is a dead zone so the view does not drift while you work.

### Settings

| Section | What is in it |
|---------|---------------|
| This PC | The name shown to the other machine: anonymous, your user name, the network name, or one you choose |
| Hosting | Codec, encoder backend, which GPU encodes, H.264 profile, resolution and frame-rate ceilings, what to give up when behind, rate control, bitrate limit, whether the pointer is drawn into frames |
| Audio | Opus bitrate, and applications to leave out of the stream — a voice chat, say |
| Virtual display | Installing the driver, adding and removing displays, their size, and whether one follows the guest's resolution |
| Unattended access | Installing the service, and whether it answers when nobody is signed in |
| Security | Approval timeout, code autoreset, interrupting guest input, trusted devices, which permissions are pre-ticked |
| Viewing | Scaling, drawing frames straight from the GPU, fullscreen on connect |
| Interface | Scale of this app's own window (`Ctrl+plus` and `Ctrl+minus` too) |
| Shortcuts | The session controls above |
| Language | Follows the system unless you choose |
| About | Copyright, licence, and the full licence of everything Tain includes |

Settings are saved to `settings.json` in `%APPDATA%\Tain`. Hosting settings apply
to new connections.

## Sharing Screens and Windows

A host with several monitors can offer all of them, any subset, or none. The guest
picks between them from the toolbar, or watches every screen at once as one
picture arranged the way the host has them. That composed picture rebuilds itself
when someone moves or plugs in a monitor, and pointer input maps through it, so a
click lands where it was aimed.

**Sharing windows is not sharing a smaller screen.** A window share sends the
chosen application windows and nothing else. There is no desktop behind them, and
nothing that happens to sit on the same screen. The host picks them from a list
with thumbnails. The guest sees one at a time, can switch between them, or can
see them all tiled. The pointer maps through the window, and the guest is told
when a window is minimised or closed instead of being left looking at a frozen
picture.

The host can change what a guest sees in the middle of a session, from the
guest's card, without the guest reconnecting.

## Virtual Displays

A virtual display is a monitor that exists only in software. It lets a machine
with no monitor be streamed at all. It gives a guest a screen at its own
resolution instead of a scaled copy of the host's, and it turns a spare tablet or
laptop into a second screen.

Settings → Virtual display installs the driver, adds displays and removes them.
Installing needs administrator rights, and Windows also asks whether to trust
the publisher. That second prompt names the driver's author rather than Tain,
because the signed package is installed exactly as shipped. New displays go to
the right of everything else, with sizes picked from the modes the driver
actually reports. With *Match the resolution of whoever connects* on, a virtual
display takes the size of the guest watching it, so their window is drawn 1:1. A
real monitor is never resized.

A virtual display does not outlive the app that created it: closing Tain removes
it. If Tain is killed instead, the unattended-access agent notices and removes
it, where the service is installed. The driver is
[Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver)
by MikeTheTech, MIT licensed, vendored in `third_party/` with its provenance and
hashes, and compiled into the binary, so the button works offline.

## Unattended Access

Most of Tain needs nothing installed. Reaching a UAC prompt, the lock screen, or a
machine with nobody signed in needs a Windows service, which Settings → Unattended
access installs (with administrator rights).

The service runs as LocalSystem. While the app is running, the app hosts. When
the app is closed, the service starts a host of its own on the console session's
desktop, using the owner's settings: codec, frame rate, audio exclusions, name and
trusted devices. A per-desktop agent covers the secure desktop that the lock screen
and consent prompts run on. *Answer when nobody is signed in* hands this machine's
identity to the service, so the same connection code reaches the sign-in screen.

Install it only where you want unattended access: a host with nobody to ask
refuses every guest code and every untrusted device, so only trusted admin devices
get in. The install also adds an inbound firewall rule for the service's own copy
of the executable, which can never raise the firewall prompt itself.

## Video Pipeline

| Codec | State | Use it for |
|-------|-------|------------|
| **H.264 / AVC** | Default | Everything. ~an order of magnitude smaller than lossless on real desktop use |
| **H.265 / HEVC** | Available where the GPU can | 30–50% smaller than H.264 at equal quality, for more encode cost |
| **AV1** | Available where the GPU can | Smallest streams. Needs recent hardware — Arc, Ada, RDNA3 and up |
| **Lossless (delta + zstd)** | Available | Pixel-exact text work on a fast link. Cheap on a still screen, expensive the moment it moves |
| **Raw BGRA** | Available | Debugging, or a LAN where 400 MB/s is free |
| VP9 | Planned | — |

H.264 is encoded on the GPU through Media Foundation where there is a hardware
encoder, and by OpenH264 in software where there is not. H.265 and AV1 are GPU-only
— a software path for either would be a poor trade for a stream that has to stay
interactive — and are asked of the *machine*, not assumed. A host whose GPU cannot
encode one advertises H.264 instead, so the two ends never negotiate something
neither can run. Choosing an unavailable codec falls back to H.264 rather than to
raw, so it never silently costs you hundreds of MB/s.

The **encoder backend** is *Automatic* (GPU when there is one), *Hardware* (GPU
only — sessions refuse to start without one) or *Software*. A hardware encoder that
fails mid-stream is restarted, and if it keeps failing the session continues on
the software encoder rather than ending. *Hardware* chosen by name never falls
back. On a machine with two GPUs you can choose which one encodes. Baseline
**H.264 profile** works with every guest. Main and High are smaller, but need the
guest to be decoding on its GPU.

The **lossless delta + zstd** path is still worth choosing for text-heavy work on a
fast link: each frame is XOR-ed against the previous one, which turns a mostly-static
desktop into runs of zero that zstd collapses to almost nothing. Pixel-exact,
CPU-only, and an unchanged screen costs a few hundred bytes instead of 16 MB. Its
weakness is motion — the delta stops being zeroes and the cost tracks whatever the
screen did, which is why it is not the default.

### Capture

Capture follows the **host's actual display resolution** — a remote desktop shows
the host's screen, so there is no capture-size setting. Frame sizes are snapped to
what the encoder will accept (AV1 wants widths in multiples of 64), keeping the
aspect ratio, and never exceed what the guest's window can show.

**DXGI Desktop Duplication** comes first: the compositor hands over the
already-composed desktop, with no capture border, the pointer reported separately,
and a frame the moment the desktop changes. Duplication is *lost* rather than
interrupted by resolution changes, driver restarts, UAC prompts, and the lock
screen; the capturer rebuilds itself and keeps serving the last good frame
meanwhile.

Duplication only works from the GPU that composites the desktop, and on a hybrid
laptop Windows often runs the process on the other one. Where it fails, Tain falls
back to **Windows.Graphics.Capture**, which asks the compositor for frames and has
no adapter rule to break. (An earlier version pinned itself to the integrated chip
instead, which left the encoder on the weaker GPU. That preference is removed once
on startup.) HDR desktops are tone-mapped back into range on the GPU.

### When it can't keep up

**Resolution** (Native, 75%, 50% or 33%) and **frame rate** are ceilings you set
outright. The stream runs at them whenever nothing is wrong, and the adaptive
controller only goes below them while something is behind. The *If it can't keep
up* slider decides what goes first. Towards *keep detail*, frame rate drops before
resolution and audio is buffered generously. Towards *keep motion*, resolution
softens first and audio is buffered as little as possible. The stats line names
whatever is holding the stream back (capture, encoding, the network, decoding,
drawing, or latency) and says what to do about it.

*Let the slider set the encoder details* is on by default. Turn it off to set the
quality target and keyframe interval yourself. The **bitrate limit never follows
it**: that is a property of your link, not of the trade-off. With no limit set, the
bitrate scales with pixels and frame rate, more generously towards the detail end.

**Patents**: OpenH264's source is BSD licensed, but H.264 is patent-encumbered and
Cisco's royalty coverage applies only to binaries Cisco distributes — not to a build
from source, which is what this does. Fine for personal use; a licensing question if
this is ever shipped commercially.

## Input Injection

- **Windows**: `SendInput` for mouse and keyboard, and `InjectTouchInput` for touch
- **macOS and Linux**: log-only stubs

The `input_inject.rs` module provides a trait-based abstraction so the actual injection logic can be swapped per platform without touching protocol code.

## Security Considerations

- All traffic is encrypted via QUIC/TLS 1.3 (built into iroh)
- Passcodes are cryptographically random 256-bit secrets — brute-force resistant
- Guest connections are streamed **nothing** until explicitly allowed — not the screen, not input — and every reconnection asks again
- Timing out, or having nobody to answer, **denies**
- Admin codes are approved once per device, keyed by endpoint id. Resetting the admin passcode revokes it and forgets every device it let in
- The admin code is behind a slide-to-reveal cover, with a one-time warning about what it grants
- Codes and this machine's identity key live in the OS credential store, never in a file
- Device names arriving from a peer are sanitized and length-capped before they are drawn, and are labelled as self-reported
- Remote input gives way to whoever is physically at the host

Two things to understand before installing the optional parts:

- **The service runs as LocalSystem**, so it can reach screens an ordinary program
  cannot. With it installed, the admin code reaches the lock screen and
  administrator prompts too.
- **The virtual display driver's control pipe is open to every process** on the
  machine, so anything running locally can add or remove virtual monitors. That
  is the driver's decision, not one Tain can tighten.

## Status

Verified by `cargo test --workspace` plus the `#[ignore]`d suites that need the
network or real hardware. Pure logic is tested without a GPU; capture, encode,
decode and display-driver paths are tested on the hardware that has them.

| Area | Status | Notes |
|------|--------|-------|
| Protocol framing | ✅ Works | Length-prefixed bincode; oversized and truncated messages rejected |
| Passcode auth over iroh | ✅ Works | Valid codes admitted, invalid rejected with `SessionEnd` |
| Session management | ✅ Works | Host-assigned IDs, per-session approval, cleanup on disconnect |
| Guest approval flow | ✅ Works | Nothing streamed until allowed; deny, timeout and nobody-to-ask all refuse |
| Admin device approval | ✅ Works | First connection per device approved; trusted after; reset forgets |
| Per-device control | ✅ Works | Mouse and keyboard granted separately; guest notified via `ControlState` |
| Connection codes | ✅ Works | One base58check string carries type, secret, host address and relay |
| Device names | ✅ Works | Anonymous / system user / network name / custom, both directions, sanitized |
| Guest connect from GUI | ✅ Works | Paste a code, the host is checked, then connect |
| Saved connections | ✅ Works | Names in JSON, codes in the credential store; reconnect, rename, forget |
| Mouse, keyboard, touch (guest) | ✅ Works | Sent on change only, gated on grants |
| Input injection (host) | ✅ Windows | `SendInput` and touch injection. macOS/Linux are log-only stubs |
| Local input priority | ✅ Windows | Guest input yields to the host's own; releases always delivered |
| Pointer capture | ✅ Works | Relative movement for games; automatic or by shortcut |
| Immersive mode | ✅ Windows | Win key forwarded via a keyboard hook, pointer confined to the window |
| Session window | ✅ Works | Separate window, one tab per session, several at once |
| Screen choice | ✅ Windows | Host limits which screens; guest switches, or sees all at once |
| Window sharing | ✅ Windows | Chosen windows only; one at a time or tiled; changeable mid-session |
| Virtual displays | ✅ Windows | Install, add, remove, follow the guest's size |
| Unattended access | ✅ Windows | Service, unattended host, and per-desktop agent for lock screen and UAC |
| Screen capture | ✅ Windows | Desktop Duplication, with Windows.Graphics.Capture as the fallback; test pattern elsewhere |
| HDR hosts | ✅ Windows | Tone-mapped back into range by shader, with a video-processor fallback |
| Hardware encode | ✅ Windows | H.264, H.265 and AV1 via Media Foundation, with software fallback |
| Hardware decode | ✅ Windows | libavcodec D3D11VA; drawn straight from the GPU where interop exists |
| Adaptive quality | ✅ Works | Frame rate, scale and bitrate follow measurement; the session names the bottleneck |
| Audio | ✅ Windows | WASAPI loopback with per-app exclusion, Opus, a jitter buffer that grows when the link breaks it |
| Remote cursor | ✅ Works | Sent as data, so it moves at the guest's frame rate rather than the stream's |
| Direct path required | ✅ Works | A relayed session is refused rather than run; both ends check |
| Local discovery | ✅ Works | mDNS finds a host on the same network in under a second, with no relay or DNS |
| DHT discovery | ✅ Works | Mainline DHT, ~8 s; works where DNS is blocked |
| Connection graph | ✅ Works | Six stages with their results; a failure stays on screen where it happened |
| Notifications | ✅ Windows | Tray notification when a guest connects, admin included |
| Localisation | ✅ Works | English and Russian; a test fails on any key the catalogue lacks |
| Settings | ✅ Works | Persisted to JSON; unknown or bad fields fall back without losing the rest |
| CLI subcommands | ❌ Not wired | `host.rs` / `guest.rs` are not compiled in |

## Roadmap / Next Steps

1. **Dirty-rectangle capture** — DXGI reports which regions changed; sending only
   those would cut both encode cost and bandwidth further.
2. **Capture on macOS and Linux** — ScreenCaptureKit and PipeWire; both are stubs today
3. **Input injection on macOS and Linux** — `CGEventPost` and XTest
4. **Hardware encode off Windows** — VideoToolbox and VAAPI, once there is
   anything to capture on those platforms
5. **Clipboard sync**
6. **VP9** — declared, not implemented
7. **H.265 colour** — a round-trip test in `codec_mf.rs`, committed failing on
   purpose, catches HEVC frames coming back the wrong colour. Characterised, not
   fixed.

## Testing between two machines

1. Run Tain on both. Windows will prompt for **firewall access** the first time —
   allow it on both. Without it a direct path cannot be punched, and since a
   relayed session is refused, the connection will fail rather than crawl.
2. On the host, slide the cover aside and click **Copy admin passcode** (full
   control), or click **Copy guest passcode** (nothing until you allow it).
3. Get that code to the other machine and click **Paste passcode**. Once it says the
   host is ready, click the arrow.
4. On the host, the approval popup opens by itself. For a guest code, tick what to
   share and click **Allow**. An admin code asks only the first time each device
   connects.

**On the same network, neither machine needs the internet.** mDNS finds the host
by multicast in under a second and the two punch directly, with no relay and no
DNS involved. The connection graph's *Find* step says `LAN` when that is what
happened.

Across the internet they do need to be introduced, and there are three ways that
can happen — the relay named in the connection code, the public lookup service,
or the mainline DHT. The graph says which, and hovering it shows the whole path.
No address is remembered from last time: a stale one still routes somewhere, and
kept taking connections down with it. If it says `code` and the connection then fails at `Direct`, the
relay was the only route there was and nothing published an address worth
dialling: a VPN or a strict NAT on either machine is the usual reason.

## Licence

Copyright © 2026 x.iso

Tain is free software: you can redistribute it and/or modify it under the terms
of the GNU General Public License as published by the Free Software Foundation,
either version 3 of the License, or (at your option) any later version. It is
distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
without even the implied warranty of MERCHANTABILITY or FITNESS FOR A
PARTICULAR PURPOSE. See [LICENSE](LICENSE).

### Third-party software

Tain redistributes other people's work, all under licences that allow it. Every
one is listed with its full text in [licenses/](licenses/README.md), and in the
app under **Settings → About → Licences…**. The ones that ask for more than a
notice:

| Component | Licence | What it asks |
|-----------|---------|--------------|
| FFmpeg (avcodec, avutil, swresample) | LGPL-3.0-or-later | Its source must be available to whoever gets the binary, and a modified FFmpeg must be substitutable. DLLs of the same names placed beside `tain.exe` are loaded instead of the embedded copies |
| resvg, usvg, attohttpc | MPL-2.0 | Their source must stay available. They are used unmodified from crates.io |
| egui's fonts | UFL-1.0, OFL-1.1, MIT | The font licences travel with the fonts |
| OpenH264 | BSD-2-Clause | A notice only — but H.264 itself is patent-encumbered. See [the patents note](#when-it-cant-keep-up) |

Everything else (the Virtual Display Driver, the Hugeicons icons, and the Rust
crates) is MIT, Apache-2.0, BSD, ISC or similar, and asks for its notice to be
kept, which `licenses/` does.

**Giving someone a build is distributing it under the GPL.** Whoever receives
`tain.exe` is entitled to its complete source: Tain's at the matching tag, which
GitHub attaches to every release, and FFmpeg's, whose release is named in its
entry in the licence list and whose build script, `scripts/build-ffmpeg.sh`, is
in this repository.
