//! Which graphics chip Windows starts this process on.
//!
//! # Why a remote desktop has to care
//!
//! Desktop Duplication has one hard rule: the D3D device and the output must be
//! on the same adapter, and that adapter must be the one compositing the
//! desktop. Break it and `DuplicateOutput` answers `DXGI_ERROR_UNSUPPORTED` —
//! which reads as "this system cannot do that" and means nothing of the kind.
//!
//! On a laptop with two graphics chips it is broken by default, and not in a way
//! any amount of care inside the capture code can repair. Reported twice, and
//! the second report carried the whole shape of it:
//!
//! ```text
//! no adapter could capture \\.\DISPLAY1. Tried 6:
//!   NVIDIA GeForce RTX 2060 with Max-Q Design: this GPU cannot duplicate that display (0x887A0004)
//!   Intel(R) UHD Graphics: \\.\DISPLAY1 is not attached to this adapter (it has none)
//!   Intel(R) UHD Graphics: \\.\DISPLAY1 is not attached to this adapter (it has none)
//!   Microsoft Basic Render Driver: … (it has none)
//!   the default adapter: this GPU cannot duplicate that display (0x887A0004)
//!   the software rasteriser: … (it has none)
//! ```
//!
//! The panel is driven by the Intel chip. Intel enumerates **no outputs at
//! all**, and NVIDIA — enumerated first, so this process's adapter zero —
//! claims the display and then refuses to duplicate it. That is what an Optimus
//! laptop looks like from inside a process the driver has placed on the discrete
//! GPU: the discrete chip presents *through* the integrated one, so it appears to
//! own the desktop while owning nothing that can be duplicated.
//!
//! Trying every adapter, which is what the previous two attempts at this added,
//! cannot help. There is exactly one adapter with the output and it is the wrong
//! one, and DXGI will not pair an output with a device from a different adapter.
//!
//! # The one lever there is, and why it is no longer pulled
//!
//! Which GPU a process runs on is decided before `main`, from the executable and
//! from a per-application setting, so nothing at the point of failure can change
//! it for the run that failed. It can change it for the next one.
//!
//! `HKCU\Software\Microsoft\DirectX\UserGpuPreferences` is that setting: one
//! value per executable, and since Windows 10 1803 it overrides the graphics
//! vendors' own control panels. It is what **Settings > System > Display >
//! Graphics** writes, and it needs no administrator rights, because an
//! application's own preference belongs to the user running it.
//!
//! An earlier version wrote *power saving* there whenever duplication failed.
//! That was not a coin toss between the two chips: the integrated GPU is the one
//! the panel is wired to on these machines, and a process placed there sees an
//! honest topology — Intel owns the internal display, the discrete chip owns
//! whatever is plugged into the ports it drives, and the adapter search in
//! [`super::capture_dxgi`] finds the right one. Duplication worked.
//!
//! It also put the encoder on the weaker chip, for the life of the installation:
//! a laptop with a real GPU encoding on its processor, the discrete card idle,
//! because Tain had asked Windows not to use it. The fault is now answered by
//! [`super::capture_wgc`] instead. The compositor delivers frames onto whichever
//! device it is given, so the process stays where it is and captures anyway.
//!
//! # What is left here
//!
//! Taking that preference back, once, on startup — see
//! [`undo_our_old_preference`] — and the reading and writing it needs. [`set`]
//! and [`worth_switching`] belong to the old fix and nothing in the application
//! calls them; only the tests here do. A test in `screen_capture` fails if the
//! capture path starts calling `set` again.

/// Which chip Windows should start an executable on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preference {
    /// Nothing has been said, so the graphics driver's own profile decides —
    /// which for anything that opens an OpenGL context on an Optimus laptop
    /// commonly means the discrete chip, and hence the failure above.
    #[default]
    Unset,
    /// Windows decides. Distinct from [`Self::Unset`]: somebody wrote this.
    Automatic,
    /// The integrated chip. What a machine whose desktop cannot be duplicated
    /// needs, for the reason in the module note.
    PowerSaving,
    /// The discrete chip.
    HighPerformance,
}

