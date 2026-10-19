# Architecture Overview

## Shape of the thing

```
┌─────────────────── HOST ───────────────────┐        ┌────────────────── GUEST ───────────────────┐
│                                            │        │                                            │
│  DXGI duplication, or the compositor (WGC) │        │  Decode (libavcodec D3D11VA, or CPU)       │
│      │  one capture per display, shared    │        │      │                                     │
│      ▼                                     │        │      ▼                                     │
│  NV12 on the GPU ──► hardware encoder      │        │  NV12 ──► RGBA on the GPU ──► OpenGL       │
│      │              (H.264 / H.265 / AV1)  │        │      (no readback, where it is enabled)    │
│      ▼                                     │        │                                            │
│  adaptive controller ◄── telemetry ────────┼────────┼──── measures decode, render, round trip    │
│      │  decides fps, scale, bitrate        │        │                                            │
│      ▼                                     │        │                                            │
│  video stream (uni) ───────────────────────┼───────►│  frames                                    │
│  audio stream (uni) ───────────────────────┼───────►│  Opus, into a jitter buffer                │
│  control stream (bi) ◄─────────────────────┼───────►│  input, telemetry, cursor, sharing         │
│                                            │        │                                            │
│  PasscodeStore + SessionManager            │        │  session window, one tab per session       │
└────────────────────────────────────────────┘        └────────────────────────────────────────────┘
                                      iroh QUIC, ALPN "tain/6"
                             direct only — a relayed session is refused
```

## Connecting

Six stages, and the guest's UI draws them as a row that fills up
(`gui/progress.rs`). Named here in the same order.

### 1. Start

Both sides bind an iroh endpoint through `discovery::bind`, which is
`presets::N0` written out plus two more lookups. It has to be written out
because each resolving service is wrapped so the app can tell which one
answered — see *Find* below.

### 2. Find

An endpoint id is a public key, not a route. Something has to turn one into the
other, and there are four things that can:

| Route | Cost | When it works |
|-------|------|---------------|
| The connection code | none | Always — the code carries the host's relay |
| mDNS | < 1 s | Same local network |
| The n0 lookup service | a few seconds | DNS or HTTPS reachable |
| Mainline DHT | ~8 s, sometimes two asks | Almost anywhere; no operator to block |

They race. Whichever answers first is credited, and shown to the user as the
`Find` result — see `discovery::Found`.

Nothing is remembered from last time. The address a host answered on used to
be kept and handed to the next dial, and three ways of gating it each still
broke connections that worked the moment it was deleted: an old address still
*routes*, to a NAT that now maps it to somebody else, and it could only be
refreshed by the successful connection it was preventing. See *The Remembered
Address Is Gone* in `decisions.md`.

A dial that fails is asked again, every two seconds, for up to 45 seconds, with
no single ask given more than 12. Every failure is retried, not only
`NoAddress`: a lookup that has not been told anything yet returns *empty*, not
slow, and on a filtered link a relay handshake that fails is a route that did
not work this second rather than a refusal. The slowest lookup is the one most
likely to be the only one working, and how fast the session runs afterwards
depends on the path it found, not on how long finding it took.

### 3. Direct

Every iroh connection begins on a relay and upgrades if it can. Tain waits for
that upgrade — 25 seconds on the guest, five more on the host — and **refuses
the session if it never comes**. The host waits before it reads the passcode, so
it never starts a capture, an encoder and an audio stream for a session that is
about to be refused from the far side.

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
host authenticates on the secret inside it, and rejects a code addressed to a
different endpoint. Who then has to be asked is `network::must_be_approved`,
and it is two rules, because the two codes are asking different questions:

- A **guest** code, every time. It gets nothing — no screen, no input — until a
  person allows it, and that person chooses which screens or windows it may see
  and whether it may drive the mouse and keyboard. Every outcome that is not an
  explicit allow denies: refusing, the timeout (30 seconds by default), or a
  host with nobody to ask.
- An **admin** code, the first time from each device. It is unrestricted access
  — every screen, mouse and keyboard, and the lock screen too where the service
  is installed — and codes get out: pasted into the wrong window, read off a
  screen share. So the first connection from each device has to be allowed at
  the host, which makes a leak visible. Allowing it adds the device to
  `SessionConfig::trusted_admins`, and after that it connects straight in.
  Asking every time would train the answer out of anyone using their own
  machine. Resetting the admin passcode empties the set, because the reset is
  what somebody reaches for when a code has got out.

A device is its iroh endpoint id: the public key its connection is
authenticated with, never a name or an address, which a caller chooses for
itself. That only works if the id is stable, so a machine dials with a key of
its own, derived one-way from the key it hosts on (`network::outgoing_key`). A
fresh key per dial was a stranger every time; the hosting key itself put two
endpoints on one id, and the relay keeps only one of them.

### 5. Session

The host builds its capturer first, because the capturer is the authority on the
frame size about to be announced. One that cannot start ends the session with a
reason code the guest can show, rather than a connection that simply drops.

