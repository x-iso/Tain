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
| **Hosting**: sharing a screen | ✅ | The screen cast portal and PipeWire, on Wayland |
| Hosting: mouse, keyboard, touch, pointer capture | ✅ | The RemoteDesktop portal, in the same session |
| Hosting: GPU encode, H.264, H.265, AV1 | ✅ | VA-API; OpenH264 for H.264 where the GPU cannot |
| Hosting: sharing windows — picked from pictures of them, several at once, controlled | ✅ on Plasma | KWin's own protocols; see [Sharing windows](#sharing-windows) |
| Hosting: sharing a window on other desktops | ⚠️ | One, picked in the desktop's own dialog, and view-only |
| **Sound**, both directions, on the device chosen in Settings | ✅ | PulseAudio's API, which PipeWire also answers |
| Leaving applications out of the sound | ❌ | The whole mix |
| The menu entry, the taskbar icon | ✅ | Tain writes its own `.desktop` entry and icon; see [Installing](#installing-the-release) |
| Notifications of a guest connecting | ✅ | `org.freedesktop.Notifications`, critical while a guest waits for approval; a click asks for Tain to come forward |
| Paste passcode | ✅ | Through the window's own clipboard connection, Wayland or X11 |
| The wallpaper on a host's card | ✅ | GNOME and its descendants, Plasma, Xfce, swaybg, hyprpaper; a plain colour elsewhere, never a picture of the screen |
| The session window's own title bar | ✅ | Decorations off; the desktop moves and resizes it on request |
| Immersive mode | ✅ | A second `wl_keyboard`, `keyboard-shortcuts-inhibit`, a confined pointer. On X11 only the pointer |
| Drawing decoded frames straight from the GPU | ❌ | Frames are read back and converted on the processor |
| The pointer sent separately from the picture | ❌ | Drawn into the picture by the desktop |
| `tain://` links, the mini view | ❌ | Stubs; a Wayland window cannot place itself or stay on top |
| Unattended access, virtual displays | ❌ | No portal equivalent; see the end |
| Hosting on X11 | ⚠️ | Through the portal, where the desktop's portal shares an X11 session; untested. KWin's window sharing needs Wayland |

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

### Sharing windows

**On Plasma, as on Windows.** KWin lists every window, streams any one of them
on request and takes pointer, keys and touch for it, through its own protocols
(`org_kde_plasma_window_management`, `zkde_screencast_unstable_v1`,
`org_kde_kwin_fake_input`; see `tain-core/src/kwin.rs`). So it goes as it does
on Windows:

1. The accept dialog's *Individual windows…* opens Tain's own picker, *Windows
   to share*, with a picture of each window, before anything is allowed.
2. Pick one or several and press *Done*; the dialog's button then says how many.
3. *Allow*. The guest sees them tiled into one picture, or one at a time through
   its switcher, and drives them: each click is mapped into the window it was
   aimed at. A press first brings that window forward, because KWin streams the
   window whole even where another covers it on the host, and the click would
   otherwise land on whatever is on top. Keys go to whatever has the focus,
   as on Windows.

KWin allows this only for a program whose `.desktop` entry asks, which Tain
sees to itself — see [Installing the release](#installing-the-release) — through
Plasma 6.5; later KWin keeps these from sandboxed programs only. The log says
which way it went at startup: `Sharing windows through KWin`, or `Windows are
shared through the portal here:` followed by what KWin refused. A refusal is
asked about again ten seconds later, so a desktop slow to notice the entry
still comes round.

**On other desktops**, and on Plasma when KWin refuses, an application cannot
list another's windows, so the accept dialog has no list of them. *Share one
window…* asks the desktop instead, in its own dialog, right after the guest is
allowed in, and every time: the remembered choice is only for whole screens.
That session is view-only and shows one window:

- **Several windows** would need no second dialog: the portal's `multiple`
  option lets the desktop's picker take several in one go. What is missing is
  reading several streams from one portal session.
- **Control** comes down to the desktop. KDE's RemoteDesktop portal makes streams
  only for outputs, a workspace or a virtual output, so a window comes through a
  plain ScreenCast session, which takes no input. GNOME's remote desktop dialog
  takes the same source types as its screen cast one and maps pointer positions
  to a stream, so a window there should be controllable; that is untried.

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
| `libpulse-simple.so.0`, `libpulse.so.0` and a sound server | Sound, and the list of devices to choose from | No sound; only the default named | `libpulse0` | `pulseaudio-libs` | `libpulse` |
| `libpipewire-0.3.so.0` | Hosting | Cannot share the screen or a window | `libpipewire-0.3-0` | `pipewire-libs` | `libpipewire` |
| `xdg-desktop-portal` and the desktop's backend | Hosting a screen; a window off Plasma | Cannot share the screen | `xdg-desktop-portal-gnome` or `-kde` | present | `xdg-desktop-portal-kde` or `-gnome` |
| A Secret Service | Saved codes, a stable host code | A new code every launch | `gnome-keyring` | present (see below) | `gnome-keyring` or KWallet |
| A notification service | Notifications | None shown | present on any desktop | present | present |
| `gsettings`, `xfconf-query` | The wallpaper on GNOME's family and Xfce | A plain colour on the card | present with the desktop | present | present |

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
| The menu entry and its icon | `~/.local/share/applications/tain.desktop`, `~/.local/share/icons/hicolor/scalable/apps/tain.svg` (under `$XDG_DATA_HOME` if set) |

## Installing the release

The tarball holds `tain`, `tain.desktop`, `tain.svg` and `LICENSE`. Put `tain`
anywhere and run it; into your home directory works the same on an immutable
system like Bazzite or SteamOS:

```bash
tar xzf tain-x86_64-linux.tar.gz
```

```bash
install -Dm755 tain/tain ~/.local/bin/tain
```

**Tain adds itself to the application menu** the first time it runs, and puts
the entry right again whenever it starts from somewhere else: it writes
`~/.local/share/applications/tain.desktop`, naming the binary by its full path,
and its icon beside it. The tarball's `tain.desktop` and `tain.svg` are for a
package to install instead, and a packaged `tain.desktop` that names the same
binary is left in charge.

That entry matters more than it looks:

- **The taskbar icon.** A Wayland window cannot hand the desktop an icon of its
  own through winit, so the desktop matches the window's application id,
  `tain`, against `tain.desktop`.
- **Notifications** carry Tain's name and icon through it.
- **The portal** files the remembered sharing choice under the same id.
- **On Plasma, sharing windows as on Windows** — see [Sharing
  windows](#sharing-windows). KWin keeps its window list, window capture and
  fake input from programs that have not asked for them in
  `X-KDE-Wayland-Interfaces`, and finds the entry by matching the running binary
  against the first word of its `Exec=` line, by full path. Tain rebuilds
  Plasma's application database after writing the entry, so the first run
  already works; its log says `Sharing windows through KWin` when it does.

It does not register `tain://` yet, because nothing would receive the link.

To remove Tain, delete the binary and the two files under
[Where it keeps things](#where-it-keeps-things); `~/.config/Tain` and
`~/.local/state/Tain` hold its settings and log.

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
   change of scaling, goes fullscreen and back. The log's first lines include
   `Installed Tain's desktop entry` on the first run.
2. Quit and start again: the same host code means the credential store works.
   A new one, or `No credential store` in the log, is the KDE setting above.
3. Copy a code from another application and press *Paste passcode*.
4. Connect with the host on H.264, then H.265, then AV1. The stats line names
   the decoder; `VA-API` in it is the GPU. A codec the iGPU cannot decode ends
   the session saying so.
5. The session window's header holds the tabs and tools; drag it by its empty
   part, resize it from an edge, and use its minimize, maximize and close.
6. Sound from the host, its volume and mute, and *Play on* in the speaker's menu
   listing this machine's devices; choosing one moves the sound there.
7. Mouse, keyboard (a non-US layout if you use one), scrolling, touch. After a
   tap, the host's pointer still shows when the mouse moves again.
8. Immersive mode: `Super` and `Alt+Tab` go to the host, not to Plasma, the
   pointer stays in the window, and `Ctrl+Shift+E` leaves it. GNOME asks once
   whether Tain may take the shortcuts.

As a **host**, with a Windows or Linux guest:

1. A guest connecting raises a notification, which stays while it waits for
   approval. Clicking it should bring Tain forward.
2. The first screen share shows the desktop's sharing dialog. Allow it, with
   "remember" if offered. The guest sees the screen and can drive it.
3. Disconnect and connect again: no dialog this time.
4. Share windows instead: *Individual windows…* in the accept dialog, pick two
   in the picker, *Done*, *Allow*. The guest sees both, can switch to one, and
   can drive it — including one partly behind another window here, which comes
   forward on the first click. Off Plasma, *Share one window…* asks the
   desktop's dialog after *Allow*, and the guest can only watch.
5. The stats line on the guest names the encoder: `h264_vaapi (VA-API, …)` is
   the GPU. Try H.265 and AV1 in Settings → Video.
6. Sound from the host reaches the guest, from the device chosen in Settings →
   Audio.
7. The guest's card for this machine shows this desktop's wallpaper, blurred,
   once the session ends.
8. Decline the sharing dialog once: the guest's session ends and says why.

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
| `capture_kwin::tests::against_a_running_kwin` | A KWin session (below) with PipeWire and WirePlumber, one or two Tain windows open, and `TAIN_TEST_OUT` for the pictures it saves; `TAIN_TEST_CLICK=x,y` clicks there in the window |
| `capture_kwin::tests::drive_a_tain_window` | The same; clicks in and photographs the Tain window of process `TAIN_TEST_PID` by `TAIN_TEST_STEPS`, for watching the interface work with nobody at it |

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

For the KWin tests, a headless KWin in a session bus of its own will do — Plasma
5.27's has the same protocols and the same permission check as Plasma 6:

```bash
KWIN_WAYLAND_NO_PERMISSION_CHECKS=1 kwin_wayland --virtual --width 1600 --height 1000 --no-lockscreen --socket kwin-0
```

The variable is read by KWin, not by the client, and lets the test binary in,
which no `.desktop` entry names. Leave it out to see Tain earn the permission
through the entry it writes. KWin under software rendering answers slowly; give
a window a few seconds to appear in its list.

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
4. **Sound.** Leaving applications out, which means building the capture from
   their own PipeWire streams rather than the mix.
5. **Windows off Plasma.** Several at once through the portal's `multiple`
   option, which needs reading several streams from one portal session; and
   control on GNOME, whose remote desktop dialog may take a window — untried.
6. **The guest's extras.** `tain://` links through `MimeType=` in the `.desktop`
   entry and a socket in `$XDG_RUNTIME_DIR`; immersive mode's keyboard on X11,
   through `XGrabKeyboard`; pointer capture through winit's pointer lock; and a
   notification's click raising the window for certain, by handing the
   activation token the notification service sends to `xdg-activation`, which
   eframe has no way to take today — without it Plasma may only highlight the
   taskbar entry.
7. **NVIDIA.** Vulkan Video, which FFmpeg 7.1 decodes H.264, H.265 and AV1
   through on every vendor, would cover it, and could replace VA-API outright.
8. **Unattended access and virtual displays.** A portal session needs someone
   logged in to grant it. The lock screen and a machine nobody is signed in to
   need the desktop's own remote desktop service (GNOME Remote Desktop, KDE's
   KRdp), or what Sunshine on Bazzite does: KMS capture with `CAP_SYS_ADMIN`
   and input through `/dev/uinput`, which needs a privileged install. The
   portal's *virtual* source type is the road to virtual displays.
9. **Packaging.** A Flatpak on Flathub: portals are what a Flatpak may use
   anyway, it brings its own runtime, and it is how Bazzite and SteamOS
   install things. It needs a reverse-DNS application id, and gives up
   KWin's window sharing: KWin keeps those interfaces from a sandboxed program
   unless its desktop id is granted them.

## Open decisions

- **The application id.** It is `tain`, which the `.desktop` entry, the window
  and the portal's remembered permissions all key on. Flathub requires a
  reverse-DNS one (`io.github.x_iso.Tain`, or one under a domain of your own).
  Changing it later loses remembered sharing choices and leaves the old
  `tain.desktop` behind under anyone whose Tain wrote one, so it is worth
  settling before a Flatpak.
- **The glibc floor.** 2.35, which leaves out RHEL 9.
- **Release gating.** `publish` waits for both platforms, so a broken Linux
  build holds back a Windows release.
