//! Attaching, sizing and putting away a virtual display.
//!
//! # What this is for
//!
//! Three things people actually want, all the same mechanism:
//!
//! - **A headless host.** A machine with no monitor plugged in has no desktop
//!   to duplicate, so there is nothing to stream. A virtual display gives it
//!   one.
//! - **The guest's own resolution.** Streaming a 2560x1600 desktop into a
//!   1920x1080 window means every frame is scaled twice — once on the host to
//!   fit, once on the guest to draw — and text never looks right. A display
//!   made the size of the guest's window is sharp at 1:1.
//! - **A spare screen.** An old tablet or laptop becomes a second monitor:
//!   attach a display the host is not otherwise using, and stream only that.
//!
//! # What this module does not do
//!
//! It does not install anything, and it does not *create* a virtual monitor.
//! It drives one that exists. Installing a driver is a separate, elevated,
//! reversible act, and mixing it in here would mean every call site had to
//! reason about it.
//!
//! # An adapter is not a monitor, and this is the whole shape of the feature
//!
//! An indirect display driver registers a display *adapter* per monitor it can
//! present, and those adapters exist from the moment the driver is installed.
//! They enumerate, they have names, and they are useless: until the driver is
//! told to present a monitor behind one, it has no modes, reports
//! `DISPLAY_DEVICE_MODESPRUNED`, and `ChangeDisplaySettingsExW` answers
//! `DISP_CHANGE_FAILED` to every mode offered.
//!
//! Measured on a machine with Parsec's driver installed: sixteen adapters,
//! `\\.\DISPLAY6` through `\\.\DISPLAY21`, every one of them with `monitors:
//! none` and zero modes, and attaching refused outright.
//!
//! Creating the monitor is the driver's own business and each does it
//! differently — a companion service, a settings file read when the device is
//! enabled, a private control channel. So "add a virtual display" is not one
//! Win32 call; it is *make the driver present a monitor* followed by everything
//! in this module. Which one of those a host can do depends on which driver it
//! has, and is why the driver Tain ships with is the one Tain can drive.
//!
//! What is here is the half that is the same for all of them, and it is
//! immediately useful on any machine whose driver has presented a monitor —
//! including one somebody else's software created.
//!
//! # Why GDI rather than DXGI
//!
//! [`super::screen_capture::displays`] uses DXGI because it needs to know about
//! HDR, and DXGI only enumerates outputs that are *on the desktop*. A virtual
//! display that has not been attached yet has no output, so it does not exist
//! as far as DXGI is concerned — which is precisely the state this module has
//! to see in order to do anything about it.
//!
//! `EnumDisplayDevicesW` lists every display adapter the machine has, attached
//! or not, and `ChangeDisplaySettingsExW` is what moves one between those two
//! states. Both are the ordinary user's APIs: attaching a display needs no more
//! privilege than the Settings app does, which is why nothing here asks for
//! any.

use anyhow::Result;

/// A display this machine could put on the desktop, or already has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualDisplay {
    /// The device name, `\\.\DISPLAY3`.
    ///
    /// The same string [`super::screen_capture::DisplayTarget::One`] carries
    /// and [`super::protocol::DisplayInfo::name`] reports, so a display
    /// attached here can be asked for by name without translation.
    pub name: String,
    /// What the driver calls itself, e.g. "Virtual Display Driver".
    pub adapter: String,
    /// Whether it is currently part of the desktop.
    ///
    /// An attached one is capturable and appears in `displays()`; a detached
    /// one exists only here.
    pub attached: bool,
    /// Whether the driver has actually presented a monitor behind this adapter.
    ///
    /// False is the ordinary state of a freshly installed driver, and it is the
    /// difference between a display that can be attached and one that cannot —
    /// see the note on adapters and monitors at the top of this module. A UI
    /// offering the second as if it were the first sends somebody to a button
    /// that answers `DISP_CHANGE_FAILED`.
    pub ready: bool,
}

/// A display mode a device says it can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub width: u16,
    pub height: u16,
    /// Hz, as the driver reports it. Zero when it does not say.
    pub refresh: u32,
}