It then answers with `SessionStart` — the codec, its name, and the displays or
windows this session may see — and `ControlState`, and opens two
**unidirectional** streams, one for audio and one for video, each of which says
what it carries in its first byte. Frames and sound go down those; everything
else stays on the bidirectional one, so a keyframe cannot queue in front of a
mouse movement or put a gap in the sound. Where a uni stream cannot be opened,
what it would have carried shares the control stream.

### 6. Video

Frames arrive, decode, and are drawn. On a machine with `WGL_NV_DX_interop2`,
and with *Draw frames on the GPU* turned on in Settings → Viewing, the decoded
Direct3D texture is handed to OpenGL directly and never touches the CPU. The
readback it saves measured 3 ms a frame on one machine and 8.8 ms on another,
and it is still off by default: the first machine to try it froze on the first
frame, and the cause found and fixed may not have been the whole of it.

## Wire format

Length-prefixed bincode: `[4 bytes big-endian length][bincode Message]`, capped
at 48 MiB so a length prefix alone cannot make a peer allocate without bound.
Each unidirectional stream opens with one byte saying what it carries
(`protocol::stream_kind`), so audio can never be decoded as video because the
two happened to open in the other order.

`Message` (see `protocol.rs`) carries, guest to host: `Auth`, `Config`, `Input`,
`Telemetry`, `FrameAck`, `RequestKeyframe`, `WatchDisplay`, `Probe`. Host to
guest: `SessionStart`, `Frame`, `CodecChanged`, `DisplayChanged`,
`SharedWindow`, `SharedChanged`, `GuestApprovalRequest`, `ControlState`,
`StreamStatus`, `Audio`, `Cursor`, `HostPreview`, `ProbeResult`. Either way:
`SessionEnd`, `Ping`, `Pong`.

`SessionEnd` carries a reason *code* and its arguments, never a sentence: it is
the one string written on one machine and read out on the other, and prose
would arrive in the sender's language. Each end words it from its own
catalogue.

bincode is positional and has no version field, so a peer handed a shape it
does not know misreads it rather than reporting a mismatch. The ALPN is bumped
whenever the format changes, which makes mismatched builds refuse each other
during the TLS handshake, with both ends saying so.

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

## What the host captures

### Two backends

Desktop Duplication first, Windows.Graphics.Capture second — see
`capture_shared::start_capturing`. Duplication is the leaner path: no capture
border, the pointer reported separately so the guest can draw it at its own
frame rate, and a frame the moment the desktop changes. It has one hard rule,
which is that the D3D device and the output must be on the adapter compositing
the desktop. On a laptop with two graphics chips the process is commonly placed
on the other one, which then claims the display and refuses to duplicate it, and
nothing inside the process can move it.

The compositor delivers frames onto whichever device it is given, so it has no
such rule, and it is what those machines fall back to. It is not the only path
because it costs something everywhere else: a yellow capture border on Windows
10, and frames on the compositor's schedule rather than the moment the desktop
changes. `TAIN_CAPTURE=wgc` forces it, to compare the two on a machine where
duplication works.

An earlier version answered the two-chip case by writing a per-application
graphics preference that pinned Tain to the integrated chip. Capture worked, and
the encoder sat on the weaker chip for the life of the installation.
`gpu_preference` now only takes that preference back, once.

Both backends are wrapped by `capture_follow`. When a lock screen or a consent
prompt takes the input to the secure desktop, frames come from that desktop's
agent until the input comes back, and nothing above the capturer can tell.

### Screens

`SessionStart` lists the displays this session may see, each with its position
on the host's virtual desktop — negative when it sits left of or above the
primary — whether it is HDR, and whether it is virtual. The guest asks for
something else with `WatchDisplay`: one display, all of them, or one shared
window (`DisplayChoice`). Anything outside the session's grant is refused rather
than guessed at, and the host answers with `DisplayChanged` whether or not it
honoured the request, so the guest's menu never shows a selection that is not
real.

*All of them* is one picture, composed by `capture_wall` the way the host has
the screens arranged. One stream rather than one per display, for three
reasons: an HDR monitor beside an SDR one needs a single answer for the
composed picture, hardware encode sessions are few, and the guest's window size
is a ceiling the picture can be shrunk to once, before the encoder, rather than
encoded at full size and thrown away on arrival.

### Windows

A window share is not a smaller screen. A window partly behind another one is
not in the desktop image at all, so it cannot be cropped out of it — only the
compositor holds a window's own pixels. Window sharing is therefore
Windows.Graphics.Capture with no duplication equivalent and no fallback, since a
window share that quietly became a screen share would break the promise it was
made on. Several windows are tiled into a grid rather than placed where they
sit, because windows overlap and two maximised ones would show as one.

A window can be minimised or closed, which a monitor cannot. Neither is a
failure: the stream pauses, and `SharedWindow` tells the guest which, so it is
not left looking at a frozen picture it cannot tell from a dead link.

What a guest may see can change mid-session, from that guest's card on the host.
`SharedChanged` replaces the whole offer rather than describing an edit — a
guest that missed one message would otherwise hold a menu built from half an
edit — and `DisplayChanged` follows it to say what is on screen now.

