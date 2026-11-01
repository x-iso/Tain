# Tain on Linux

What the Linux build does, what it needs from the machine it runs on, how to
test it, and what is left. Kept current with the code; where it is not, the
code and the tests win.

## What it does

Tain on Linux hosts and views, with sound and GPU video, on x86-64. Every
release ships `tain-x86_64-linux.tar.gz` beside `tain.exe`.

| | Linux | How |
|---|---|---|
| Codes, approval, saved connections, discovery, relays, settings, languages | ✅ | The same code as Windows |
| **Viewing**: H.264, H.265, AV1 | ✅ | GPU decode through VA-API; OpenH264 for H.264 on the processor where the GPU cannot |
| **Hosting**: sharing a screen or a window | ✅ | The screen cast portal and PipeWire, on Wayland |
| Hosting: mouse, keyboard, touch, pointer capture | ✅ | The RemoteDesktop portal, in the same session |
| Hosting: GPU encode, H.264, H.265, AV1 | ✅ | VA-API; OpenH264 for H.264 where the GPU cannot |
| **Sound**, both directions | ✅ | PulseAudio's API, which PipeWire also answers |
| Leaving applications out of the sound, choosing the sound device | ❌ | The whole mix, on the default device |
| Drawing decoded frames straight from the GPU | ❌ | Frames are read back and converted on the processor |
| The pointer sent separately from the picture | ❌ | Drawn into the picture by the desktop |
| Immersive mode, `tain://` links, notifications | ❌ | Stubs |
| Unattended access, virtual displays | ❌ | No portal equivalent; see the end |
| Hosting on X11 | ⚠️ | Through the portal, where the desktop's portal shares an X11 session; untested |

### GPU video

Intel and AMD GPUs decode and encode through VA-API, Linux's standard video
acceleration interface: Intel's driver (`intel-media-driver`, "iHD") for
Broadwell and newer, Mesa's for AMD. H.264 and H.265 on anything recent; AV1
decode on Intel Tiger Lake and AMD RDNA2 onwards, and AV1 encode on Intel Arc
and Meteor Lake and AMD RDNA3 onwards. NVIDIA has no VA-API of its own: H.264
falls back to the processor, and H.265 and AV1 are unavailable there for now.

As on Windows, a host offers H.265 and AV1 only if its GPU encodes them, and a
guest whose GPU cannot decode the codec a host chose ends the session saying
so. The host chooses without asking the guest — `DisplayConfig` has no field
for it — so a host on H.265 or AV1 cannot be watched from a guest without that
decoder. Letting a guest say what it decodes is a protocol change (bincode is
not self-describing, so `network::ALPN` would move to `tain/7`), and worth it
for Windows guests on older GPUs as much as for Linux.

### Sharing the screen

On Wayland no application may read another's pixels. The way in that GNOME,
KDE and the others agree on is `xdg-desktop-portal`: the first time a guest
connects, the desktop asks the person at the host which screen or window to
share and whether to allow control, in its own dialog. That choice is
remembered — Tain asks to persist and keeps the restore token — so later
sessions start without it. While anything is shared, the desktop says so in
its own way. Declining ends the guest's session with that reason.

One RemoteDesktop session carries both the picture and the input, so one
dialog covers both. On a desktop with no RemoteDesktop portal (wlroots-based
ones, today) the screen is still shared, without control.

## What it needs from the machine

### Linked: almost nothing

The binary links only the C and C++ runtimes:

    libc.so.6  libm.so.6  libgcc_s.so.1  libstdc++.so.6  ld-linux-x86-64.so.2

and needs **glibc 2.35 or later**, because the release is built in an
`ubuntu:22.04` container. The release job refuses a binary that links anything
else or asks for a newer glibc: a missing linked library stops a program before
`main`, with a loader message nobody launching from a menu sees. Everything
below is opened at run time instead, and a missing piece costs only the
feature that needs it, with a line in the log saying which.