/// Where a display sits on the virtual desktop, for [`next_position`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placed {
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
}

/// Where to put a display being attached, whatever size it is.
///
/// To the right of everything else, top-aligned with the origin. Not clever on
/// purpose: the alternative is guessing at an arrangement nobody asked for, and
/// the pointer mapping already follows whatever the desktop reports — so being
/// predictable matters more than being tasteful.
///
/// Overlapping is the one thing to avoid rather than merely dislike. Two
/// displays sharing desktop coordinates make the pointer ambiguous, and Windows
/// will silently shove one of them somewhere else, which leaves this machine's
/// idea of the layout disagreeing with the host's.
pub fn next_position(existing: &[Placed]) -> (i32, i32) {
    let right = existing
        .iter()
        .map(|d| d.x.saturating_add(i32::from(d.width)))
        .max()
        .unwrap_or(0);
    // Zero rather than the topmost edge: the primary display's top is the
    // desktop's origin, and a screen hung off the top-left of a monitor that is
    // itself above the origin is further from where anyone would look for it.
    (right.max(0), 0)
}

/// The mode closest to what was asked for.
///
/// # Why closest and not exact
///
/// The caller's ideal is the guest's window, which is whatever size somebody
/// dragged it to. A virtual display driver offers a list — often a long one,
/// sometimes a short one — and demanding an exact match would refuse the
/// feature to anyone whose window is not on it.
///
/// Ranked by area difference, then by not being smaller: given a choice between
/// two modes equally far from the target, the larger one loses nothing but
/// bandwidth, while the smaller one loses pixels that cannot come back. A mode
/// that matches exactly always wins, because its difference is zero.
pub fn best_mode(available: &[Mode], wanted: (u16, u16)) -> Option<Mode> {
    let want = (u64::from(wanted.0), u64::from(wanted.1));
    available.iter().copied().min_by_key(|mode| {
        let (w, h) = (u64::from(mode.width), u64::from(mode.height));
        let area = w * h;
        let target = want.0 * want.1;
        let difference = area.abs_diff(target);
        // Then the tie-break, then the refresh rate, so the answer does not
        // depend on the order the driver happened to enumerate in.
        (difference, u8::from(w < want.0 || h < want.1), u32::MAX - mode.refresh)
    })
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::*;
    use anyhow::{bail, Context};
    use tracing::*;
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        ChangeDisplaySettingsExW, EnumDisplayDevicesW, EnumDisplaySettingsW, CDS_NORESET,
        CDS_TYPE, CDS_UPDATEREGISTRY, DEVMODEW, DISPLAY_DEVICEW, DISP_CHANGE_SUCCESSFUL,
        DM_BITSPERPEL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, DM_POSITION,
        ENUM_CURRENT_SETTINGS,
    };

    /// `DISPLAY_DEVICE_ATTACHED_TO_DESKTOP`, which the crate does not name.
    const ATTACHED_TO_DESKTOP: u32 = 0x0000_0001;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn text(buffer: &[u16]) -> String {
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        String::from_utf16_lossy(&buffer[..end])
    }

    fn device(index: u32) -> Option<DISPLAY_DEVICEW> {
        let mut dev = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        // SAFETY: `dev` is sized as the API requires and outlives the call; a
        // null device name enumerates adapters.
        let ok = unsafe { EnumDisplayDevicesW(None, index, &mut dev, 0) }.as_bool();
        ok.then_some(dev)
    }

    /// Whether the driver has presented a monitor behind this adapter.
    ///
    /// The one question that separates an adapter which can be put on the
    /// desktop from one that only looks like it can. Asked by enumerating the
    /// adapter's children: an indirect display driver that has not been told to
    /// present anything has none.
    pub fn has_monitor(name: &str) -> bool {
        let wide_name = wide(name);
        let mut dev = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        // SAFETY: a non-null device name enumerates the monitors behind that
        // adapter; `dev` is sized as required and outlives the call.
        unsafe { EnumDisplayDevicesW(PCWSTR(wide_name.as_ptr()), 0, &mut dev, 0) }.as_bool()
    }

    /// What the driver reports about one display device, for the diagnostic.
    ///
    /// `StateFlags` and whether a monitor exists behind the adapter are the two
    /// facts that decide whether a display can be attached at all, and neither
    /// is visible from anywhere else.
    pub fn describe(name: &str) -> (u32, Vec<String>) {
        let mut flags = 0;
        for index in 0.. {
            let Some(dev) = device(index) else { break };
            if text(&dev.DeviceName) == name {
                flags = dev.StateFlags;
                break;
            }
        }

        let wide_name = wide(name);
        let mut monitors = Vec::new();
        for index in 0.. {
            let mut dev = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            // SAFETY: a non-null device name enumerates the monitors behind
            // that adapter; `dev` is sized as required and outlives the call.
            let ok = unsafe {
                EnumDisplayDevicesW(PCWSTR(wide_name.as_ptr()), index, &mut dev, 0)
            }
            .as_bool();
            if !ok {
                break;
            }
            monitors.push(format!("{} [{:#x}]", text(&dev.DeviceString), dev.StateFlags));
        }
        (flags, monitors)
    }

    /// Every virtual display this machine has, attached or not.
    ///
    /// Empty on a machine with no such driver, which is the ordinary case and
    /// not an error — the feature is simply unavailable and the UI says so.
    pub fn available() -> Vec<VirtualDisplay> {
        let mut found = Vec::new();
        for index in 0.. {
            let Some(dev) = device(index) else { break };
            if !super::super::screen_capture::is_virtual_adapter(&dev.DeviceString) {
                continue;
            }
            let name = text(&dev.DeviceName);
            found.push(VirtualDisplay {
                ready: has_monitor(&name),
                name,
                adapter: text(&dev.DeviceString),
                attached: dev.StateFlags & ATTACHED_TO_DESKTOP != 0,
            });
        }
        found
    }

    /// Make the desktop extend rather than clone.
    ///
    /// # Why this is needed at all
    ///
    /// A monitor arriving does not choose its own topology — Windows does, from
    /// what it remembers about that display and what the machine was last doing.
    /// It very often chooses **clone**, and a cloned display is a second copy of
    /// the first: same coordinates, same picture, nothing new to capture and
    /// nowhere for a pointer to go. Reported for a virtual display and for a
    /// dummy plug alike, and both times the fix was a trip to Settings.
    ///
    /// A position cannot undo it. `ChangeDisplaySettingsExW` moves a display
    /// within a topology; it does not change which topology is in force, so
    /// placing a cloned display somewhere else is a request Windows is entitled
    /// to ignore — and does. `SetDisplayConfig` is the call that owns that
    /// question.
    ///
    /// Applies to the whole desktop, because a topology is a property of the
    /// desktop and not of one display. That is a real cost: a machine
    /// deliberately mirroring two screens will stop. It is the lesser one — the
    /// alternative is a virtual display that exists, reports a size, and shows
    /// somebody their own primary monitor.
    pub fn extend_desktop() -> Result<()> {
        use windows::Win32::Devices::Display::{
            SetDisplayConfig, SDC_APPLY, SDC_TOPOLOGY_EXTEND,
        };
        // SAFETY: no paths or modes are supplied, which is what asks Windows to
        // use the topology it has stored for these displays.
        let result = unsafe { SetDisplayConfig(None, None, SDC_TOPOLOGY_EXTEND | SDC_APPLY) };
        if result != 0 {
            bail!("could not switch the desktop to extend ({result})");
        }
        debug!("Desktop topology set to extend");
        Ok(())
    }

    /// Where every attached display sits, apart from one.
    ///
    /// The exclusion is what lets a display already on the desktop be moved:
    /// see [`attach`].
    pub fn occupied_except(skip: &str) -> Vec<Placed> {
        placements(Some(skip))
    }

    /// Where every attached display currently sits, for [`next_position`].
    ///
    /// Read here rather than taken from `displays()` so that attaching does not
    /// depend on DXGI having been able to describe every screen: a display this
    /// cannot see is one whose space could be handed to something else.
    pub fn occupied() -> Vec<Placed> {
        placements(None)
    }

    fn placements(skip: Option<&str>) -> Vec<Placed> {
        let mut found = Vec::new();
        for index in 0.. {
            let Some(dev) = device(index) else { break };
            if dev.StateFlags & ATTACHED_TO_DESKTOP == 0 {
                continue;
            }
            let device_name = text(&dev.DeviceName);
            if skip.is_some_and(|s| s == device_name) {
                continue;
            }
            let name = wide(&device_name);
            let mut mode = DEVMODEW {
                dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            // SAFETY: the name outlives the call and `mode` is sized as the API
            // requires. `ENUM_CURRENT_SETTINGS` is only meaningful for an
            // attached device, which is what was just checked.
            let ok = unsafe {
                EnumDisplaySettingsW(PCWSTR(name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode)
            }
            .as_bool();
            if !ok {
                continue;
            }
            // SAFETY: `dmPosition` is valid whenever `DM_POSITION` is set,
            // which `ENUM_CURRENT_SETTINGS` always sets for an attached
            // display.
            let position = unsafe { mode.Anonymous1.Anonymous2.dmPosition };
            found.push(Placed {
                x: position.x,
                y: position.y,
                width: mode.dmPelsWidth.min(u16::MAX as u32) as u16,
                height: mode.dmPelsHeight.min(u16::MAX as u32) as u16,
            });
        }
        found
    }

    /// The modes a device says it can run.
    ///
    /// Works on a detached device, which is the point: what a virtual display
    /// can do has to be known *before* deciding to attach it at some size.
    /// Duplicates — the same size at several colour depths — are collapsed.
    pub fn modes(name: &str) -> Vec<Mode> {
        let wide_name = wide(name);
        let mut found: Vec<Mode> = Vec::new();
        for index in 0.. {
            let mut mode = DEVMODEW {
                dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            // SAFETY: the name outlives the call; `mode` is sized as required.
            let ok = unsafe {
                EnumDisplaySettingsW(
                    PCWSTR(wide_name.as_ptr()),
                    windows::Win32::Graphics::Gdi::ENUM_DISPLAY_SETTINGS_MODE(index),
                    &mut mode,
                )
            }
            .as_bool();
            if !ok {
                break;
            }
            // Nothing here can encode a mode this large, and a driver offering
            // one is offering it to something else.
            if mode.dmPelsWidth > u16::MAX as u32 || mode.dmPelsHeight > u16::MAX as u32 {
                continue;
            }
            let found_mode = Mode {
                width: mode.dmPelsWidth as u16,
                height: mode.dmPelsHeight as u16,
                refresh: mode.dmDisplayFrequency,
            };
            if !found.contains(&found_mode) {
                found.push(found_mode);
            }
        }
        found
    }

    /// Refuse to touch anything that is not a virtual display.
    ///
    /// These calls detach a monitor just as happily as they detach a virtual
    /// one, and a bug that blanks somebody's real screen mid-session is not a
    /// bug they can recover from remotely — the session they would fix it
    /// through is the one that went dark. So the name is checked against the
    /// enumeration every time rather than trusted from wherever it came.
    fn must_be_virtual(name: &str) -> Result<VirtualDisplay> {
        available()
            .into_iter()
            .find(|d| d.name == name)
            .with_context(|| format!("{name} is not one of this machine's virtual displays"))
    }

    fn apply(name: &str, mode: Option<&mut DEVMODEW>) -> Result<()> {
        let wide_name = wide(name);
        // `CDS_NORESET` writes the change to the registry without applying it,
        // so several displays could be moved in one go. Only one is moved here,
        // and the second call below is what makes it happen — but the pair is
        // kept because a single call that both writes and applies has been seen
        // to reorder the desktop when it lands on a machine mid-change.
        let flags = CDS_UPDATEREGISTRY | CDS_NORESET;
        // SAFETY: the name outlives the call, and `mode` is either a valid
        // DEVMODEW or absent.
        let result = unsafe {
            ChangeDisplaySettingsExW(
                PCWSTR(wide_name.as_ptr()),
                mode.map(|m| m as *const DEVMODEW),
                None,
                flags,
                None,
            )
        };
        if result != DISP_CHANGE_SUCCESSFUL {
            bail!("the display change was refused ({})", result.0);
        }

        // Apply everything written above. A null device name with no mode is
        // the documented way to say "make the registry the truth".
        // SAFETY: documented call; every pointer is null by construction.
        let applied =
            unsafe { ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None) };
        if applied != DISP_CHANGE_SUCCESSFUL {
            bail!("the display change could not be applied ({})", applied.0);
        }
        Ok(())
    }

    /// Put a virtual display on the desktop at this size.
    ///
    /// Idempotent in the way that matters: attaching one that is already
    /// attached moves it to the requested size rather than failing, because the
    /// caller's intent is "a display of this size exists", not "a state
    /// transition happened".
    pub fn attach(name: &str, size: (u16, u16), refresh: u32) -> Result<()> {
        // Named `screen`, not `display`: `use tracing::*` brings a `display`
        // function into scope and the macros below resolve the identifier to it.
        let screen = must_be_virtual(name)?;
        // Checked here rather than left to fail below, because what comes back
        // otherwise is `DISP_CHANGE_FAILED` — a number that says the driver
        // refused the mode, when the truth is that there is nothing to set a
        // mode on. That reading cost an hour.
        if !has_monitor(name) {
            bail!(
                "{name} ({}) has no monitor behind it — presenting one is a thing \
                 only that driver can do, and it has not been asked",
                screen.adapter
            );
        }
        // Its own space, worked out from what is on the desktop *now* — which
        // is why this is not cached: a monitor can be plugged in between one
        // attach and the next.
        //
        // Excluding this one, which is not pedantry: a display that arrived in
        // clone mode is already *attached*, so it is part of `occupied()` and
        // would be placed to the right of itself. Its own edge is not a
        // constraint on where it goes.
        let elsewhere: Vec<Placed> =
            occupied_except(name).into_iter().collect();
        let (x, y) = next_position(&elsewhere);

        let mut mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            dmPelsWidth: u32::from(size.0),
            dmPelsHeight: u32::from(size.1),
            dmBitsPerPel: 32,
            dmDisplayFrequency: refresh,
            dmFields: DM_POSITION
                | DM_PELSWIDTH
                | DM_PELSHEIGHT
                | DM_BITSPERPEL
                | DM_DISPLAYFREQUENCY,
            ..Default::default()
        };
        // Writing a union field needs no `unsafe` — only reading one does,
        // because only reading can observe a variant that was never written.
        // Which variant this is, is declared by `DM_POSITION` in `dmFields`.
        mode.Anonymous1.Anonymous2.dmPosition =
            windows::Win32::Foundation::POINTL { x, y };

        apply(name, Some(&mut mode))?;
        info!(
            "Attached {} ({}) at {}x{} and ({x}, {y})",
            screen.name, screen.adapter, size.0, size.1
        );
        Ok(())
    }

    /// Take a virtual display off the desktop.
    ///
    /// A zeroed mode is how detaching is spelled: there is no separate call,
    /// and a display with no width is not on the desktop.
    pub fn detach(name: &str) -> Result<()> {
        let screen = must_be_virtual(name)?;
        if !screen.attached {
            return Ok(());
        }
        let mut mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            dmFields: DM_POSITION
                | DM_PELSWIDTH
                | DM_PELSHEIGHT
                | DM_BITSPERPEL
                | DM_DISPLAYFREQUENCY,
            ..Default::default()
        };
        apply(name, Some(&mut mode))?;
        info!("Detached {} ({})", screen.name, screen.adapter);
        Ok(())
    }
}