### Virtual displays

Two halves that meet cleanly. `tain-cli/src/display_driver.rs` installs the
vendored driver, elevated and only when asked, and makes it present a monitor
by asking its control pipe for a display count. Everything after that —
putting the display on the desktop, sizing it, taking it off again — is
`tain-core/src/virtual_display.rs`, through ordinary GDI calls that need no more
privilege than the Settings app does, and it works with any driver that has
presented a monitor.

With *Match the resolution of whoever connects*, a virtual display takes the
size of the first guest watching it, so that guest is drawn 1:1. A display can
only be one size, so a second guest does not take it away from the first, and a
real monitor is never resized at all. A virtual display does not outlive the
application that created it: closing Tain removes it, and if Tain is killed
instead, the agent notices and removes it where the service is installed.

## Unattended hosting

The service runs as LocalSystem in session zero, which has no desktop, so it
does no work there. It supervises processes in the console session instead:

- **An agent per desktop** — the ordinary one, and the secure one that the lock
  screen and UAC prompts run on. Each works out for itself whether it has the
  input. A host whose desktop has lost it takes frames from the secure desktop's
  agent over a named pipe, and sends input back the same way, because
  `SendInput` from the ordinary desktop cannot reach the secure one either.
- **The unattended host**, whenever the application is not hosting and the
  service has been given an identity to host as. It runs the application's own
  hosting path, on the ordinary desktop, with the owner's settings. Two hosts on
  one identity would be worse than none, so the application holds a lease: it
  says it is hosting every five seconds, and the service starts its own host
  only once fifteen seconds pass without hearing it, or at once when the
  application says on its way out that it has stopped. Starting up, silence
  counts as somebody hosting for the first lease. The host
  stops on a named event when the application takes over or the console session
  changes, and it sits in a job the service holds, so it cannot outlive the
  service.

What the unattended host needs arrives over the control pipe
(`service_control`): this machine's identity, its codes, and the application's
settings as JSON — the whole file, so a setting added later reaches it without
anyone remembering to forward it. *Answer when nobody is signed in* is what
sends them. With it off, nothing secret leaves the user's credential store, and
the service still covers consent prompts and the lock screen for the
application's own sessions. Both pipes admit LocalSystem and the user signed in
at the console, nobody else (`pipe_security`).

The unattended host differs from the application's in two ways, and only these.
There is nobody to approve anything, so every guest code, and every admin code
from a device not already trusted, is refused. And the guest code never
regenerates itself, because there is no screen to show the new one on. Trusted
devices come from the service's own credential store, not the settings file
beside it, since anyone who could write that file could otherwise approve
themselves.

## Security model

- **Encryption**: QUIC/TLS 1.3, from iroh.
- **Identity**: the endpoint id is a public key; the handshake checks it, so a
  wrong or stale address can only fail, never reach the wrong machine.
- **Authentication**: 32-byte random secrets, base58**check** encoded so a
  mangled paste fails loudly rather than dialling somewhere unintended.
- **Authorisation**: an admin code is let in at once only from a device already
  approved at the host, keyed by endpoint id; the first connection from any
  other device waits for a yes. A guest code is denied until allowed, every
  connection, with the screens or windows it may see chosen in the same answer
  and mouse and keyboard granted separately, both off by default. The host
  filters every inbound event against the current grants and tells the guest
  with `ControlState` whenever they change.
- **The person at the host wins**: their own mouse or keyboard makes guest input
  give way for two seconds, detected by a hook that ignores injected events.
  Releases always go through, because a dropped key-up is how a key gets stuck
  down on somebody else's machine.
- **Names are self-reported**: a name arriving from a peer is sanitised,
  length-capped, and shown as the peer's own claim, because it is drawn next to
  a button that hands over the machine.
- **Storage**: connection codes and this machine's identity key live in the OS
  credential store, never a file. Trusted devices are public keys and sit in
  `settings.json`; the service keeps its own copy in its own credential store.
- **The service runs as LocalSystem**, so it can reach screens no ordinary
  program can, and with *Answer when nobody is signed in* it holds the host key
  too. It is opt-in, and turning it off makes it forget what it was given.
- **The virtual display driver's control pipe is open to every process** on the
  machine, so anything running locally can add or remove virtual monitors. That
  is the driver's decision, not one Tain can tighten.

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
| Screen capture | DXGI Desktop Duplication, with Windows.Graphics.Capture as the fallback | test pattern only |
| Window sharing | Windows.Graphics.Capture | — |
| Virtual displays | The vendored Virtual Display Driver | — |
| Unattended access | Windows service, per-desktop agents, unattended host | — |
| Hardware encode | Media Foundation (H.264, H.265, AV1) | — |
| Hardware decode | libavcodec D3D11VA | — |
| Zero-copy display | `WGL_NV_DX_interop2`, opt-in | — |
| Audio | WASAPI loopback + Opus | — |
| Input injection | `SendInput`, `InjectTouchInput` | log-only stubs |
| Networking, protocol, adaptive control, localisation | ✅ | ✅ |