| Supported | Not supported |
|---|---|
| Ubuntu 22.04 and later, and its derivatives (Mint 21+, Pop!\_OS 22.04+) | Ubuntu 20.04, Debian 11 (both out of support) |
| Debian 12 and later | RHEL 9, AlmaLinux 9, Rocky 9 (glibc 2.34) |
| Fedora (every supported release), **Bazzite**, Bluefin, Nobara | |
| Arch, Manjaro, EndeavourOS, current SteamOS 3 releases | |
| openSUSE Tumbleweed, Leap 16 | |
| RHEL 10 and its rebuilds | |

### Carried inside: FFmpeg

`libtain-media.so` — the VA-API and Opus shims and a static FFmpeg 7.1.5 with
libopus — is compressed into the binary and opened from memory at startup. It
links only libc and libm. A `libtain-media.so` beside the binary is opened
instead, so a modified FFmpeg can be substituted.

### Opened at run time

| What | For | If it is missing | Debian / Ubuntu | Fedora / Bazzite | Arch / SteamOS |
|---|---|---|---|---|---|
| Wayland or X11, xkbcommon, EGL/GL | The window | Tain does not start (see below) | present on any desktop | present | present |
| `libva.so.2`, `libva-drm.so.2` | GPU video | Processor H.264 only; sound still works | `libva2`, `libva-drm2` | `libva` | `libva` |
| A VA-API driver | GPU video | The same | `intel-media-va-driver` or `mesa-va-drivers` | `intel-media-driver` (in Bazzite) or `mesa-va-drivers` | `intel-media-driver` or `mesa` |
| `libpulse-simple.so.0` and a sound server | Sound | No sound | `libpulse0` | `pulseaudio-libs` | `libpulse` |
| `libpipewire-0.3.so.0` | Hosting | Cannot share the screen | `libpipewire-0.3-0` | `pipewire-libs` | `libpipewire` |
| `xdg-desktop-portal` and the desktop's backend | Hosting | Cannot share the screen | `xdg-desktop-portal-gnome` or `-kde` | present | `xdg-desktop-portal-kde` or `-gnome` |
| A Secret Service | Saved codes, a stable host code | A new code every launch | `gnome-keyring` | present (see below) | `gnome-keyring` or KWallet |

The window's libraries are opened by winit and glutin, and a missing one is a
panic in the log rather than an error dialog. Every desktop installation has
them; on a stripped-down Debian or Ubuntu:

```bash
sudo apt install libwayland-client0 libwayland-egl1 libxkbcommon0 libxkbcommon-x11-0 libx11-6 libx11-xcb1 libxcursor1 libxi6 libegl1 libgl1 libgl1-mesa-dri
```

**The credential store.** Codes and this machine's network identity live in
the desktop's Secret Service, never a file. Without one the app still works
and says `No credential store — this machine's codes will change every launch`
in its log. GNOME's keyring is on by default. KDE's is KWallet through
`ksecretd`, whose Secret Service interface **may need switching on**: System
Settings → KDE Wallet → *Use KWallet for the Secret Service interface*, or
`apiEnabled=true` under `[org.freedesktop.secrets]` in `~/.config/kwalletrc`.

### Where it keeps things

| What | Where |
|---|---|
| Settings, saved connections (names only) | `~/.config/Tain/` |
| Log | `~/.local/state/Tain/tain.log`; `TAIN_LOG_FILE` overrides it |
| The remembered screen sharing choice | `~/.local/state/Tain/screen-sharing-token`, readable only by you |
| Codes, identity | The Secret Service, above |

## Installing the release

The tarball holds `tain`, `tain.desktop`, `tain.svg` and `LICENSE`. Into your
home directory, which works the same on an immutable system like Bazzite or
SteamOS:

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

The `.desktop` file matters more than it looks. A Wayland window cannot hand
the desktop an icon of its own through winit, so the desktop matches the
window's application id, `tain`, against `tain.desktop` for the icon and the
taskbar entry. And the portal files the remembered sharing choice under the
same id. It does not register `tain://` yet, because nothing would receive the
link.