#[cfg(target_os = "windows")]
pub use windows_impl::{
    attach, available, describe, detach, extend_desktop, has_monitor, modes, occupied,
    occupied_except,
};

#[cfg(not(target_os = "windows"))]
mod stub {
    use super::*;

    pub fn available() -> Vec<VirtualDisplay> {
        Vec::new()
    }

    pub fn occupied() -> Vec<Placed> {
        Vec::new()
    }

    pub fn modes(_name: &str) -> Vec<Mode> {
        Vec::new()
    }

    pub fn attach(_name: &str, _size: (u16, u16), _refresh: u32) -> Result<()> {
        anyhow::bail!("virtual displays are a Windows feature")
    }

    pub fn detach(_name: &str) -> Result<()> {
        anyhow::bail!("virtual displays are a Windows feature")
    }
}

#[cfg(not(target_os = "windows"))]
pub use stub::{attach, available, detach, modes, occupied};

#[cfg(test)]
mod tests {
    use super::*;

    fn placed(x: i32, y: i32, width: u16, height: u16) -> Placed {
        Placed { x, y, width, height }
    }

    #[test]
    fn a_new_display_goes_to_the_right_of_everything_else() {
        let desk = [placed(0, 0, 2560, 1600), placed(2560, 0, 1920, 1080)];
        assert_eq!(next_position(&desk), (4480, 0));
    }

