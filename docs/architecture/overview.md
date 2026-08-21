# Architecture Overview

## System Design

```
┌─────────────────────── HOST ───────────────────────┐         ┌─────────────────────── GUEST ───────────────────────┐
│                                                      │         │                                                       │
│  iroh Endpoint (ALPN: "rdp")                        │         │  iroh Endpoint (ALGN: "rdp")                       │
│       │                                             │         │        │                                              │
│       ├─► Accept incoming QUIC connections          │◄────────┤  Connect via EndpointId + ALPN                     │
│       │    (one per guest session)                  │  iroh   │    P2P with NAT traversal                          │
│       │                                             │  P2P    │                                                       │
│       ├─► PasscodeStore                             │         │  Passcode (MASTER-xxxx or GUEST-xxxx)               │
│       │    - Master: auto-approved, full control     │         │    Sent as first protocol message                  │
│       │    - Guest: requires host approval for input │         │                                                       │
│       │                                             │         │  InputInjector (mouse/keyboard events → OS)             │
│       ├─► SessionManager                            │         │    Platform-specific APIs                          │
│       │    - Tracks active sessions                 │         │                                                       │
│       │    - Per-session control state              │         │                                                       │
│       └─► ScreenCapturer → Encoder                  │         │  ScreenDecoder (receives + renders frames)          │
│           sends Frame messages over QUIC             │         │                                                       │
└──────────────────────────────────────────────────────┘         └───────────────────────────────────────────────────────┘
```

## Connection Flow

### 1. Host Startup
```rust
let server = HostServer::bind().await?;        // Creates iroh Endpoint with presets::N0
// presets::N0 enables DNS/Pkarr discovery + public relay servers (free)
server.endpoint_id()                           // Returns the unique EndpointId (public key)
```

### 2. Passcode Generation
```rust
let master_code = server.generate_master_passcode().await?;  // "MASTER-<base58>"
let guest_code = server.generate_guest_passcode().await?;    // "GUEST-<base58>"
```

The encoded string is base58 of: `[type_byte] [32-byte random secret]` where type_byte distinguishes Master vs Guest.

### 3. Guest Connection
```rust
// Guest receives the host's EndpointId (from --show-ticket or manual sharing)
let addr = EndpointAddr::new(host_endpoint_id);
let conn = endpoint.connect(addr, b"rdp").await?;  // ALPN "rdp" matches host listener
let (mut send, mut recv) = conn.open_bi().await?;  // Single bidirectional QUIC stream

// First message must be Auth
write_length_prefixed(&send, &Message::Auth { passcode: "...", session_id: 1 }).await?;
```

### 4. Host Accepts Connection
```rust
let conn = endpoint.accept().await?.await??;       // iroh Incoming → Connection
let (mut send, mut recv) = conn.accept_bi().await?; // Single bidirectional stream
let data = read_length_prefixed(&recv).await?.unwrap(); // Read Auth message
let msg: Message = bincode::deserialize(&data)?;

match &msg {
    Message::Auth { passcode, session_id } => {
        if let Some((PasscodeType::Master, secret)) = store.validate(passcode).await {
            send SessionStart immediately → control_granted = true
        } else if let Some((PasscodeType::Guest, secret)) = store.validate(passcode).await {
            send GuestApprovalRequest → control_granted = false (until host approves)
        }
    }
}
```

### 5. Multi-User Session Management
Each guest gets an independent session tracked by `SessionManager`:
```rust
session_manager.add_session(secret, control_granted);  // Assigns unique session_id
session_manager.set_control(session_id, true);         // Approve a pending guest
session_manager.remove_session(session_id);            // Guest disconnected
```

## Protocol Messages (Wire Format)

All messages use **length-prefixed bincode** serialization:
```
[4 bytes: big-endian message length][bincode-serialized Message]
```

### Message Enum (`protocol.rs`)

