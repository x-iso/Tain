# Tain — Architecture Documentation

A context map for a fresh session. Kept current with the code; where it is not,
the code and the tests win.

## Quick reference

| File | Purpose |
|------|---------|
| [structure.md](structure.md) | Every module, what it is for, and where to look first |
| [architecture/overview.md](architecture/overview.md) | How a connection is made, what crosses the wire, how quality is decided |
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
3. **ALPN is `"tain/4"`.** In `network::ALPN`.
4. **Messages are length-prefixed bincode 1.x.** Not bincode 2.
5. **Passcodes are 32-byte random secrets**, base58check with a `MASTER-` or
   `GUEST-` prefix, carried inside a self-contained connection code along with
   the host's endpoint id and relay.
6. **Secrets are never written to a file.** Connection codes live in the OS
   credential store, or nowhere.
7. **A guest code is denied until a person allows it**, per connection, and
   anything that is not an explicit allow is a denial.

## Running it

`cargo run` launches the GUI. There is no command surface — `main.rs` reads a
couple of bare arguments (`--console` among them) rather than parsing
subcommands, and the service and agent modes are entered by the service control
manager rather than by hand.

Debug-only flags, compiled out of release builds entirely:

```bash
TAIN_DEBUG=demo,loopback,settings cargo run
```

| Flag | Effect |
|------|--------|
| `demo` | Fill the connection lists with sample rows |
| `loopback` | Connect to our own host endpoint on startup, to exercise the video path on one machine |
| `settings` | Open the settings window immediately |

## Testing

```bash
cargo test --workspace
```

Tests that need real hardware or real network traffic are `#[ignore]`d and named
so they can be run one at a time. **Do not run `-- --ignored` wholesale** unless
you mean to exercise encoders, decoders and the public DHT at once:

```bash
cargo test -p tain-core --test e2e_iroh reach_the_dht -- --ignored --nocapture
cargo test --release -p tain-core --test pipeline_timing -- --ignored --nocapture
```
