# Codebase Structure

## File Tree

```
├── Cargo.toml                  # Workspace root (members: tain-core, tain-cli)
├── README.md                   # Project overview and usage instructions
├── .gitignore
│
├── docs/                       # Architecture documentation (this directory)
│   ├── README.md               # Documentation index & quick reference
│   └── structure.md            # This file — codebase layout with descriptions
│
├── tain-core/                   # Shared library crate (protocol, networking, pipeline)
│   ├── Cargo.toml              # Dependencies: iroh 1.x, serde/bincode, tokio, bs58
│   │                             Optional: scap (screen capture)
│   └── src/
│       ├── lib.rs              # Crate root — declares all pub modules + re-exports
│       ├── protocol.rs         # Message types serialized over QUIC streams
│       ├── network.rs          # HostServer + GuestClient; approval flow with 30s timeout via oneshot channels
│       ├── passcode.rs         # Master/guest passcode generation, validation, approval
│       ├── screen_capture.rs   # Screen frame capture trait + scap/stub backends
│       ├── encoder.rs          # Raw BGRA passthrough (H.264 placeholder for hardware-accelerated impl)
│       ├── input_inject.rs     # Platform-specific keyboard/mouse injection traits
│       └── session.rs          # Multi-session manager (tracks guest connections)
│
└── tain-cli/                    # GUI-first binary crate (also supports CLI modes)
    ├── Cargo.toml              # Dependencies: tain-core, iroh 1.x, tokio 1, clap 4, egui 0.29, eframe 0.29 (glow)
    └── src/
        ├── main.rs             # `cargo run` with no args → unified GUI; explicit flags for CLI modes
        ├── host.rs             # Host mode: passcode gen/list/revoke (CLI only, fallback path)
        ├── guest.rs            # Guest mode: headless connection simulation (CLI only, fallback path)
        └── gui/                # Unified egui interface — Host + Guest tabs in one window
            ├── mod.rs          # Module declarations and re-exports
            ├── app.rs          # GuestApp tab container; polls approval_rx channel each frame
            ├── host_panel.rs   # Host UI: passcodes w/ copy buttons, endpoint ticket, approval popup modal
            ├── guest_panel.rs  # Guest UI: connect form (passcode + ticket inputs); stub for frame rendering
            └── network_bridge.rs  # Async networking bridge: iroh connection lifecycle, BGRA→RGBA decode
```

## Module Dependencies

```
tain-core::network  ──►  tain_core::protocol  (Message types)
       │                  tain_core::passcode  (PasscodeStore, PasscodeType)
       │                  tain_core::session   (SessionManager)
       │
       ◄── iroh::Endpoint, iroh::endpoint::{Connection, EndpointAddr, presets}

tain-core::screen_capture ──► [scap]          (optional: native capture API)
                             or              (fallback: StubCapturer generating test frames)

tain-core::encoder  ──► [h264-crab]           (optional: H.264 encoding)
                       or                     (fallback: RawBgraEncoder passthrough)

tain-core::input_inject  ──► Windows: SendInput API
                            macOS:   CGEventPost API
                            Linux X11: XTestFakeKeyEvent/ButtonEvent FFI
```

## What Lives Where

| Concern | Module | Why Here |
|---------|--------|----------|
| Wire format (all message types) | `protocol.rs` | Pure data structures, no external deps beyond serde/bincode |
| Connection lifecycle + auth flow | `network.rs` | Orchestrates iroh endpoint, passcode validation, session management |
| Passcode crypto + approval state | `passcode.rs` | Self-contained: random secrets, base58 encoding, HashMap-based store |
| Session tracking (multi-guest) | `session.rs` | Central registry of active connections and their control states |
| Frame capture abstraction | `screen_capture.rs` | Trait-based so backends can be swapped without touching protocol code |
| Video encoding pipeline | `encoder.rs` | Encodes raw BGRA frames into network-ready compressed data |
| Input injection abstraction | `input_inject.rs` | Platform-specific OS API calls isolated from networking logic |

## External Crate Dependencies

### Required (always linked)
- **iroh 1.x** — P2P QUIC connections, NAT traversal (hole punching + relay)
- **serde + bincode (v1.x)** — Binary serialization of protocol messages (use `bincode::serialize`/`deserialize`, NOT v2 which requires serde feature flag)
- **tokio 1.x** — Async runtime (required by iroh's `tokio` feature)
- **anyhow** — Error handling with context propagation
- **tracing / tracing-subscriber** — Structured logging throughout the codebase
- **rand 0.9 + bs58 0.5** — Passcode generation and base58 encoding

### Optional (feature-gated via `tain-core` Cargo features)
- **scap 0.0.8** — Cross-platform screen capture (macOS ScreenCaptureKit, Windows Graphics Capture API, Linux PipeWire). Activated with `--features tain-core/full` or `--features scap`. Without it, the stub capturer generates gradient test frames.
- **h264-crab 0.5** — Low-latency H.264 encoder for video streaming. Activated with `--features h264-crab`. Without it, raw BGRA frames are sent uncompressed (highest bandwidth, zero encoding latency).

### Transitive (resolved by cargo)
- **quinn** — QUIC implementation used internally by iroh
- **tokio-rustls / rustls** — TLS 1.3 for QUIC encryption
- **pin-project, bytes, thiserror** — Common Rust async utilities