| Variant | Direction | Purpose |
|---------|-----------|---------|
| `Auth { passcode, session_id }` | Guest → Host | Initial authentication with encoded passcode |
| `Config(DisplayConfig)` | Guest → Host | Client display capabilities (fps, bitrate, resolution) |
| `Input(InputEvent)` | Guest → Host | Mouse move / button / scroll / key events |
| `SessionStart(SessionStart)` | Host → Guest | Session established with screen info + encoder type |
| `Frame(FrameMessage)` | Host → Guest | Encoded video frame (H.264 NAL units or raw BGRA pixels) |
| `GuestApprovalRequest(...)` | Host → Guest | Asks host to approve/deny guest input control |
| `SessionEnd { reason }` | Either | Graceful disconnection with human-readable reason |
| `Ping(timestamp)` / `Pong(timestamp)` | Both | Keepalive and round-trip latency measurement |

### InputEvent Variants (Guest → Host)
```rust
enum InputEvent {
    MouseMove { x: u16, y: u16 },           // Absolute position in host screen coords
    MouseButton { button: u8, pressed: bool }, // 0=left, 1=right, 2=middle
    Scroll { dx: i16, dy: i16 },             // Horizontal + vertical scroll deltas
    Key(KeyEvent)                            // Virtual key code + press/release flag
}
```

### FrameMessage Structure
```rust
struct FrameMessage {
    sequence_number: u32,     // Monotonically increasing; guest uses this to detect drops
    width: u16, height: u16,  // Display dimensions
    timestamp_ms: u64,        // Milliseconds since session start (for latency measurement)
    is_keyframe: bool,        // True = IDR/intra-only frame (decoder can resume here)
    data: Vec<u8>,            // H.264 NAL units OR raw BGRA pixels (width*height*4 bytes)
}
```

## Video Pipeline

### Encoding Strategy
The host encodes captured frames before sending them over the QUIC stream:

1. **Capture**: `ScreenCaptureBackend::capture_next()` returns a `ScreenFrame` in BGRA format
2. **Encode**: `VideoEncoder::encode_frame()` compresses (or passes through) the frame → `EncodedFrame`
3. **Serialize**: The encoded data is wrapped in a `Message::Frame` and sent via QUIC

### Encoder Selection
- **Raw BGRA** (default, no compression): Zero encode latency but requires ~30+ Mbps for 1080p@60fps
- **H.264** (placeholder): Not yet implemented — see `encoder.rs` comments for platform-specific hardware encoding options

### Adaptive Bitrate
The guest sends `DisplayConfig` with target_fps and max_bitrate_kbps. The host adjusts encoding parameters accordingly. This enables:
- Gaming mode: high FPS, high bitrate → H.264 encoder
- Desktop mode: lower FPS, compressed → AV1 or lower-bitrate H.264

## NAT Traversal (Handled by iroh)

iroh provides automatic hole punching and relay fallback — no manual STUN/TURN setup needed:

1. **Direct connection** (fastest): Both peers are on the same network or have public IPs → direct QUIC
2. **Hole punching**: When both peers are behind NAT but not symmetric → iroh sends coordinated probes to punch through NAT mappings
3. **Relay fallback** (always works): If direct/hole-punch fails → data routes through iroh's public relay servers

The `presets::N0` configuration enables all three modes automatically with free public relays. For production deployments, commercial relay options are also available from [n0.computer](https://n0.computer).

## Security Model

- **Encryption**: All traffic is encrypted via QUIC/TLS 1.3 (built into iroh)
- **Authentication**: Passcodes are 256-bit random secrets — brute-force resistant even when shared over chat
- **Authorization levels**: Master passcodes grant immediate full control; guest passcodes start with input denied and require explicit host approval
- **Revocation**: Host can revoke any passcode at any time via `revoke <encoded_code>` command

## Platform-Specific Notes

| Feature | Windows | macOS | Linux (X11) |
|---------|---------|-------|-------------|
| Screen capture | Graphics Capture API (via scap) | ScreenCaptureKit (via scap) | PipeWire (via scap) or XShm |
| Input injection | SendInput API | CGEventPost API | XTest extension FFI |
| H.264 encoding | Media Foundation (DXVA2) | VideoToolbox FFI | VAAPI / GStreamer