    /// The one arrangement that must not produce an overlap.
    ///
    /// A monitor left of the primary one has a negative origin, so the widest
    /// screen is not the rightmost edge and the sum of the widths is not
    /// either. Only the edges themselves answer it.
    #[test]
    fn a_display_left_of_the_primary_does_not_confuse_the_placement() {
        let desk = [placed(-1920, 0, 1920, 1080), placed(0, 0, 2560, 1600)];
        let (x, y) = next_position(&desk);
        assert_eq!((x, y), (2560, 0));
        for screen in &desk {
            let overlaps = x < screen.x + i32::from(screen.width) && x + 1280 > screen.x;
            assert!(!overlaps, "the new display lands on top of {screen:?}");
        }
    }

    #[test]
    fn the_first_display_on_a_headless_machine_starts_at_the_origin() {
        assert_eq!(next_position(&[]), (0, 0));
    }

    /// Every screen being left of the origin must still not push one negative.
    #[test]
    fn placement_is_never_negative() {
        let desk = [placed(-3840, 0, 1920, 1080)];
        let (x, _) = next_position(&desk);
        assert!(x >= 0, "a display was placed at {x}");
    }

    fn mode(width: u16, height: u16, refresh: u32) -> Mode {
        Mode { width, height, refresh }
    }

