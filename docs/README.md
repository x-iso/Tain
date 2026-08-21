# P2P Remote Desktop — Architecture Documentation

This directory contains architectural documentation and codebase structure reference. It is kept up-to-date to serve as a context map for fresh sessions and future development.

## Quick Reference

| File | Purpose |
|------|---------|
| [architecture/overview.md](architecture/overview.md) | High-level system design, data flow, protocol messages |
| [structure.md](structure.md) | Complete file tree with module descriptions |
| [decisions.md](decisions.md) | Key design decisions and rationale |

## How to Use This Documentation

- **Starting a new session**: Read `structure.md` for the codebase layout, then `architecture/overview.md` for how components interact.
- **Making changes**: Check `decisions.md` first — it documents *why* things are built the way they are, so you don't repeat past mistakes.
- **Adding features**: Update this directory as soon as structural or architectural changes occur. The goal is zero context loss between sessions.

## Key Constraints (Always True)

1. **iroh 1.x** handles all networking — P2P QUIC with hole punching + relay fallback. Never bypass iroh for direct socket connections.
2. **ALPN = `"rdp"`** is the application protocol identifier across all connections.
3. **Messages are bincode-serialized** and sent over length-prefixed QUIC bidirectional streams.
4. **Passcodes are 32-byte random secrets**, encoded as base58 with `MASTER-` or `GUEST-` prefix.

## Launch Behavior (Always True)

**`cargo run` with no arguments** launches the unified egui GUI directly — Host tab active by default, background tokio task runs the host server and generates master/guest passcodes immediately. No subcommand needed.

CLI-only modes are still available explicitly:
- `cargo run host --generate-master`
- `cargo run guest --passcode "MASTER-xxx" --ticket <hex>`
- `cargo run host --gui` (same as default, but explicit)
