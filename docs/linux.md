# Tain on Linux

Where the Linux build stands, what it needs from the machine it runs on, how to
test it, and what each missing piece would take. Kept current with the code;
where it is not, the code and the tests win.

## Where it stands

Tain builds on x86-64 Linux, passes the same test suite as on Windows, and every
release ships `tain-x86_64-linux.tar.gz` beside `tain.exe`. It is a **preview,
and a guest**:

| | Linux today |
|---|---|
| Connection codes, approval, saved connections, discovery (mDNS, DHT), relays, settings, languages | ✅ The same code as Windows |
| Connecting to a Windows host | ✅ H.264, decoded on the processor by OpenH264 |
| Mouse, keyboard, touch, pointer capture (sent to the host) | ✅ |
| H.265 or AV1 from the host | ❌ No decoder. See [The codec gap](#the-codec-gap) |
| Sound from the host | ❌ No ffmpeg build for Linux yet, and nothing to play it through |
| Hardware decode, drawing straight from the GPU | ❌ |
| Immersive mode (system keys to the host, pointer confined) | ❌ Logged as Windows-only |
| `tain://` links, connection notifications | ❌ Stubs |
| **Hosting** | ❌ Streams a test pattern, and input from a guest is only logged |

So the useful thing it does today is put a Windows machine's screen on a Linux
desktop. Everything in the first row is platform-neutral code that already
worked there; what made it a build was fixing the places a Windows-only build
had stopped compiling for anything else.

### The codec gap

The host chooses the codec from its own settings, and the guest is never asked
what it can decode — `DisplayConfig` has no field for it. A Windows guest
without an HEVC decoder already ends a session with "cannot decode H.265"; a
Linux guest always does, for H.265 and AV1 alike. **Until that changes, a
Windows host has to be on H.264 for a Linux guest to connect.**

The fix is for the guest to say what it can decode in `Config`, and for the host
to choose the best codec both ends have. That changes the wire format (bincode
1.x is not self-describing, so a new field breaks an older peer), which means
moving `network::ALPN` from `tain/6` to `tain/7`. It is the first thing worth
doing for Linux guests, and it would help Windows guests on older GPUs too.

On a Linux build H.265 and AV1 are shown as *planned* in Settings, because
nothing there can encode or decode them. That is the honest state, and it also
stops a Linux host from offering them.

## What it needs from the machine

### Linked: almost nothing

The binary links only the C and C++ runtimes:

    libc.so.6  libm.so.6  libgcc_s.so.1  libstdc++.so.6  ld-linux-x86-64.so.2

`libstdc++` is for OpenH264, which is C++. The release job refuses a binary that
links anything else, because a missing linked library stops a program before
`main` with a loader message nobody launching from a menu will see.

It needs **glibc 2.35 or later**. A Linux binary runs on the glibc it was linked
against and anything newer, so the release is built in an `ubuntu:22.04`
container and the job checks no symbol asks for more than 2.35:

| Supported | Not supported |
|---|---|
| Ubuntu 22.04 and later, and its derivatives (Mint 21+, Pop!\_OS 22.04+) | Ubuntu 20.04, Debian 11 (both out of support) |
| Debian 12 and later | RHEL 9, AlmaLinux 9, Rocky 9 (glibc 2.34) |
| Fedora (every supported release), **Bazzite**, Bluefin, Nobara | |
| Arch, Manjaro, EndeavourOS, current SteamOS 3 releases | |
| openSUSE Tumbleweed, Leap 16 | |
| RHEL 10 and its rebuilds | |

RHEL 9 is the one notable exclusion. Building in an AlmaLinux 9 container
instead would bring it in at the cost of a dnf-based job; nobody has asked yet.

### Opened at run time: the desktop libraries

Everything that talks to the desktop is loaded with `dlopen` when the window
opens, not linked: winit, glutin, x11-dl and xkbcommon-dl all work that way. A
Wayland session uses the first three rows below and EGL; an X11 session (or
XWayland) uses the X11 rows. Every desktop installation of every distribution
above already has all of them, because GTK, Qt and Mesa depend on them.

| Library | For | Debian / Ubuntu | Fedora / Bazzite | Arch / SteamOS | openSUSE |
|---|---|---|---|---|---|
| `libwayland-client.so.0` | Wayland window | `libwayland-client0` | `libwayland-client` | `wayland` | `libwayland-client0` |
| `libwayland-egl.so.1` | GL on Wayland | `libwayland-egl1` | `libwayland-egl` | `wayland` | `libwayland-egl1` |
| `libxkbcommon.so.0` | Keyboard layouts | `libxkbcommon0` | `libxkbcommon` | `libxkbcommon` | `libxkbcommon0` |
| `libxkbcommon-x11.so.0` | Keyboard layouts on X11 | `libxkbcommon-x11-0` | `libxkbcommon-x11` | `libxkbcommon-x11` | `libxkbcommon-x11-0` |
| `libX11.so.6`, `libX11-xcb.so.1`, `libxcb.so.1` | X11 window | `libx11-6`, `libx11-xcb1`, `libxcb1` | `libX11`, `libX11-xcb`, `libxcb` | `libx11`, `libxcb` | `libX11-6`, `libX11-xcb1`, `libxcb1` |
| `libXcursor.so.1`, `libXrender.so.1`, `libXi.so.6` | X11 cursor and input | `libxcursor1`, `libxrender1`, `libxi6` | `libXcursor`, `libXrender`, `libXi` | `libxcursor`, `libxrender`, `libxi` | `libXcursor1`, `libXrender1`, `libXi6` |
| `libEGL.so.1` | GL context | `libegl1` | `libglvnd-egl` | `libglvnd` | `libEGL1` |
| `libGL.so.1` | GL on X11 | `libgl1` | `libglvnd-glx` | `libglvnd` | `libGL1` |
| A Mesa or vendor GL driver | Drawing | `libgl1-mesa-dri` | `mesa-dri-drivers` | `mesa` | `Mesa-dri` |

**What a missing one looks like**: winit panics, and the panic is in the log. A
minimal Ubuntu container without `libxkbcommon-x11-0` produced exactly this in
`~/.local/state/Tain/tain.log`:

    ERROR tain::logging: Panic at .../xkbcommon-dl-0.4.2/src/x11.rs:59: Library libxkbcommon-x11.so could not be loaded.

Only a stripped-down or server installation can hit this. One line installs the
lot on Debian or Ubuntu:

```bash
sudo apt install libwayland-client0 libwayland-egl1 libxkbcommon0 libxkbcommon-x11-0 libx11-6 libx11-xcb1 libxcursor1 libxi6 libegl1 libgl1 libgl1-mesa-dri
```

### Services: a credential store

Saved connection codes, and this machine's network identity, go in the desktop's
**Secret Service** over D-Bus (through `keyring`, which speaks D-Bus itself, so
there is no `libdbus` to link). Nothing falls back to a file — see "Secrets are
never written to a file" in `docs/README.md`.

Without a Secret Service the app still works, and says so in its log:

    WARN tain::gui::app: No credential store — this machine's codes will change every launch

That is the whole cost, and it is a real one: a new identity on every start
means a new host code every start, and nothing saved survives a restart.

| Desktop | Provides it | Notes |
|---|---|---|
| GNOME | `gnome-keyring` | On by default |
| KDE Plasma 6 | KWallet, through `ksecretd` | **May need switching on**: System Settings → KDE Wallet → *Use KWallet for the Secret Service interface*, or `apiEnabled=true` under `[org.freedesktop.secrets]` in `~/.config/kwalletrc`. Check this on Bazzite's KDE edition |
| Others | `gnome-keyring` or KeePassXC's Secret Service integration | |

### Where it keeps things

| What | Where |
|---|---|
| Settings, saved connections (names only) | `~/.config/Tain/` (`$XDG_CONFIG_HOME`) |
| Log | `~/.local/state/Tain/tain.log` (`$XDG_STATE_HOME`); `TAIN_LOG_FILE` overrides it |
| Codes, identity | The Secret Service, above |

## Installing the release

The tarball holds `tain`, `tain.desktop`, `tain.svg` and `LICENSE`. Into your
home directory, which works the same on an immutable system like Bazzite or
SteamOS whose `/usr` cannot be written:

```bash
tar xzf tain-x86_64-linux.tar.gz
```

```bash
install -Dm755 tain/tain ~/.local/bin/tain
```

```bash
install -Dm644 tain/tain.desktop ~/.local/share/applications/tain.desktop
```

```bash
install -Dm644 tain/tain.svg ~/.local/share/icons/hicolor/scalable/apps/tain.svg
```

Tain then appears in the application menu. `~/.local/bin` is on `PATH` by
default on Fedora and its derivatives; elsewhere, launch it from the menu or by
its full path.

**Why the `.desktop` file matters on Wayland**: the icon Tain gives its window
is an X11 and Windows mechanism, and winit has no Wayland equivalent (the newer
`xdg-toplevel-icon` protocol is not one it speaks). So the compositor matches
the window's application id, `tain` (`gui::APP_ID`), against an installed
`tain.desktop`, and takes the icon and the taskbar entry from there. Without the
file the window works but shows a generic icon.

`tain.desktop` does not register the `tain://` scheme yet, because nothing on
Linux would receive the link.

## Building it

Rust stable, a C and C++ compiler, and `nasm`. No `-dev` packages, because
nothing desktop-related is linked:

```bash
sudo apt install build-essential nasm
```

```bash
sudo dnf install gcc gcc-c++ nasm
```

```bash
sudo pacman -S --needed base-devel nasm
```

then `cargo build --release -p tain-cli`. `nasm` matters exactly as much as on
Windows — "The nasm Trap" in `decisions.md` — and `tain-core/build.rs` now warns
about it on Linux too.

On **Bazzite** (or any Fedora Atomic system), build inside a container rather
than layering packages onto the host. The binary runs on the host afterwards:

```bash
distrobox create --name tain-build --image fedora:latest
```

```bash
distrobox enter tain-build
```

Then, inside it, `sudo dnf install gcc gcc-c++ nasm`, install Rust with rustup,
and build.

A build on a newer distribution than the release's links against that
distribution's glibc, and only runs on it and newer. That is fine for testing on
the same machine.

## Testing on Bazzite

The machine available for this has an Intel iGPU and runs Bazzite, which is
Fedora Atomic with Mesa, Intel's VA-API driver (`intel-media-driver`) and the
multimedia codecs already in the image. It is also the most demanding of the
likely targets in one respect: Wayland by default, KDE or GNOME, and an
immutable `/usr`.

**First, write down what it is** — these are the answers every later bug report
will want:

```bash
cat /etc/os-release; echo "$XDG_SESSION_TYPE $XDG_CURRENT_DESKTOP"; rpm-ostree status --booted | head -5; lspci -k | grep -A3 -E 'VGA|Display'
```

and, if it is installed, `vainfo`, which says what the iGPU can decode and
encode — the starting point for the hardware work below.

**Then run it from a terminal**, so the log is in front of you as well as in
`~/.local/state/Tain/tain.log`:

```bash
RUST_LOG=info tain
```

What to check, in order:

1. **The window**. Opens, has the Tain icon in the taskbar (the `.desktop`
   file is installed), resizes, survives a change of display scaling, goes
   fullscreen and back. Any panic is in the log with a file and line.
2. **The credential store**. Note the guest code, quit, start again. A
   different code, or a `No credential store` line in the log, means the
   Secret Service is not answering — on the KDE edition, see the table above.
3. **Connecting to a Windows host**, with the host's codec on **H.264**.
   Picture, then mouse, keyboard (a non-US layout if you use one), scrolling,
   and touch if the machine has a touchscreen.
4. **What the stats line blames**. Decoding on the processor is the expected
   bottleneck at 1080p and above on an iGPU; note the decode time it shows.
   That number is the case for hardware decode.
5. **Discovery on the same network**. A host on the same LAN should be found
   in under a second. If not, `firewall-cmd --get-active-zones`: Fedora's
   desktop zone allows mDNS and high ports, and a changed zone may not.
6. **X11 instead of Wayland**. `env -u WAYLAND_DISPLAY tain` runs it through
   XWayland. Worth one comparison, since the two paths share nothing below
   winit.
7. **Hosting**. Expect the test pattern. It proves the encoder and the network
   path from this machine, and nothing else.

Bazzite's Game Mode is gamescope with Steam, not a desktop. Test in Desktop
Mode.

## What it would take to match Windows

In order of value. The guest comes first because it is closest; hosting is most
of the work.

### Guest

1. **Codec negotiation**, above. A protocol change, and small.
2. **Hardware decode through VA-API.** Build FFmpeg for Linux the way
   `scripts/build-ffmpeg.sh` builds it for Windows, with `--enable-vaapi` and
   the `h264_vaapi`, `hevc_vaapi` and `av1_vaapi` hwaccels, and give the shim
   a VA-API path beside its D3D11VA one. First read the picture back (as the
   Windows path still can) and upload it; then draw it without the copy, by
   exporting the surface as a DMA-BUF (`vaExportSurfaceHandle`) and importing
   it into the GL context as an `EGLImage` (`EGL_EXT_image_dma_buf_import`).
   That is the Linux equivalent of `gl_interop`.
   - *Keeping it optional.* A missing linked `.so` stops a program before
     `main`, exactly as a missing DLL does, and Linux has no delay loading. So
     the FFmpeg libraries are opened with `dlopen` and their functions looked up
     by name, which is also what lets them be embedded and unpacked the way
     `ffmpeg_runtime` does on Windows.
   - *Drivers.* Intel: `intel-media-driver` (iHD) for Broadwell and newer,
     already in Bazzite. AMD: Mesa's VA-API driver. NVIDIA has no VA-API of its
     own; Vulkan Video decode (in FFmpeg 7.1 for H.264, HEVC and AV1) is the
     vendor-neutral route that covers it, and could later replace VA-API
     entirely.
3. **Sound.** The Opus decoder comes with the FFmpeg build, as on Windows.
   Playback through the PulseAudio API (`libpulse`, opened at run time), which
   PipeWire also answers through `pipewire-pulse` — so one backend covers
   every desktop from the last decade. ALSA through `cpal` would link
   `libasound` and break the link-nothing rule.
4. **Immersive mode.** Wayland: the keyboard-shortcuts-inhibit protocol, which
   KWin, Mutter and wlroots all implement, for the system keys; winit's
   `CursorGrabMode::Locked` (pointer-constraints) for confinement. X11:
   `XGrabKeyboard`.
5. **`tain://` links.** `MimeType=x-scheme-handler/tain;` in the `.desktop`
   file, and a Unix socket in `$XDG_RUNTIME_DIR` to hand a link to the instance
   already running, in place of the Windows named pipe.
6. **Notifications.** `org.freedesktop.Notifications` over D-Bus; `zbus` is
   already in the tree through `keyring`.

### Host

1. **Capture.** On Wayland there is one way that works across desktops: the
   **ScreenCast portal** (`xdg-desktop-portal`, through the `ashpd` crate) for
   permission, and a **PipeWire** stream for the frames, as DMA-BUFs that can
   go to a VA-API encoder without touching the processor. The portal shows the
   desktop's own picker, not Tain's, so choosing screens and windows changes
   shape: Tain receives streams rather than enumerating windows itself. Its
   `persist_mode` and restore tokens let a host skip the picker after the
   first time. On X11, XShm.
2. **Input.** The **RemoteDesktop portal**, opened as the same session as the
   ScreenCast one so the person at the host approves both at once; events go
   either as D-Bus calls or, with lower latency, through libei
   (`ConnectToEIS`). GNOME and KDE implement it; wlroots compositors do not.
   On X11, XTest (`libXtst`).
3. **Hardware encode.** FFmpeg's `h264_vaapi`, `hevc_vaapi` and `av1_vaapi`,
   fed straight from the PipeWire DMA-BUFs. Intel iGPUs from Skylake encode
   H.264 and HEVC; AV1 encode needs Arc or Meteor Lake and later. NVENC through
   FFmpeg for NVIDIA. OpenH264 stays the fallback.
4. **Sound.** The monitor source of the default PulseAudio sink, which works
   on PipeWire too. Leaving out chosen apps, as Windows does, means building
   the capture from individual PipeWire streams instead — a second step.
5. **Unattended access** has no portal equivalent: a portal session needs
   someone logged in to approve it. The lock screen and a machine with nobody
   signed in need either the desktop's own remote-desktop service (GNOME
   Remote Desktop, KDE's KRdp), or what Sunshine — preinstalled on Bazzite —
   does: KMS capture with `CAP_SYS_ADMIN` and input through `/dev/uinput`,
   which works everywhere including Game Mode and needs a privileged install.
6. **Virtual displays.** The ScreenCast portal's *virtual* source type asks
   the compositor for a monitor that exists only for the stream. GNOME and KDE
   support it.

### Packaging

- **Now**: the tarball. It suits a binary that needs nothing beyond glibc.
- **Next**: Flatpak, on Flathub. On Wayland, capture and input go through
  portals anyway, which is what a Flatpak is allowed to use, and it is how
  Bazzite and SteamOS users expect to install things. It carries its own
  runtime, so the glibc floor stops mattering there. KMS capture and uinput
  cannot work inside it.
- **Not planned**: an AppImage. With no libraries to bundle it adds a FUSE
  dependency and nothing else.

## Open decisions

- **The application id.** It is `tain` today. Flathub requires a reverse-DNS
  id (`io.github.x_iso.Tain`, for a project on GitHub as `x-iso`, or one under
  a domain of your own), and the portals key their remembered permissions on
  the id too. Changing it later renames the `.desktop` file under anyone who
  installed the old one, so it is worth settling before Flatpak or portal work.
- **The glibc floor.** 2.35, leaving out RHEL 9; see above.
- **Release gating.** `publish` waits for both platforms, so a broken Linux
  build holds back a Windows release. That keeps every release complete; the
  alternative is a Linux job allowed to fail.
