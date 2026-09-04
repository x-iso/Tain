//! Installing and removing the virtual display driver.
//!
//! # Why Tain ships a driver rather than using whichever one is there
//!
//! An indirect display driver registers a display *adapter* per monitor it can
//! present, and those adapters exist from the moment it is installed. They are
//! useless until the driver is told to present a monitor behind one — see
//! [`tain_core::virtual_display`], which found sixteen of them on a machine with
//! Parsec's driver and could attach none of them.
//!
//! Telling a driver to present a monitor is the driver's own business, and each
//! does it differently: a companion service, a private control channel, a
//! settings file read when the device is enabled. Parsec's is driven by Parsec.
//! So the only virtual display Tain can actually create is one belonging to a
//! driver Tain knows how to talk to, which means shipping one.
//!
//! Everything after that — putting it on the desktop, sizing it, taking it away
//! — is the same for all of them and lives in `virtual_display`.
//!
//! # Opt-in, and separately from the service
//!
//! Installing a driver is a bigger ask than installing a service: Windows shows
//! its own publisher dialog, the decision is remembered for that publisher, and
//! a driver is a thing people are right to be careful about. So nothing here
//! happens on its own — the settings panel offers it, and a host with no
//! virtual display driver works exactly as it does today, minus the feature.
//!
//! This mirrors [`super::service`] deliberately, down to the elevation dance:
//! the unprivileged app relaunches itself under `runas` with a flag, which is
//! the only way to get administrator rights, and doing it the same way twice
//! beats two half-remembered variants.

/// Argument that makes this executable install the driver and exit.
pub const INSTALL_FLAG: &str = "--install-display-driver";

/// The same, for removal.
pub const UNINSTALL_FLAG: &str = "--uninstall-display-driver";

/// Where the driver package lives, relative to the executable.
///
/// A folder beside the exe rather than anywhere in the user's profile: this is
/// a portable application, and a driver package that does not travel with the
/// copy somebody moved is one that stops working when they move it.
pub const PACKAGE_DIR: &str = "drivers/virtual-display";

/// The device this creates, and the driver bound to it.
///
/// A root-enumerated device — there is no hardware to discover, so Windows is
/// told to make a node and then given a driver that claims this id. It has to
/// match the `.inf` in [`PACKAGE_DIR`] exactly; a mismatch installs a device
/// nothing will ever drive, which looks like a working install and is not.
pub const HARDWARE_ID: &str = "Root\\TainVirtualDisplay";

/// Whether the driver is installed, and whether it can be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DriverState {
    /// Installed, and presenting at least one monitor Tain can attach.
    Ready,
    /// Installed, but no monitor is present behind any of its adapters.
    ///
    /// The driver is there and Windows has bound it; it simply has not been
    /// asked to present anything. Distinguished from [`Self::NotInstalled`]
    /// because the fix is different and neither one is an error.
    Idle,
    /// Not installed on this machine.
    #[default]
    NotInstalled,
    /// Nothing to install: this build ships no driver package.
    ///
    /// A source build, or a copy where the `drivers` folder did not travel.
    /// Said plainly rather than shown as a failed install, because there is
    /// nothing the person clicking can do about it.
    NotBundled,
}

impl DriverState {
    /// Whether a virtual display can be created right now.
    pub fn is_usable(self) -> bool {
        self == Self::Ready
    }

    /// Whether the driver is on this machine at all.
    pub fn is_installed(self) -> bool {
        matches!(self, Self::Ready | Self::Idle)
    }

