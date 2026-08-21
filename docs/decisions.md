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

**Gotcha**: `scap` crate returns BGRA frames natively on all platforms it supports, so our `ScreenFrame` struct uses BGRA order consistently regardless of which backend is active. If you add a non-BGRA backend (e.g., YUV420 from hardware encoder), convert to BGRA in the capturer before returning.

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

**Decision**: Use `clap` subcommands (`host`, `guest`) rather than a single command with many flags.

**Rationale**:
- Clearer intent — `rdp host --generate-master` is more readable than `rdp --mode host --action generate_master`.
- Easier to add new subcommands (e.g., `rdp relay`, `rdp benchmark`) without flag explosion.
- Standard Rust CLI convention that users are already familiar with.

**Gotcha**: The `host` and `guest` subcommands share the global `--verbose` flag via `#[command(global = true)]`. This is a clap 4 feature — make sure all subcommand structs have matching field types for global flags.

## Future: GUI Layer (egui / Tauri)

**Decision**: The current CLI demo uses stdin/stdout for input simulation. A real guest would render frames in an egui or Tauri window and capture real mouse/keyboard events from the OS.

**Rationale**:
- CLI is sufficient for protocol testing without pulling in GUI dependencies (egui, wgpu, sdl2, tauri).
- The `input_inject.rs` trait is designed so that a GUI layer can implement it by reading from egui's input capture rather than from stdin.

**When adding GUI**: Replace the CLI input loop in `guest.rs` with an egui window that:
1. Receives `Message::Frame` from the network stream → decodes H.264/BGRA → displays via `egui::Image`
2. Captures mouse/keyboard events from egui's input system → sends as `Message::Input`

The protocol layer (`protocol.rs`) and networking layer (`network.rs`) require zero changes for this migration — only the guest mode implementation needs updating.