    #[test]
    fn an_exact_mode_wins() {
        let modes = [mode(1920, 1080, 60), mode(2560, 1440, 60), mode(1280, 720, 60)];
        assert_eq!(best_mode(&modes, (1920, 1080)), Some(mode(1920, 1080, 60)));
    }

    /// A guest's window is whatever size it was dragged to, so the list almost
    /// never contains it.
    #[test]
    fn the_nearest_mode_is_taken_when_none_matches() {
        let modes = [mode(1280, 720, 60), mode(1920, 1080, 60), mode(3840, 2160, 60)];
        assert_eq!(best_mode(&modes, (1856, 1020)), Some(mode(1920, 1080, 60)));
    }

    /// Between two equally distant modes, the one that keeps the pixels.
    #[test]
    fn a_tie_is_broken_towards_the_larger_mode() {
        // 1000x1000 either side of a 1100x1100 target, by area.
        let target = (1100u16, 1100u16);
        let modes = [mode(1000, 1000, 60), mode(1187, 1187, 60)];
        let picked = best_mode(&modes, target).expect("a mode");
        assert!(
            picked.width >= target.0 && picked.height >= target.1,
            "picked {picked:?}, which is smaller than what was asked for"
        );
    }

    #[test]
    fn the_highest_refresh_rate_wins_among_identical_sizes() {
        let modes = [mode(1920, 1080, 30), mode(1920, 1080, 144), mode(1920, 1080, 60)];
        assert_eq!(best_mode(&modes, (1920, 1080)), Some(mode(1920, 1080, 144)));
    }