impl Preference {
    /// The number Windows stores, or `None` for "no value at all".
    fn code(self) -> Option<u32> {
        match self {
            Self::Unset => None,
            Self::Automatic => Some(0),
            Self::PowerSaving => Some(1),
            Self::HighPerformance => Some(2),
        }
    }

    /// What to call it where somebody will read it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Unset => "unset",
            Self::Automatic => "automatic",
            Self::PowerSaving => "power saving (the integrated chip)",
            Self::HighPerformance => "high performance (the discrete chip)",
        }
    }
}

/// What Windows stores for one executable, e.g. `GpuPreference=1;`.
///
/// Pure, and separate from the registry, so the format can be pinned in a test
/// rather than discovered by writing something Windows ignores. The trailing
/// semicolon is not decoration — the value is a `;`-separated list of settings
/// and Windows writes one that ends in it.
pub fn encode(preference: Preference) -> Option<String> {
    Some(format!("GpuPreference={};", preference.code()?))
}

/// Read one back, tolerantly.
///
/// Anything unrecognised reads as [`Preference::Unset`] rather than as an
/// error: this value belongs to Windows, other settings may appear in it, and
/// a version of Windows that writes something new here is not a reason for a
/// remote desktop to stop working.
pub fn parse(value: &str) -> Preference {
    for setting in value.split(';') {
        let Some((key, number)) = setting.split_once('=') else { continue };
        if !key.trim().eq_ignore_ascii_case("GpuPreference") {
            continue;
        }
        return match number.trim() {
            "0" => Preference::Automatic,
            "1" => Preference::PowerSaving,
            "2" => Preference::HighPerformance,
            _ => Preference::Unset,
        };
    }
    Preference::Unset
}

/// Whether moving this process to the integrated chip is worth trying.
///
/// The old fix's decision, and not called from the application any more — see
/// the module note. Pure, because it is the whole decision and it should be
/// readable without a hybrid laptop to hand.
///
/// Only from a state nobody chose. A preference already set to power saving has
/// been tried and did not help, and one set to high performance was set by
/// somebody — by hand in Settings, or by a game they wanted on the fast chip —
/// and quietly reversing a person's decision is not a bug fix. Both cases fall
/// back to saying what is wrong, which is what the message is for.
pub fn worth_switching(current: Preference) -> bool {
    matches!(current, Preference::Unset | Preference::Automatic)
}

/// Undo a graphics preference an earlier version of Tain wrote for itself.
///
/// # Why this exists rather than simply not writing it any more
///
/// A previous version pinned Tain to the *integrated* chip when Desktop
/// Duplication failed, so that the next run would capture. It did, and it also
/// put the encoder on the weaker of the two chips for the life of the
/// installation — a laptop with a real GPU encoding on its processor, with the
/// discrete card idle, because Tain had asked Windows not to use it.
///
/// Not writing it any more fixes nothing on a machine that already has it: the
/// value is in `HKCU` and outlives the build that put it there. So it is taken
/// back, once, on startup.
///
/// **Only `PowerSaving`, and only once.** That is the only value the old code
/// ever wrote. Somebody who has deliberately chosen *high performance* has
/// chosen something this never asked for and must keep it, and somebody who
/// deliberately chooses power saving after this has run gets to keep that too —
/// which is what `already_done` is for. Returns whether anything was removed.
pub fn undo_our_old_preference(already_done: bool) -> bool {
    if already_done || current() != Preference::PowerSaving {
        return false;
    }
    match clear() {
        Ok(()) => {
            tracing::info!(
                "Removed the graphics preference an older version of Tain wrote for itself. \
                 It pinned this program to the integrated chip, which also kept the encoder \
                 off the discrete one. Restart Tain for it to take effect."
            );
            true
        }
        Err(e) => {
            tracing::warn!("Could not remove Tain's old graphics preference: {e:#}");
            false
        }
    }
}

