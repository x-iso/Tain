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

pub use app::{init_host_server, init_prober, AppState, GuestApp};