    #[test]
    fn a_driver_offering_nothing_is_not_a_panic() {
        assert_eq!(best_mode(&[], (1920, 1080)), None);
    }

    /// Attach a virtual display, check it arrived, and put it away again.
    ///
    /// # Why this is worth running on a real machine
    ///
    /// Every interesting thing here is the driver's answer rather than this
    /// code's: whether a detached device takes an arbitrary size, whether it
    /// appears in `displays()` once attached, whether detaching leaves the
    /// desktop as it was. None of that can be asserted without a driver, and
    /// all of it decides how the feature above has to be shaped.
    ///
    /// Detaches whatever happens, including on a failed assertion — a test that
    /// leaves a monitor attached to somebody's desktop is worse than no test.
    #[test]
    #[ignore = "attaches a display to this machine's desktop — run explicitly"]
    fn a_virtual_display_can_be_attached_and_put_away() {
        let Some(spare) = available().into_iter().find(|d| !d.attached && d.ready) else {
            println!(
                "None of this machine's {} virtual display adapter(s) has a monitor \
                 behind it, so there is nothing to attach — see the note on adapters \
                 and monitors at the top of this module.",
                available().len()
            );
            return;
        };
        let before = crate::screen_capture::displays().len();
        println!("Using {} ({})", spare.name, spare.adapter);

        let wanted = (1920u16, 1080u16);
        let outcome = attach(&spare.name, wanted, 60);

        // Read everything before undoing anything, so a failure still reports.
        let seen = outcome.as_ref().ok().map(|()| {
            let listed = crate::screen_capture::displays();
            let mine = listed.iter().find(|d| d.name == spare.name).cloned();
            (listed.len(), mine)
        });

        let put_away = detach(&spare.name);

        let () = outcome.expect("the driver refused to attach a display");
        let (after, mine) = seen.expect("attached, so this was read");
        let mine = mine.unwrap_or_else(|| {
            panic!("{} attached but does not appear in displays()", spare.name)
        });
        println!("  attached as {}x{} at ({}, {})", mine.width, mine.height, mine.x, mine.y);
        assert_eq!(after, before + 1, "the display count did not go up by one");
        assert_eq!(
            (mine.width, mine.height),
            wanted,
            "the driver took a size other than the one asked for"
        );
        assert!(mine.virtual_display, "an attached virtual display was not marked as one");

        put_away.expect("the display could not be detached");
        assert_eq!(
            crate::screen_capture::displays().len(),
            before,
            "detaching left the desktop with a display it did not start with"
        );
    }

