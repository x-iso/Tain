# Architecture Overview

## Shape of the thing

```
┌───────────────── HOST ──────────────────┐        ┌───────────────── GUEST ─────────────────┐
│                                          │        │                                          │
│  DXGI Desktop Duplication                │        │  Decode (libavcodec D3D11VA, or CPU)     │
│      │  shared by every session           │        │      │                                   │
│      ▼                                    │        │      ▼                                   │
│  NV12 on the GPU ──► hardware encoder     │        │  NV12 ──► RGBA on the GPU ──► OpenGL     │
│      │              (H.264 / H.265 / AV1) │        │      (no readback, where interop works)  │
│      ▼                                    │        │                                          │
│  adaptive controller ◄── telemetry ───────┼────────┼──── measures decode, render, round trip  │
│      │  decides fps, scale, bitrate        │        │                                          │
│      ▼                                    │        │                                          │
│  video stream (uni) ──────────────────────┼───────►│  frames                                  │
│  control stream (bi) ◄────────────────────┼───────►│  input, telemetry, cursor, audio         │
│                                           │        │                                          │
│  PasscodeStore + SessionManager           │        │  session window, one tab per session     │
└───────────────────────────────────────────┘        └──────────────────────────────────────────┘
                              iroh QUIC, ALPN "tain/4"
                        direct only — a relayed session is refused
```

## Connecting

Six stages, and the guest's UI draws them as a row that fills up
(`gui/progress.rs`). Named here in the same order.

### 1. Start

Both sides bind an iroh endpoint through `discovery::bind`, which is
`presets::N0` written out plus two more lookups. It has to be written out
because each resolving service is wrapped so the app can tell which one
answered — see *Finding a host* below.

### 2. Find

An endpoint id is a public key, not a route. Something has to turn one into the
other, and there are four things that can:

| Route | Cost | When it works |
|-------|------|---------------|
| The connection code | none | Always — the code carries the host's relay |
| A remembered address | none | The host answered there before |
| mDNS | < 1 s | Same local network |
| The n0 lookup service | a few seconds | DNS or HTTPS reachable |
| Mainline DHT | ~8 s, sometimes two asks | Almost anywhere; no operator to block |

They race. Whichever answers first is credited, and shown to the user as the
`Find` result — see `discovery::Found`.

A dial that fails with `NoAddress` is retried for 30 seconds rather than
reported as a dead host: a lookup that has not been told anything yet returns
*empty*, not slow, and the slowest lookup is the one most likely to be the only
one working.

### 3. Direct

Every iroh connection begins on a relay and upgrades if it can. Tain waits for
that upgrade — 25 seconds on the guest, a little longer on the host — and
**refuses the session if it never comes**.

A relay is a shared, rate-limited public server. It is ample for the handshake
and the address exchange it exists to carry, and hopeless for the ten to twenty
Mbit/s of a desktop stream: the measured relayed session collapsed to a third of
full resolution at 8 fps and stayed there. Failing to connect at least says so.

So both directories Tain publishes to — the n0 service and the DHT — carry the
relay *and* any address routable from outside, so a peer has somewhere to dial
as well as somewhere to meet. Private addresses are filtered out: unreachable
for the peer they would be published for, and mDNS already answers that case.

### 4. Auth

The guest opens a bidirectional stream and sends the whole connection code. The
host authenticates on the secret inside it.

A **master** code is admitted at once. A **guest** code gets nothing — no
screen, no input — until a person allows it, and every outcome that is not an
explicit allow denies. Approval is per connection.

### 5. Session

The host answers with `SessionStart` and starts its capture, encoder and audio.
It then opens a **unidirectional** stream for video. Frames go down that;
everything else stays on the bidirectional one, so a keyframe cannot queue in
front of a mouse movement. Where the uni stream cannot be opened, both share the
control stream.

### 6. Video

Frames arrive, decode, and are drawn. On a machine with `WGL_NV_DX_interop2` the
decoded Direct3D texture is handed to OpenGL directly and never touches the CPU.

## Wire format

Length-prefixed bincode: `[4 bytes big-endian length][bincode Message]`.

`Message` (see `protocol.rs`) carries, guest to host: `Auth`, `Config`, `Input`,
`Telemetry`, `FrameAck`, `RequestKeyframe`, `Probe`. Host to guest:
`SessionStart`, `Frame`, `CodecChanged`, `GuestApprovalRequest`, `ControlState`,
`StreamStatus`, `Audio`, `Cursor`, `HostPreview`, `ProbeResult`. Either way:
`SessionEnd`, `Ping`, `Pong`.

## Adapting

The host measures capture, encode and send-blocking; the guest reports decode,
render, frames decoded, and round trip. `adaptive.rs` turns those into a
bottleneck set and a plan — a frame rate, a scale, and a bitrate — and the
`StreamStatus` message tells the guest what is holding the stream back, because
a stuttering picture and an idle bandwidth graph look identical from the guest's
side.

Two traps are worth knowing about, because both were paid for once:

- **`decoded_fps`, not `displayed_fps`.** A guest on a 60 Hz monitor cannot
  present more than sixty a second however perfect the link. Comparing against
  what it *drew* reads a healthy stream as a lossy one.
- **A guest that has not started decoding is not a failing link.** Telemetry
  flows from the moment the stream opens, well before there is a decoder, and
  every one of those reports says zero frames. Reading that as loss collapsed a
  perfectly good LAN session to 8 fps for a full minute.

## Security model

- **Encryption**: QUIC/TLS 1.3, from iroh.
- **Identity**: the endpoint id is a public key; the handshake checks it, so a
  wrong or stale address can only fail, never reach the wrong machine.
- **Authentication**: 32-byte random secrets, base58**check** encoded so a
  mangled paste fails loudly rather than dialling somewhere unintended.
- **Authorisation**: master is immediate; guest is denied until allowed, with
  mouse and keyboard granted separately and both off by default.
- **Storage**: connection codes live in the OS credential store. Never a file.

## What is published, and where

Worth being explicit, since two of these leave the machine.

| To | What | Who can read it |
|----|------|-----------------|
| Local network (mDNS) | This endpoint id and its addresses | Anyone on the same network |
| n0 lookup service | Relay + routable addresses, signed | Anyone with the endpoint id |
| Mainline DHT | The same, signed under the endpoint's own key | Anyone with the endpoint id |

The endpoint id is in every connection code, so this is readable by anyone who
has ever been given one. None of it grants access — a passcode is still
required, and approval still applies.

## Platform

| Feature | Windows | macOS / Linux |
|---------|---------|---------------|
| Screen capture | DXGI Desktop Duplication | test pattern only |
| Hardware encode | Media Foundation (H.264, H.265, AV1) | — |
| Hardware decode | libavcodec D3D11VA | — |
| Zero-copy display | `WGL_NV_DX_interop2` | — |
| Audio | WASAPI loopback + Opus | — |
| Input injection | `SendInput` | log-only stubs |
| Networking, protocol, adaptive control | ✅ | ✅ |
