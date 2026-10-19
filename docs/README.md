# Tain — Architecture Documentation

A context map for a fresh session. Kept current with the code; where it is not,
the code and the tests win.

## Quick reference

| File | Purpose |
|------|---------|
| [structure.md](structure.md) | Every module, what it is for, and where to look first |
| [architecture/overview.md](architecture/overview.md) | How a connection is made and approved, what crosses the wire, how quality is decided, what is captured, and what answers with the app closed |
| [decisions.md](decisions.md) | Why things are the way they are, and what each change cost to learn |

## How to use it

- **Starting a session**: `structure.md` for the layout, then
  `architecture/overview.md` for how the pieces meet.
- **Changing something**: `decisions.md` first. It records the sessions that
  were spent finding out why an obvious approach does not work.
- **After changing something**: update these files in the same commit. The point
  is that the next session starts informed rather than re-deriving.

## Constraints that hold everywhere

1. **iroh 1.x does all networking.** Hole punching, relays, discovery. Never
   open a raw socket to a peer.
2. **A session runs on a direct path or not at all.** A relay carries the
   handshake and the address exchange; it never carries video. See
   `network::await_direct_path`.
3. **ALPN is `"tain/6"`.** In `network::ALPN`.
4. **Messages are length-prefixed bincode 1.x.** Not bincode 2.
5. **Passcodes are 32-byte random secrets**, base58check with a `MASTER-` or
   `GUEST-` prefix, carried inside a self-contained connection code along with
   the host's endpoint id and relay.
6. **Secrets are never written to a file.** Connection codes and this machine's
   identity key live in the OS credential store, or nowhere.
7. **A guest code is denied until a person allows it**, per connection, and
   anything that is not an explicit allow is a denial.
8. **An admin code is approved once per device**, keyed by endpoint id and
   never by a name or an address. Resetting the admin passcode forgets every
   device it let in. See `network::must_be_approved`.
9. **What the interface says lives in `i18n/`**, as keys, and a session ends
   with a reason code rather than a sentence, so each end words it in its own
   language. Log output stays English.

## Running it

`cargo run` launches the GUI. There is no command surface — `main.rs` reads a
handful of bare flags (`--console` among them) rather than parsing subcommands,
and every other mode is started for you: the service by the service control
manager, the unattended host and the agents by the service, and the service and
driver installers by the app relaunching itself with administrator rights.

Debugging aids are a comma-separated `TAIN_DEBUG` list, read by `debug_flag` in
`gui/app.rs` and, for `frametime`, by `gui/frametime.rs` itself:

```bash
TAIN_DEBUG=demo,loopback,settings cargo run
```

| Flag | Effect | Builds |
|------|--------|--------|
| `demo` | Fill the connection lists with sample rows | debug |
| `approval` | Open the approval popup for the pending demo guest | debug, since it needs `demo` |
| `settings` | Open the settings window immediately | all |
| `loopback` | Connect to our own host endpoint on startup, to exercise the video path on one machine | all |
| `frametime` | Report where a frame of UI time went, once a second | all |

`loopback` and `frametime` are in release builds on purpose: measuring the
pipeline needs both ends running, and a debug build's per-pixel decode is slow
for reasons that say nothing about the shipped one. Reporting debug numbers as
if they described the real thing has sent an investigation down a wrong turn
once already. A loopback session disables input forwarding, because injection
is real and would move the pointer being read from.

Other environment variables:

| Variable | Effect |
|----------|--------|
| `TAIN_CONSOLE=1` | Give a release build a console of its own, the same as `--console` |
| `TAIN_LOG_FILE` | Write the log somewhere other than `%APPDATA%\Tain\tain.log` |
| `TAIN_PROFILE` | Give a second instance on one machine its own identity, so two can connect to each other |
| `TAIN_CAPTURE=wgc` | Capture through the compositor even where Desktop Duplication works, to compare the two |
| `TAIN_HOOK_WATCH` | Probe the immersive-mode keyboard hook every few seconds, to see when it stops being called. Injects a keystroke each time |

## Testing

```bash
cargo test --workspace
```

Tests that need real hardware or real network traffic are `#[ignore]`d and named
so they can be run one at a time. **Do not run `-- --ignored` wholesale** unless
you mean to exercise encoders, decoders, display drivers and the public DHT at
once:

```bash
cargo test -p tain-core --test e2e_iroh reach_the_dht -- --ignored --nocapture
cargo test --release -p tain-core --test pipeline_timing -- --ignored --nocapture
```

Every string the source asks for has to exist in the English catalogue, and a
test says which do not:

```bash
cargo test -p tain-core --test i18n_keys -- --nocapture
```

The hardware encoder tests occasionally block inside Media Foundation, in the
driver rather than in this code. `watchdog.rs` puts a time limit on them, so a
stall aborts the run with the test's name instead of leaving it hanging.