    /// What this machine actually has, for someone wiring up a driver.
    ///
    /// Run with `cargo test -p tain-core -- --ignored --nocapture what_virtual_displays`.
    /// Prints nothing on a machine with no such driver, which is the answer.
    #[test]
    #[ignore = "diagnostic — run explicitly"]
    fn what_virtual_displays_this_machine_has() {
        let found = available();
        if found.is_empty() {
            println!("No virtual display driver on this machine.");
            println!("Attached displays: {:?}", occupied());
            return;
        }
        for screen in &found {
            let modes = modes(&screen.name);
            let (flags, monitors) = describe(&screen.name);
            println!(
                "{} — {} ({}{}), state {flags:#x}, {} mode(s), monitors: {}",
                screen.name,
                screen.adapter,
                if screen.attached { "on the desktop" } else { "not attached" },
                if screen.ready { "" } else { ", no monitor" },
                modes.len(),
                if monitors.is_empty() { "none".to_string() } else { monitors.join(", ") },
            );
            let mut sizes: Vec<_> =
                modes.iter().map(|m| (m.width, m.height)).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
            sizes.sort_by_key(|(w, h)| std::cmp::Reverse(u32::from(*w) * u32::from(*h)));
            for (w, h) in sizes.iter().take(12) {
                println!("    {w}x{h}");
            }
            if sizes.len() > 12 {
                println!("    ... and {} more", sizes.len() - 12);
            }
        }
        println!("Attached displays: {:?}", occupied());
        println!("A new one would go at {:?}", next_position(&occupied()));
    }
}
