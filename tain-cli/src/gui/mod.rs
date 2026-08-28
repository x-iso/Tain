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
pub mod network_bridge;
pub mod notify;
pub mod session;
pub mod session_window;
pub mod settings;
pub mod settings_panel;
pub mod shortcuts;
pub mod theme;

pub use app::{init_host_server, init_prober, AppState, GuestApp};
