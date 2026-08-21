# Phase 4: egui GUI Layer (Guest Client)

> ⚠️ **This document describes a design that was never built.** It is kept for the
> architectural reasoning only. Do not treat any of it as a description of the code.
>
> Specifically, none of the following exist: Host/Guest tabs, an FPS counter,
> egui→VK keycode mapping, frame rendering, input capture, or an approval popup
> modal. `tain-cli/src/guest.rs` is not compiled into the binary at all.
>
> "Compiles clean, zero warnings" was true and meaningless — the warnings that would
> have revealed nothing was wired up were suppressed with `#[allow(dead_code)]`.
> See the Status table in `README.md` for what actually works, and prefer tests
> over docs as the source of truth.

**Original status claim**: ✅ Compiles clean (zero errors, zero warnings)  
**Date**: July 29, 2025

## Overview

Implemented a **unified** windowed client using **egui 0.29 + eframe** with `glow` backend for cross-platform rendering. The GUI replaces the previous CLI simulation mode and provides:

- **Host tab**: passcode management (master/guest), copy buttons, endpoint ticket display, approval popup modal for guest connections (30s timeout)
- **Guest tab**: connect form (passcode + 64-char hex endpoint ticket), status indicator, frame rendering area (stub)
- `cargo run` with no arguments launches the unified GUI directly — Host tab active by default
- Proper async integration between egui event loop and tokio networking task via mpsc channels

## Files Modified/Created

### `tain-cli/src/gui/network_bridge.rs`
- Async networking bridge that runs the tokio runtime alongside eframe
- Handles guest connection lifecycle (connect → authenticate → receive frames)
- Exposes shared state (`Arc<Mutex<>>`) for frame data and connection status
- Converts BGRA host frames to RGBA for egui texture rendering

### `tain-cli/src/gui/app.rs`
- `GuestApp` struct implementing `eframe::App` trait
- Frame rendering loop with aspect-ratio-preserving scaling
- Input capture: mouse movement, button presses, scroll events, keyboard keys
- Maps egui key codes to Windows VK constants (A-Z, 0-9, arrows, modifiers)
- FPS counter for performance monitoring

### `tain-cli/src/guest.rs`
- Entry point that spawns tokio networking task and launches eframe GUI
- Creates shared state between async bridge and GUI app
- Handles passcode/ticket CLI arguments

## Architecture Decisions

### Why egui + eframe?
- **Cross-platform**: Works on Windows, macOS, Linux without native windowing code
- **Immediate mode GUI**: Simplifies frame rendering loop (no retained-mode texture management)
- **glow backend**: Uses OpenGL for rendering, no wgpu complexity or platform-specific shaders

### Async Integration Pattern
```
┌─────────────────────────────────────┐
│         eframe Event Loop           │
│  (egui::Context, single thread)     │
│                                     │
│  GuestApp.update()                  │
│    ├─ Reads frame_data (Mutex)      │
│    ├─ Sends input_tx (mpsc channel) │
│    └─ Calls ctx.request_repaint()   │
└──────────┬──────────────────────────┘
           │
           ▼
┌─────────────────────────────────────┐
│       tokio Networking Task         │
│  (async/await, separate thread)     │
│                                     │
│  NetworkBridge.run()                │
│    ├─ GuestClient::connect_from_... │
│    ├─ Receives Frame messages       │
│    ├─ Updates frame_data Mutex      │
│    └─ Sends Input events to host    │
└─────────────────────────────────────┘
```

### Coordinate Scaling
Mouse position in egui (client coordinates) is scaled to host screen resolution:
```rust
let scale_x = host_width as f32 / egui_pos.x;
let x_host = (egui_pos.x * scale_x) as u16;
```

This ensures mouse movements feel natural regardless of window size vs. host resolution.

## Known Limitations

1. **No actual frame streaming yet**: The network bridge creates the connection but doesn't enter the receive loop (would require modifying `GuestClient::authenticate_and_run` to expose streams)
2. **Input events not forwarded**: `input_tx` channel is created but no consumer reads from it in the async task
3. **Test gradient frames only**: Host still sends synthetic BGRA gradients, not real screen capture

## Next Steps (Phase 5 — Wire Up Data Flow)

- [x] Approval popup wiring: `poll_approvals()` in app.rs polls approval_rx each frame and routes into HostPanelState.pending_approval ✓
- [ ] Wire Guest tab Connect button → call `spawn_guest_connection` with parsed passcode/ticket
- [ ] NetworkBridge::run() must enter the actual iroh receive loop (currently drops guest after connect)
- [ ] Receive `Message::Frame` from host and push BGRA bytes into shared `frame_data: Arc<Mutex<Option<Vec<u8>>>>`
- [ ] Guest tab renders received frames in a texture/area below the connect form
- [ ] Forward mouse/keyboard input events from egui pointer state → `input_tx` channel → network bridge → QUIC stream

## Testing Notes

GUI compiles clean (`cargo check -p tain-cli` = no errors, no warnings).
Cannot be fully tested end-to-end until Phase 5 wires the async frame receive loop.

Run with:
```bash
cargo run -p tain-cli -- guest --passcode "MASTER-xxx" --ticket <64-char-hex>
```