## Building it

Rust stable, a C and C++ compiler, `nasm`, `pkg-config`, and headers for the
two libraries that are opened at run time but compiled against — libva and
PipeWire:

```bash
sudo apt install build-essential nasm pkg-config libva-dev libpipewire-0.3-dev
```

```bash
sudo dnf install gcc gcc-c++ nasm pkgconf libva-devel pipewire-devel
```

```bash
sudo pacman -S --needed base-devel nasm pkgconf libva pipewire
```

FFmpeg, in either of two ways:

- `./scripts/build-ffmpeg-linux.sh` builds the static FFmpeg a release
  carries, into `third_party/ffmpeg-linux` (gitignored), from the same pinned
  sources as the Windows build. A few minutes, once.
- Or your distribution's FFmpeg development packages (`libavcodec-dev
  libavutil-dev libswresample-dev`, `ffmpeg-free-devel` on Fedora). Fine for
  working on Tain, and the build says so, because the result only runs where
  that FFmpeg is installed.

then `cargo build --release -p tain-cli`. Without FFmpeg the build still works,
with no GPU video and no sound; without the PipeWire headers, with no screen
capture. Each is a warning, and the release job fails on any of them.

On **Bazzite** (or any Fedora Atomic system), build in a container rather than
layering packages onto the host; the binary runs on the host afterwards:

```bash
distrobox create --name tain-build --image fedora:latest
```

```bash
distrobox enter tain-build
```

then the `dnf` line above, Rust through rustup, and the build. A binary built
there needs that container's glibc or newer, which the host has.

## Testing on Bazzite

The machine available for this has an Intel iGPU and runs Bazzite: Fedora
Atomic, Wayland, KDE or GNOME, with Mesa, Intel's VA-API driver and PipeWire
in the image. It is the most demanding of the likely targets, and the right
one to test on.

**First, what it is** — every report after will want this:

```bash
cat /etc/os-release; echo "$XDG_SESSION_TYPE $XDG_CURRENT_DESKTOP"; rpm-ostree status --booted | head -5; lspci -k | grep -A3 -E 'VGA|Display'
```

and what its GPU can do, if `vainfo` is installed (in a toolbox if not):

```bash
vainfo 2>&1 | grep -E 'Driver version|VAProfile(H264|HEVC|AV1).*(VLD|EncSlice)'
```

**Then run Tain from a terminal**, so the log is in front of you:

```bash
RUST_LOG=info tain
```

The first lines say what loaded: `Media library loaded … with VA-API`, and
when a session starts, `VA-API device: …`.

As a **guest**, connecting to a Windows host:

1. The window opens with the Tain icon in the taskbar, resizes, survives a
   change of scaling, goes fullscreen and back.
2. Quit and start again: the same host code means the credential store works.
   A new one, or `No credential store` in the log, is the KDE setting above.
3. Connect with the host on H.264, then H.265, then AV1. The stats line names
   the decoder; `VA-API` in it is the GPU. A codec the iGPU cannot decode ends
   the session saying so.
4. Sound from the host, and its volume and mute.
5. Mouse, keyboard (a non-US layout if you use one), scrolling, touch.

As a **host**, with a Windows or Linux guest:

1. The first connection shows the desktop's sharing dialog. Allow it, with
   "remember" if offered. The guest sees the screen and can drive it.
2. Disconnect and connect again: no dialog this time.
3. Share a window instead: delete the token file above so the dialog asks
   again, and pick a window in it.
4. The stats line on the guest names the encoder: `h264_vaapi (VA-API, …)` is
   the GPU. Try H.265 and AV1 in Settings → Video.
5. Sound from the host reaches the guest.
6. Decline the dialog once: the guest's session ends and says why.

Discovery on the same network should find a host in under a second. If not,
`firewall-cmd --get-active-zones`: Fedora's desktop zone allows mDNS and high
ports; a changed zone may not. Game Mode is gamescope and Steam, not a desktop
with a portal — test in Desktop Mode.

## Testing without a desktop

Most of the Linux path runs in a container with no GPU and no compositor, and
the tests that need more than that are `#[ignore]`d with what they need:

