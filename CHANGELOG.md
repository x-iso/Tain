# Changelog

All notable changes to Tain are documented here. This project adheres to
[Semantic Versioning](https://semver.org/).

## [0.4.0] - 2026-10-20

### Added

- **Tain works under a finger.** It notices when it's being used by touch and,
  while it is, makes every control easier to hit: a tap reaches further around
  a button, a finger can wander further before a tap stops being one, and a
  drag over text scrolls instead of selecting it. A ring marks where the
  finger is and fills as a press is held, toward the long press that
  right-clicks. Buttons and toggles show being pressed. Controls that only
  appeared on hover have a way in too: a saved host's forget button shows, a
  long press pins the volume slider open, and the fullscreen bar has a handle
  to tap. Moving the mouse puts everything back the way it was.

- **Rename a host from its tab.** Double-click a session's tab, or its name
  when it's the only session, and type. It renames the host's saved card too,
  and what you type there isn't sent to the host.

- **Send a link instead of a code.** Each passcode button now has a link
  segment at its end, which copies the code as a `tain://` link. Clicked on a
  computer with Tain, the link opens Tain and connects. If Tain is already
  open, the link goes to it rather than starting a second copy, and clicking
  the same link again brings that session forward. Tain registers the link
  type for your Windows account the first time it runs, and again whenever
  it's started from somewhere else. Settings → Network turns that off. A link
  also works in *Paste passcode*.

- **Use your own relays.** Settings → Network takes a list of iroh relay
  servers, used beside n0's public relays or, with *Use only these relays*,
  instead of them. Codes and links carry the relay this machine is on, so the
  people connecting to it need no relay settings of their own. If it moves to
  another relay while Tain is running, the codes you copy from then on name
  the new one, and the passcodes themselves stay the same. The unattended host
  uses the same list.

### Changed

- **One row of chrome in the session window.** It used to spend three:
  Windows' title bar, a strip of tabs, and a toolbar over the top of the
  picture. Tabs and tools now share the title bar's row, with minimize,
  maximize and close at its end. The empty part of the row drags, snaps and
  maximizes on a double-click like any title bar, and hovering maximize offers
  Windows 11's snap layouts. Tabs appear once two or more sessions are open;
  one session shows its name instead. In fullscreen there's no header at all:
  the same row comes down over the picture when the pointer reaches the top.
  Disconnect is only in that fullscreen row now, since windowed, closing the
  tab or the window already does it.

- **Tabs are named after the host.** A tab used to show eight hex digits of
  the host's id. It now shows what the host's saved card shows: the name you
  gave it, else the name the host publishes, else "Host #N", numbered per
  saved host. Saved hosts an older version left as "PC 1A2B3C4D" are numbered
  too.

### Fixed

- **Fullscreen covers the taskbar.** The taskbar stayed on top of a fullscreen
  session window. It now stays behind the window for as long as the window is
  in front, and the window steps back as soon as something else takes the
  focus.

## [0.3.0] - 2026-10-19

The first release. Tain is a peer-to-peer remote desktop for Windows: one
connection code reaches a host through NAT, and a session runs on a direct
path or not at all. Versions before this one were never released.

### Added

- **Connection codes.** The host copies a code, the guest pastes it, and
  that's all either side needs: the code carries the host's address and
  relay along with its secret. An admin code is full control. A guest code is
  streamed nothing, not even the screen, until the host allows it, every time
  it connects. Pasting checks the host first and says whether it's ready,
  ready but going to ask, expired, or not answering.

- **Admin codes are approved once per device.** The first connection from
  each device has to be allowed at the host; after that the device is trusted
  and connects straight in. Resetting the admin code forgets every device it
  let in, and Settings → Security can forget them all without a reset.

- **Each guest gets only what it's given.** The popup that allows a guest
  grants mouse and touch, and keyboard, separately, and picks what it may see.
  Both toggle on its card while it watches, and clicking the card changes what
  it sees without it reconnecting. Several guests can be connected at once,
  each with its own grants. Whenever someone uses the host's own mouse or
  keyboard, guest input is ignored for two seconds.

- **Share screens or windows.** A host can share all its screens, a chosen
  few, or every screen at once laid out the way the host has them — or just
  some application windows, with no desktop behind them, one at a time or
  tiled. The guest picks between them from the session toolbar, and is told
  when a shared window is minimised or closed.

- **Virtual displays.** Settings → Virtual display installs a driver and adds
  screens that exist only in software: for a machine with no monitor, or a
  guest who wants a screen at their own resolution. With *Match the resolution
  of whoever connects* on, a virtual display takes the size of the guest
  watching it. Closing Tain removes the displays it made.

- **Unattended access.** An optional Windows service reaches the lock screen
  and UAC prompts, and hosts by itself while the app is closed, with the
  owner's settings and trusted devices. With *Answer when nobody is signed
  in*, the same code reaches the sign-in screen.

- **Found without one server.** On the same network, a host is found in under
  a second with no internet at all. Across the internet, the mainline DHT
  works beside the public lookup service, so a host can be found where DNS is
  blocked. The connection graph shows each step of connecting, how the host
  was found, and where a connection failed. A session the relay would have to
  carry is refused rather than run.

- **Hardware video.** H.264, H.265 and AV1, encoded on the host's GPU where it
  can and decoded on the guest's, with software H.264 and a lossless mode
  behind them. HDR desktops are captured as HDR and brought into range.
  Frame rate, resolution and bitrate follow what the machines and the link
  manage, and the stats say which of them is holding the stream back.

- **Sound and the host's pointer.** The host's system audio streams over Opus,
  minus any applications left out in Settings → Audio — a voice chat, say. The
  host's pointer is sent on its own, so it moves at the guest's frame rate
  rather than the stream's.

- **Pointer capture, for games.** It hides this machine's cursor and sends
  movement instead of a position, so looking around never runs out at the
  window's edge. It switches on when the host hides its own pointer, or with
  Ctrl+Shift+C, and has its own aim sensitivity.

- **Sessions in a window of their own.** One tab per host, several at once,
  with fullscreen, Fit or 1:1 scaling, and an immersive mode that sends every
  key to the host, Win and Alt included. The session shortcuts can be rebound
  in Settings → Shortcuts.

- **Saved connections.** Hosts you've connected to are kept as cards with a
  blurred picture of their screen, to reconnect, rename or forget. Their codes
  are kept in Windows' credential store, never in a file.

- **English and Russian.** The interface follows the system's language unless
  Settings → Language picks one.

- **The version in Settings.** The Settings footer says which version this
  is, beside Reset to defaults.

- **Licences in Settings → About.** Tain is free software under the GNU GPL,
  version 3 or later. Settings → About shows its copyright and licence, and
  **Licences…** lists everything tain.exe includes, with each one's full
  licence.

- **Release builds.** Each release on GitHub has tain.exe attached, built from
  the tagged commit, with its SHA-256 beside it.

[0.4.0]: https://github.com/x-iso/Tain/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/x-iso/Tain/releases/tag/v0.3.0
