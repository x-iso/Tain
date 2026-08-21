# Tain

A peer-to-peer remote desktop application built in Rust, using [iroh](https://iroh.computer) for NAT-traversing P2P QUIC connections. Designed for wide hardware compatibility — from low-latency gaming to light desktop tasks.

## Features

- **P2P via iroh 1.0** — Direct QUIC connections with automatic hole punching and relay fallback (works through NAT, VPNs, firewalls)
- **Passcode-based auth** — Master passcodes grant immediate control; guest passcodes are streamed **nothing** until the host allows the connection, every time
- **Multi-user support** — Multiple guests can connect to one host simultaneously, each independently approved
- **H.264 video** — Software encoding via OpenH264, with selectable lossless and raw paths
- **Saved connections** — Hosts you have connected to are remembered and reconnectable
- **Cross-platform** — Works on Windows, macOS, and Linux via native screen capture APIs

## Architecture

```text
┌──────────────────┐    iroh P2P QUIC (ALPN: "rdp")     ┌──────────────────┐
│        Host       │ ◄════════════════════════════════► │       Guest      │
│                   │                                     │                  │
│  Screen Capture   │  Frame stream (H.264 / raw BGRA)    │  Decoder         │
│  H.264 Encoder    │ ────────────────────────────────►   │  Display Surface │
│  Passcode Store   │                                     │                  │
│  Session Manager  │  Input stream (mouse/keyboard)      │  Input Events    │
│  Host Server      │ ◄═══════════════════════════════  │  (GUI window /   │
│                   │                                     │   CLI simulation)│
└──────────────────┘                                     └──────────────────┘
```

### Protocol Messages

All messages are `bincode`-serialized and sent over iroh QUIC bidirectional streams:

| Direction | Message | Purpose |
|-----------|---------|---------|
| Guest → Host | `Auth { passcode, session_id }` | Initial authentication with master or guest code |
| Host → Guest | `SessionStart { screen_info, encoder_type }` | Session established, share display info |
| Guest → Host | `Config { fps, bitrate, client_res }` | Client's display capabilities |
| Guest → Host | `Input { MouseMove / MouseButton / Scroll / Key }` | User input events (only forwarded when approved) |
| Host → Guest | `Frame { sequence_number, data }` | Encoded screen frame (H.264 NAL units or raw BGRA pixels) |
| Host → Guest | `GuestApprovalRequest { session_id, label }` | Ask host to approve a guest's input control |
| Host → Guest | `ControlState { session_id, grants }` | Authoritative input permissions for this session |
| Bidirectional | `Ping / Pong { timestamp }` | Keepalive and latency measurement |

### Approval

A guest passcode gets nothing on connecting — no screen, no input — until the host
allows it. The connection handler waits before it captures a single frame, so a
refused guest never sees a pixel.

Every outcome that is not an explicit allow denies: clicking Deny, letting the prompt
time out, or running a host with no UI attached. That last one matters most — an
unattended machine must not share its screen because nobody was there to say no.
Closing the dialog is the one exception: it leaves the guest waiting rather than
disconnecting it, so a stray Escape does not end a connection.

Approval is per *connection*. Reconnecting with the same code asks again.

Master passcodes skip all of this — they are the "this is my own machine" path.

### Input Permissions

Mouse and keyboard are granted **independently** per session, chosen in the same
dialog that allows the connection, and both **off by default**:

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

### Connection Codes

```
MASTER-<base58check>   ← Full control, granted immediately
GUEST-<base58check>    ← View-only until the host approves input
```

A code is **self-contained**: it encodes a type byte, a 32-byte random secret, and
the host's 32-byte iroh endpoint ID. That is why the UI needs only a copy button on
the host and a paste button on the guest — there is no separate ticket to shuttle
around. The host authenticates on the secret inside the code and rejects codes
addressed to a different endpoint.

Base58**check** is deliberate: codes get pasted through chat clients, and a truncated
or mangled paste must fail loudly rather than producing a valid-looking code that
dials the wrong host.

### Multi-User Support

Each guest connection gets its own iroh QUIC stream and session ID. The `SessionManager` tracks:
- Per-session input control state (granted/denied)
- Display configuration from each guest
- Approval status for guest passcodes

The host can approve or deny individual guests without affecting others.

## Project Structure

```
├── Cargo.toml              # Workspace root
├── README.md               # This file
├── tain-core/               # Shared library crate
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs          # Crate root with module declarations
│       ├── protocol.rs     # Message types (Frame, InputEvent, Auth, etc.)
│       ├── network.rs      # HostServer + GuestClient using iroh endpoints
│       ├── passcode.rs     # Master/guest passcode generation and validation
│       ├── screen_capture.rs  # Screen frame capture (scap feature / stub)
│       ├── encoder.rs          # Raw BGRA passthrough (H.264 placeholder for future hardware-accelerated impl)
│       ├── input_inject.rs       # Platform-specific keyboard/mouse injection
│       └── session.rs            # Multi-session management for concurrent guests
└── tain-cli/                # CLI binary crate
    ├── Cargo.toml
    └── src/
        ├── main.rs         # clap-based argument parsing, host/guest subcommands
        ├── host.rs         # Host mode: passcode generation, approval management
        └── guest.rs        # Guest mode: input simulation loop (CLI demo)
```

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

The winget package does not add itself to `PATH`, so this repo's
`.cargo/config.toml` points at the binary directly. If you build elsewhere,
update that path or put `nasm` on `PATH`. `tain-core/build.rs` warns loudly if it
cannot find it — **do not ignore that warning**, it is the difference between a
working app and a broken-feeling one.

```bash
cargo build
```

```bash
cargo test --workspace
```

The end-to-end tests exercise a real iroh connection and are `#[ignore]`d because
they need network access to the n0 relay:

```bash
cargo test -p tain-core --test e2e_iroh -- --ignored
```

### When the stream feels slow

Measure before theorising — the symptoms of a capture-bound stream and a
network-bound one are identical from the outside, and both make the bandwidth
graph look idle:

```bash
cargo test --release -p tain-core --test pipeline_timing -- --ignored --nocapture
```

It reports milliseconds per frame for capture, encode and decode at every
resolution the priority slider can pick. Anything at or above 33 ms cannot hold
30 fps, and the bottleneck is this machine rather than the link. Build it in
release — a debug build's numbers are dominated by unoptimised pixel loops.

### Dependencies

| Component | Dependency | Purpose |
|-----------|-----------|---------|
| Networking | `iroh` 1.x | P2P QUIC connections, NAT traversal (hole punching + relay) |
| Serialization | `serde` + `bincode` (v1.x) | Efficient binary protocol messages (NOT v2, which requires serde feature flag) |
| Screen capture | `windows` (DXGI) | Desktop Duplication on Windows; test pattern elsewhere |
| Video encoding | `openh264` 0.9 | H.264 encode and decode, built from source — **needs `nasm`** |
| Lossless codec | `zstd` | The delta+zstd path |
| Passcode encoding | `bs58` | Base58 human-readable passcodes |
| Async runtime | `tokio` 1.x | Required by iroh and used throughout |

## Usage

```bash
cargo run
```

That launches the GUI, which is the only entry point today. There is **no argument
parsing yet** — `host` / `guest` subcommands are not implemented, and `tain-cli/src/host.rs`
and `tain-cli/src/guest.rs` are not currently compiled into the binary (`main.rs` declares
only `mod gui;`).

The middle and footer sections are empty until a real session exists, which makes
their layout hard to iterate on. Debug builds accept an opt-in flag that fills them
with sample rows — it is compiled out of release builds entirely:

```bash
TAIN_DEBUG=demo cargo run
```

Flags are comma-separated and debug-only — all of them are compiled out of release
builds:

| Flag | Effect |
|------|--------|
| `demo` | Fill the connection lists with sample rows |
| `loopback` | Connect to our own host endpoint on startup, to smoke-test the video path on one machine |
| `settings` | Open the settings window immediately |

```bash
TAIN_DEBUG=loopback,settings cargo run
```

Release builds are linked as a GUI app, so no console window appears beside them.
Output is still available when you want it: run the binary from a terminal and it
attaches to that terminal, or pass `--console` (or set `TAIN_CONSOLE=1`) to get
a console of its own.

A loopback session **disables input forwarding**, and says so in the session toolbar.
Injection is real on Windows, so forwarding the pointer to our own endpoint would
move the pointer we are reading from.

### Sessions

Sessions open in a **separate window**, one tab each, so several can run at once.
That window owns fullscreen — the main window can never be left stranded by
disconnecting from it, and there is no app header taking up space over the remote
screen. Closing the window ends every session in it; closing a tab ends just that one.

### Managing guests

A waiting guest shows as a dashed pill with a spinner. Clicking **any** of its three
icons — person, mouse, or keyboard — opens the approval dialog, which explains what
allowing actually shares, offers separate mouse and keyboard checkboxes (both off),
and gives you three answers:

| Button | Effect |
|--------|--------|
| **Allow** | Start streaming, with whichever devices are ticked |
| **Deny** | Refuse and disconnect — the guest never sees a frame |
| **Cancel** | Decide later; the guest keeps waiting and is shown nothing |

On an **established** connection the mouse and keyboard icons toggle directly, because
revoking should be instant. The person icon swaps to the kick icon on hover, and
kicking asks for confirmation.

If the guest published a name, the dialog says so — the host cannot verify a name the
other machine chose for itself.

A pill disappears as soon as its session ends, however it ended. The UI reconciles
against the server's live session list each frame rather than waiting for a
notification per event.

### Previous connections

Hosts you connect to are saved to `connections.json` (beside `settings.json`) and
appear as cards. The play triangle reconnects, the name is editable, and the ✕ that
appears on hover forgets the host.

The card stores the connection code, which is what makes reconnecting work — and which
also means **the file holds a shared secret**. Two things limit that: a host with guest
autoreset enabled invalidates its code the moment you disconnect, and approval is per
connection, so a stored code still cannot get in without the host allowing it. Forget a
card to delete its code.

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
| `Esc` | Leave fullscreen — always available, and never disconnects, because a reflexive press should not end a session |

These keys are consumed locally and never forwarded to the host, which is why the
settings panel warns when a binding has no modifier: a bare letter would be stolen
from the host mid-sentence. Keeping the defaults on one modifier pair bounds that
cost — `Ctrl+Shift` plus four letters is the whole hole in the keymap. Duplicate
bindings are flagged rather than blocked, so swapping two shortcuts doesn't get
stuck halfway.

### Immersive mode

Makes the session window behave like the host's own machine:

- **The Win key goes to the host.** egui never reports it — Windows routes it
  straight to the shell — so this installs a low-level keyboard hook that swallows
  the Win key before the shell sees it. Nothing else is ever swallowed.
- **The pointer stays inside the window**, so aiming at the far edge of a remote
  screen doesn't drop the cursor onto a local monitor.

It releases both the hook and the pointer when you toggle it off, disconnect, or
the window loses focus, and Windows tears them down automatically if the process
dies. `Ctrl+Alt+Del` is unaffected — Windows reserves it from low-level hooks
regardless. Windows-only for now; elsewhere it logs and does nothing.

**Scaling** handles a remote screen that doesn't match your window: `Fit` preserves
aspect ratio and letterboxes, `Stretch` fills and distorts, `1:1` shows one remote
pixel per screen pixel and crops.

On launch the app binds an iroh endpoint, generates a master and a guest passcode, and
begins accepting connections. The window shows:

- **Host panel** (left): admin and guest passcodes with copy / regenerate / autoreset controls
- **Connect panel** (right): passcode and endpoint-ID fields — the Connect button is
  deliberately disabled until the guest path is wired up
- **Previous connections**: saved endpoints (nothing persists them yet)
- **Incoming connections**: pending guest approvals with grant / revoke controls

## Video Pipeline

| Codec | State | Use it for |
|-------|-------|------------|
| **H.264 / AVC** | Default | Everything. ~an order of magnitude smaller than lossless on real desktop use |
| **Lossless (delta + zstd)** | Available | Pixel-exact text work on a fast link. Cheap on a still screen, expensive the moment it moves |
| **Raw BGRA** | Available | Debugging, or a LAN where 400 MB/s is free |
| H.265, AV1, VP9 | Planned | — |

H.264 is encoded in **software** by OpenH264 — no driver dependency, works anywhere.
Selecting the `Hardware` backend is currently a preference, not a guarantee: no GPU
encoder is wired up yet, and both the log and the settings panel say so rather than
letting you conclude your GPU is broken.

With no bitrate limit set, the encoder targets ~0.07 bits per pixel per frame — about
4 Mbit/s at 1080p30. Set an explicit limit to override that.

### Priority: quality ↔ latency

One slider, because the underlying settings are not independent — lower latency means
fewer pixels *and* less encoder search *and* a shorter queue, together. Measured on a
2560×1600 display:

| Slider | Streams | Encode cost | Holds 30 fps? |
|--------|---------|-------------|---------------|
| Quality (0–33) | Full resolution | ~45 ms/frame | No, ~20 fps |
| **Balanced (34–66, default)** | 75%, 30 fps | ~26 ms/frame | Yes |
| (67–85) | 50%, 45 fps | ~11 ms/frame | Yes, easily |
| Latency (86–100) | 33%, 60 fps | ~5 ms/frame | Yes |

The midpoint is 75% rather than full resolution on purpose: at 2560×1600 the software
encoder cannot hold 30 fps at native, and a default that stutters is a bad default
however good it looks written down.

*Let the slider set the details* is on by default. Turn it off to set frame rate,
quality target and keyframe interval yourself — resolution still follows the slider,
since that is the lever the trade is actually about. The **bitrate limit never follows
it**: that is a property of your link, not of the trade-off.

**Patents**: OpenH264's source is BSD licensed, but H.264 is patent-encumbered and
Cisco's royalty coverage applies only to binaries Cisco distributes — not to a build
from source, which is what this does. Fine for personal use; a licensing question if
this is ever shipped commercially.

### Scaling

**Fit** letterboxes the remote screen into the window. **1:1** shows one remote pixel
per screen pixel and auto-pans: move the pointer into the outer 20% and the view slides
that way, faster the closer you get to the edge. The middle 60% is a dead zone so the
view does not drift while you work.

## Input Injection

Platform-specific implementations:
- **Windows**: `SendInput` API with virtual key codes
- **macOS**: `CGEventPost` via CoreGraphics framework
- **Linux X11**: `XTestFakeKeyEvent` / `XTestFakeButtonEvent` (via libxdo or raw FFI)

The `input_inject.rs` module provides a trait-based abstraction so the actual injection logic can be swapped per platform without touching protocol code.

## Security Considerations

- All traffic is encrypted via QUIC/TLS 1.3 (built into iroh)
- Passcodes are cryptographically random 256-bit secrets — brute-force resistant
- Guest connections are streamed **nothing** until explicitly allowed — not the screen, not input — and every reconnection asks again
- Timing out, or having no UI attached to answer, **denies**
- Device names arriving from a peer are sanitized and length-capped before they are drawn, and are labelled as self-reported
- The host can revoke any passcode at any time via `revoke <code>`

**`connections.json` stores connection codes in plaintext.** That is what makes the
reconnect button work, and it means the file is as sensitive as a saved password.
Approval is still required per connection, so a stolen code alone does not grant
access. Forget a saved connection to delete its code.

## Status

Verified by `cargo test --workspace` plus the `#[ignore]`d end-to-end suite. "Works"
below means there is a test covering it, not that it compiles.

| Area | Status | Notes |
|------|--------|-------|
| Protocol framing | ✅ Works | Length-prefixed bincode; oversized and truncated messages rejected |
| Passcode auth over iroh | ✅ Works | Valid codes admitted, invalid rejected with `SessionEnd` |
| Frame streaming | ✅ Works | Host pushes frames on a timer, independent of guest traffic |
| Session management | ✅ Works | Host-assigned IDs, per-session approval, cleanup on disconnect |
| Guest approval flow | ✅ Works | Nothing streamed until allowed; deny, timeout and headless all refuse |
| Per-device control | ✅ Works | Mouse and keyboard granted separately; guest notified via `ControlState` |
| Connection codes | ✅ Works | One base58check string carries type, secret, and host address |
| Device names | ✅ Works | Anonymous / system user / network name / custom, both directions, sanitized |
| GUI host panel | ✅ Works | Copy, regenerate, autoreset, per-device approval |
| Guest connect from GUI | ✅ Works | Paste a code, hit connect; validated on paste |
| Saved connections | ✅ Works | Persisted to JSON, reconnectable, renameable, forgettable |
| Frame rendering | ✅ Works | BGRA→RGBA, texture reused per upload, aspect-fit |
| Mouse input (guest) | ✅ Works | Move/buttons/scroll, sent on change only, gated on grants |
| Keyboard input (guest) | ✅ Works | egui→virtual-key map, synthesized modifiers, repeat suppressed |
| Input injection (host) | ✅ Windows | Real `SendInput`. macOS/Linux still log-only stubs |
| Settings | ✅ Works | Codec, encoder, quality, rate control, security, session; persisted to JSON |
| Configurable shortcuts | ✅ Works | Disconnect / fullscreen / toolbar / immersive, rebindable, conflict-checked |
| Immersive mode | ✅ Windows | Win key forwarded via a keyboard hook, pointer confined to the window |
| Session window | ✅ Works | Separate window, one tab per session, several at once |
| Approve / kick guests | ✅ Works | Person icon, with confirmation on both |
| Fullscreen + toolbar | ✅ Works | Scaling modes, auto-hiding overlay, toggleable |
| Screen capture | ✅ Windows | DXGI Desktop Duplication at native resolution; test pattern elsewhere |
| Video compression | ✅ Works | H.264 (software) by default, plus lossless delta+zstd and raw |
| Priority slider | ✅ Works | One knob for resolution, frame rate, quantiser and keyframes |
| Pipeline threading | ✅ Works | Capture and encode off the async runtime, bounded backpressure |
| Hardware encoding | ❌ Not wired | `Hardware` backend still runs the software encoder |
| CLI subcommands | ❌ Not wired | `host.rs` / `guest.rs` are not compiled in |

### Video

Capture follows the **host's actual display resolution** — a remote desktop shows the
host's screen, so there is no capture-size setting. What you choose is a *stream
quality*: Native, or 75% / 50% / 33% of each axis. Dimensions always round down to
even, because odd sizes break chroma subsampling in every real codec.

Capture uses **DXGI Desktop Duplication** on Windows — the compositor hands over the
already-composed desktop, so there is no per-window enumeration and no redraw cost.
Duplication is *lost* rather than interrupted by resolution changes, driver restarts,
UAC prompts, and the lock screen; the capturer rebuilds itself transparently and keeps
serving the last good frame meanwhile.

The default codec is **H.264**, encoded in software by OpenH264. See
[Video Pipeline](#video-pipeline) above for the trade-offs and the CPU cost.

The **lossless delta + zstd** path is still there and still worth choosing for
text-heavy work on a fast link: each frame is XOR-ed against the previous one, which
turns a mostly-static desktop into runs of zero that zstd collapses to almost nothing.
Pixel-exact, CPU-only, and an unchanged screen costs a few hundred bytes instead of
16 MB. Its weakness is motion — the delta stops being zeroes and the cost tracks
whatever the screen did, which is why it is no longer the default.

H.265, AV1 and VP9 are declared with encoder backend (auto/hardware/software), rate
control (CBR/VBR/constant-quality), bitrate limit and keyframe interval, but are **not
implemented** — they show as "(planned)" and unselectable. Choosing one falls back to
H.264 rather than to raw, so an unavailable codec never silently costs you hundreds of
MB/s.

## Roadmap / Next Steps

1. **Hardware H.264** — Media Foundation on Windows, VideoToolbox on macOS, VAAPI on
   Linux. This is now the single highest-value item: even with SIMD assembly the
   software encoder needs ~45 ms per frame at 2560×1600, so full resolution cannot hold
   30 fps and the priority slider has to trade pixels away to compensate. A GPU encoder
   would remove that trade entirely. The `Hardware` backend setting also currently
   promises something it does not deliver.
2. **Dirty-rectangle capture** — DXGI reports which regions changed; sending only those
   would cut both encode cost and bandwidth further.
3. **Capture on macOS and Linux** — ScreenCaptureKit and PipeWire; both are stubs today
4. **Input injection on macOS and Linux** — `CGEventPost` and XTest
5. **Passcode persistence** — codes are in-memory only and do not survive a restart.
   This is why a saved connection's stored code usually stops working after a host
   restart, even with autoreset off.
6. **Clipboard sync, audio, multi-monitor**

## Testing between two machines

1. Run Tain on both. Windows will prompt for **firewall access** the first time —
   allow it on both, or iroh cannot hole-punch and will fall back to a slower relay.
2. On the host, click **Copy admin passcode** (full control) or **Copy guest passcode**
   (view-only until approved).
3. Get that code to the other machine and click **Paste passcode**, then the arrow.
4. A guest code appears in the host's footer awaiting approval — click the mouse and
   keyboard icons to grant each independently.

Both machines need internet access for the n0 relay to introduce them, even when they
end up talking directly.