| Test | Needs |
|---|---|
| `audio_capture::tests::*` | A PulseAudio or PipeWire sound server; a null sink will do |
| `capture_portal::tests::frames_arrive_from_a_pipewire_video_node` | PipeWire with WirePlumber, and a video source node |
| `capture_portal::tests::the_host_path_against_a_stand_in_portal` | The above, and `scripts/mock-portal.py` on a session bus |

A video source node, standing in for a compositor's screen:

```bash
gst-launch-1.0 videotestsrc is-live=true pattern=smpte ! video/x-raw,format=BGRx,width=640,height=360,framerate=30/1 ! pipewiresink mode=provide stream-properties="props,media.class=Video/Source"
```

Then, with its node id from `pw-cli ls Node`, the stand-in portal, which
answers every request as if the person at the machine clicked Share, hands
out that node as the screen, and logs every call:

```bash
./scripts/mock-portal.py <node-id> /tmp/portal.log "$XDG_RUNTIME_DIR/pipewire-0"
```

```bash
TAIN_TEST_MOCK_PORTAL_LOG=/tmp/portal.log cargo test -p tain-core -- --ignored the_host_path
```

That runs the whole host path: the portal conversation, frames from PipeWire,
input sent back and where it lands, the session closing, and the restore token
carried into the next session. The GPU paths need a GPU.

## What is left

In order of value.

1. **Codec negotiation**, above. A protocol change, and small.
2. **No copies on the GPU paths.** Decoded frames are read back and converted
   on the processor; exporting the VA-API surface as a DMA-BUF and importing
   it into the GL context as an `EGLImage` would draw it where it is, as
   `gl_interop` does on Windows. The same on the host: PipeWire can hand over
   DMA-BUFs that VA-API encodes without the processor touching them.
3. **The pointer as data.** The desktop draws it into the picture today; the
   portal's metadata cursor mode would send it separately and let it move at
   the guest's frame rate, as on Windows.
4. **Sound.** Choosing the device (the picker shows only the default), and
   leaving applications out, which means building the capture from their own
   PipeWire streams rather than the mix.
5. **The guest's extras.** Immersive mode through the keyboard-shortcuts-inhibit
   protocol and winit's pointer lock; `tain://` links through `MimeType=` in
   the `.desktop` file and a socket in `$XDG_RUNTIME_DIR`; notifications over
   `org.freedesktop.Notifications`.
6. **NVIDIA.** Vulkan Video, which FFmpeg 7.1 decodes H.264, H.265 and AV1
   through on every vendor, would cover it, and could replace VA-API outright.
7. **Unattended access and virtual displays.** A portal session needs someone
   logged in to grant it. The lock screen and a machine nobody is signed in to
   need the desktop's own remote desktop service (GNOME Remote Desktop, KDE's
   KRdp), or what Sunshine on Bazzite does: KMS capture with `CAP_SYS_ADMIN`
   and input through `/dev/uinput`, which needs a privileged install. The
   portal's *virtual* source type is the road to virtual displays.
8. **Packaging.** A Flatpak on Flathub: portals are what a Flatpak may use
   anyway, it brings its own runtime, and it is how Bazzite and SteamOS
   install things. It needs a reverse-DNS application id.

## Open decisions

- **The application id.** It is `tain`, which the `.desktop` file, the window
  and the portal's remembered permissions all key on. Flathub requires a
  reverse-DNS one (`io.github.x_iso.Tain`, or one under a domain of your own).
  Changing it later loses remembered sharing choices and renames the
  `.desktop` file under anyone who installed the old one, so it is worth
  settling before a Flatpak.
- **The glibc floor.** 2.35, which leaves out RHEL 9.
- **Release gating.** `publish` waits for both platforms, so a broken Linux
  build holds back a Windows release.