    /// One line for the settings panel.
    pub fn summary(self) -> &'static str {
        match self {
            Self::Ready => "Installed. A virtual display can be created.",
            Self::Idle => "Installed, but no virtual monitor is present yet.",
            Self::NotInstalled => "Not installed. Virtual displays need it.",
            Self::NotBundled => "This copy of Tain ships no driver to install.",
        }
    }
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::*;
    use anyhow::{bail, Context, Result};
    use std::path::PathBuf;

    /// The driver package this build ships, if it ships one.
    ///
    /// Looks for exactly one `.inf` so that adding the package is a matter of
    /// dropping a folder in rather than also editing a name in here — and so
    /// that a folder containing something else entirely is reported as absent
    /// rather than half-found.
    pub fn package() -> Option<PathBuf> {
        let beside = std::env::current_exe().ok()?.parent()?.join(PACKAGE_DIR);
        let mut infs = std::fs::read_dir(&beside)
            .ok()?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("inf"))
            });
        let first = infs.next()?;
        if infs.next().is_some() {
            tracing::warn!(
                "{} holds more than one .inf — not guessing which is the driver",
                beside.display()
            );
            return None;
        }
        Some(first)
    }

    /// Whether Windows has this driver's device installed.
    ///
    /// Asked of the device tree rather than of the display list, because a
    /// driver that is installed but presenting nothing has no display to find
    /// and is still installed.
    fn device_present() -> bool {
        // The adapters this driver registers carry its name, which is the same
        // match `virtual_display` uses to recognise one. A driver that has been
        // installed but has never presented a monitor still registers its
        // adapters, so this sees it either way.
        tain_core::virtual_display::available()
            .iter()
            .any(|screen| screen.adapter.to_ascii_lowercase().contains("tain"))
    }

    /// What the settings panel shows.
    pub fn state() -> DriverState {
        if device_present() {
            let ready = tain_core::virtual_display::available()
                .iter()
                .any(|screen| screen.ready);
            return if ready { DriverState::Ready } else { DriverState::Idle };
        }
        if package().is_none() {
            return DriverState::NotBundled;
        }
        DriverState::NotInstalled
    }

    /// Ask for elevation and install the driver.
    ///
    /// Relaunches this executable with the install flag under the `runas` verb.
    /// Installing a driver needs administrator rights and the app does not have
    /// them, so the prompt is unavoidable; expecting it beats being surprised.
    ///
    /// Windows will ask a second time, on its own, about trusting the
    /// publisher — that dialog belongs to Windows and cannot be suppressed,
    /// which is the point of it.
    pub fn request_install() -> Result<()> {
        if package().is_none() {
            bail!("this copy of Tain ships no virtual display driver to install");
        }
        super::super::service::elevate_with(INSTALL_FLAG)
    }

    /// The same, for removal.
    pub fn request_uninstall() -> Result<()> {
        super::super::service::elevate_with(UNINSTALL_FLAG)
    }

    /// Create the device node and bind the driver to it.
    ///
    /// Must already be running as administrator; this is what the elevated
    /// relaunch above ends up calling.
    pub fn install() -> Result<()> {
        let inf = package().context("no driver package beside this executable")?;
        let inf = inf
            .canonicalize()
            .with_context(|| format!("could not resolve {}", inf.display()))?;
        device::create(HARDWARE_ID)?;
        device::bind(&inf, HARDWARE_ID)?;
        tracing::info!("Installed the virtual display driver from {}", inf.display());
        Ok(())
    }

    /// Remove the device node. Must already be running as administrator.
    pub fn uninstall() -> Result<()> {
        let removed = device::remove(HARDWARE_ID)?;
        if removed == 0 {
            tracing::info!("No virtual display device to remove");
        } else {
            tracing::info!("Removed {removed} virtual display device(s)");
        }
        Ok(())
    }

    /// The SetupAPI half: making a device out of nothing and giving it a driver.
    ///
    /// This is what `devcon install` does, and there is no higher-level way to
    /// do it — a root-enumerated device has no hardware to be discovered, so
    /// something has to assert that it exists.
    mod device {
        use anyhow::{Context, Result};
        use std::path::Path;
        use windows::core::PCWSTR;
        use windows::Win32::Devices::DeviceAndDriverInstallation::{
            SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW,
            SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
            SetupDiGetDeviceRegistryPropertyW, SetupDiSetDeviceRegistryPropertyW,
            UpdateDriverForPlugAndPlayDevicesW, DICD_GENERATE_ID, DIF_REGISTERDEVICE,
            DIF_REMOVE, DIGCF_PRESENT, HDEVINFO, INSTALLFLAG_FORCE, SPDRP_HARDWAREID,
            SP_DEVINFO_DATA,
        };
        use windows::Win32::Foundation::{BOOL, HWND};

        /// No parent window: this runs from a relaunched, elevated process with
        /// no interface of its own, and every one of these calls takes one only
        /// to parent a dialog it will not show.
        const NO_PARENT: HWND = HWND(std::ptr::null_mut());

        /// `GUID_DEVCLASS_DISPLAY` — the class a display adapter belongs to.
        const DISPLAY_CLASS: windows::core::GUID =
            windows::core::GUID::from_u128(0x4d36e968_e325_11ce_bfc1_08002be10318);

        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain(std::iter::once(0)).collect()
        }

        /// A hardware id list is REG_MULTI_SZ: each string NUL-terminated, and
        /// the whole thing terminated again. One id here, so two NULs.
        fn multi_sz(s: &str) -> Vec<u16> {
            s.encode_utf16().chain([0, 0]).collect()
        }

        /// Assert that a device with this hardware id exists.
        ///
        /// Idempotent: a second call makes a second device node, so this checks
        /// first. Two nodes claiming one driver is not fatal, but it doubles
        /// the adapters and leaves one of them impossible to remove by name.
        pub fn create(hardware_id: &str) -> Result<()> {
            if count(hardware_id)? > 0 {
                tracing::info!("The virtual display device is already registered");
                return Ok(());
            }

            // SAFETY: every pointer below is either null or points at a local
            // that outlives the call; the info list is destroyed on both paths.
            unsafe {
                let list = SetupDiCreateDeviceInfoList(Some(&DISPLAY_CLASS), NO_PARENT)
                    .context("could not start a device installation")?;

                let result = (|| -> Result<()> {
                    let class = wide("Display");
                    let mut info = SP_DEVINFO_DATA {
                        cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                        ..Default::default()
                    };
                    SetupDiCreateDeviceInfoW(
                        list,
                        PCWSTR(class.as_ptr()),
                        &DISPLAY_CLASS,
                        PCWSTR::null(),
                        NO_PARENT,
                        DICD_GENERATE_ID,
                        Some(&mut info),
                    )
                    .context("could not create the device entry")?;

                    let ids = multi_sz(hardware_id);
                    let bytes = std::slice::from_raw_parts(
                        ids.as_ptr() as *const u8,
                        std::mem::size_of_val(&ids[..]),
                    );
                    SetupDiSetDeviceRegistryPropertyW(
                        list,
                        &mut info,
                        SPDRP_HARDWAREID,
                        Some(bytes),
                    )
                    .context("could not name the device")?;

                    // The step that makes it real. Everything above it lives
                    // only in this process's copy of the device info list.
                    SetupDiCallClassInstaller(DIF_REGISTERDEVICE, list, Some(&info as *const _))
                        .context("Windows refused to register the device")?;
                    Ok(())
                })();

                let _ = SetupDiDestroyDeviceInfoList(list);
                result
            }
        }

        /// Give the device a driver, from an `.inf` on disk.
        ///
        /// `INSTALLFLAG_FORCE` because the device was just created with no
        /// driver at all, so there is nothing to compare against and the
        /// ordinary "is this better than what is there" rule has no answer.
        pub fn bind(inf: &Path, hardware_id: &str) -> Result<()> {
            let inf_path = wide(&inf.to_string_lossy());
            let id = wide(hardware_id);
            let mut reboot = BOOL(0);

            // SAFETY: both strings outlive the call and `reboot` is a valid
            // out-parameter.
            unsafe {
                UpdateDriverForPlugAndPlayDevicesW(
                    NO_PARENT,
                    PCWSTR(id.as_ptr()),
                    PCWSTR(inf_path.as_ptr()),
                    INSTALLFLAG_FORCE,
                    Some(&mut reboot),
                )
            }
            .with_context(|| format!("Windows would not install {}", inf.display()))?;

            if reboot.as_bool() {
                tracing::warn!("Windows says the driver needs a restart to finish");
            }
            Ok(())
        }

        /// Remove every device with this hardware id. Returns how many.
        pub fn remove(hardware_id: &str) -> Result<u32> {
            with_devices(hardware_id, |list, info| {
                // SAFETY: both come from the enumeration above and are live.
                unsafe { SetupDiCallClassInstaller(DIF_REMOVE, list, Some(info as *const _)) }
                    .context("Windows refused to remove the device")
            })
        }

        fn count(hardware_id: &str) -> Result<u32> {
            with_devices(hardware_id, |_, _| Ok(()))
        }

        /// Walk the display class for devices with this hardware id.
        fn with_devices(
            hardware_id: &str,
            mut act: impl FnMut(HDEVINFO, &SP_DEVINFO_DATA) -> Result<()>,
        ) -> Result<u32> {
            let wanted = hardware_id.to_ascii_lowercase();

            // SAFETY: the handle is destroyed on both paths; every out-param
            // below points at a local that outlives its call.
            unsafe {
                let list = SetupDiGetClassDevsW(
                    Some(&DISPLAY_CLASS),
                    PCWSTR::null(),
                    NO_PARENT,
                    DIGCF_PRESENT,
                )
                .context("could not enumerate display devices")?;

                let result = (|| -> Result<u32> {
                    let mut matched = 0;
                    for index in 0.. {
                        let mut info = SP_DEVINFO_DATA {
                            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                            ..Default::default()
                        };
                        if SetupDiEnumDeviceInfo(list, index, &mut info).is_err() {
                            break;
                        }

                        let mut buffer = [0u8; 512];
                        let mut needed = 0u32;
                        if SetupDiGetDeviceRegistryPropertyW(
                            list,
                            &info,
                            SPDRP_HARDWAREID,
                            None,
                            Some(&mut buffer),
                            Some(&mut needed),
                        )
                        .is_err()
                        {
                            continue;
                        }

                        // REG_MULTI_SZ, and any of the ids may be the one.
                        let words = std::slice::from_raw_parts(
                            buffer.as_ptr() as *const u16,
                            buffer.len() / 2,
                        );
                        let found = words
                            .split(|&c| c == 0)
                            .filter(|part| !part.is_empty())
                            .any(|part| String::from_utf16_lossy(part).to_ascii_lowercase() == wanted);
                        if !found {
                            continue;
                        }

                        matched += 1;
                        act(list, &info)?;
                    }
                    Ok(matched)
                })();

                let _ = SetupDiDestroyDeviceInfoList(list);
                result
            }
        }

        /// Sanity: the multi-sz encoding is what the registry expects.
        #[cfg(test)]
        mod tests {
            use super::*;

            #[test]
            fn a_hardware_id_list_is_terminated_twice() {
                let encoded = multi_sz("Root\\TainVirtualDisplay");
                assert_eq!(&encoded[encoded.len() - 2..], &[0, 0]);
                assert_eq!(
                    String::from_utf16_lossy(&encoded[..encoded.len() - 2]),
                    "Root\\TainVirtualDisplay"
                );
            }

            #[test]
            fn the_display_class_guid_is_the_documented_one() {
                assert_eq!(format!("{:?}", DISPLAY_CLASS).to_lowercase(),
                    "4d36e968-e325-11ce-bfc1-08002be10318");
            }
        }
    }
}

