# Design Decisions

## Default Launch Behavior: GUI-First

**Decision**: `cargo run` with no arguments launches the unified egui GUI directly (Host tab active). No subcommand required.

**Rationale**:
The app is primarily a graphical remote desktop client — the CLI modes exist as fallbacks for scripting, CI testing, or headless server use cases. Most users will want to see the window immediately.

**Gotcha**: The `--gui` flag on host/guest subcommands still works but is redundant with the default behavior. Explicit flags like `host --generate-master` bypass GUI and run CLI-only code paths.

## Project Name: Tain

The application is named **Tain** (placeholder — verify availability on app stores, domains, and existing projects before finalizing).

This file documents *why* the codebase is structured the way it is. Consult this when making changes to avoid repeating past reasoning or introducing regressions.

## Networking: iroh over libp2p / WebRTC

**Decision**: Use [iroh](https://github.com/n0-computer/iroh) 1.x for all P2P networking.

**Rationale**:
- iroh handles NAT traversal, hole punching, and relay fallback out of the box — no manual STUN/TURN configuration needed.
- Built on QUIC (via quinn), giving us multiplexed streams over a single connection with built-in TLS 1.3 encryption.
- The `presets::N0` configuration provides free public relays, so peers behind symmetric NATs still connect.
  - *Later, twice over:* the relay is no longer allowed to carry a session — see
    **A Session Runs on a Direct Path or Not at All**. And `presets::N0` is no
    longer called: `discovery::endpoint_builder` writes out what it does, so
    each lookup can be attributed and two more can be added beside them.
- iroh is actively maintained by n0.computer and has reached v1.0 (stable API).
- Compared to libp2p: simpler API surface, less boilerplate for our use case.
- Compared to WebRTC: iroh gives us raw QUIC streams which are more natural for binary protocol messaging than WebRTC data channels.

**Gotcha**: `Endpoint::bind(presets::N0).await?` returns the endpoint in one call — don't forget `.online().await` before trying to connect (ensures relay is ready). The returned `EndpointId` is the public key of the secret key used to construct it; this IS the address peers use to dial you.

**Gotcha (cost us a debugging cycle)**: `Endpoint::bind(preset)` does **not** register any ALPN. An endpoint with an empty ALPN list accepts zero incoming connections — the guest's handshake fails with `error 120: peer doesn't support any known protocol`, while the host logs look completely healthy ("Listening for incoming connections…"). The host must bind through the builder:

```rust
iroh::Endpoint::builder(presets::N0)
    .alpns(vec![ALPN.to_vec()])
    .bind()
    .await?
```

This is invisible to code review and to `cargo check`; only an end-to-end test catches it. That is why `tain-core/tests/e2e_iroh.rs` exists.

**Gotcha**: the **guest** calls `open_bi()`, the **host** calls `accept_bi()`. If both sides call the same one, nobody opens a stream and both hang forever with no error.

**Gotcha**: returning from a connection handler drops the `Connection`, which tears down QUIC immediately and can discard in-flight stream data. When the last thing you send matters — a rejection reason, say — call `send.finish()` and await `send.stopped()` before dropping, or the peer just sees "connection lost".

## Protocol: Single Bidirectional Stream per Connection

**Decision**: Each guest-host connection uses a single QUIC bidirectional stream for all messages (auth, frames, input events, control).

**Rationale**:
- Simpler than multiplexing multiple streams — one stream per logical "session" maps 1:1 to the iroh `Connection`.
- QUIC's built-in stream multiplexing means we don't lose head-of-line blocking at the transport layer.
- Length-prefixed bincode serialization on a single stream is straightforward to implement and debug.

**Alternative considered**: Separate streams for video vs input (one uni-directional, one bi-directional). Rejected because:
  - Adds complexity without measurable benefit — QUIC handles multiple streams efficiently anyway.
  - Our protocol messages are small enough that framing overhead on a single stream is negligible.

## Serialization: bincode over protobuf / MessagePack

**Decision**: Use `bincode` for all in-flight message serialization.

**Rationale**:
- Zero-copy friendly — produces compact binary output with no schema registration or code generation.
- Works directly with Rust serde derives (`#[derive(Serialize, Deserialize)]`).
- Faster than JSON/MessagePack for our use case (no string parsing overhead).
- No external dependencies beyond `serde`.

**Gotcha**: We use **bincode v1.x** (NOT v2). V2 requires a serde feature flag and has different API (`bincode::serde::*`). V1 provides flat `bincode::serialize()`/`bincode::deserialize()` functions that work directly with Rust types.

All message types must derive both `Serialize` AND `Deserialize`.

## Connection Codes: Address Travels With the Secret

**Decision**: A connection code encodes `[type byte] + [32-byte secret] + [32-byte endpoint id]` as base58**check**, prefixed `MASTER-`/`GUEST-`. `PasscodeStore` owns only the secret; `HostServer` wraps it with its own endpoint ID on the way out.

**Rationale**: the UI design has a single copy button on the host and a single paste button on the guest — no field for a ticket. That is only possible if the code carries the address. It also removes a whole class of user error: there is no way to pair a code with the wrong endpoint.

**Why base58check and not plain base58**: codes get pasted through chat clients that wrap, truncate, and mangle. A checksum turns "half a code" into a clear error instead of a code that parses and dials nowhere. `connection_code::tests` covers truncation and single-character corruption.

**Gotcha**: the prefix is a human-readable hint, the type *byte* is authoritative, and they are checked against each other. Re-labelling a `GUEST-` code as `MASTER-` does not grant anything — it fails to parse.

**Gotcha**: the host rejects codes whose endpoint ID isn't its own, even if the secret happens to validate. Pinned by `e2e_iroh::a_code_for_a_different_host_is_rejected`.

## Passcodes: Base58 with Type Prefix (not iroh tickets)

**Decision**: Generate application-level passcodes (`MASTER-xxxx`, `GUEST-xxxx`) rather than using iroh's built-in ticket system directly.

**Rationale**:
- Human-friendly format — easier to type/share over chat than raw endpoint IDs or binary tickets.
- Decouples authorization (passcode) from addressing (endpoint ID). The passcode is the *what*, the ticket is the *where*.
- Supports two authorization levels (Master/Guest) with different behavior without needing separate protocols.

**How it works**: Each passcode encodes `[type_byte: 0x00|0x01] + [32-byte random secret]` as base58, prefixed with `MASTER-` or `GUEST-`. The host validates the code against its in-memory store; if found, extracts the type byte and grants appropriate access.

**Gotcha**: Passcodes are stored in memory only — they don't persist across restarts. If you need persistent passcodes (e.g., for a server that must survive reboots), add a file-based backend to `PasscodeStore`.

## Guest Approval Gates the Stream, Not Just Input

**Decision**: a guest passcode gets **nothing** until the host allows the connection — no `SessionStart`, no capture, no frames. The host is asked, the connection handler waits, and only an explicit allow starts the pipeline.

**This replaced the opposite design.** Approval used to be non-blocking: the guest started receiving frames immediately on the theory that "view-only is a useful state", and approval only decided whether *input* was forwarded. That was wrong, and it was wrong in the direction that matters. Anyone who obtained a guest code once could watch the screen indefinitely without the host ever clicking anything — and reconnecting with the same code silently resumed the view, because nothing about the second connection required a decision. Watching someone's screen is the thing people actually want to control; input is the smaller half.

**Every path that is not an explicit allow denies:**

| Situation | Outcome |
|---|---|
| Host clicks Allow | Stream starts, with the granted devices |
| Host clicks Deny | `SessionEnd`, guest disconnected, never saw a frame |
| Host closes the dialog (Cancel) | Still waiting — no decision, nothing streamed |
| Nobody answers within the timeout | Denied |
| No approval channel attached (headless) | Denied |
| Approval receiver dropped (UI gone) | Denied |

The timeout **denies** rather than downgrading. An unattended machine must not end up sharing its screen because nobody was there to say no.

**Approval is per connection, not per code.** Reconnecting with the same passcode asks again, because it is a new session with a new id. `reconnecting_with_the_same_code_asks_again` pins it.

**Approval is keyed by session ID, not by passcode secret.** A `GUEST-` code is shared with everyone the host hands it to, so secret-keyed approval would mean approving one guest silently approves every other guest holding that code. `session::tests::approving_one_guest_does_not_approve_another_on_the_same_passcode` pins this.

**Session IDs are assigned by the host.** The `session_id` field in the guest's `Auth` message is advisory and ignored — a guest that picks its own ID could collide with, or claim the approval state of, another live session.

**"Allowed with no devices" is not the same as "denied".** `ApprovalDecision::Allow(ControlGrants::NONE)` streams the screen with no input — the default the checkboxes offer. Collapsing the two into a bare `ControlGrants` (which is what the oneshot used to carry) made them indistinguishable, and the UI showed a screen-sharing guest as "denied".

**Master codes skip all of this.** They are the "this is my own machine" path and grant full control outright.

**Gotcha**: everything between `add_session` and cleanup runs inside `run_authenticated` so that `end_session` is reached on *every* exit. A `?` in that stretch would strand a dead entry in the session table — a pill for a guest that is gone, and a `guest_session_count` that never reaches zero so the guest code never regenerates. The approval wait makes that window tens of seconds long, which is plenty of time for a guest to give up and vanish. `a_guest_that_gives_up_while_waiting_leaves_no_session_behind` pins it.

## Device Names: One Setting, Both Directions

**Decision**: a single "This PC" name setting, published to the other machine in both roles — in `Auth.client_name` when connecting out, and in `SessionStart.host_name` when accepting. Modes are Anonymous (default), system user name, network PC name, or a custom string.

**Rationale**: the two directions are the same question — "what should the other machine call me?" — and splitting them would mean setting your own name twice. Anonymous stays the default because a name is identifying information, and the fallback (`Guest #3`, `PC ABABABAB`) is perfectly usable.

**Names from the wire are sanitized and never trusted.** `sanitize_device_name` strips control characters and caps length; a newline would otherwise let a peer break out of the label it is drawn in, and an unbounded string would stretch a card off the screen. The approval dialog additionally says *"This name was chosen by the connecting machine"* — the host cannot verify it, and a guest calling itself "IT Support" must not read as though we vouched for it.

## Input Permissions: Mouse and Keyboard Granted Separately

**Decision**: `ControlGrants { mouse: bool, keyboard: bool }` rather than a single `control_granted` flag. Every inbound `Input` event is classified by `InputEvent::device()` and checked against the session's current grants.

**Rationale**: The UI has always had separate mouse and keyboard toggles, and "let them move the pointer but not type" is a genuinely useful safety posture — for demos, for guided support, for anything where a stray keystroke on the host is worse than a stray click. With one bit the two toggles collapsed into a meaningless OR.

**The host announces grants; the guest does not infer them.** `Message::ControlState` is sent at session start and again whenever grants change. Without it the guest's only feedback is that some of its events silently vanish, which is indistinguishable from packet loss or a hung host.

**Gotcha**: `SessionManager` holds a clone of each session's outbound sender so `set_control` can push `ControlState`. That means the outbound channel closing is **no longer** a reliable "guest disconnected" signal — the writer loop must watch the reader task's `JoinHandle` instead. Getting this wrong hangs the session forever after the guest leaves; `network::tests::session_ends_when_the_guest_disconnects` pins it.

## Guest Rendering: Take the Frame, Reuse the Texture

**Decision**: the session task parks the newest decoded frame in `Arc<Mutex<Option<FrameBuffer>>>`; the render pass `take()`s it and calls `TextureHandle::set`.

**Rationale**: `take()` means a repaint between frames costs nothing — no upload, no conversion. Replacing rather than queueing means a slow renderer shows the *newest* frame instead of working through a backlog of stale ones, which is what you want for remote desktop. Reusing the `TextureHandle` avoids reallocating a GPU texture every frame.

**The network task drives repaints.** It holds a callback that calls `ctx.request_repaint()` when a frame lands, so the UI redraws on arrival rather than polling at a fixed rate.

**Input is sent on change only.** egui reports pointer state every frame; forwarding it unconditionally would flood the channel at the repaint rate. Button state is diffed against what we last told the host, and gating it on `hovered` also releases a held button when the pointer leaves the view rather than stranding it down on the host.

**Gotcha**: the guest checks its own `ControlGrants` before sending input. Not for security — the host enforces that — but to avoid filling the wire with events that will be dropped.

## Sessions Get Their Own Window

**Decision**: guest sessions render in a separate viewport with one tab each, not inside the main window.

**Rationale**: two bugs came from sharing a window, and a second window removes them rather than working around them. Fullscreen still showed the app header — useless once a session is up, and a dedicated window has no header to hide. Disconnecting while fullscreen left the *main* window stuck fullscreen with nothing to click; fullscreen now belongs to a window that simply closes. Tabs then fall out for free, which is what makes several concurrent sessions possible.

**It is an `show_viewport_immediate`, not deferred.** A deferred viewport's callback must be `Send + Sync + 'static`, and the render pass needs `&mut` access to the sessions it draws.

**Gotcha**: shortcuts are handled inside the session window's own `Context`. An immediate viewport has a separate input state, so reading them from the parent would miss every press made while the session window had focus.

**Gotcha**: `SessionWindow::reset` clears fullscreen when the last session closes. Leaving it set would make the next session open fullscreen for no reason the user asked for — pinned by `session_window::tests::resetting_clears_fullscreen_so_the_next_session_opens_windowed`.

## Guest Lifecycle: Reconcile, Don't Notify

**Decision**: the GUI diffs its incoming list against `SessionManager::active_sessions()` every frame instead of receiving a "session ended" message.

**Rationale**: a guest disconnecting, timing out, being kicked, or the server dropping a session all converge on one code path. The notification approach needs a message per event and silently rots when a new way to end a session is added — which is exactly what happened: a disconnected guest kept its pill and green dot as though still connected.

**Gotcha**: a *pending* card keeps its status rather than mirroring grants. Grants are `NONE` while pending, so mirroring them would flip the card to "denied" before anyone had a chance to allow it.

**Gotcha**: reconciliation is skipped under `TAIN_DEBUG=demo`, since demo rows have no real session and would be deleted on the first frame.

## Approve and Kick Are Confirmed

**Decision**: the person icon accepts a pending guest; hovering it on an established one swaps to the kick icon. Both raise a confirmation dialog.

**Rationale**: the device toggles were the only way to admit a guest, which made granting the mouse and *accepting the connection* the same gesture — and there was no way at all to remove a connected guest. Approving hands control of the machine to someone else and kicking ends their session; both deserve a deliberate second click.

**Gotcha**: kicking answers any outstanding approval oneshot with `NONE`. Dropping it instead would leave the server's approval task waiting out its full timeout on a session that no longer exists.

## Release Builds Have No Console

**Decision**: `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`, plus `AttachConsole(ATTACH_PARENT_PROCESS)` at startup.

**Rationale**: a GUI app spawning a black terminal beside itself looks broken. Attaching to the *parent* console means running the binary from a terminal still prints where you would expect, without ever creating a window. `--console` or `TAIN_CONSOLE=1` allocates one for diagnosing a double-clicked build.

**Gotcha**: the tracing subscriber is only installed when stdio is actually connected. Formatting log lines into a void is pure waste.

## One Priority Slider, Not Four Independent Knobs

**Decision**: a single 0-100 slider from *image quality* to *low latency*, driving resolution, frame rate, quantiser and keyframe interval together. It replaced a standalone "stream quality" dropdown.

**Rationale**: those settings are not independent. Getting latency down means spending less time per frame, which means fewer pixels *and* less encoder search *and* a shorter queue — all at once. Exposing them separately asks the user to solve a constrained optimisation the app already knows the answer to, and the old dropdown asked them to pick one input to a trade-off without ever showing them the trade-off.

**The boundaries come from measurement, not taste.** On a 2560x1600 display the software encoder costs ~45 ms per frame at native resolution against a 33 ms budget for 30 fps, and ~26 ms at 75%. So the midpoint is 75%, not native: a default that cannot hold its own target frame rate is a bad default however good it looks written down. Ask for quality explicitly and you get every pixel, and the trade is then yours rather than a surprise.

**Resolution always follows the slider, even with "let the slider set the details" off.** It is the lever the trade-off is actually about — releasing it would leave a slider that moves without doing much.

**The bitrate limit does not follow it.** That is a constraint imposed by the *link*, not a position on the trade: a metered connection does not become less metered because you dragged towards quality.

**Gotcha**: `resolution_scale` reuses `QualityScale` rather than introducing a second scaling path. The rounding rules there — even dimensions, minimum size — are subtle enough that two implementations would eventually disagree, and an odd dimension reaching the H.264 encoder is an error rather than a rounding detail.

## Secrets Live in the OS Credential Store

**Decision**: connection codes are stored via `keyring` in the platform credential store — Windows Credential Manager, macOS Keychain, Linux Secret Service. `connections.json` holds only non-secret metadata.

**This replaced plaintext storage in `connections.json`.** A connection code *is* the shared secret that reaches a host, so the previous design left a file of working keys in the user's config directory: readable by any process running as them, and swept up by any backup or synced profile folder. Defensible for one developer's machine; not defensible for something people install, and certainly not for a public repository.

**Failure degrades to "no saved code", never to a fallback file.** A headless Linux box with no Secret Service cannot remember codes, and that is fine — the UI already has a `NoCode` state. Silently downgrading encrypted storage to a plaintext file would reintroduce the exact bug being fixed.

**Old files are migrated on load**, then rewritten without the codes. `legacy_code` is `skip_serializing`, so simply saving the file is what removes the plaintext. Leaving it in place would keep working secrets on disk for everyone who upgrades.

**Gotcha, and it bit twice**: the store's mutating methods persist as a side effect, so tests that used the real backends wrote into the developer's own machine — 62 junk rows in `connections.json`, then 87 stray credentials in Credential Manager. Namespacing test ids and cleaning up afterwards was **not** enough; one test that forgot to opt in put them all back. The fix both times was to make the *type* carry the decision: `ConnectionStore::new()` has `path: None` and an in-memory `SecretStore`, with no path from there to the real ones. If you add a backend that persists, give it the same seam before writing the first test.

## Saved Connections: Keyed by Endpoint, Storing the Code

**Decision**: hosts the guest has connected to are remembered in `connections.json`, next to `settings.json`. Rows are keyed by the host's **endpoint id**, and each stores the connection code last used to reach it.

**Rationale for the key**: a host regenerates its guest code constantly (that is the default), so keying on the code would grow a new row on every reconnect. The endpoint id is the machine.

**Rationale for storing the code**: without it a row is a history entry, not a connection — there would be nothing for the play button to do. The code *is* the shared secret, so `connections.json` is as sensitive as a saved password. Two things bound the exposure and both are worth knowing: a host with guest-code autoreset on invalidates its code the moment you disconnect, and approval is per connection, so even a valid stored code cannot get back in without the host saying yes again. Forgetting a row deletes the code with it.

**Recorded on attempt, not on success.** The reported bug was that disconnecting left no trace of the host at all. Recording when the connection is *attempted* means a failed or refused attempt still leaves a row to retry from, which is when you most want one.

**Status is derived, never stored.** `#[serde(skip)]` on the status field, recomputed each frame from the open sessions. The host-side pill had exactly this bug — a stored status that got stuck showing a live connection after the session ended — and storing it here would have reproduced it.

**No status is claimed without evidence.** This went wrong twice in opposite directions. First the card said "offline" beside a working connect button; the fix removed the offline state entirely, and the card then said "ready" with a green arrow for a host that had been switched off for a week — because holding a code was the only evidence available. The answer was to get real evidence: see *Saved Connections Are Probed, Not Guessed*. `Unknown` covers the gap before the first probe lands, and it says "checking..." rather than making a claim.

**Colour tracks confidence; the label tracks state; pressability is separate from both.** The old card coloured the triangle by status, so a perfectly usable row got a red triangle that reads as "broken". Green now means *measured and confirmed*, and everything else is grey or amber — but an unreachable or stale row is still pressable, because the probe is a snapshot and the machine may have come back in the last few seconds. Refusing the attempt on our own guess would be the same overreach in a new place.

**A published host name never overwrites a name the user typed.** `adopt_host_name` only replaces the generated `PC ABABABAB` placeholder. Renaming a machine is deliberate, and losing it on every reconnect would make the rename pointless.

**Gotcha**: a store only writes to disk if it was **loaded** from disk — `ConnectionStore::new()` carries no path and `save()` is a no-op for it. The mutating methods persist as a side effect, so before that distinction existed the unit tests wrote their fixtures into the developer's own `connections.json`; 62 rows named "PC" appeared in the running app. Any test touching the store must build it with `new()`, and `AppState::new()` (which loads the real file) has to be overridden — see `app_with_scratch_connections`.

## Scaling: Fit or 1:1, and 1:1 Pans

**Decision**: two scaling modes. `Fit` letterboxes; `Actual` is one remote pixel per screen pixel and **auto-pans** when the pointer nears an edge. Stretch was removed.

**Rationale**: a distorted desktop is never what anyone wanted, so stretch was a mode that existed because it was easy rather than because it was useful. The real choice is "see all of it" versus "see part of it sharply" — and the second is only usable if you can reach the part that is off-screen.

**Pan behaviour**: the outer 20% on each side pulls the view, ramping linearly from zero at the inner boundary to full speed at the edge. The dead zone in the middle 60% is what stops the view drifting while you are simply using the remote machine. The pan is clamped to `pan_limit` — whatever does not fit — so a stale offset from a larger frame can never scroll the image into nothing, and an axis that already fits stays centred.

**Gotcha**: removing an enum variant would normally cost the user every other preference in the file. `#[serde(default)]` covers *missing* fields, not unparseable ones, so an old `"scaling_mode": "Stretch"` would fail the whole `Settings` struct and silently reset everything. `ScalingMode` therefore has a hand-written `Deserialize` that falls back to `Fit` on anything unrecognised.

**Gotcha**: panning is driven by the pointer, not by frames, so the session window has to request a repaint while a pan is possible. Without it a still screen scrolls one step and stops.

## GUI Layout: Fixed Top/Bottom Panels, Flexible Middle

**Decision**: The header is a `TopBottomPanel::top` with `exact_height`, the incoming-connection strip is a `TopBottomPanel::bottom`, and previous connections live in the `CentralPanel` with `ScrollArea::auto_shrink([false, false])`.

**Rationale**: Previous connections is the section the user actually works in, so it should absorb all spare space. Rendering all three bands inside a single `CentralPanel` lets the header size itself to its contents and squeeze the middle down to a sliver.

**Gotcha**: the header must fit the width it is *given*. At 175% display scaling a 1200pt window is only ~690 logical points wide, and fixed button widths pushed the Connect block off the right edge. `theme::passcode_button_width` derives the button size from `ui.available_width()`; `theme::tests::header_fits_the_width_it_is_given` pins it.

**Gotcha**: inside a `horizontal_wrapped` row, use a child `Ui` (`ui.new_child`) for a card's internals — **not** `ui.put`. `put` allocates in the *parent*, which grows the row's used rect and staircases every subsequent card progressively downward.

**Gotcha**: anchor a card's regions to its rect rather than flowing them top-down. Flow layout let the name field's real height push the status bar out through the bottom border.

**Icons** come from `tain-cli/assets/*.svg` via `egui_extras` with the `svg` feature; `egui_extras::install_image_loaders` must be called before the first `include_image!`. The palette in `theme.rs` is lifted from those SVGs so hand-drawn chrome matches imported art.

**Window decorations stay native.** Matching the mockup's integrated title bar would mean `with_decorations(false)` plus hand-rolled drag and resize, costing Windows snap layouts and resize borders. Not worth it — so the header carries no logo row (the mark is the window icon instead, see `gui/icon.rs`) and settings sits in its own rule-separated section pinned to the right window edge.

**The window icon is rasterised by hand** rather than through an SVG library. `usvg` 0.37 splits parsing across several crates, and the mark is three flat shapes in one colour — `gui/icon.rs` fills them directly with 4× supersampling. Keep it in sync if `logo.svg` changes; its tests assert specific pixels.

**Clipboard**: egui only surfaces paste as a keyboard event, so the design's paste *button* needs `arboard` to read the clipboard on demand.

## Session Loop: Separate Reader Task and Writer Loop

**Decision**: Each session runs a reader task owning the `RecvStream` and a writer loop owning the `SendStream`, connected by an mpsc channel. They are *not* merged into one `select!`.

**Rationale**: `read_stream_message` is **not cancel-safe** — it reads a 4-byte length prefix and then the body as two separate awaits. Racing it in a `select!` against a frame ticker can drop the future between those two reads, leaving the stream desynchronised and every subsequent message garbage. Giving the reader exclusive ownership removes the hazard; the writer races only `mpsc::Receiver::recv` and `Interval::tick`, both of which *are* cancel-safe.

**Gotcha**: the frame pump must not live inside the read loop. It originally did, which meant frames were only emitted after the guest sent something — and the guest sends nothing after `Auth`, so a connected guest received exactly zero frames. `network::tests::host_streams_frames_without_the_guest_sending_anything` pins this.

## Capture Resolution: Derived From the Display, Never Chosen

**Decision**: `CaptureConfig` has no width/height. It carries a [`QualityScale`] — Native / 75% / 50% / 33% — applied to whatever `primary_display_size()` reports.

**Rationale**: a remote desktop shows the host's screen, so the native resolution is the only correct starting point. An earlier version exposed a capture-size dropdown, which was really a bandwidth workaround wearing a feature's clothes: it let you "capture" at a resolution the host does not have.

**Gotcha**: scaled dimensions always round **down to even**. Odd width or height breaks chroma subsampling in every real codec. This means Native on an odd-sized display legitimately drops one row or column — `settings::tests::native_quality_streams_the_full_display` allows exactly that tolerance.

**Gotcha**: `GetSystemMetrics` reports *virtualised* pixels to a process that has not declared DPI awareness — a 2560×1600 display at 175% scaling comes back as 1463×914. The app sees real pixels because winit declares per-monitor awareness during window creation; a bare `cargo test` binary does not. Do not assume the two agree.

## Screen Capture: DXGI Desktop Duplication

**Decision**: capture through `IDXGIOutputDuplication` using the `windows` crate, rather than `scap` (dependency conflicts) or `windows-capture` (another crate).

**Rationale**: the `windows` crate was already a dependency for input injection, so this adds no new supply chain. Desktop Duplication also hands over the *already-composed* desktop, so there is no window enumeration and no redraw cost — it is what most remote desktop software on Windows uses.

**Gotcha (cost a debugging cycle)**: `AcquireNextFrame` also fires for **pointer-only** updates, where `LastPresentTime == 0` and the returned texture holds no desktop content. Copying it yields a blank frame — the first version captured a flat grey screen and looked like a broken staging copy.

**Gotcha**: `RowPitch` is almost never `width * 4`. The driver pads rows for alignment, so copying the mapping in one block produces a skewed image. Rows must be copied individually.

**Gotcha**: duplication is *lost*, not interrupted, by resolution changes, driver restarts, UAC prompts, fast user switching, and the lock screen — all as `DXGI_ERROR_ACCESS_LOST`. The only fix is rebuilding the whole chain. `DxgiCapturer` does this transparently and keeps serving the last good frame meanwhile, so a UAC prompt does not tear down the session.

**Gotcha**: the capturer is the authority on frame size, not the config. It is built *before* `SessionStart` is sent, because a fallback to the test pattern or a display that changed since the settings were written would otherwise make the host advertise dimensions its frames do not match.

## Video Pipeline: H.264 by Default, Lossless as an Option

**Decision**: the default codec is **H.264**, encoded in software by OpenH264 (`openh264` crate, `source` feature). `DeltaZstd` and `Raw` remain selectable.

**This replaced delta+zstd as the default.** The lossless path is genuinely excellent on a *static* screen — an XOR delta against the previous frame is nearly all zeroes and compresses to almost nothing. The trouble is that its cost is proportional to how much of the screen changed, so the moment anything moves (a video, a scrolling page, a dragged window) the delta stops being zeroes and bandwidth goes through the roof. At 1080p that is the difference between negligible and unusable. H.264 is roughly an order of magnitude smaller on real desktop use and holds a bitrate rather than tracking the content.

**Software, not hardware.** OpenH264 works on every machine with no driver dependency, which is what a "does this work at all" baseline needs. Hardware encoders (NVENC, Quick Sync, AMF, Media Foundation) are each a separate integration with their own failure modes. `EncoderBackend::Hardware` is therefore a *preference* that currently still runs on the CPU — and both the log and the settings panel say so, because letting someone conclude their GPU is broken is worse than admitting the feature is unfinished.

**Patents**: OpenH264's source is BSD licensed, but H.264 itself is patent-encumbered, and Cisco's royalty coverage only applies to the binaries Cisco distributes — not to a copy built from source, which is what the `source` feature does. Fine for personal and testing use; a licensing question, not a code question, if this is ever shipped commercially.

**Unimplemented codecs fall back to H.264, not to raw.** Degrading to raw would mean an unavailable choice silently costs hundreds of MB/s — worse than whatever it stood in for. H.265/AV1/VP9 are all lossy, so H.264 is the nearest neighbour.

**`is_lossy()` is the gate for bitrate and quality controls, not `is_compressed()`.** The delta path compresses but cannot hit a bitrate target — its size is whatever the screen did. Offering it a bitrate limit would be a lie.

**Gotcha**: the encoder is built lazily on the first frame, because the sensible default bitrate depends on the resolution, which capture — not configuration — decides. It rebuilds when the frame size changes.

**Gotcha**: H.264 needs even dimensions for 4:2:0 chroma. `QualityScale::apply` already rounds down to even (including at `Native`, which is why an odd-width display gets a `Scaled` wrapper rather than passing through), but the encoder rejects odd input rather than producing garbage.

**Gotcha**: the *decoder* is the authority on frame size, not the frame header. A lossy codec pads to whole macroblocks, and `write_rgba8` panics on a length mismatch, so the target buffer is sized from what the decoder actually produced.

**Gotcha**: an H.264 payload can legitimately produce no picture — parameter sets, or a frame the encoder skipped to hold its bitrate. `Ok(None)` has to mean "keep reading", not "decode failed", or a bitrate-limited session tears itself down.

**Gotcha**: `adaptive_quantization` and `background_detection` default to on and are both unsupported in screen-content mode. Leaving them set makes OpenH264 print a warning per session and then disable them anyway.

## The nasm Trap: a 17x Silent Regression

**If you read one thing in this file, read this one.**

`openh264-sys2` builds OpenH264 from source and uses `nasm` to assemble its SIMD kernels. When `nasm` is missing it does **not** fail. It prints one line into build output nobody reads and silently produces a plain-C build. Its own source comments the decision: *"is it a good idea to 'silently' disable assembly if this fails?"*

Measured on a 2560x1600 desktop, one frame:

| Build | Encode time | Effective rate |
|---|---|---|
| No assembly (default when nasm is absent) | **1147 ms** | 0.8 fps |
| With assembly | **67 ms** | 15 fps |

The symptoms do not point at the encoder. The stream crawls, the pointer lags seconds behind, and **the bandwidth graph looks idle** — because nothing is being produced to send. It reads as a network problem, and every hour spent looking at the network is wasted. This cost a full debugging session before measurement found it.

Two defences, because one was clearly not enough:

- `.cargo/config.toml` points `nasm-rs` straight at the binary, so the fix lives in the repository rather than depending on each machine's PATH.
- `tain-core/build.rs` checks for the same binary and prints a loud multi-line `cargo:warning` if it is missing, naming the exact slowdown and the install command. A warning rather than a hard error: a slow encoder still works, and blocking the build would be wrong for someone who knowingly accepts it. `OPENH264_NO_ASM=1` silences it.

**The general lesson**: a dependency that degrades silently is worse than one that fails. When a build script says "not using X", treat it as an error until proven otherwise.

## Capture and Encode Do Not Belong on the Async Runtime

**Decision**: each session runs capture and encode on a **dedicated OS thread**, handing finished frames to the writer over a bounded channel.

**Rationale**: capturing and encoding is synchronous CPU work — around 95 ms for a 2560x1600 frame even with assembly enabled. Running it inline on a tokio task, which the first version did, blocks that worker thread for the whole duration. The consequences are indirect and badly misleading: iroh's own networking and the session's input reader are starved, so the pointer lags, frames trickle, and the bandwidth graph again looks idle. Two separate causes, one identical-looking symptom.

**The channel is bounded (depth 2), and that is the point.** It applies backpressure: a slow link stops the encoder rather than accumulating frames that are stale before they are sent. Depth 2 keeps one frame in flight and one ready — enough to overlap encode with send, while every extra slot would be a whole frame of added latency when the link is the bottleneck.

**Frames are never dropped.** With an inter-frame codec, discarding one corrupts everything until the next keyframe, so "skip a frame to catch up" is not available. Blocking the producer is the correct response, and the pacing loop then simply starts the next frame late rather than running a burst to catch up.

**Gotcha**: an encoder that skipped a frame to hold its bitrate returns an empty payload. The pipeline drops it there — sending it would look like a decode failure to the guest.

## Colour Conversion: Hand-Rolled, Pinned Against the Original

**Decision**: BGRA to I420 is converted by `I420Frame::read_bgra` rather than the `openh264` crate's `YUVBuffer::read_rgb`.

**Rationale**: the crate's version measured **26 ms** on a 2560x1600 frame — a quarter of the entire frame budget — for arithmetic that is not expensive. Three avoidable reasons: it works in `f32`, it calls through a closure per plane per pixel, and it iterates **column-major over a row-major image**, so nearly every read misses cache. The replacement does identical arithmetic in integers, in row order, two rows at a time so the 2x2 chroma average falls out of the same pass. That took the full-resolution frame from 103 ms to 85 ms.

**It uses OpenH264's own coefficients** (66/129/25 luma, BT.601 limited range) and is pinned against the crate's implementation by `the_converter_agrees_with_the_one_it_replaced`, which allows ±2 for integer-versus-float rounding and nothing more. The risk in hand-rolling a colour transform is not a crash, it is a subtle tint that nobody can trace back — so the test exists to make drift impossible rather than unlikely.

**Gotcha**: `MAX_MESSAGE_SIZE` had to grow from 10 MiB to 48 MiB. Uncompressed 4K BGRA is 33 MiB, and a compressed keyframe of a photographic desktop is a large fraction of that. The old cap was fine for a 640×360 test pattern and would have killed the first real session at native resolution.

**Gotcha for tests**: a flat or short-period buffer compresses to almost nothing on its own, so any keyframe-vs-delta ratio measured against one passes trivially and proves nothing. The compression tests use pseudo-random pixels for exactly this reason.

**Decision**: `VideoCodec` declares the codecs a remote desktop realistically needs (DeltaZstd, Raw, H.264, H.265, AV1, VP9) alongside encoder backend, rate control, bitrate and keyframe interval. H.264, DeltaZstd and Raw have backends; the rest carry an honest `Availability::Planned`.

**Rationale**: the settings surface is where the shape of the pipeline gets decided. Declaring it up front means adding a backend is filling in `create_encoder`, not redesigning the config, the persistence format and the UI at the same time. Unimplemented codecs are visible but unselectable, so the roadmap is legible without lying about what works.

**Gotcha**: `EncodeConfig::codec` is what was *requested*; `effective_codec()` is what will actually run. Always advertise `effective_codec()` in `SessionStart` — an earlier version told the guest "h264" while sending raw pixels.

**Gotcha**: raw is uncompressed, so bitrate limit, rate control and keyframe interval do nothing. The panel greys them out and states the real cost instead, because raw at native resolution is 400+ MB/s and that surprises people.

Adding a codec means implementing `VideoEncoder`, adding a `FrameDecoder` arm on the guest, and flipping its `Availability` — no protocol changes. `network_bridge::tests::every_available_codec_has_a_decoder` fails if a codec becomes selectable on the host without the guest being able to decode it.

**Resolved**: hardware encoding, on Windows — see below. VideoToolbox (macOS) and VAAPI (Linux) still need their own `VideoEncoder` implementations.

## Hardware Encoding: Media Foundation, Not a Vendor SDK

**Decision**: H.264 is encoded on the GPU through a Media Foundation transform, with the OpenH264 software encoder as the fallback. `EncoderBackend::Auto` (the default) prefers hardware; `Software` forces the CPU; `Hardware` fails rather than silently downgrading, because the label promises otherwise.

**Rationale**: this is the difference between native resolution working and not. Measured on a 2560x1600 display, worst-case content:

| | ms/frame | frame-rate ceiling |
|---|---|---|
| Software (OpenH264, SIMD + multi-slice) | 35–45 | 22–28 fps |
| Media Foundation (AMD VCN) | **6.0** | **166 fps** |

NVENC or AMF directly would be quicker to get working and useless to everyone with the wrong GPU. Media Foundation enumerates whatever encoder the machine has — NVIDIA, AMD, Intel — behind one interface that ships with Windows. For an app installed on hardware you have never seen, "works on all of them" beats "optimal on one of them".

**Async transforms are not optional ceremony.** Hardware MFTs are almost always asynchronous: you do not call `ProcessInput` when you like, you wait for `METransformNeedInput`. Two consequences that cost real debugging time:

- A synchronous drain loop deadlocks. Worse, calling `ProcessOutput` without a matching `METransformHaveOutput` returns `E_UNEXPECTED` — indistinguishable from a real fault. So `process_output` deliberately is **not** a loop; the caller decides, and only the synchronous path loops.
- Surplus `NeedInput` requests must survive the call that observed them (`pending_input_requests`). Dropping one desynchronises us permanently: the transform waits for a frame against a request we threw away, and the next blocking wait never returns.

**The encoder blocks only for permission to feed, never for output.** A hardware encoder typically emits frame N once frame N+1 has gone in, so waiting for output after feeding would hang the capture thread on a frame that is never coming. The cost is one frame of pipeline latency and an empty packet or two at session start, both of which callers already handle.

**A per-size, per-session fallback, not a one-off check.** A hardware encoder is not simply present or absent: it has a minimum frame size, and — far more relevant here — a cap on concurrent encode sessions. Several guests at once each build an encoder, and on consumer hardware the third or fourth is refused while the first three work. `HardwareH264Encoder` drops that session to the CPU rather than disconnecting a guest for a reason they cannot act on.

**Two interop bugs made every frame undecodable, and neither is visible from the encoder's side.** Both were caught by running the app, not by the tests, because the first version of the round-trip test only checked that *bytes* came out. It now requires a **picture out of the decoder**, which is the only assertion that would have caught either.

- **Profile.** The output type asked for Main. OpenH264 — which is what the guest decodes with — supports Constrained Baseline only, and Main brings CABAC and B-frames with it. The failure surfaced as a bare `dsOutOfMemory` from the decoder, pointing nowhere near the profile. Baseline is the right shape for an interactive stream regardless: B-frames buy compression by adding latency.
- **Access Unit Delimiters.** AMD's MFT emits a NAL of type 9 in front of every frame. It is legal H.264 and carries no picture data, and OpenH264 fails the whole access unit on seeing one. `strip_unsupported_nals` drops types 9 and 12 (filler) before the bytes go on the wire.

**Gotcha**: most of what a CBR hardware encoder produces on simple content is **filler**. A 5426-byte frame became 164 bytes once type 12 was stripped — that is not data loss, it is padding the encoder added to hold the bitrate.

**Gotcha**: the colour conversion must agree with the software path exactly. `nv12_and_i420_agree_on_colour` pins them together — a mismatch would tint the picture depending on which encoder happened to be chosen, which is close to untraceable.

**Gotcha**: NV12 is written **directly into the media buffer**, and the conversion is split across threads. At 1600p that conversion was ~7 ms of the original 10.7 — most of the frame cost, on the CPU, for a GPU encoder. Removing the staging copy and threading the loop took it to 6.0 ms.

**Still open**: zero-copy. Handing the encoder the D3D11 texture Desktop Duplication already produced would remove the readback *and* the conversion. It needs the MFT bound to a device manager and the capture backend to surrender its texture.

## `primary_display_size` Must Report Physical Pixels

**Decision**: query `EnumDisplaySettingsW`, not `GetSystemMetrics(SM_CXSCREEN)`.

**Rationale**: a process that has not declared DPI awareness is told a 2560x1600 display at 175% scaling is **1463x914**. Believing it meant "native resolution" streamed 31% of the pixels the user actually had, and looked soft for no visible reason. `EnumDisplaySettings` reports the display *mode*, which is never virtualised, so the answer does not depend on when — or whether — anything declared DPI awareness.

**Gotcha**: capture itself was never affected, because DXGI reports its own size and `create_capturer` scales against that. Only the settings-panel estimate and the measurement harness were wrong — which is exactly why it went unnoticed, and why the harness was measuring the wrong resolution.

## Quality Adapts to Measurement; the Slider Only Says What to Sacrifice

**Decision**: `AdaptiveController` starts every stream at the user's stated ceiling and lowers bitrate, resolution or frame rate **only** when a stage is measured to be behind — restoring them when it clears. `LatencyPriority` sets no stream value at all; it decides what is given up first and how quickly.

**Rationale**: the first version mapped the slider straight onto a resolution and frame rate, so asking for low latency permanently threw away two thirds of the pixels whether or not anything was struggling. That is backwards. A stream is *supposed* to run at what was asked for; degrading it is a response to a problem, not a setting. The priority answers a different question — when something has to give, does detail go or does smoothness?

- towards **quality**: the frame rate sags, the pixels stay
- towards **latency**: pixels go, the frame rate holds

Both ends still have floors on the axis they protect, because holding native resolution at 2 fps serves nobody either.

**Bitrate first for a slow link, pixels first for a slow encoder.** Encode and decode cost is per *pixel*; a bandwidth limit binds on *bits*. Lowering the bitrate for a slow encoder leaves it doing exactly as much work, and shrinking the picture for a bandwidth problem costs detail that a bitrate cut would have covered. The controller reads which one is actually happening.

**Changes are spaced and hysteretic.** A resolution change rebuilds the encoder and forces a keyframe, so `MIN_DWELL` (1.5 s) bounds how often anything moves, `strain_tolerance` requires several consecutive bad windows before acting, and `recovery_patience` is always larger — recovering as fast as it degrades makes a marginal link visibly pump between two quality levels, which is more distracting than sitting one step low.

**A long round trip is reported and never acted on.** Shrinking the picture does not shorten a round trip. `Bottleneck::is_actionable` is what keeps the controller from spending quality for no benefit.

## An Idle Desktop Must Cost Nothing

**Decision**: Desktop Duplication returning "nothing changed" sends **nothing**, apart from one refresh a second.

**Rationale**: it used to re-send the last frame on every tick. A session watching a still screen therefore re-cloned 16 MB, re-encoded it and re-transmitted it sixty times a second — measured at **28.8 Mbit/s to transmit a screen that was not changing**. On a link that could not carry that, the bounded frame queue filled, the capture thread blocked on `blocking_send`, and the picture stopped. The user saw a frozen stream reporting "slow network", and the network was fine: the cause was an idle screen. Measured after the fix: **0.4 Mbit/s**.

The once-a-second refresh remains for one reason only — a guest connecting to an idle machine needs *a* picture, and a desktop can sit unchanged for minutes.

**Gotcha**: `last_frame` is `Bytes`, not `Vec<u8>`. At 2560x1600 that is a 16 MB memcpy per tick if it is cloned, which is what it was.

## Waiting Is Not the Same as Being Slow

**Decision**: `ScreenFrame::cost` carries the time the backend spent *producing* the frame, and the bottleneck detector uses that rather than wall-clock time around the call.

**Rationale**: `AcquireNextFrame` blocks until the desktop changes. Timing the call from outside therefore charges an idle screen the full wait — 16 ms against a 16.7 ms budget — and reports it as **slow capture**: a machine doing nothing, accused of not keeping up. Only the backend can separate the wait from the work, so only the backend can report it.

## `thread::sleep` Is the Wrong Tool for a Sub-Millisecond Wait

**Decision**: the async-MFT wait yields for its first 20 ms and only then sleeps.

**Rationale**: Windows' default timer resolution is 15.6 ms, so a `thread::sleep(200µs)` in the encoder's poll loop really slept for milliseconds. Measured cost: encode went from 5.4 ms to **25 ms per frame** — four times the 60 fps budget — caused entirely by the waiting strategy and nothing to do with the encoder. `yield_now` gives the core up at microsecond granularity, which is what a wait this short needs. After 20 ms the frame is late regardless, so it switches to sleeping rather than burning a core until the stall timeout.

**Gotcha**: the wait is bounded (`ENCODER_STALL_TIMEOUT`) because `GetEvent` has no timeout of its own. A wedged driver used to hang the capture thread forever, which presents as a freeze with nothing in the log — indistinguishable from a slow network.

## Capture and Encode Overlap

**Decision**: capture runs on its own thread and hands frames to the encode thread over a depth-1 channel.

**Rationale**: run in sequence, the frame rate is bounded by their **sum** — measured at 5.1 ms capture plus 5.4 ms encode, or 10.5 ms, which clears the 16.7 ms budget at 60 fps but not the 8.3 ms one at 120. Overlapped, it is bounded by the slower of the two. Per-frame *latency* is unchanged, because a frame still waits for its own capture and its own encode; only throughput improves.

**Dropping is allowed here and nowhere else.** If the encoder is still busy the newest frame is discarded rather than queued, because the next capture will be fresher than a frame waiting behind another. This is safe only *before* the encoder — once a frame has been encoded, dropping it corrupts everything until the next keyframe.

**Not a bottleneck, despite appearances**: input sharing a stream with video. Input travels guest-to-host and frames host-to-guest, which are opposite halves of one bidirectional QUIC stream — independently flow-controlled and independently ordered. They cannot block each other, so moving input to its own stream buys nothing.

## Rate Control: Measure Against Content That Resembles a Desktop

**Decision**: the bitrate measurement in `pipeline_timing` uses a mostly-static frame with one moving region.

**Rationale**: measured against a full-screen change every frame, the hardware encoder appeared to ignore its bitrate limit by 2x to 5.4x while the software encoder tracked it exactly — which reads as a serious bug, and nearly got "fixed" as one. Against desktop-like content the same encoder tracks the limit fine (0.56x-1.90x). The difference was entirely the test content: a full-screen flip sixty times a second is worse than anything a desktop does, and the software encoder only appeared to cope because it drops frames to hold its rate and the hardware one does not.

The general lesson is worth keeping: **a synthetic benchmark that does not resemble the real workload can manufacture a bug that is not there.**

## The Host Paces Itself to the Guest

**Decision**: the guest acks each frame it displays, and the host stops encoding once it is more than a window ahead of those acks.

**Rationale**: without it, a guest that decodes slower than the host encodes builds an **unbounded backlog**. QUIC is reliable and its send buffer is megabytes, so the host cheerfully queues seconds of video the guest works through long after it stopped being useful. Reported from a real two-machine session: roughly one frame a minute, with the guest still drawing buffered frames *after the host application had been quit* — which is what proved the backlog was real rather than the link being slow.

Neither existing mechanism catches this. The bounded frame channel only bounds the host's **own** queue; the adaptive controller lowers quality rather than slowing down, and a smaller frame sent too often still overruns a guest that cannot keep up.

**The window scales with the round trip** (`send_window`), because frames in flight to a distant guest are mostly propagation delay. A fixed count would cap throughput at one frame per RTT — 5 fps on a 200 ms link. It is still clamped, and that clamp is the actual latency guarantee: the host is never more than `SEND_WINDOW_MAX` frames ahead of what the guest has shown.

**Dropping happens before the encoder, and only there.** No inter-frame state exists yet, so the next frame encoded still references one the guest has. Dropping after encoding would corrupt everything until the next keyframe.

**Gotcha**: a guest that has **never** acked is not throttled at all. Acks were added after the fact, and starving an older guest of every frame after the first two would be a worse failure than the backlog. A guest that acked and then went silent *is* throttled — that is the correct response to a peer that stopped consuming — and the guest's own silence watchdog tears the session down.

## The Session Says What It Is Doing

**Decision**: the toolbar shows measured fps, round trip, actual bitrate, and whether the path is direct or relayed.

**Rationale**: the first real two-machine test was unusable, and had to be diagnosed from log files, because the window showed nothing at all. Every number that turned out to matter — what frame rate was really reaching the screen, what the round trip was, how much video was actually arriving — existed inside the app and was not displayed. These are what is *measured*, never what was requested: a session crawling while the host believes it is sending 60 fps is exactly the case they exist to expose.

**A relayed path gets its own warning**, because it is the single biggest determinant of whether a session works and no quality setting compensates for it. iroh falls back to a shared public relay when hole punching fails; that is fine for control traffic and hopeless for video. `Connection::paths()` is read for the **selected** path — a connection commonly holds an idle relay path beside a live direct one, so reporting the first would label a healthy session "relay".

**Gotcha**: the guest also watches for silence from the host (`HOST_SILENCE_LIMIT`). QUIC can believe a connection is healthy long after the peer is gone, and a guest chewing through buffered frames will happily keep drawing and keep claiming to be connected.

## Bottlenecks Are Named, Because They All Look Identical

**Decision**: the host diagnoses which stage is behind and tells the guest, which shows one chip per active bottleneck.

**Rationale**: a slow capture, a slow encoder, a slow link, a slow decoder and a slow display produce the *same* symptom — a stuttering picture and an idle bandwidth graph. Three of the five are made worse by the instinctive fix of lowering quality at the other end. Naming the real one is most of the value; acting on it correctly is the rest.

**The host is the single authority.** The guest measures its own decode and draw cost and round trip and reports them (`Message::Telemetry`); the host combines them with what only it can see and sends back the verdict (`Message::StreamStatus`). Only one side has the whole picture, so only one side decides.

**Gotcha**: guest telemetry crosses a trust boundary. Those numbers steer this machine's encoder, so `sanitize_telemetry` clamps them — NaN would poison every comparison in the controller, and an absurd decode time would pin the stream at its floor.

**Gotcha**: `BottleneckSet` travels as one byte and `from_bits` drops bits it does not recognise, so a newer host can add a stage without an older guest showing an unrelated icon.

## Saved Connections Are Probed, Not Guessed

**Decision**: `Message::Probe` asks a host whether it is reachable and whether a code is still valid, and is answered and closed **without** creating a session or raising an approval request.

**Rationale**: the card was showing "ready" with a green arrow for a host that had been switched off for a week, because holding a code was the only evidence available. The honest states — online, offline, code expired — require actually asking, and the only way to ask used to be to dial for real, which puts a prompt on someone else's screen. A card that refreshes every 45 seconds cannot be a doorbell.

`ConnectionStatus` now separates *evidence* from *actionability*: green means measured-and-confirmed, and an unreachable or stale row is still pressable, because the probe is a snapshot and the machine may have come back since. `a_probe_answers_without_disturbing_the_host` asserts the silence.

**Gotcha**: `Prober` owns **one** endpoint for all probes. Binding an endpoint costs a socket, a keypair and a relay handshake — per probe, checking six saved rows would cost more than a session.

**Gotcha**: an invalid probe is answered after a fixed 400 ms pause, and carries no host name. A probe is cheaper to send than an authentication, so without the pause it would be the fastest oracle for testing guesses — hopeless against a 32-byte secret either way, but there is no reason to hand out the faster one.

## Colour Conversion Belongs on the GPU

**Decision**: `Nv12Converter` runs `ID3D11VideoProcessor` over the duplicated desktop texture, so what crosses PCIe is NV12 rather than BGRA and the encoder gets the layout it wanted without the CPU touching a pixel.

**Rationale**: the desktop arrives as BGRA and the hardware encoder wants NV12, and doing that on the CPU cost twice over. Measured at 2560x1600: the conversion alone was 3.48 ms across sixteen threads — **52% of total encode time** — and the readback moved 15.62 MB instead of 5.86 MB. Neither compresses anything.

Measured on a live desktop with the hardware encoder running at the same time:

| path | capture | encode | total | ceiling |
|---|---|---|---|---|
| BGRA readback, CPU convert | 3.74 ms | 5.68 ms | 9.42 ms | 106 fps |
| GPU convert, NV12 readback | 2.33 ms | 2.55 ms | 4.88 ms | **205 fps** |

A fixed-function video processor rather than a compute shader: it is the path the driver already optimises for its own video pipeline, it does colour conversion and chroma subsampling in one pass, and it exists on every GPU that can decode video. A shader would need writing, tuning and validating per vendor.

**The colour space is not a detail.** `bgra_to_nv12_into` is BT.601 limited range and so is the decoder, so the converter asks for `DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601` exactly. `every_colour_converts_to_the_same_yuv_the_cpu_produces` pins this against black, white, all three primaries and grey, because a BT.601/709 mix-up barely moves grey while throwing red and blue tens of levels apart, and a full/limited-range mix-up shows only on black and white.

**Gotcha**: the format is negotiated **once**, by `negotiate_format`, before either thread starts, and `ScreenFrame` carries it. Per-frame it would mean every consumer handling both; configured-once-and-hidden it would mean a mismatch is silent — NV12 read as BGRA is a quarter of the picture in the wrong colours and every length check still passes.

**Gotcha**: the D3D11 device now asks for `VIDEO_SUPPORT` and turns on `ID3D11Multithread` protection. Capture, the video processor and the encoder are on different threads; without it the driver may assume single-threaded access and the failure is a corrupt frame or a device removal, not an error that points here.

**Gotcha**: `DepthPitch` is not the surface size for a planar format. This machine's driver reports `RowPitch * height` — the luma plane alone — where others report the whole thing, so it can only be checked as a lower bound. The chroma offset rests on the NV12 layout convention.

**Gotcha**: if duplication is lost and the rebuilt chain cannot do the conversion, capture **stops** rather than reverting to BGRA. The encoder has no way to be told mid-session, and a stream of misread bytes is worse than no stream.

**Still on the table**: full zero-copy, binding the MFT to a D3D11 device manager and feeding it the texture directly, would remove the remaining ~1.85 ms readback.

## Pasting a Code Says What Will Happen

**Decision**: the Paste button outlines itself in the answer's colour, and the line beneath it reports what the host said — after actually asking.

**Rationale**: the field a paste fills is not on screen, so a good paste and a bad one looked identical: nothing happened either way. And "the code parsed" is not the question anyone has. What they want to know is whether pressing Connect will work, which needs the host asked — so pasting now runs the same `Message::Probe` the saved-connection cards use, and reports the host's own name, whether it will prompt before letting you in, or that the code has expired.

Green means one thing and only one thing: the host answered and this code opens it. `every_paste_outcome_says_something_and_only_one_is_green` holds both halves of that.

**Gotcha**: the probe shares the one `Prober` the saved-connection sweep already owns, via a channel, with `biased` select so a person waiting on an answer goes ahead of the background sweep. Binding a second endpoint would cost a socket, a keypair and a relay handshake to duplicate something already there.

**Gotcha**: an answer is matched against the code it was asked about. Paste a slow code, paste another, and the first reply must not land against the second — that is how "ready" ends up next to a dead code.

## A Code That Survives a Restart Needs the Identity Too

**Decision**: the host's iroh secret key is always persisted, and each passcode is persisted as a whole connection code whenever its auto-reset is off. Both live in the platform credential store, never on disk.

**Rationale**: "do not reset my code" was ticked and the code changed anyway. Persisting the passcode alone would not have fixed it: `iroh::Endpoint::builder` mints a fresh keypair on every bind, and a code embeds the endpoint id, so the string handed out yesterday named a host that no longer existed. Both halves have to survive or neither is worth saving. `bind_with_identity` restores the key; `adopt_passcode` registers a secret the store did not invent.

The identity is kept unconditionally, independent of either auto-reset setting. Auto-reset is about the *secret* — the address is what everyone's saved-connection list already points at, and rotating it silently breaks every one of them.

**Codes are stored whole, not as bare secrets.** The round trip therefore goes through `ConnectionCode::parse`, which is base58**check** — a corrupted credential fails to parse instead of being adopted as 32 bytes of garbage that looks like a working passcode. `restorable_secret` additionally refuses a code whose type or endpoint id does not match, which is what catches the half-restored case: identity lost, code kept.

**Gotcha**: auto-reset *deletes* the stored code rather than ignoring it. An unshown secret is still a working key until something destroys it.

**Gotcha**: `HostSecrets`, like `SecretStore`, defaults to the in-memory backend, and `AppState::new` is the test-safe constructor while `AppState::persistent` is the production one. A default that reached the real Credential Manager is how 87 stray credentials got there the first time.

## Symbols Are Checked Against the Font That Draws Them

**Decision**: `every_symbol_the_ui_draws_has_a_glyph` asserts coverage for every non-ASCII character the interface uses.

**Rationale**: egui bundles a small font set, and a character outside it renders as an empty box. Nothing fails to compile, nothing logs, and it is invisible in every test that inspects widget state — it only shows up in a screenshot. `✕` (U+2715) on the close buttons was one; writing this test immediately found a second, `●` (U+25CF), which had been sitting behind the painted status dot on every session tab.

**Gotcha**: the tab's status dot is *painted*, not typed, because it carries the live/offline colour. The label leads with spaces to reserve the room — a bullet character doing that job is the bug above.

## Screen Capture: Trait-Based Abstraction

**Decision**: Define a `ScreenCaptureBackend` trait with pluggable implementations rather than hardcoding one capture method.

**Rationale**:
- Platform differences (macOS uses ScreenCaptureKit, Windows uses Graphics Capture API) mean we need different backends anyway.
- The stub capturer (always available, generates gradient frames) allows testing the full pipeline without screen permissions.
- Adding a new backend only requires implementing the trait — no changes to protocol or network layers.

**Gotcha**: `ScreenFrame` is BGRA regardless of backend, so a non-BGRA backend
converts before returning.

> **Partly overtaken.** `scap` was never adopted — see **Screen Capture: DXGI
> Desktop Duplication** for why. And the capture path no longer insists on BGRA:
> `capture_nv12` converts on the GPU so the encoder is handed NV12 directly,
> which is what it wanted all along. BGRA remains the format the trait describes
> and what a CPU-side capturer must produce.

## Input Coordinates: Normalized on the Wire

**Decision**: `MouseEvent` carries `0..=65535` across the full screen, not pixels. `ScrollEvent` carries Windows wheel-delta units (120 per detent). `KeyEvent` always carries **Windows virtual-key codes**, on every platform.

**Rationale**: normalizing decouples input from capture resolution — changing capture size or resizing the guest window cannot desync the pointer — and it happens to be exactly the range `MOUSEEVENTF_ABSOLUTE` wants, so the Windows injector does no conversion at all. Picking one canonical keycode space keeps translation on the injector side, where the OS is already known, instead of making every guest guess what the host expects.

**Gotcha**: extended keys (arrows, Insert/Delete/Home/End, PageUp/Down, right Ctrl/Alt, numpad divide) need `KEYEVENTF_EXTENDEDKEY` or Windows delivers their numpad twins — arrows arrive as digits. The list is in `input_inject.rs`.

**Gotcha**: `SendInput` returning 0 usually means UIPI blocked it, because the foreground window is elevated and we are not. The error message says so, since "input silently does nothing" is otherwise very hard to diagnose.

## Keyboard: Modifiers Are Synthesized

**Decision**: `keymap::ModifierTracker` diffs egui's modifier *state* each frame and emits the key-down/key-up events the host needs.

**Rationale**: egui's `Key` enum has no Shift/Ctrl/Alt variants — modifiers arrive only as a `Modifiers` struct describing the current state. Without synthesis, Ctrl+C on the guest arrives at the host as a bare `C`.

**Gotcha**: modifiers and buttons held when a session ends must be explicitly released, or they stay stuck down on the host with no way to clear them. `stop_session` does this; `app::tests::stopping_a_session_releases_held_input` pins it.

**Gotcha**: auto-repeat is dropped. The host's own OS regenerates repeat from the held key, so forwarding it would double up.

**The Windows/Super key needs a hook.** egui never reports it — Windows routes it straight to the shell — so forwarding it is only possible via a `WH_KEYBOARD_LL` hook that swallows the key before the shell sees it. That is what immersive mode installs.

## Immersive Mode: A Keyboard Hook and a Cursor Clip

**Decision**: `Ctrl+Shift+E` installs a low-level keyboard hook that captures **only** the Win keys, and clips the cursor to the window rect.

**Rationale**: without it, aiming at the far edge of a remote screen drops the pointer onto a local monitor, and the Win key opens the *local* Start menu — the two things that most break the illusion of working on the remote machine.

**Safety, because a stuck key hook makes a machine feel broken:**

- Only `VK_LWIN`/`VK_RWIN` are ever swallowed. Everything else passes through, so the toggle shortcut always works.
- `Ctrl+Alt+Del` is untouchable regardless — Windows reserves it from low-level hooks.
- Hook and clip are released on toggle-off, on disconnect, and **on losing focus** — trapping the pointer for a window the user cannot see looks like a hang.
- Any Win key still held is explicitly released, or it stays down on the host with nothing local able to clear it. Pinned by `app::tests::leaving_immersive_mode_releases_held_win_keys` and `stopping_a_session_leaves_no_input_trapped`.
- If the process dies, Windows tears both down automatically.

**Gotcha**: the hook callback is a bare `extern "system"` function, so its state has to be static. The captured-key queue is bounded — if the app ever stopped draining it, an unbounded queue would grow for as long as the hook lived.

**Gotcha**: egui reports viewport rects in *logical points*; `ClipCursor` wants physical screen pixels. Multiply by `pixels_per_point`.

## Shortcut Defaults: All On Ctrl+Shift

**Decision**: `Ctrl+Shift+Q` disconnect, `Ctrl+Shift+W` fullscreen, `Ctrl+Shift+A` toolbar, `Ctrl+Shift+E` immersive.

**Rationale**: every session shortcut is consumed locally and never forwarded, so each one is a combination the host can *never* receive. Keeping them on a single modifier pair makes that cost explicit and bounded, rather than scattering holes through the keymap. `Shortcut::is_safe` flags any binding without a modifier, since a bare letter would be stolen from the host mid-sentence.

**Safety**: a session pointed at our own endpoint disables input forwarding entirely. Injection is real, so forwarding would drive the pointer we are reading from. Covered by `app::tests::loopback_sessions_never_forward_input`.

## Input Injection: Platform-Specific Traits

**Decision**: Each OS has its own `InputInjector` implementation behind a trait, with a stub fallback for unsupported platforms.

**Rationale**:
- Windows (SendInput), macOS (CGEventPost), and Linux X11 (XTest) all use completely different APIs — no shared abstraction possible.
- Stub injector logs events but doesn't actually inject them, allowing development on any platform without OS-specific code.

**Gotcha**: The stub implementations in `input_inject.rs` are **not production-ready**. They log debug messages but don't call any real system API. Before shipping:
1. Windows: Implement via `winapi::um::winuser::SendInput` (requires linking to user32.dll)
2. macOS: Implement via CoreGraphics FFI bindings or use the `cocoa` crate's `CGEvent` types
3. Linux X11: Link against libxdo or call `XTestFakeKeyEvent`/`XTestFakeButtonEvent` directly

## Session Management: In-Memory HashMap

**Decision**: Track active sessions in a single `SessionManager` protected by `Mutex`. No persistence layer.

**Rationale**:
- Sessions are ephemeral — they exist only for the lifetime of the connection, which maps naturally to an in-memory store.
- Multiple concurrent guests each get their own session entry keyed by auto-incrementing ID.
- Approval state is transient and only relevant while the guest is connected; persisting it would add complexity without benefit.

**Gotcha**: If you need persistent sessions (e.g., for a relay/proxy server that stays online 24/7), replace `SessionManager` with a file-based or database-backed store. The trait interface in session.rs can be adapted to support this.

## CLI: clap Subcommands, Not Flags-Only

> **Superseded.** `main.rs` reads bare `std::env::args` and declares no
> subcommands. The binary is a GUI application with a handful of modes —
> service, agent, console — entered by the service control manager or by a
> single flag, not chosen from a command surface. `clap` is still a dependency
> of `tain-cli`; nothing in the compiled path uses it.

**Decision**: Use `clap` subcommands (`host`, `guest`) rather than a single command with many flags.

**Rationale**:
- Clearer intent — `rdp host --generate-master` is more readable than `rdp --mode host --action generate_master`.
- Easier to add new subcommands (e.g., `rdp relay`, `rdp benchmark`) without flag explosion.
- Standard Rust CLI convention that users are already familiar with.

**Gotcha**: The `host` and `guest` subcommands share the global `--verbose` flag via `#[command(global = true)]`. This is a clap 4 feature — make sure all subcommand structs have matching field types for global flags.

## Future: GUI Layer (egui / Tauri)

> **Done, and not as described.** The GUI is egui/eframe 0.29 on glow, it is the
> only entry point, and nearly all the work since has gone into it. `guest.rs`
> and `host.rs` still exist and are no longer reached from `main.rs`. The claim
> below that the protocol and networking layers would need zero changes did not
> survive contact: video moved to its own unidirectional stream, telemetry and
> adaptive control were added in both directions, and approval grew per-device
> grants.

**Decision**: The current CLI demo uses stdin/stdout for input simulation. A real guest would render frames in an egui or Tauri window and capture real mouse/keyboard events from the OS.

**Rationale**:
- CLI is sufficient for protocol testing without pulling in GUI dependencies (egui, wgpu, sdl2, tauri).
- The `input_inject.rs` trait is designed so that a GUI layer can implement it by reading from egui's input capture rather than from stdin.

**When adding GUI**: Replace the CLI input loop in `guest.rs` with an egui window that:
1. Receives `Message::Frame` from the network stream → decodes H.264/BGRA → displays via `egui::Image`
2. Captures mouse/keyboard events from egui's input system → sends as `Message::Input`

The protocol layer (`protocol.rs`) and networking layer (`network.rs`) require zero changes for this migration — only the guest mode implementation needs updating.

---

# Networking and discovery

## A Session Runs on a Direct Path or Not at All

**Decision**: Wait for hole punching to produce a direct path, and refuse the
session if it never comes. The relay carries the handshake and the address
exchange that make the punch possible, and nothing else.

**Rationale**: Every iroh connection begins relayed and upgrades if it can, so
"connected" was never the same as "able to stream" and nothing checked the
difference. A relay is a shared public server, rate limited, carrying everyone
else who landed on it — ample for the few hundred kbit/s it exists to carry, and
hopeless for ten to twenty Mbit/s of video.

**Measured**: the relayed session collapsed to a third of full resolution at
8 fps and stayed there, while the same pair of machines managed 41 fps in the
other direction on a direct path to the same LAN. A session over a relay is not
degraded, it is unusable, and failing to connect at least says so in a second
rather than after a minute of watching a slideshow.

**Cost when it works**: about a tenth of a second. A loopback connection is
carried directly in ~100 ms against a 25-second budget.

**Both ends check**, so an older build on the far end cannot make a host
capture, encode and stream into a relay. The host allows a margin over the
guest's deadline: both wait on the same punch, and whoever closes first decides
what the user is told — which should be the guest, since it is the side with
something to say about it.

**Not covered**: reachability probing still uses the relay. It is control
traffic, which is what a relay is for, and it closes too quickly to have waited
for a punch anyway. A relay-only host therefore still shows as online, and
explains itself when the connection is refused.

## Finding a Peer Without Asking One Particular Server

**Decision**: Run mDNS and mainline-DHT address lookup alongside the n0 ones.

**Rationale**: `presets::N0` installs three lookup services, which reads like
redundancy and is not — a pkarr publisher, a pkarr resolver over HTTPS, and DNS
TXT, all three pointed at `dns.iroh.link`. With the four relays under
`relay.n0.iroh.link`, the entire ability to find anybody rested on five names
belonging to one organisation, resolved through whatever the local network says
DNS is. The cache on the development machine had learned exactly those five,
which is the whole DNS surface there is.

That is a poor arrangement anywhere, not only where a network is interfered
with: one operator's uptime, one DNS answer that can be wrong, and — for two
machines in the same building, which is what a remote desktop is mostly used for
— a round trip through the public internet to learn the address of a computer
three metres away.

**Measured**: two endpoints built from `presets::Minimal` with only these
attached, told nothing about each other, connect in under a second. The DHT
bootstraps in two seconds against 115 peers on a heavily filtered connection,
and a publish-then-resolve round trip takes about eight seconds and sometimes
two asks.

**Attached after `bind`, not through the builder.** iroh calls
`into_address_lookup` during `bind` and propagates the error, so a machine where
multicast will not open or the DHT cannot bind its port would fail to start the
application at all. These are extra ways to find a peer; not having one is worth
a log line. `discovery::bind` does both halves, because an endpoint built
without the attach has no resolving lookups at all — it would look perfectly
healthy while being unable to find anybody.

**Local disclosure**: mDNS announces this endpoint id on the local network. Not
enough to connect, since a passcode is still required, but worth knowing on a
network you do not own.

## Publish an Address a Peer Can Dial, Not Only a Place to Meet

**Decision**: Both directories Tain publishes to carry the relay *and* any
address routable from outside. Private addresses are filtered out.

**Rationale**: the default everywhere is relay-only, which is safe and leaves a
peer with somewhere to meet and nothing to dial. Where punching then fails there
is nothing else to try, and since a relayed session is refused, the connection
fails outright.

**Measured**: a tunnel on one machine and not the other. DNS answered, gave the
relay, the connection came up on it, and no direct path ever opened. Waiting
longer would not have helped and neither would waiting for the DHT — it had the
same relay and no addresses of its own, because it had been told to publish
none. The address that would have worked was never published by anybody.

**Not unfiltered**: a `192.168` address is the worst of both — unreachable for
the remote peer it would be published for, while telling anyone who reads the
record how the local network is laid out. The case it would serve is answered by
mDNS in under a second without publishing anything anywhere. Carrier-grade NAT
is dropped alongside RFC1918, which `is_private` does not cover.

**The trade**: a machine's public address goes into a public directory under a
long-lived identity. It is the address any peer it connects to sees anyway; what
is new is that it can be read without connecting.

## Discovery Is Allowed to Be Slow

**Decision**: retry a dial that fails with `NoAddress` for thirty seconds.

**Rationale**: how fast a session runs is decided by the path it found, not by
how long finding it took, so there is nothing to gain by giving up early.

iroh already waits on lookups that are *still in flight* — mDNS keeps its stream
open, and a host that appeared ten seconds into a dial is picked up on the first
attempt. What it does not wait for is lookups that **complete** with nothing,
which returns `NoAddress` at once with the whole budget unspent. A guest whose
mDNS could not start and whose DNS is blocked has only the DHT, and one attempt
called the host offline.

**Only for `NoAddress`.** That is the difference between nobody having said
where this host is, where more time is the entire fix, and a host that answered
and refused, where retrying only turns a clear failure into a slow one.

## Which Lookup Answered Is Worth Knowing, and iroh Does Not Say

**Decision**: wrap every resolving lookup in a decorator that passes results
through untouched and records which service produced the first one.

**Rationale**: `Item::provenance()` exists on every result and nothing carries
it out to where an application can see it — `TransportAddrInfo` has the address
and whether it is in use, and no record of where it came from.

Since a relayed session is refused, "direct" is the only thing a status chip
could ever say and it carries no information. *How* the host was found does:
whether the local network, the public lookup service, or the DHT is currently
doing the work is the part that varies, and the part that explains a slow
connect or a host that could not be reached.

**Inference was rejected.** Attributing by elimination — assume DNS whenever
neither of ours answered — would have been wrong often enough to matter: DNS
answering first with mDNS a moment later is indistinguishable from the reverse
unless something is watching, and this label is shown to a user as fact.

**Cost**: wrapping the n0 services means adding them individually, so
`discovery::endpoint_builder` writes out what `presets::N0` does rather than
calling it. It is the one place that has to track that.

**"Known" was not good enough** as a fourth answer. A connection code carries a
relay, so most dials never need a lookup at all, and one word folded that common
case together with arriving on a stale remembered address and arriving on the
code's relay with nothing else contributing — which is the exact state that then
fails at `Direct`. The three are told apart by what happened: a lookup noting an
answer, or failing that whether the connection landed on a relay, which only the
code could have supplied.

# The connection graph

## A Failure Must Stay on the Graph

**Decision**: draw connecting as six stages that fill up, keep the graph on
screen when it fails, and show what each stage produced.

**Rationale**: the status line can only ever show the step happening now. That
answers "what is it doing" and is no help with "how much of this is left", which
is what someone watching a spinner is asking. The stages are deliberately
coarser than `DialStep` — that enum exists to say precisely which thing failed,
which wants detail; this is read at a glance, which wants the opposite.

**A failure must stay put.** `GuestStatus::Failed` carries a message and nothing
else, so by the time a failure is visible the step it failed on is gone. The
furthest stage reached is kept in shared state, updated at the one place status
changes. A graph that vanishes at the moment it becomes most informative is the
wrong answer.

**Results, not only steps.** A row of stage names says how far a connection got
and nothing about what it found, leaving the interesting question answerable
only by hovering something else. Every stage with an answer worth six characters
carries it, and a stage that stopped says what it was left holding — `Find:
code`, `Direct: relayed` is the whole diagnosis of the common failure.

## Stats Are Anchored, Because They Are Read by Glancing

**Decision**: fixed-width fields in a monospaced font, conditional ones appended
last.

**Rationale**: these are read while something is going wrong, which means
glancing at the same spot repeatedly. Built the obvious way — join whatever
happens to apply — every field moved whenever any field to its left changed
width, and a figure that jumps as it updates has to be re-found every second
instead of watched. Both byte-rate units are six characters wide, and the
latency column holds itself open with a dash until the first measurement lands.

# Adaptive control and codecs

## A Guest That Has Not Started Decoding Is Not a Failing Link

**Decision**: the host's delivered-frame check does not run until the guest has
reported decoding something at least once.

**Rationale**: a guest reports telemetry from the moment its stream opens, well
before it has a decoder — it is still authenticating, sizing its window and
negotiating a codec. Every one of those reports carries `decoded_fps: 0`, which
is the honest answer when there is nothing to decode with. The host is already
capturing and sending by then, and compared the two.

**Measured**: the host began sending at 17:01:32 and the guest opened its
session at 17:01:55. In those twenty-three seconds the host sent three hundred
frames, saw none decoded, diagnosed a slow network and collapsed to 33%
resolution at 8 fps — then held exactly 8.0 captured/s for another minute. The
link was fine; every frame that reached the guest decoded.

**Latched, not a simple non-zero test**: once a guest has decoded something, a
later zero does mean the frames stopped arriving, which is the whole point of
the check. This is the same guard `acks_stale` already carried, applied to the
signal beside it.

## Availability and Buildability Must Be Tied Together

**Decision**: a test asserts that every codec `is_available()` reports can
actually be built.

**Rationale**: `is_available` is three things at once — what the settings list,
what negotiation promises a guest, and what `effective_codec` stops falling back
from. `create_encoder` is what has to build the thing, and nothing tied them
together. AV1 shipped listed, offered, negotiated, and unbuildable: the match
had no arm for it.

**What that looked like**: a guest negotiated AV1, built a decoder, logged that
it was decoding AV1 on the GPU, and had the host drop the connection 418 ms
later. It read as "AV1 crashes", in both directions, because both machines had
the same gap.

**The existing test did not catch it** because it accepted *any* failure from a
GPU-only codec as "the GPU declined a 16×16 frame", which absorbed "nobody wrote
this arm". Telling those apart needs the error text, not the presence of an
error.

## The Virtual Display Driver Is Vendored, Not Fetched

**Decision**: the driver package ships in `drivers/virtual-display/` beside the
executable. The Install button in Settings installs what is there; it does not
download anything.

**Rationale**: fetching on click was considered and the numbers decided it. The
driver-only package is **132 KB** — a fetch would add an HTTP client and a zip
reader as direct dependencies, plus download progress, hash verification and
network error handling, to avoid committing a file smaller than several of this
project's icons.

It also fails hardest where it is least affordable. A censored connection cannot
reach GitHub's release CDN, and "the machine has no monitor and no internet" is
exactly the case a virtual display exists for. A vendored package works on a
machine that has never been online.

**What a fetch would have had to do anyway**, and what is therefore recorded in
`drivers/virtual-display/README.md` instead: the exact release tag, the asset
name, and its SHA-256. "Download the latest" is not verifiable in advance — the
hash of a release that does not exist yet cannot be pinned — so the choice was
never *fetch* versus *vendor*, it was *pin* versus *trust whatever arrives*.
Pinning is the part that matters, and once a version is pinned there is little
left for the download to buy.

**And it is embedded, not shipped beside the executable.** A folder next to the
binary was the first answer and it was a maintainer's path wearing a user's
clothes: anybody able to extract a driver package into the right directory could
have run the driver's own installer and skipped Tain. A feature that works only
for the people who did not need it is not a feature. So `build.rs` compiles the
package into the binary and the install button writes it out — which is what
this project already does with 66 MB of ffmpeg DLLs. Tain stays one file.

**What it costs**: a driver fixed upstream is stale until Tain ships a build
with the new one. Acceptable at this size and for this kind of component. A
folder beside the executable still overrides the embedded package, for trying a
newer one without a rebuild — a developer's escape hatch, and named as one
rather than offered in the interface.

**Not changed by this**: the package is installed exactly as its authors signed
it, and Windows names them rather than Tain in its publisher dialog. That is
true whichever way the bytes arrive.

## Tain Writes Its Own Graphics Preference Rather Than Explaining One

> **Reversed.** The write is gone — see *Capture Where the Desktop Is, Encode on
> the Best Chip* below, which explains what it cost. The reasoning here is kept
> because the failure it describes is real and the diagnosis was right; only the
> remedy was wrong.


**Problem**: on a laptop with two graphics chips, hosting failed outright and no
amount of care inside the capture code could fix it. The report that finally
explained it:

```text
no adapter could capture \.\DISPLAY1. Tried 6:
  NVIDIA GeForce RTX 2060 with Max-Q Design: this GPU cannot duplicate that display (0x887A0004)
  Intel(R) UHD Graphics: \.\DISPLAY1 is not attached to this adapter (it has none)
  Intel(R) UHD Graphics: … (it has none)
  Microsoft Basic Render Driver: … (it has none)
  the default adapter: this GPU cannot duplicate that display (0x887A0004)
  the software rasteriser: … (it has none)
```

The panel is driven by the Intel chip. Intel enumerates **no outputs at all**,
and NVIDIA — this process's adapter zero — claims the display and then refuses
to duplicate it. That is an Optimus laptop seen from a process the driver placed
on the discrete GPU: it presents *through* the integrated chip, so it appears to
own the desktop while owning nothing duplicable.

**Why the two previous attempts could not have worked**: both added more
adapters to try. There is exactly one adapter carrying the output, it is the
wrong one, and DXGI will not pair an output with a device from another adapter.
No search over adapters can succeed, however thorough — the search space does
not contain the answer.

**Decision**: when every adapter has failed and at least one failed with
`DXGI_ERROR_UNSUPPORTED` on a machine with more than one real chip, Tain writes
its own per-application preference to
`HKCU\Software\Microsoft\DirectX\UserGpuPreferences` and asks to be restarted.

**Rationale**: which chip a process runs on is fixed before `main`, so the run
that failed cannot be repaired — only the next one. The previous message told
the person at the keyboard to change it in *Settings > System > Display >
Graphics*, and came back twice with the fault unfixed. That page is hard to find
and these machines are usually being helped with from somewhere else. Writing
the value is the same act, done by the program that knows it is needed.

It is a modest thing to write: Tain's own executable, under `HKCU`, no
administrator rights, reversible from that same Settings page and from
`gpu_preference::clear`.

**Why "power saving" and not the other option**: it is not a coin toss. The
integrated chip is what the panel is wired to, and a process placed there sees
an honest topology — Intel owns the internal display, the discrete chip owns the
ports it drives — which the existing adapter search then resolves correctly. The
preference is not "try the other GPU", it is "stop being lied to about which one
owns the desktop".

**What it will not do**: overwrite a preference somebody chose. Already on the
integrated chip and still failing means this is not the wrong-chip problem;
already on the discrete chip means somebody put it there. Both say so instead.

**What it costs**: the whole application moves to the integrated chip, so its
own rendering and any encoder chosen from that device are the integrated one's.
On the machines this fires on, that is the difference between working and not.

**The failure is carried as a type**, `DesktopOnAnotherGpu`, not recognised by
its message — the Windows half of which arrives in the machine's own language,
as the report above demonstrated in Russian.

**Still unaddressed**: the *host* is who must restart, and the message reaches
their log and their guest rather than their window. Windows.Graphics.Capture
would avoid the whole question by not requiring the device and the desktop to
share an adapter, and is the durable answer if this recurs.

## There Are Two Ways to Capture a Screen, and the Second One Has No Adapter Rule

**Problem**: Desktop Duplication requires the D3D device and the output to be on
the same adapter — the one compositing the desktop. On a hybrid laptop the
process is commonly placed on the *other* chip, which then claims the display
and refuses to duplicate it. The full diagnosis is in the decision above.

The remedy there works and was confirmed on a machine that could not host and
then could. It is also a workaround wearing every bit of awkwardness that
implies: the preference is filed under an exact executable path, so renaming or
re-downloading Tain silently undoes it, and it cannot take effect until the
process is restarted, because the decision it changes is made before `main`.

**Decision**: add `capture_wgc`, a second backend built on
Windows.Graphics.Capture, and fall back to it when duplication will not start.

**Rationale**: WGC asks DWM for frames rather than duplicating an output. The
compositor delivers onto whatever device the frame pool was given, so there is
no ownership rule to break — nothing to restart, no path to match, no Windows
setting to write. It is the mode OBS tells people with gaming laptops to use,
for this exact fault.

**Why duplication stays first**: it hands over a frame the moment the desktop
changes, draws no capture border, and reports the pointer separately so the
guest can draw it locally at its own frame rate. WGC gives up all three:

- **The capture border.** A yellow rectangle around the captured screen.
  `SetIsBorderRequired(false)` removes it, and only on Windows 11 21H2 and
  later.
- **The compositor's schedule**, not the desktop's — one more hop and one more
  buffer.
- **No pointer metadata.** WGC can bake the cursor in or leave it out and offers
  no third answer, so it is baked in and `take_cursor` reports nothing.

Every one of those is a real cost, and every one is paid only where the
alternative is no picture at all.

**Both failures are reported when both fail.** The duplication error names every
adapter it tried, and that is the one that says what is wrong with the machine;
losing it because a second attempt failed differently would throw away the
diagnosis it exists to give.

**Proved before it was wired in**, on real hardware rather than by reasoning: a
2560x1600 frame of a live desktop through the frame pool, and the same again
converted to NV12 by the video processor at exactly 12 bits per pixel — which is
the path a session actually takes, and the one where a video processor that
refused the size would only ever have shown up on somebody else's machine.

**`TAIN_CAPTURE=wgc` forces it**, because a machine where duplication works never
reaches the fallback and there would otherwise be no way to compare them — which
is precisely what somebody wants when duplication works and looks wrong. An
environment variable rather than a setting: one capture is shared between every
session watching a display, so a per-session switch would mean the second
session silently taking whatever the first one chose.

**What this does not change**: the graphics preference stays. It is written, it
works, and it costs nothing to leave in place as the line behind this one.

## The Admin Passcode Stopped Being the Whole of the Authorisation

**Problem**: the admin passcode is unrestricted access — every screen, mouse and
keyboard from the first frame, and with the service installed the lock screen
and UAC prompts too. Until now the code alone was the entire authorisation, so
anything that came to hold it *was* the owner, silently and immediately. Codes
do get out: pasted into the wrong window, read off a screen share, left in a
clipboard manager.

**Decision**, two halves that only work together:

**A cover over the copy button.** The admin code cannot be copied by a click.
The button is covered by a slider that has to be dragged aside, and on first
contact a one-time dialog says what the code actually grants. The cover closes
again on the copy it was opened for, on a passcode reset, and on its own after
twenty seconds.

*Why a drag and not a confirmation dialog*: what is being guarded against is the
copy happening at all. Once the code is on the clipboard it can leave in the
next paste, into whatever window has focus, and nothing afterwards calls it
back. A dialog appears *after* the click, and the answer to a dialog you did not
mean to summon is to dismiss it — which people do without reading. A drag cannot
be produced by a stray click or a keyboard repeat.

*Why the warning is shown once*: a dialog that appears every time is one nobody
reads. The cover handles every later occasion, which is what it is for.

**Approval on first use, per endpoint.** An admin connection from an endpoint
the host has not seen before waits for somebody at the machine to say yes. The
answer is remembered, so the owner is asked once from each of their own devices
and never again.

*Why per endpoint and not per connection*: asking every time would train the
answer out of the person it protects, and an approval nobody reads protects
nothing. Once per device is rare enough to be read and frequent enough to catch
the case that matters — a code being used from somewhere it should not be.

*What an endpoint is*: the guest's iroh endpoint id, the public key its
connection is authenticated with. Not a name and not an address, both of which
are things a caller says about itself.

*What clears it*: resetting the admin passcode, which is the revocation the
interface already offers and the one people reach for when a code has got out. A
reset that left the machine that took the code still trusted would look like it
had worked and would not have.

**The dialog asks a different question for an admin.** A guest is asking to
watch and the host chooses what to grant; an admin already has everything the
moment it is let in. So the screen and input checkboxes are gone — offering them
would imply a limit the protocol does not have — and the wording says what is
being decided, which is whether this device is one of yours.

**What this costs**: a host with no interface has nobody to ask, so it refuses.
That was already true of guests; it is now true of admin connections from an
unknown endpoint, which means a headless host cannot be reached from a device
that has never been approved at the machine itself. The log says so in those
words rather than reporting a bad passcode.

**Caught by a test rather than by a user**: the e2e harness connects with the
admin passcode and had no approval channel, so every one of its tests began
failing — correctly. It now answers yes, which is what a real host with a window
open does, and reads past the "waiting for approval" message that an admin
connection is now sent before its session starts.

## The Approval Popup Belongs to Its Card

**Problem**: approving a guest was a modal dialog in the middle of the window,
and it was a decision about *a* connection rather than about *that* one. With
two guests waiting it was worse than untidy — the dialog named one of them, the
strip showed both, and only the name tied the sentence to a card. The code
enforced one at a time, on the reasoning that a second click was a mis-click.

**Decision**: each waiting connection gets its own popup, drawn directly above
its card and exactly as wide as it, and all of them are shown at once.

**Why several is safe here when it would not be anywhere else**: the popups
cannot overlap, because the cards cannot. Each takes its card's width, and the
strip already lays cards out without collision — so the arrangement is inherited
rather than invented. The card grows to `PILL_DECIDING_W` while a decision is
attached and shrinks back once the guest is admitted, since a card carrying a
question needs room for one.

**Placed by its bottom edge.** The popup sits above the card, so what has to
line up is its bottom — and its height is not known until its contents are laid
out. `Area::pivot(LEFT_BOTTOM)` with `fixed_pos` says where the bottom-left
corner goes and lets egui work out the rest, which is the difference between
this and measuring on one frame to place on the next.

**The Cancel button is gone.** It existed because a modal has to be got rid of
before anything else can be done. A popup attached to its card is put away by
clicking that card again, which leaves the guest exactly where it was — waiting,
seeing nothing. That was the whole point of `Outcome::Dismiss` being distinct
from `Deny`, and the distinction survives without the variant.

**Screens became icons.** The list named every display in full, one per row —
a paragraph to read before a decision that is really about shape: how many
screens there are and which of them somebody is about to see. A row of monitors
answers that at a glance, a virtual display is drawn as one, and the detail
moves to the hover text where it is available to anyone who wants it and in the
way of nobody who does not. It is shown even with a single screen, which the
list was not: the single-monitor case previously said nothing at all about what
was being shared.

**The card reflects what the popup is offering.** Ticking Mouse lights the
card's mouse mark immediately, with a tooltip saying it is offered rather than
granted — the two used to disagree while a decision was open, the card showing
the grants the session actually had and the popup showing what was proposed.


## The One String That Crosses the Wire Is a Code, Not a Sentence

Every other piece of text in Tain is written on the machine that shows it, so
translating it is a local matter. `SessionEnd.reason` was the exception: the
host composed a sentence and the guest printed it. That is invisible while both
ends speak English and wrong the moment they do not — a Russian host telling an
English guest, in Russian, that it would not allow the connection. No amount of
catalogue work fixes it, because the words are chosen on the wrong machine.

**Decision**: `reason` carries one of the codes in `protocol::end_reason`, and
each end renders it through its own catalogue as `end.<code>`.

**There was never anything dynamic to preserve.** The reasons are fixed
sentences with, at most, a value or two to name: a display, an adapter setting,
the operating system's own account of a failure. So the code travels with
`args`, a list of named values for the placeholders in whichever catalogue ends
up rendering it — and a value nobody can translate, like an OS error, is a value
rather than part of the words.

**`invalid_passcode` was already a code**, and had been since the first version
of the protocol. It was the one reason the guest already rewrote into a sentence
of its own, in `explain_session_end`, because "Failed: invalid_passcode" sent
people hunting for a bug that wasn't there. That function now does for every
code what it used to do for one.

**A code with no entry still reads as an ending.** An older build talking to a
newer one will meet reasons it has never heard of, so an unmatched code falls
through to `end.unknown`, which says a session ended and quotes what it was
told. The alternative — printing the bare code — is precisely the failure this
replaced.

**The wire format changed.** `args` is a new field, and bincode is not
self-describing, so old and new builds cannot talk to each other. That is
acceptable exactly once, before there are versions in the wild, and it is the
reason to do it now rather than after.

### The Wrong-GPU Remedy Is the Case That Proves It

The failure worth the most words is a laptop with two graphics chips where the
desktop belongs to one and Tain was started on the other. `gpu_preference`
explains it at length; what matters here is who reads the explanation. It is
four paragraphs about the *host's* Windows settings, and the only screen it ever
reaches is the *guest's* — the host writes it to a log and hands it to the
session, which sends it on. Written as prose it was therefore prose in the wrong
language by construction, and no catalogue could have fixed it.

**Decision**: `screen_capture::Remedy` carries the code and its arguments, and
travels up the error chain as a typed link.

**A link, not a context.** Session start wraps this error in more context before
anything looks at it, and anyhow's chain yields the wrappers rather than what
they wrap. `Remedy::in_chain` finds it by type at any depth, which
`downcast_ref` on the outermost context would not — the same reason
`DesktopOnAnotherGpu` is a real error type rather than a recognisable sentence.

**Five endings, not one with a value substituted.** They differ in what is left
to do: Tain has already asked for the other chip and needs a restart; it just
asked; it wanted to and the registry refused; it is already on the chip that
usually fixes this, so the fault is elsewhere; or somebody deliberately put it
on the discrete one, which is not this program's to undo. Two of those were one
sentence with `{current}` substituted into it, and the substituted value was
itself a phrase needing translation — a placeholder holding a translatable
string is a rendering problem with no clean end, and two codes have none.

**`Display` renders English, and only the log reads it.** A sentence shown
translated and also logged is looked up twice, once each way — `i18n::in_english`
exists for exactly this. A log in a language the maintainer cannot read is
materially harder to help with, and every diagnosis of this particular fault so
far has begun with a log file.


## Text Nothing Displays and Keys Nothing Defines Are Both Bugs

Translating the application twice still left English on a Russian settings
panel. A missing key is not a compile error and not a panic: `i18n::text` falls
back to English, and English falls back to the key itself, so the failure is a
string that looks right to the person who wrote it and wrong only on the screen
where it appears — in the language nobody testing it reads.

**Decision**: `tests/i18n_keys.rs` reads the workspace's sources, extracts every
key named as a literal — including the `i18n::text(match self { … })` shape that
most of them use — and fails if the English catalogue does not have it.

**English, not every language.** An unfinished translation is usable and
welcome; `every_language_reports_what_it_is_missing` prints what each one still
lacks without failing. A key *no* catalogue has is a different thing: it is a
key that will show on screen as itself, and it can only have come from this
repository.

**The reverse direction prints rather than asserts.** An entry nothing names may
be perfectly alive — built at run time, or held in a table of keys the scan
cannot follow. Reporting them found one that was genuinely stale, a duplicate of
a warning already in the file; asserting on them would have been noise.


## An Encoder Held Its Own Event Generator While Shutting Itself Down

Roughly one run in ten of `cargo test -p tain-core` never finished. The process
sat at no CPU with 141 threads until something killed it — twice for over an
hour, holding the build lock the whole time, with nothing anywhere saying why.

**What it is.** Measured rather than guessed, over about three hundred runs:

| what was run | hung |
| --- | --- |
| the whole suite | 5 / 30 |
| the whole suite without `codec_mf` | 0 / 25 |
| `codec_mf` on its own | 0 / 50 |
| the whole suite, one thread | 2 / 30 |
| the whole suite, one encoder test left in it | 3 / 30 |

So `codec_mf` is necessary and not sufficient: it stalls only alongside the rest
of the suite, and no single other module is required — dropping `local_input`,
dropping every other GPU module, or running strictly serially all still stall.
The stuck test is always one that activates a Media Foundation transform, and
almost always `a_size_the_gpu_cannot_take_degrades_to_the_cpu_instead_of_failing`.

**Where it is not.** The encode loop was the obvious suspect and is not it:
`encode_frame` polls with `MF_EVENT_FLAG_NO_WAIT` and gives up after
`ENCODER_STALL_TIMEOUT`, so it cannot block indefinitely. That left
`ActivateObject` and `ShutdownObject`, and no way to tell which — a thread
blocked inside a driver is not unwinding, has no symbols, and cannot be asked
where it is without a debugger attached to a process that is already wedged.

**So the watchdog was given breadcrumbs.** `Guard::at` records what is about to
be attempted; the abort quotes it. Two reproductions, both the same:

    watchdog: a_size_the_gpu_cannot_take_degrades_to_the_cpu has been stuck for 60s
    It got as far as: dropping the encoder — ShutdownObject

**What it was.** Teardown, and two things wrong with it, both about what the
transform was still holding when it was asked to shut down.

*Nothing flushed.* `encode_frame` takes only the output already queued and
leaves the rest for the next frame, so an encoder dropped mid-stream leaves the
transform holding samples it has finished with, signalled, and that nobody will
ever collect. `MFT_MESSAGE_COMMAND_FLUSH` is what abandons those, and it is the
counterpart to the flush the encoder already sends when it *starts* streaming.
Only the two end-of-stream messages were being sent.

*The event generator outlived the transform.* Rust drops fields in declaration
order, and `activated` is declared before `events`. So `ShutdownObject` ran
while the struct still held a live `IMFMediaEventGenerator` reference into the
object being shut down — a reference count the driver waits on, held by the
thread doing the waiting. Nothing in the type said the order mattered, and
nothing would have: it is a deadlock made out of two field declarations.

**Decision**: flush, end the stream, release the event generator, and only then
let `activated` hand the session back. The release is an explicit
`self.events = None` rather than a reordering of the fields, because relying on
declaration order is what went wrong.

**Verified by counting.** 100 consecutive clean runs of the suite, against a
rate that had been 5/30, 3/30, 2/40 and 2/30 across the preceding measurements.
At the old rate, a hundred clean runs in a row is a one-in-fifty-thousand event.

**This was never only a test problem.** The same `Drop` runs at the end of every
real session. A host whose encoder was torn down at the wrong moment would have
hung a thread inside the driver with no log line and no way out — which is
exactly the class of fault that gets reported as "it froze" and never
reproduced.

**The watchdog stays.** It found this, and the next driver stall will not
announce itself any more clearly than this one did. A bound of sixty seconds
costs a passing run nothing and turns the next mystery into a name.

**It aborts, and it says so first.** A thread blocked inside a driver cannot be
unwound, so there is nothing to fail gracefully — `abort` is the only way out.
The first version called it straight after `eprintln!` and lost the message
every time, because `abort` runs no at-exit handlers and does not flush; the
run ended with no explanation, which is precisely the thing being fixed. The
write is now explicit and flushed before the call.

**The firing is tested, on its own.** A watchdog nobody has watched work is not
one to trust, but a test that fires it ends the suite. So `after` takes the
deadline as an argument and the demonstration is `#[ignore]`d, to be run
deliberately — see the doc comment on it for the command.

**It is a test-only module**, `#[cfg(test)]` in `lib.rs`. Bounding somebody
else's blocking call by killing the process is a reasonable thing to do to a
test run and not a thing to do to a running application.

**Worth knowing about production.** The same `ActivateObject` and
`ShutdownObject` run on every session start and teardown, with no timeout there
either. This has never been seen outside the test suite, and the trigger is not
understood well enough to say it cannot happen — but a session that freezes
while starting its encoder would look exactly like this.


## Sharing a Window Is Not Sharing a Smaller Screen

Sharing a display answers "let them see this monitor". Sharing a window answers
something narrower and much more useful on a machine that is somebody's own: let
them see *this one program*, and nothing else that happens to be on the same
desk. Mail stays private while the spreadsheet is being looked at, and a second
monitor is not a workaround for that.

**It cannot be done by cropping.** A window that is partly behind another one,
or scrolled off the edge of the screen, is not present in the desktop image at
all — cropping would share whatever is on top of it. Only the compositor holds a
window's own pixels, which is why `DisplayTarget::Window` routes to
Windows.Graphics.Capture with no Desktop Duplication fallback and none wanted: a
window share that quietly became a screen share would be a promise broken
silently.

**Decision**: it is a *grant*, not a choice the guest makes.
`DisplayGrant::Window` sits beside `All` and `Only`, and the guest is offered no
menu at all when it is in force.

**Why the guest does not get to pick.** The other two variants bound what the
guest may choose between; this one removes the choice, because the choice is the
whole point. "Let them see the spreadsheet" is only true if they cannot then
switch to a screen with the mail on it — a window share where the guest can
change the subject is a display share with an extra step. `allows()` returns
false for every screen id, `WatchDisplay` is answered with the window whatever
was asked for, and the toolbar shows the window's name where the display picker
would have been.

**Minimising is not a failure, and that is the hard part.** A monitor being
unplugged is rare enough that a session can simply fall back; a window being
minimised is ordinary, reversible, and happens by accident several times an
hour. The compositor produces nothing for a window in that state, so a session
that did nothing about it would show a frozen picture — indistinguishable from a
dead link, which is the thing people reconnect over.

So the host follows it. `SharedWindowWatch` polls four times a second, and every
change goes to the guest as `Message::SharedWindow`. While the window is away
the capturer is dropped and replaced with a stub — keeping it would hold a
compositor session against a window the compositor has stopped drawing — and it
is rebuilt when the window comes back. The guest dims the last frame and says,
in words, that the host minimised it.

**The state travels in `SessionStart` too, not only as a change.** A window that
is *already* minimised when the guest arrives has no transition to announce, and
would otherwise be a blank picture with nothing saying why.

**A retitle is a change and not a restore.** Browsers rename their window on
every navigation, so it is the common case — and treating it as a restore would
tear the capture down and rebuild it on every page load. `came_back` is the
distinction, and it is a function rather than an expression inside the loop
because it is the one transition with a cost attached.

**The handle is the id, and it is not a promise.** Windows reuses handles once a
window has gone. The host re-reads the window's title and state every poll and
reports `Closed` for a handle that no longer resolves, rather than trusting the
number — but a handle reused between two polls by an unrelated window is a hole
this design does not close. It is bounded by the grant being per-session and the
poll being fast, and it is the reason the id is opaque to the guest.


## Capture Where the Desktop Is, Encode on the Best Chip

**Problem**: on a laptop with two graphics chips, Tain ended up encoding on the
processor with a powerful discrete card sitting idle — because Tain had asked
Windows not to use it.

**How it got there.** Desktop Duplication requires the process and the desktop
to be on the same chip, and on these machines they are not. The fix at the time
was to write a per-executable graphics preference pinning Tain to the
*integrated* chip and ask for a restart. It worked, in the narrow sense that
duplication then succeeded.

It also moved the whole process. The encoder went with it. So a machine with a
real GPU in it captured happily on the weak chip and encoded on the CPU, for the
life of the installation, and nothing said why — the registry value outlived the
run that wrote it, and nothing ever took it back.

**And it was already unnecessary.** Windows.Graphics.Capture had been added
precisely because it has no adapter rule, and `capture_shared::start_capturing`
already falls back to it when duplication refuses. The preference was written on
the way past — a side effect of the duplication attempt failing, before the
compositor had even been tried. On these machines the compositor then worked,
so the write bought nothing at all and cost the encoder.

**Decision**: nothing in the capture path changes this machine's graphics
settings. The desktop is captured wherever it lives, by whichever of the two
backends can do it, and the encoder runs on whatever encoder is best. They do
not have to be the same chip — `codec_mf` feeds its transform from system memory
exactly so that they need not be.

**The five endings became one.** `end_reason::wrong_gpu` had a code per branch
of the automatic fix. With no fix to report there is one case left worth
sending, and it is the one that is actually a fault: neither way of capturing
worked. A desktop on the other chip is ordinary and now silent.

**What was written is taken back.** Not writing it any more fixes nothing on a
machine that already has the value — it lives in `HKCU` and outlives the build
that put it there. `gpu_preference::undo_our_old_preference` removes it once, on
startup, and only `PowerSaving`, which is the only thing the old code ever
wrote. A deliberate *high performance* choice is somebody's own and is left
alone; so is a power-saving choice made after the cleanup has run, which is what
the recorded flag is for.

**Guarded by a grep, deliberately.** `the_capture_path_does_not_change_this_
machines_graphics_settings` asserts no `gpu_preference::set` appears in the
capture sources. A crude test, and the right shape for this fault: the problem
was never that the write did the wrong thing, it was that a registry write
existed on a path nobody was looking at.


## One Machine, One Identity, In Both Directions

**Problem**: approving an admin device did nothing. The owner was asked to
approve their own laptop on every single connection, for ever.

**Why.** The host remembers the endpoint ids it has approved and checks the next
connection against that list. The list could never match: the guest side bound
its endpoint with no key at all, and iroh mints a fresh keypair for an endpoint
that is not given one. Every dial arrived as a different machine. Nothing was
wrong with the remembering — there was simply nothing stable to remember, and no
amount of testing the approval logic would have found it, because the approval
logic was correct.

**Decision**: `network::adopt_identity` gives the process a stable key for the
connections it *makes*, taken from the same stored identity the host side is
known by. A machine is one identity in both directions, which is what "trust
this device" has to mean for the sentence to be true.

**One endpoint, not one per dial.** This is the part that does not follow from
the fix and had to be found. Binding per dial was fine while every dial carried
a throwaway key; under a stable identity, two endpoints live at once are two
machines claiming one name, and a person with two session tabs open has exactly
that. `OUTGOING_ENDPOINT` binds once and every session shares it — so
`GuestClient::shutdown` now closes its *connection* and leaves the endpoint
alone, where before it closed the endpoint and would have taken down the other
tabs.

**The test that caught it** was `several_guests_arriving_together_are_all_
answered`, which had been passing for a year and started failing the moment an
identity was adopted anywhere in the same test binary. It now adopts one
explicitly: a test that only covers the interesting case when it happens to run
second covers nothing.


## A Remembered Address Was Last in the Code and First in Practice

**Problem**: the address a host answered on last time was being tried before
discovery had a chance, and one failed dial with it ended the whole attempt. The
mainline DHT — the lookup that still answers where the others are blocked — was
never once observed being used.

**How a last resort became first.** The intent was already right and the comment
already said so: everything that can say where the host is *now* goes first, and
the saved address only if all of it comes back with nothing. The gate was
`nothing_found` — no address from any lookup.

That gate is decided by whichever lookup answers first, and on a filtered link
the two that answer first are the two that *fail* first. mDNS and DNS do not
hang there; they complete, with nothing, in a second or two. So `connect`
returned `NoAddress` almost immediately, the saved address went in at about two
seconds, and the DHT — which needs eight, and often a second ask — never got a
turn. A condition written to mean "after everything else" was in practice
"after the fastest two".

**Decision**: the gate is a clock. `DISCOVERY_GRACE` is fifteen seconds, and
nothing about how quickly a lookup gives up can shorten it.

**And the saved address is no longer latched.** It used to be inserted once and
left in the address set for every remaining attempt, so a stale address that
swallows packets took each retry with it. Whether it is in play is now decided
per attempt, from the clock — so a dial that fails with it is followed by one
built again from whatever discovery has since found.

**Every failure is retried while there is time.** The loop used to retry only
`NoAddress`, on the reasoning that anything else was a definite refusal and
retrying would turn a clear failure into a slow one. True on a good link. On a
filtered one a relay handshake that fails is not a refusal — it is a route that
did not work this second — and a dial that spent its whole budget was treated as
fatal, which on the network this software is written for is the *ordinary*
outcome of the first ask. One try, then a failure, with the budget unspent.

The bound is the deadline, not the first discouraging answer: `DIAL_TIMEOUT`
drops to twelve seconds so more asks fit, and `FIND_WAIT` rises to forty-five.
Waiting costs nothing that matters — how fast a session runs is a property of
the path it found, not of how long finding it took.

**Two guards.** A compile-time assertion that the budget leaves room for at
least one whole attempt after the grace, so the fallback cannot be tuned out of
existence. And `a_stale_saved_address_does_not_take_the_attempt_with_it`, which
hands a guest two dead addresses — one that refuses at once, one that swallows
packets — for a host mDNS can find, and requires it to connect anyway.

**What the existing direct-path test does not prove**, now stated in it: both
ends are on this machine, so mDNS finds the very same loopback address that is
passed as the saved one. Arriving quickly there says nothing about which of the
two got there first. An attempt to assert a timing floor on it failed for
exactly that reason, which is why the ordering is pinned by the stale-address
test instead — where the two disagree and only one of them works.


## The Remembered Address Is Gone

**Reverses** *A Remembered Address Was Last in the Code and First in Practice*,
and the two entries above it. Three attempts were made to make caching the
address a host answered on safe:

1. Added alongside the relay, on the reasoning that iroh races candidates and a
   stale one simply loses.
2. Added only once no lookup had answered.
3. Added only once a fifteen-second discovery grace had elapsed.

Each was reported as still breaking connections that worked the moment the
saved address was deleted. The third failed on two PCs on one LAN.

**Decision**: there is no saved address. `connect_reporting` takes no address
list, `Prober::probe` takes none, and `SavedConnection` has no field for one.
The parameter is gone rather than defaulted, so re-adding it is a deliberate
change to a signature and not a quiet extra argument.

**Why the idea was wrong, not just the gating.**

*It was never evidence about the present.* A host that changed network, or was
given a different port by its NAT, leaves an address that still routes — to a
NAT that now maps it to somebody else, or to nothing. Those do not lose a race
quickly.

*It could only be refreshed by the thing it prevented.* The cache was written
from a **successful** session's direct path. So a stale entry was cleared by a
successful connection, which is exactly what it was blocking. There was no path
out of a bad entry except deleting the file by hand, which is what kept being
reported.

*It duplicated work already done better.* iroh keeps its own path state per
endpoint. mDNS places a peer on a local network with no relay, no DNS and no
internet. And the DHT record carries dialable addresses rather than only a
relay — `discovery::publishable` exists for exactly the censored-link case a
remembered address was supposed to cover. Where the host *is* had three live
answers and a fourth stale one helping none of them.

*And it was in two places, not one.* Every fix went into the dial loop. The
prober — which decides what a card says — inserted the saved address
unconditionally and first, with no lookup given a chance ahead of it, so a
sweep clinging to a stale address reported a reachable host as offline. That is
the one thing a card exists to answer.

**What replaced the test.** `a_stale_saved_address_does_not_take_the_attempt_
with_it` became `discovery_alone_reaches_a_host_on_this_network`: two ends on
one machine, mDNS the only thing that can place the host, and a session that has
to start. If discovery alone were not enough, removing the cache would not be
safe — so that is the assertion.


## A Failure Has to Say What Was Tried

Several rounds of "it cannot find my other PC" were diagnosed by reading code,
because nothing anywhere said which ways of finding a host the machine even
had. Two PCs on one LAN that cannot see each other is mDNS not running. The
same pair on a filtered link is the DHT not running. The log said neither.

**mDNS failing to start was logged at `debug`**, on the reasoning that a network
without multicast is normal and not a fault. True of the network and wrong for
the log: a machine that cannot start mDNS cannot find a peer on its own LAN at
all, and on Windows the usual causes — the firewall blocking UDP 5353, another
program holding the port — are faults, and fixable ones. It is a warning now,
and so is the DHT failing to start, which on a filtered link is the lookup that
still answers.

**And a dial that gives up now says what it had**: how long it tried, over how
many attempts, which of mDNS / DNS / DHT are running on this machine, and
whether the code carried a relay. One line, at the end, naming the thing that
would otherwise take another round of guessing.


## A Remembered Frame Has to Remember Its Size

**Problem**: `Nv12 buffer too small: 1709568 < 3981312`, and the session ended.
Reproduced by resizing a shared window on the host and then pressing 1:1 on the
guest.

**What happened.** The compositor path re-sends its last picture when nothing
has changed, so a guest joining a still window has something to draw. It sent
the stored bytes labelled with whatever the emitted size had become — and the
emitted size moves for three reasons that do not produce a new frame: the guest
capping it, the controller shedding pixels, and the captured window being
resized. A window that has just been resized and then left alone produces
nothing to correct it with, so a stale buffer went out under the new dimensions
and the encoder refused it.

The dimension check upstream passed, which is what made it look like a capture
bug rather than a sizing one: the frame's *declared* size was right. Only its
length disagreed.

**The duplication path does not have this**, because it clears its copy at every
site that changes the size. That is the same discipline, and it is why the
compositor path did not get it: remembering to clear a second field at three
call sites is what gets forgotten at the fourth. It is the encoder `Drop` bug
again, one field apart.

**Decision**: the size travels with the bytes. `last_frame` holds
`(width, height, Bytes)`, and the idle refresh compares before re-sending. The
two cannot disagree, and a mismatch now means "nothing worth re-sending" rather
than something that will be refused downstream.

**And a still source is asked for a frame.** Withholding the stale picture is
correct and, on its own, leaves a guest who pressed 1:1 watching nothing happen
until something moved inside the window — the compositor delivers when the
*source* changes, and none of these change the source. So the setters recreate
the frame pool at the size it already has, which provokes a delivery. Only the
setters: `resize_to` has already recreated it at the new source size and would
otherwise do it twice.


## Reaching a Host Is Retried, Not Attempted Once

**Problem**: a LAN connection that worked about half the time, with pressing
Connect again as the fix.

Every way of reaching a host fails intermittently. Discovery can answer a moment
late. Hole punching can fail and succeed on the same pair of machines minutes
apart. A relay can have a bad second. None of those are properties of the two
machines; they are properties of the moment, and the remedy for all of them is
another go.

**Decision**: `run_guest_session` retries for `REACH_FOR` — two and a half
minutes, about three whole attempts, each of which is itself several dials.

**Why the retry is there and not in the dial.** The dial is not the only part
worth repeating. `connect_with_code_reporting` also fails *after* a successful
dial, when no direct path opens inside `DIRECT_PATH_WAIT` and the session is
refused rather than run over a relay. That is the failure most worth another go,
and raising the dial's own budget would not have touched it.

**The boundary is "room for another attempt", not "budget spent."** Checking the
latter starts an attempt it cannot finish, and the session then runs past the
point it promised to stop. Pinned by a test, along with the budget being large
enough to cover more than one real attempt — a smaller one would be pressing
Connect by hand with extra steps.


## A Window Share Is a Set, and the Picker Shows Pictures

**Problem**: sharing one window was chosen from a dropdown of text, and the
grant could hold exactly one window.

**The dropdown was the wrong control for the question.** It named each window in
a line of text, in a popup as wide as a card. That is the one thing window names
are worst at: four Explorer windows are four folder names, three browser windows
are three page titles, and anything with an unsaved document says "Untitled".
The person choosing knows their window by sight — they were looking at it a
moment ago — so the picker is now a grid of tiles with a picture of each window
in it, in a window with room to show them.

**Decision**: `DisplayGrant::Window(u64)` became `Windows(Vec<u64>)`, and the
guest may switch between the windows in it.

**Why widening it does not weaken it.** The old doc comment argued that a window
share removes the guest's choice, "because the choice is the whole point". That
was the wrong reading of its own property. What makes a window share safe is not
that there is one window; it is that **nothing outside the list is ever
streamed** — the same property `Only` has for screens. Somebody walking a
colleague through a spreadsheet usually has the spreadsheet *and* the mail it
arrived in open, and being unable to grant both is what pushes them to grant the
whole screen instead. `a_grant_of_several_windows_permits_those_and_no_others`
pins the boundary: every id in the list allowed, every id outside it refused, and
no screen reachable by either.

**The grant is ordered as the picker listed it, not by handle.** The first
window is what the session opens on, and handles sort by whatever number Windows
happened to hand out — so collecting the chosen set directly would open on an
apparently arbitrary one of them, and no arrangement of the picker could change
that.

**Windows open on the first; screens can open on all.** Two monitors sit beside
each other on a desk, the host knows where, and composing them reproduces that
arrangement. Windows overlap, in a stack whose order changes as somebody works —
a composition of them would be a layout this end invented.

## `PrintWindow`, Not the Capture Backend, for Thumbnails

**Decision**: the picker's pictures come from `PrintWindow` with
`PW_RENDERFULLCONTENT`, on a worker thread, into a GDI DIB section.

**Why not Windows.Graphics.Capture**, which is what the streaming path uses and
gives a better picture: it costs a graphics device, a frame pool and an
asynchronous wait *per window*, and on the Windows builds without
`IsBorderRequired` it draws a yellow border around whatever it captures — so
opening the picker would flash a ring around every window on the screen. The two
answer different questions and the cheap answer is the right one for a picker.

The trade is that some windows come back blank; a program drawing through its own
swapchain need not honour the request. Measured on the development machine, nine
of ten. So the caller gets an `Option` rather than a grey rectangle, and a tile
with no picture shows the program's initial — a picker cannot tell "no picture"
from "a genuinely blank window" if it is handed an empty image.

**It has to be off the interface thread.** `PrintWindow` is a `SendMessage` into
the target's message loop and does not return until that program answers. A
program that is busy — or wedged, which is often exactly the one somebody wants
help with — would freeze the interface for as long as it stayed that way, on the
machine of the person being asked to allow a connection. Results are sent one at
a time rather than as a batch, so a slow program delays only its own tile.

**Gotcha, and it produced ten black rectangles**: a device context is not the
picture. The picture is whatever bitmap is *currently selected into* it, and a
fresh DC comes with a 1×1 monochrome placeholder in that slot. Restoring the
previous bitmap after `PrintWindow` — which reads better as "borrow, use,
return" — means the shrinking blit reads that placeholder instead. It succeeds,
and every window comes out one flat colour. The restore has to happen after the
last read. Caught by `every_thumbnail_taken_is_the_size_it_says_it_is`, which
reported 0 of 10 windows drawing themselves; without that test this would have
shipped as a picker full of blank tiles.

**Gotcha**: the alpha channel is thrown away. `PrintWindow` leaves whatever the
window's own drawing put in that byte, which for most of them is zero — carried
through, the tile is invisible.

**Gotcha**: a window that declined to draw leaves the buffer exactly as
allocated, which is one flat colour. That is the check that turns "it worked and
produced nothing" into "there is no picture", and it is what made the bug above
visible rather than merely wrong.

## The Window Watch Follows the Switch

**Decision**: `SharedWindowWatch` is rebuilt when the session changes window,
and it knows whether the guest has been told what it holds.

**Two bugs, both from a watch that outlived its window.** It is pointed at one
handle, and a session granted several windows can be told to show a different one
at any moment. Left where it was, it reports the state of a window nobody is
watching — pausing the stream because the window the guest *left* was minimised,
and saying nothing when the one on screen is.

**And a switch is news even when the state is not.** The watch reports *changes*,
measured against the last thing it read. Point a fresh one at a second window
that happens to be doing the same thing as the first — both showing, the ordinary
case — and nothing has changed, so nothing was sent: the guest went on naming the
window it had left. Hence `owed`, which is false at session start (where
`SessionStart` carried the state) and true after a switch (where nothing has).
`switching_to_another_window_is_announced_even_when_nothing_changed` pins both
halves.

**A switch to a window that is not on screen is taken, not refused.** A guest can
pick a window that has since been minimised, and there is nothing to build a
capture from while it is away — so `create_capturer` fails, the switch is
reverted, and the guest is left with its switcher pointing at a window it is not
being shown. Instead the target is adopted, the capture parks on a stub, and the
guest is told the window is minimised: the same path a window minimised *during*
a session already takes.

**Gotcha**: the guest's `watching` used to be assigned twice on `SessionStart` —
once for the window case and then unconditionally from the display list, which is
empty for a window session. The second overwrote the first with `None`. Harmless
while a window session had nothing to switch between; not harmless now.

**Gotcha**: the picker re-reads the window list when it *opens*, not when the
dialog does — those can be minutes apart while a guest waits. But a window
already chosen and then minimised is kept on the list explicitly, because
`shareable()` answers "what could somebody point at" and excludes it: dropping it
would silently un-choose something nobody touched.

## In a Window Session the Pointer Maps Through the Window

**Problem, found while wiring the switcher and older than it**: nothing ever
told the injector where a shared window is.

A guest sends pointer positions across the picture it was sent, and the injector
turns them back into desktop pixels through the rectangle being shown
(`Shown`). Every display path sets that — on session start, on a display switch,
on the desk being rearranged. The window path set it nowhere, so the rectangle
stayed at its fallback: the whole primary screen. A click in the middle of a
shared window landed in the middle of the *screen*.

**Decision**: the capture loop keeps `Shown` pointed at the shared window's own
rectangle.

**Checked every tick, not on the switch.** A window is not a monitor. It is
dragged and resized while somebody is using it, so a rectangle fixed at session
start is wrong the first time the window moves. The DWM read is cheap and the
injector is only told when the answer changes.

**And the rectangle is `DWMWA_EXTENDED_FRAME_BOUNDS`, not `GetWindowRect`.**
Since Windows 10 a window's outer rectangle includes several pixels of invisible
resize border on three sides, while what the compositor hands over as the
window's picture is its visible extent. Mapping through the larger rectangle puts
every click a few pixels off, consistently — which reads as a calibration
problem and is really a border nobody can see. It is also the measurement WGC
itself sizes the capture item by, so the two agree by construction rather than by
coincidence.

## This Machine Hosts on One Key and Dials With Another

**Problem**: pasting this machine's own guest code into it sat "looking for the
host" for two and a half minutes. The log said what the interface did not:

```text
gave up after 24 attempt(s) over 46.16s:
Connecting to ourself is not supported
```

**One key, two endpoints.** The host binds an endpoint with the machine's
identity, and — since *Every Dial Carries This Machine's Identity* — so did the
client. Two endpoints, one endpoint id. So a loopback dial was literally asking
iroh to connect to itself, which it refuses outright.

**And that was the smaller half.** The relays allow one connection per endpoint
id and boot the other:

```text
Relay server reports problem: Another endpoint connected with the same
endpoint id. No more messages will be received.
```

Sixty-two of those in one log, on an ordinary run. Whichever of the two
connected last held the slot — so **making an outgoing connection knocked this
machine's own host off the relay**, and being on the relay is how a distant guest
finds it and how hole punching is arranged. A host that works or does not
depending on whether anybody has dialled out from it recently is exactly the
intermittency that was being chased under *Reaching a Host Is Retried*.

**Decision**: the dialling key is `blake3::derive_key` over the hosting key, with
a domain-separated context string.

**Why derived rather than stored.** Recognition needs the id to be *stable*, not
to be the host's — a host trusting a returning admin device remembers whichever
id dialled it. Deriving gives stability with nothing new to store, nothing to
migrate, and no second secret to keep safe; the derivation is one-way, so the
dialling key never discloses the hosting one.

**Upgrading asks once more.** A device trusted under the old id now arrives under
the new one. One approval per trusted device, once.

**And a loopback connection now works**, which is worth having: it is the
cheapest possible end-to-end test of capture, encode, discovery, approval and
display, and it needs one machine.

## A Dial That Cannot Succeed Is Not Retried

**Decision**: the dial refuses a target equal to our own endpoint id before the
loop, rather than inside it.

Every other failure is retried while the budget lasts, deliberately — on a
filtered link a route that failed this second is not a refusal. That reasoning
does not cover dialling ourselves: iroh refuses instantly and will go on
refusing. Left to the loop it spent the whole 45-second find budget over 24
identical refusals, and then the outer retry ran it twice more.

**The general shape is worth remembering**: a retry loop written for transient
failures will happily spend its entire budget on a permanent one, and the
symptom is indistinguishable from the network being slow. If a failure is
knowable before the first attempt, answer it before the first attempt.

## Windows Compose Into a Grid, Screens Into Their Arrangement

**Decision**: `DisplayTarget::Windows` tiles the granted windows into a grid;
`DisplayTarget::All` keeps composing screens where the host has them.

**This reverses an earlier entry.** *A Window Share Is a Set* argued there was no
honest way to compose windows, because "windows overlap in a stack whose order
changes, so a composition would be a layout this end invented." The layout is
invented — that part was right — and it is the wrong thing to object to.
Reproducing the desktop arrangement would be the *dishonest* option here: two
maximised windows sit exactly on top of each other, so a faithful composite of
them is a picture of one window, and seeing all of them is the entire reason for
asking. A grid says what it is.

**Cells are the size of the largest window**, so nothing is enlarged. The
alternative — a cell per window, sized to it — makes ragged rows; the
alternative to *that* upscales a 400-pixel utility window to sit beside a 4K
editor, spending the picture's resolution on whatever needed it least.

**What it costs is the pointer**, and that is the substantive change. Every other
picture this streams is a uniform scale of one rectangle of desktop — a screen, a
window, even a wall of screens, because screens tile the desktop by construction.
A grid of windows is not: the sources are scattered, possibly on different
monitors, possibly overlapping. So `Shown` became `Mapping`, which is either one
rectangle or a list of tiles, and a click is mapped by finding its tile first.

**A click between the tiles lands nowhere**, deliberately. That space is not part
of the host, and the nearest alternative — clamping to some window — puts the
pointer somewhere nobody aimed.

**Gotcha, and it is the usual off-by-one**: the position must stay a fraction
until the last division. A guest points with more precision than the picture has
— a 1000-wide window shown in a 100-wide tile means one picture pixel is ten host
pixels — so rounding to the picture pixel first and scaling up afterwards
multiplies that rounding by ten. Measured at exactly that before it was fixed.
The other half is the edge: positions are continuous over `0..span` and the
result is clamped to `onto - 1`, because mixing that convention with the
`0..=span-1` one puts a click a pixel past the tile's own right-hand edge.

**Staleness is different for the two.** A wall of screens is stale when the desk
is rearranged. A grid of windows is *not* stale when a window moves — rebuilding
on every frame of a drag would be unusable, and the grid is not where they are
anyway. It is stale when the set of granted windows that can currently be
captured changes, which is what lets a minimised one come back: comparing
against the wanted list rather than the present one is what makes that work, and
comparing against the original request alone would make it permanently stale.

## What Is Shared Can Be Changed Without Reconnecting

**Decision**: clicking an established connection card opens the sharing dialog
again, and applying changes what that session sees immediately.

**Why it was not enough to decide at the door.** Approval answers "may this
start, showing what", and then the session runs for an hour while what is on the
screen changes. Somebody who shared a screen to demonstrate one thing should be
able to narrow it to a window without disconnecting the person they are helping;
somebody who shared one window should be able to add a second.

**The permission and the picture move together.** Narrowing a grant so that it
excludes what is on screen would otherwise leave the session streaming something
it is no longer allowed to stream — the permission right and the picture wrong,
which is the worse half to get wrong. A target still inside the new grant is left
alone, because the opposite mistake is nearly as bad: adding a second window must
not yank the guest away from the one they were reading.

**The offer is replaced wholesale, not edited.** `SharedChanged` carries the
whole list for the same reason `SessionStart` does — a guest that missed one
message would otherwise hold a menu assembled from edits it half received.

**The dialog opens on what is true**, not on the defaults the approval dialog
uses. That dialog offers the smallest useful answer because it is deciding
something that has not happened; this one is editing what somebody is looking at
right now, and opening it anywhere else would misreport the session and then
apply that misreport to anyone who pressed the button without reading it.

**There is no Deny on it.** Declining to change is closing it, and ending the
session is the kick button, which has not moved.

## A Window Icon Carries a Letter, a Screen a Number

**Decision**: the guest's window switcher marks each entry with the first letter
of the program's name; the screen switcher keeps its digits.

A screen's number is what a person would count — the monitors are in a row, and
the second one is the second one. Windows have no such order: the second entry in
a list is the second entry in a list, and a "2" says nothing about which program
it is. The initial does. With a mail client, a browser and a spreadsheet shared,
M, C and E are three things somebody can aim at without reading the tooltip.

It is not unique — two Chrome windows are two Cs — which is why the caption stays
on the hover. Ambiguous beats meaningless.

**The frame it is drawn on is `empty_button.svg`, not `app_window.svg`.** The
window art has a title bar across the middle, which is exactly where the letter
goes, and the two collided. Shrinking the letter to fit between them makes it
unreadable at the sixteen points this row is drawn at. A mark drawn by the
caller wants art that leaves the middle alone.

## A Composite Follows Its Windows Without Being Rebuilt

**Problem, reported from use**: an odd-shaped window came out squashed in the
composite, and switching to that window and back corrected it.

A panel kept the slot it was given when the composite was built. A window's size
is not a fact for the life of the wall the way a display's is — somebody drags an
edge and it is a different shape — so a frame of the new shape was being fitted
into the old slot. Rebuilding is what corrected it, which is precisely the
signature of a layout that only refreshes on rebuild.

**Decision**: the placement is re-read each tick and the grid recomputed, without
touching the captures.

**Why not simply mark it stale.** Tearing down N capture sessions and building N
more would be a second of black picture to fix some arithmetic — and it would
happen on every frame of a drag. Nothing about the capture goes stale: the
compositor follows the window and does not care what shape it is. Only the
arithmetic does.

**And the arithmetic now has one home.** The build and the relayout each computed
the grid, and two implementations of a layout eventually disagree — the symptom
being the picture jumping the first time anybody resized a window. `grid` is the
one definition, and it is what the tests exercise.

**Note the whole grid moves, not one panel.** The cell is the largest window, so
growing any window regrows every cell and moves every other window. A per-panel
fix-up would have looked right in the common case and drifted in that one.

## The Composite Was Built One Window at a Time

**Measured, after "switching back to the composite takes a while"**: about 200 ms
per window — roughly a third creating a graphics device, the rest setting up the
frame pool and capture session — done one after another. Nine windows took **3.7
seconds**.

Nothing about the setups is shared, so the only reason it was serial is that it
was written as a loop. Built at once: **0.64 seconds** for the same nine.

**The order is the order they were asked for, not the order they finished.** The
grid is positional, and a composite whose cells shuffled according to which
window's driver answered first would be a different picture every time it was
opened.

**What is left is still mostly per-window setup.** Sharing one graphics device
across the panels would save another ~70 ms each, and is not done: it puts nine
capture sessions on one device's lifetime, on the hot path, for a fraction of
what parallelising already recovered. The measurement is in
`report_how_long_a_composite_takes_to_build`, which is why it is kept rather than
deleted after the fix — "a while" is not a number, and the next person to wonder
should be able to get one.

## Parking Is Not Allowed to Swallow a Request

**Problem, reported from use**: close one of several shared windows, open the tab
that says so, ask for the composite again — the stream freezes and stops
answering every further switch.

A shared window that is minimised or closed has nothing to give, so the capture
loop parks on a stub and sleeps rather than spinning. It did that by skipping
straight back to the top of the loop, and **the display switch is below that
point**. So a watch on a window that had gone held the session there for good:
the picture froze, and nothing the guest asked for was ever read.

**Decision**: parking is conditional on nobody waiting for anything else, and the
condition is a named function with a test on it rather than two lines inside a
loop.

The rule it encodes: *parking is an optimisation for having nothing to do, and
"somebody asked for a different picture" is something to do.* The same mistake
was about to be made a second time in the same loop — the composite's
nothing-left-to-compose path was also written as a sleep and a `continue` — and
naming the rule is what caught it.

**And a window composite goes stale for its own reason.** `layout_changed` is
answered by both kinds of wall, and the recovery was written entirely for
screens: it names the primary display as what is being shown and remaps the
pointer to the whole desk. For a window session the first is a lie the rest of
the loop then acts on, and the second silently sends every click to a desktop
coordinate instead of into a window. A window composite now rebuilds in place,
keeping its target, and leaves the pointer to the tiles that are maintained a few
lines above.

Closing *every* shared window left the guest looking at the last frame with
nothing saying why. Fixed next.

## The Offer Follows the Windows, and Says When There Are None

**Decision**: the host keeps the guest's list of shared windows current, and the
guest is told explicitly that a session shares windows rather than inferring it.

**A closed window loses its button.** It used to keep one, which reported it
closed on every click, for the rest of the session. The list the guest builds its
switcher from is now the windows still *open*, resent whenever that set changes —
so the button goes with the window, and when the last one goes so does the
composite button, because there is nothing left to compose.

**Why the capture loop owns this.** It is the only part of a session that already
asks, every tick, what the shared windows are doing: the composite is rebuilt
from that answer and the pointer is remapped from it. Anywhere else would be a
second poller of the same handles, and the two would disagree about when a
window went.

**Announced on change, not on a timer.** It is asked sixty times a second and the
answer changes twice an hour. A message per tick would be a message per frame,
carrying a fact nobody asked about, on the same connection as the video.

**And `sharing_windows` is a field, not an inference.** Both lists empty happens
two ways and they are not the same thing: a window session whose every window has
closed has nothing to show and should say so, while a host that cannot enumerate
its displays *also* sends no displays and is streaming its screen perfectly well.
Guessing from the lists would put "every shared window has been closed" over a
working screen share. Pinned by
`a_host_that_cannot_enumerate_its_displays_still_starts_a_session`.

**The notice is checked before any one window's state**, because there is no
longer a window to be in a state — a composite watches none of them individually
and all of them have gone. It is the same failure the per-window notice exists to
prevent, one level up: a picture that stopped, with nothing saying why.

## A Card Senses Clicks Underneath Its Own Controls

**Problem**: making the incoming card clickable took the kick button with it.

egui resolves overlapping click targets by taking the **last one registered** —
`hit_click` is `hits.filter(sense.click).last()`. The card's interaction was
added after its contents, so a card-sized target sat on top of every button on
it and swallowed their clicks.

**Decision**: the card registers its own click *before* its contents, over the
frame's content rect. Each control added afterwards is above it and takes its own
clicks back; what is left for the card is the space between them, which is what
"click the card" ever meant.

**Hover is not the same question.** `contains_pointer` is true for everything
under the pointer, unlike a click, which goes to exactly one — so the card still
tints when the pointer is over a button on it, which is what it should do.

## The Symbol Check Reads the Source, Not a List

**Problem**: `🗔` went on the window picker's button and drew an empty box.

The check for this existed and did not fire. It held a hand-written list of
symbols with a note saying that picking a new one meant adding it — and **a list
you have to remember to update cannot catch the case where you forget**, which is
the only case there is.

**Decision**: the symbols are read out of the sources and the catalogues, the way
`i18n_keys` reads the keys the code names. Putting one in a label is now enough
to have it checked.

**It found two more the moment it ran.** The connection graph drew `✓` (U+2713)
for a completed step and `✗` (U+2717) for a failed one, and neither has a glyph
in the bundled fonts — so every finished step had been showing a box.
`report_which_marks_have_glyphs` is kept beside it: choosing `✔` and `×` as the
replacements was a measurement rather than a guess, and the next person choosing
a mark should not have to guess either.

**Only what could be drawn counts.** The first version scanned whole files and
failed on box-drawing art in a module header and on the doc comment explaining
which symbol had been *replaced* — noise that would have had the next person
deleting the test rather than reading it. Rust files contribute their string and
character literals; `.lang` files contribute everything but their own comments.

**And the probe's data is written as code points.** Half of what it tests is
there *because* it has no glyph, and spelling those out would make the report
test fail the scanning one.

## A Picker Button Says What It Opens

The window picker's button read "Whole screens" while nothing was chosen. True
about the session, and wrong on that control: a button for choosing windows,
labelled with a screens answer, reads as a screens control — reported as looking
like a mistranslation, which is a fair thing to conclude from it.

It says "Individual windows…" until something is chosen and the count afterwards,
because then the button *is* the answer. What the session will show while it is
empty is already visible in the row above, which is lit.

## The Offer Carries What Each Window Is Doing

**Problem, reported from use**: minimising the last shared window left the
composite showing its last frame, frozen, with nothing saying why.

The same failure as closing them all, which had already been fixed — and not
covered by that fix, because **a minimised window does not leave the offer**. It
is still granted, still listed, and can come back. Only its state moved, and the
offer did not carry one.

**Decision**: the offer is `Vec<SharedWindow>`, not `Vec<WindowInfo>` — the same
identity fields plus the state. The guest could not work that out for itself, and
the sizes it replaced were never read at that end.

**Two states, not one flag.** "Every shared window is closed" and "every shared
window is minimised" want different sentences and offer different hope: nothing
at the host brings a closed window back, while a minimised one is one gesture
away. `WindowsAway` is that distinction, and it is what `source_is_live` now
answers to as well.

**And the offer is resent when a state changes**, not only when the set does. An
offer compared by id alone does not move when a window is minimised, which is
exactly the case that went unreported. The *title* is still not compared: a
browser retitles on every navigation, this is asked sixty times a second, and the
window being watched already carries its live title on its own message.

## A View Being Fetched Says So

**Problem**: switching views left the old picture up, unchanged, for as long as
the switch took.

Which is up to two thirds of a second for a composite of several windows,
because the host's capture thread is inside `create_capturer` for that whole time
and produces nothing. The view just asked for looked exactly like the view
already there, frozen.

**Decision**: a dimmed overlay with a spinner and one line, from the request
until the first frame after the host's answer.

**Two steps, not one.** Clearing it on the next frame alone would clear it on a
frame of the view being *left* — the host answers the request straight away and
its capture thread switches afterwards. So the answer arms it and the frame that
follows clears it.

**And it gives up.** A request the host never answers should leave the picture it
has rather than a spinner that never stops; `SWITCH_PATIENCE` is generous because
the wait is real work, and expiring also *forgets*, so the decision is made once
rather than re-made every frame.

**The old picture stays underneath, dimmed.** Same reasoning as the window
notices: what was there a moment ago is context, and a black panel with a
sentence on it throws that away to say what the sentence already says. What
separates this from those is the ring — they describe a state that lasts until
somebody at the host acts, and this is work in progress, which only a moving
thing says honestly.

## `DisplayChanged` Is Sent When It Becomes True

**Problem**: the loading indicator cleared while a stale frame of the old view
was still on screen.

`DisplayChanged` was sent by the reader, the moment a request was accepted — and
the capture thread acts on it afterwards, taking as long as building the new
capture takes. So the message announced a view whose *previous* pictures were
still being produced, and a guest waiting for the new one stopped waiting at the
first frame after it, which was a frame of what it had just left.

**Decision**: a refusal is sent from the reader, because it is final the moment
it is decided. An acceptance is sent by the capture thread, when it has actually
switched. The message then means what its own documentation says it means: what
is *actually* being streamed.

**A failed switch is announced too.** The reader now says nothing for an accepted
request precisely so this can be the one answer, whichever way it goes —
otherwise a switch that could not be built would leave the guest waiting for a
picture nobody was going to send.

**And "no picture is coming" is an answer as well.** A switch to a minimised
window produces no frame at all, so the guest also stops waiting when it is told
the window is away — otherwise it would watch a spinner until it gave up, over
the very message explaining why there was nothing.

## A Pump Loop Written for a Source That Blocks

**Reported**: "slow capture" and occasional "slow decoding", with both machines'
GPUs idle, and a blurry picture. Measured from the host's own log: **six to ten
frames a second**.

The wall gives each source a thread that pumps it into a slot, because a display
subscriber *blocks* for about twenty-five milliseconds waiting for its screen to
change — so an empty answer had already cost a wait, and going straight round
again was free.

A window does not work that way. `TryGetNextFrame` is in its name: the compositor
answers "not yet" immediately. The loop became a spin at whatever rate the CPU
allows, holding and releasing the panel's lock throughout — **one core per shared
window**, plus the contention with the thread trying to compose. Nothing was
struggling, exactly as reported; the capture thread simply could not get
scheduled.

**Decision**: a pump that got nothing waits `IDLE_POLL` before asking again.
Short enough to be invisible against a sixteen-millisecond budget, and a source
that blocks on its own pays it only on top of a wait it already did.

## A Composite Fits Each Panel Once, Not Once a Frame

The other half of the same report. A panel whose frame is not its slot's size is
resampled to fit — and a panel is usually the wrong size for a *while*: it is
showing a still window whose last frame was captured before the slot was worked
out, and nothing new arrives until that window changes. So the same picture was
being resampled on every composition.

Measured before: **3.2 seconds per composed frame** across seven windows, in a
debug build — a third of a frame per second. After keeping the fitted copy beside
the frame it came from, and clearing it when the pump stores a new one:
**62 composed/s at 1.0 ms each**, release.

The measurement is kept as `report_what_a_live_composite_costs`, for the same
reason as the build timer beside it: the next person to be told "it feels slow"
should be able to get a number.

**On the blurriness.** The composite is capped to the guest's viewport, and a
grid of windows has an aspect nothing like a guest's window — so fitting it costs
a great deal of detail. `ScalingMode::Actual` reports *no* ceiling at all, so 1:1
is the lever, exactly as it is for a wall of screens. The window hint now says so;
it was the one place that had been left out.

## The Resize Was the Frame Rate

**Reported**: a shared window streaming at sixteen frames a second, with nothing
on either machine busy. "Is there really no way to improve this?"

There was. `box_downscale` is the resize used by every capture that cannot hand
the job to the GPU — a window captured as BGRA, and every panel of a composite —
and it ran on one thread. Measured in release:

| | one thread | threaded |
|---|---|---|
| 2446x1516 → 1200x750 | **22.0 ms** | 5.9 ms |
| 2560x1440 → 1280x720 | 14.4 ms | 4.2 ms |
| 3840x2160 → 1920x1080 | **33.0 ms** | 6.4 ms |

Against a 16.7 ms budget at 60 fps, a single window was over it on the resize
*alone*, before the readback — so the frame rate could not reach its target
however idle the machines were, and the adaptive controller settled where the
budget finally fit. Sixteen frames a second is a 62 ms budget, which is what 22
ms of resize plus a readback plus encoding adds up to.

**Decision**: the resize is threaded, a band of output rows per worker. The rows
are independent, so there is nothing to synchronise beyond the join — the same
shape as the NV12 conversion in `codec_mf`, and for the same reason.

**Each worker addresses its rows twice** — once as a position in the source and
once as an offset into its own slice — and getting that pair wrong produces a
subtly striped picture rather than a crash. The existing tests all used frames
small enough to stay on one thread, so
`resizing_in_bands_matches_resizing_in_one_pass` compares against a plain
implementation at sizes that do not divide evenly.

**Still on the table**: giving the BGRA path the GPU as well. The video processor
already scales for NV12, and a BGRA output format would remove this cost rather
than divide it — which would matter for a composite of many large windows, where
several panels resize at once. Not done here, because threading brought it inside
the budget and the structural change is a larger one.

**And the log now says which path a session took.** `negotiate_format` logged the
GPU case at `debug` and the CPU case at `info`, so a host that was fine left no
trace and a host that was not looked like the only possibility. Both are `info`
now: it is the first question to ask about a session that will not reach its
frame rate.

## A Captured Pointer Is Sent as Movement

**Reported**: in a 3D game the camera was uncontrollable from the guest — but
that same game's menus, and every other cursor-driven interface, worked
perfectly.

That difference is the whole diagnosis. A program that *follows* a cursor wants a
position, which is what every pointer event here carried. A program that
**captures** it does not: it hides the cursor, reads movement, and warps the
cursor back to the middle after every frame so it can keep reading movement
forever. Inject an absolute position into that and the game centres the cursor,
the next position arrives from wherever the guest's pointer happens to be, and
the game reads the jump between them as an enormous flick — sixty times a
second.

**Decision**: `InputEvent::MouseMotion` carries a delta, and the injector sends
it as `MOUSEEVENTF_MOVE` without `ABSOLUTE` — which is what reaches a program
through raw input, so movement arrives as movement.

**It switches itself, on a fact the session already had.** A guest cannot tell
from its own side which kind of program is on the far end; the host can, because
a program that captured the pointer hides it — and that is already reported so
the remote cursor is not drawn as a ghost. So the same fact decides both, nobody
has to know what their game is doing, and a game's own menus go straight back to
positions mid-session.

**The guest holds its pointer in the middle of the picture** while this is on.
Movement is only movement while there is somewhere to move from, and a pointer
left where the user pushed it stops producing anything at the window's edge —
which, in a game, is the moment you try to keep turning.

**The delta is read from the system, not from egui.** The warp is itself a
movement, and egui would report it as one — so the delta would include the
correction, doubling every input and cancelling it on alternate frames. Reading
the position, subtracting the centre and putting it back is one exchange with
nothing to get wrong.

**Buttons and the wheel are not part of the switch.** A game being looked around
in is still being shot in, so this replaces the movement and falls through to
the rest.

**Escaping is focus.** Nothing is warped while the session window is not focused,
so alt-tab frees the pointer, and the `Ctrl+Shift` session shortcuts keep
working throughout.

**Gotcha**: the agent pipe's record kinds are shared between input and
non-input, and 7 was already the touch release. `each_record_kind_decodes_as_itself`
caught the collision — which is exactly what a test that enumerates a wire
format's tags is for.

## Workers in Proportion to the Work

**Reported**: on a sixteen-thread host, a composite pinned every core — with no
loss of performance, and no such behaviour on a smaller machine.

Threading the resize asked for `available_parallelism` workers *per call*, and a
composite resizes every panel at once on a thread each. Sixteen cores times six
panels is ninety-six threads spawned per composed frame, for work that could not
go any faster.

**Decision**: workers scale with the source pixels, capped at eight. A panel in a
grid asks for one or two; a 4K window on its own asks for eight and gets its
resize back inside the frame budget. The whole machine is never handed to one
caller, and no caller is capped for the sake of a neighbour it does not have.

**What is left is real work**, and worth being clear about: resizing several
large windows on the CPU costs what it costs, and a six-window composite genuinely
needs several cores of it. Dividing it differently cannot change that — only
moving it to the GPU can, which is the change deferred under *The Resize Was the
Frame Rate*.

## One Video Processor, Two Answers

**Decision**: `Nv12Converter` became `VideoScaler`, with an [`Output`] of `Nv12`
or `Bgra`. The deferred change, taken.

It was built to convert BGRA to NV12 because that is what the encoder wants, and
the *resize* came along for free — the processor rescales in the same pass. That
made the resize the valuable half for everything the encoder does not take NV12
from: a window composite is assembled in BGRA, so its panels could not use it and
were resized on the CPU instead.

Converting BGRA to BGRA is a scaling blit as far as the driver is concerned. The
only thing that has to be right is the colour space — telling it the output is in
the space the input already was, rather than asking for a conversion to a space
it is already in. Get that wrong and every window in a composite is tinted,
consistently, in a way that reads as a display setting rather than a bug;
`scaling_bgra_on_the_gpu_keeps_the_colours_the_cpu_would` compares against the
CPU resize it replaces, on real hardware.

**Asked for only when the size changes.** At 1:1 there is nothing to do and the
readback is the whole job.

**A refusal is fatal for NV12 and survivable for BGRA.** The encoder was promised
NV12 and cannot be told otherwise mid-stream, so that failure reaches the caller.
A resize has a CPU path that is correct and only slower.

**Gotcha, and it panicked immediately**: the CPU resize that followed the
readback was not gated. With the GPU now producing the emitted size, that block
ran *again* on an already-shrunk buffer while still treating it as full size, and
read off the end of it. Caught by the composite panicking exactly one index past
its own frame.

## Movement Is Sent As It Was Made

**Decision**: relative pointer movement is no longer scaled by the ratio between
the picture and the host's screen. It is multiplied by a sensitivity the user can
see, defaulting to one.

The ratio was inherited from the absolute path, where it is right: a *position*
should land where it was aimed, so it is measured against the picture. Movement
has no such anchor — a program that captured the pointer is reading movement, and
what it does with the number is its own sensitivity setting. Multiplying by a
window-size ratio put a second, invisible sensitivity in front of it, so a guest
window half the host's size doubled every input. Reported as ordinary movements
producing extreme rotations.

**Why it needs a setting at all, when nothing else about the pointer does.** A
position lands where it was aimed whatever anybody prefers. Movement depends on
the game's own sensitivity, the mouse, and the window — so two people on
identical hardware want different values, and the same person wants different
ones per game.

**It lives behind the control it belongs to**: right-click the pointer button on
the session toolbar. A permanent slider would be a toolbar item for a mode most
sessions never enter; in the settings only, it could not be adjusted against the
game it is being adjusted *for*. The setting seeds new sessions; the session
holds the live value.

**Clamped on the way out as well as on the way in.** It multiplies every delta in
the one mode with no cursor on screen to show what went wrong, and a NaN rounds
to zero — a pointer that has simply stopped, in a game, with nothing saying why.

## The Unattended Host Runs Where the Desktop Is

**Decision**: the service no longer hosts. It starts a separate process — this
executable with `--unattended-host` — on the ordinary desktop of the console
session, as Local System, the same way it starts the agents, and that process
runs the application's own hosting path.

Hosting from inside the service meant hosting from session zero, which has no
desktop. Every frame therefore came from an agent: raw pixels over a named pipe,
at native resolution, in BGRA, sent whether or not anyone wanted them. The
controls that make the application's picture cheap — the quality scale, the
guest's viewport ceiling, the GPU format conversion — all talk to a local
capturer, and in agent-only mode there was none, so all three answered "not
honoured" and changed nothing. At 4K that is about 33 MB a frame through a pipe
before encoding. The low-level hook that lets somebody at the machine take the
mouse back sat in a session with no input. The pointer was painted into the
frames instead of being sent separately.

**Why not forward those controls down the agent pipe.** It was the plan, and it
fixes three of the symptoms while keeping the architecture that produced all of
them: frames would still cross a process boundary uncompressed, the pointer and
the local-input hook would still be missing, and every future capture feature
would need a second implementation for the pipe. The only thing the application
has that the service lacked is a desktop, and a process on one is something the
service already knows how to start.

**"Does this desktop have the input" is now asked by name.** It used to be
"can `OpenInputDesktop` open it", which is the same question for the application
— the signed-in user is refused the secure desktop — and the wrong one for Local
System, which is refused nothing. The host would have believed the input was
still on its desktop through every consent prompt and every lock. The agents
already compared names; that code now lives in one place and both use it.

**A capturer can start before its desktop is in front.** Desktop Duplication
opens an output only for the input desktop, so a session that began while the
machine was locked failed before its first frame. The application met this only
when a guest connected during a consent prompt; the unattended host meets it
whenever the machine is locked, which is most of the time nobody is at it. The
wrapper now shows the secure desktop's agent, builds its local capturer when its
own desktop comes back, and replays the scale, ceiling, granularity and format
the session asked for in the meantime — without the replay, a session that began
locked would have run at native size in BGRA for its whole life.

**Stopped by a named event, bound to the service by a job.** A pipe on the host's
standard input would have been one mechanism for both, but `CreateProcessAsUser`
documents that handles cannot be inherited across sessions. A `Global\` event
crosses sessions by design and cannot be pre-created by anyone without
`SeCreateGlobalPrivilege`. Set, the host tells its guests and closes its endpoint
properly, which is what keeps the application's endpoint — same identity — from
being refused by the relay as a duplicate. The job has `KILL_ON_JOB_CLOSE`, so a
host cannot outlive a service that crashed and go on answering as the machine.

**Gotcha, found before it shipped**: the host runs from the same copy of the
executable a reinstall replaces, and stopping it gracefully can take longer than
the two seconds `uninstall` waited for the service to stop. That is the race that
left a week-old service running after a "successful" reinstall. The wait is now
ten seconds, agents are waited for after they are terminated, and the copy is
retried across the moment a just-ended process still holds the file.

**Removed**: agent-only capture, and with it the rotation between desktops that
was added to make it show the right one. Nothing hosts from session zero any
more.

## A Link to the Agent Has to Close When It Is Let Go

**Decision**: a process holds at most one connection to the secure desktop's
agent, shared by every session that needs it, and closing it cancels the pending
read. The host's end of the pipe is opened for overlapped I/O. An agent treats a
failed peek as its host having gone.

Reported as: lock the machine from the guest, click the lock screen, and the
picture freezes — and no new connection shows anything until somebody unlocks
it at the machine. The first lock of the session had worked.

**Three faults, each enough on its own.** The agent's pipe has one instance.
The host's frame reader held that pipe from a thread blocked in `ReadFile`, so
when the input came back and the session let go of its reader, the pipe stayed
open behind it. The agent, which by then was sending nothing because its desktop
no longer had the input, never found out: `take_input` read a failed
`PeekNamedPipe` as "nothing waiting". So the pipe stayed busy until the agent
exited, and every later connection — the next lock, and every reconnect — was
told `All pipe instances are busy`. Separately, each session opened a connection
of its own, so a second session could not have had the secure desktop even with
the first two fixed.

**Why overlapped, found by the test for the fix.** The first version of the
close path sent "frames no longer wanted" before cancelling the read, and hung.
Windows performs the I/O on a handle opened without `FILE_FLAG_OVERLAPPED` one
operation at a time, per handle: the write waited behind a read waiting for a
frame that was never coming. The same rule had been delaying every click and
key sent to the agent until the next frame arrived, which on a still consent
prompt is whenever the caret next blinks. Overlapped, each operation stands
alone, and `CancelIoEx` abandons a wait from any thread.

**The agents capture with `create_desktop_capturer`.** Once desktops were told
apart by name, an agent's own capturer — wrapped, like every capturer, to follow
the input — noticed the input leave and opened its own pipe to ask for frames.
Refused only because a host already held it; between hosts it would have taken
the pipe from itself.

**Verified against a real named pipe**, served the way the agents serve theirs.
Removing either the host's cancel or the agent's disconnect check reproduces the
reported error exactly; the tests run inside a time limit so that a
regression fails with a name instead of hanging the run, which is how the
deadlock above first showed itself.

**Also**: the app hands its settings over again at the moment it hands over
hosting, because a service installed while the app was running had never been
sent them; the second of two control messages sent back to back is retried
briefly across the moment the service's single pipe instance is rebuilt, which
is what had delayed the handover by one heartbeat; and a session logs the first
input it cannot inject rather than every one — one report held 450 identical
warnings in three seconds.