/// Set once [`set`] has succeeded in this process.
static SWITCHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether this run has already asked for a different chip.
///
/// Only [`set`] raises it, so in the application it is always false now; see
/// the module note.
///
/// # Why this is remembered rather than read back
///
/// Writing the preference does not move *this* process — it moves the next one
/// — so a second capture failure a minute later would read the value back, find
/// exactly what it just wrote, and report "already set to power saving and
/// still failing", which is true of the registry and a lie about the machine.
/// What is wanted then is still "restart Tain", and only the process that wrote
/// it knows that.
pub fn switched_this_run() -> bool {
    SWITCHED.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::*;
    use anyhow::{Context, Result};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    /// Windows' own per-application graphics preferences.
    const KEY: &str = r"Software\Microsoft\DirectX\UserGpuPreferences";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// The executable the preference is filed under: this one.
    ///
    /// Whichever process is asking. The application and the agent are separate
    /// executables in separate places, they capture on their own account, and
    /// each needs its own entry — so this is right by construction rather than
    /// by remembering to pass the correct path.
    fn this_executable() -> Result<String> {
        Ok(std::env::current_exe()
            .context("could not find this executable")?
            .to_string_lossy()
            .into_owned())
    }

    /// What Windows will do with this executable next time it starts.
    pub fn current() -> Preference {
        let Ok(exe) = this_executable() else { return Preference::Unset };
        let key = wide(KEY);
        let name = wide(&exe);
        let mut handle = HKEY::default();

        // SAFETY: both strings outlive the call; the handle is closed below.
        unsafe {
            if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), 0, KEY_READ, &mut handle)
                != ERROR_SUCCESS
            {
                return Preference::Unset;
            }
            let mut buffer = [0u16; 260];
            let mut size = std::mem::size_of_val(&buffer) as u32;
            let read = RegQueryValueExW(
                handle,
                PCWSTR(name.as_ptr()),
                None,
                None,
                Some(buffer.as_mut_ptr().cast()),
                Some(&mut size),
            );
            let _ = RegCloseKey(handle);
            if read != ERROR_SUCCESS {
                return Preference::Unset;
            }
            let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
            parse(&String::from_utf16_lossy(&buffer[..end]))
        }
    }

    /// Ask Windows to start this executable on a particular chip.
    ///
    /// Takes effect at the next launch and never at this one, which is the
    /// whole awkwardness of the feature: the driver reads it before any code
    /// here runs. Nothing in the application calls this any more; see the
    /// module note.
    pub fn set(preference: Preference) -> Result<()> {
        let exe = this_executable()?;
        let data = encode(preference)
            .with_context(|| format!("{} is not something to write", preference.name()))?;

        let key = wide(KEY);
        let name = wide(&exe);
        let text = wide(&data);
        let mut handle = HKEY::default();

        // SAFETY: every string outlives its call; the handle is closed on both
        // paths.
        unsafe {
            let created = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                0,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut handle,
                None,
            );
            if created != ERROR_SUCCESS {
                anyhow::bail!("could not open {KEY} for writing ({})", created.0);
            }
            // Bytes including the terminator, which `RegQueryValueExW` relies
            // on being there — a REG_SZ written without it reads back with
            // whatever follows it in the hive.
            let bytes = std::slice::from_raw_parts(
                text.as_ptr().cast::<u8>(),
                std::mem::size_of_val(&text[..]),
            );
            let written =
                RegSetValueExW(handle, PCWSTR(name.as_ptr()), 0, REG_SZ, Some(bytes));
            let _ = RegCloseKey(handle);
            if written != ERROR_SUCCESS {
                anyhow::bail!("could not set the graphics preference ({})", written.0);
            }
        }
        super::SWITCHED.store(true, std::sync::atomic::Ordering::Relaxed);
        tracing::info!("Asked Windows to run {exe} on {}", preference.name());
        Ok(())
    }

    /// Stop expressing a preference for this executable.
    ///
    /// Anything that writes a setting on somebody's behalf should be able to
    /// unwrite it. This is how [`super::undo_our_old_preference`] takes back
    /// what an older Tain wrote, and how the test below leaves the registry as
    /// it found it.
    pub fn clear() -> Result<()> {
        use windows::Win32::System::Registry::RegDeleteValueW;

        let exe = this_executable()?;
        let key = wide(KEY);
        let name = wide(&exe);
        let mut handle = HKEY::default();

        // SAFETY: both strings outlive the call; the handle is closed below.
        unsafe {
            if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), 0, KEY_WRITE, &mut handle)
                != ERROR_SUCCESS
            {
                // Nothing has ever been written here, so there is nothing to
                // undo. Not a failure.
                return Ok(());
            }
            let _ = RegDeleteValueW(handle, PCWSTR(name.as_ptr()));
            let _ = RegCloseKey(handle);
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
pub use windows_impl::{clear, current, set};

