//! Unified egui-based interface for Tain — single window with Host,
//! Connect, Previous Connections, and Incoming panels.

pub mod app;
pub mod connections;
pub mod frametime;
#[cfg(target_os = "windows")]
pub mod gl_interop;
pub mod secrets;
pub mod icon;
pub mod immersive;
pub mod preview;
pub mod keymap;
pub mod licenses;
pub mod network_bridge;
pub mod notify;
pub mod progress;
pub mod session;
pub mod session_window;
pub mod settings;
pub mod settings_panel;
pub mod shortcut_hint;
pub mod shortcuts;
pub mod theme;
pub mod title_bar;
pub mod touch;
#[cfg(target_os = "linux")]
pub mod wayland_keys;

pub use app::{init_host_server, init_prober, AppState, GuestApp};

/// The application id every Tain window carries.
///
/// A Wayland desktop finds a window's icon and taskbar entry by matching this
/// against the name of an installed `.desktop` file, because the icon
/// `with_icon` sets is X11 and Windows only — winit has no Wayland way to hand
/// one over. So this has to stay the same as `packaging/linux/tain.desktop`.
/// Read on Linux only; Windows ignores it.
pub const APP_ID: &str = "tain";