#[cfg(target_os = "windows")]
pub use windows_impl::{install, request_install, request_uninstall, state, uninstall};

#[cfg(not(target_os = "windows"))]
mod stub {
    use super::*;
    use anyhow::{bail, Result};

    pub fn state() -> DriverState {
        DriverState::NotBundled
    }

    pub fn request_install() -> Result<()> {
        bail!("virtual displays are a Windows feature")
    }

    pub fn request_uninstall() -> Result<()> {
        bail!("virtual displays are a Windows feature")
    }

    pub fn install() -> Result<()> {
        bail!("virtual displays are a Windows feature")
    }

    pub fn uninstall() -> Result<()> {
        bail!("virtual displays are a Windows feature")
    }
}

#[cfg(not(target_os = "windows"))]
pub use stub::{install, request_install, request_uninstall, state, uninstall};

#[cfg(test)]
mod tests {
    use super::*;

    /// A state nobody can act on must not look like one they can.
    #[test]
    fn only_a_ready_driver_can_make_a_display() {
        assert!(DriverState::Ready.is_usable());
        for state in
            [DriverState::Idle, DriverState::NotInstalled, DriverState::NotBundled]
        {
            assert!(!state.is_usable(), "{state:?} claimed to be usable");
        }
    }

    /// Installed-but-idle is installed. The uninstall button depends on it.
    #[test]
    fn a_driver_presenting_nothing_is_still_installed() {
        assert!(DriverState::Idle.is_installed());
        assert!(DriverState::Ready.is_installed());
        assert!(!DriverState::NotInstalled.is_installed());
        assert!(!DriverState::NotBundled.is_installed());
    }

    #[test]
    fn every_state_says_something_different() {
        let all = [
            DriverState::Ready,
            DriverState::Idle,
            DriverState::NotInstalled,
            DriverState::NotBundled,
        ];
        let mut seen = std::collections::BTreeSet::new();
        for state in all {
            assert!(seen.insert(state.summary()), "{state:?} repeats another summary");
        }
    }
}