#[cfg(not(target_os = "windows"))]
mod stub {
    use super::*;

    pub fn current() -> Preference {
        Preference::Unset
    }

    pub fn set(_preference: Preference) -> anyhow::Result<()> {
        anyhow::bail!("per-application graphics preferences are a Windows feature")
    }

    pub fn clear() -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
pub use stub::{clear, current, set};

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact string Windows stores. Getting this wrong writes a value
    /// Windows ignores, which looks from here exactly like writing a correct
    /// one — so it is pinned rather than trusted.
    #[test]
    fn a_preference_is_written_the_way_windows_writes_it() {
        assert_eq!(encode(Preference::PowerSaving).as_deref(), Some("GpuPreference=1;"));
        assert_eq!(encode(Preference::HighPerformance).as_deref(), Some("GpuPreference=2;"));
        assert_eq!(encode(Preference::Automatic).as_deref(), Some("GpuPreference=0;"));
        // Nothing to write is not the same as writing a zero.
        assert_eq!(encode(Preference::Unset), None);
    }

    #[test]
    fn what_is_written_reads_back_as_what_was_meant() {
        for preference in
            [Preference::Automatic, Preference::PowerSaving, Preference::HighPerformance]
        {
            let written = encode(preference).expect("something to write");
            assert_eq!(parse(&written), preference, "{written} did not survive a round trip");
        }
    }

    /// The value is a list, and Windows is free to put other things in it.
    #[test]
    fn a_value_carrying_other_settings_is_still_read() {
        assert_eq!(parse("SomethingElse=4;GpuPreference=1;"), Preference::PowerSaving);
        assert_eq!(parse("GpuPreference=2;AutoHDREnable=2197;"), Preference::HighPerformance);
    }

    /// And a version of Windows that writes something new here must not stop a
    /// session from starting.
    #[test]
    fn anything_unrecognised_is_read_as_nothing_set() {
        for value in ["", "GpuPreference=;", "GpuPreference=9;", "nonsense", "GpuPreference"] {
            assert_eq!(parse(value), Preference::Unset, "{value:?} was read as a choice");
        }
    }

    /// The value really reaches the registry and reads back as itself.
    ///
    /// # Why this is worth a test that touches the machine
    ///
    /// Everything above pins the *format*, which is only half the risk. The
    /// other half is writing a perfectly well-formed value into a key Windows
    /// does not read, under a name it does not match — and from inside the
    /// process that looks exactly like success. The only way to tell the
    /// difference is to write one and read it back.
    ///
    /// Files itself under the test binary's own path, which is what
    /// `current_exe` answers here, so it never touches the preference for a
    /// real Tain. Puts it back either way, including on a failed assertion: a
    /// test that leaves a graphics preference behind on somebody's machine is
    /// worse than no test.
    #[test]
    #[ignore = "writes to this user's registry — run explicitly"]
    #[cfg(target_os = "windows")]
    fn a_preference_written_here_is_a_preference_windows_can_read() {
        let before = current();
        assert_eq!(before, Preference::Unset, "a test binary should have no preference");

        let wrote = set(Preference::PowerSaving);
        let read_back = current();
        let cleared = clear();
        let after = current();

        wrote.expect("could not write a graphics preference");
        assert_eq!(
            read_back,
            Preference::PowerSaving,
            "the value did not come back as what was written"
        );
        assert!(switched_this_run(), "a successful write has to be remembered");
        cleared.expect("could not remove the preference again");
        assert_eq!(after, Preference::Unset, "the preference outlived the test");
    }

    /// Somebody else's decision is not this program's to reverse.
    #[test]
    fn only_a_preference_nobody_chose_is_overwritten() {
        assert!(worth_switching(Preference::Unset));
        assert!(worth_switching(Preference::Automatic), "letting Windows decide is not a choice of chip");

        assert!(
            !worth_switching(Preference::PowerSaving),
            "already on the integrated chip and still failing — switching again changes nothing"
        );
        assert!(
            !worth_switching(Preference::HighPerformance),
            "somebody put this executable on the discrete chip on purpose"
        );
    }
}
