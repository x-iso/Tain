//! tain-cli: Tain — P2P remote desktop via iroh.
//!
//! Launches the unified GUI. CLI host/guest subcommands are not wired up yet.

// Release builds are a GUI app: no console window should appear beside them.
// Debug builds keep the console so `cargo run` still prints. See `console.rs`
// for how output is restored when someone actually wants it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent;
mod console;
mod gui;
mod logging;
mod migrate;
mod service;

/// What this application calls itself, in every place it stores something.
///
/// One constant rather than a literal per call site: it names the credential
/// store, the configuration directory, the log directory and the unpacked
/// runtime cache, and those four have to agree or state goes missing.
pub const APP_NAME: &str = "Tain";

use anyhow::Result;
use tracing::*;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let has_console = console::attach(console::requested(&args));

    // Always to a file, and to the console as well when there is one. See
    // `logging` for why: a release build launched by double-click used to
    // produce no diagnostics whatsoever, which makes a problem on someone
    // else's machine impossible to look into.
    // The service and the agent write to `%ProgramData%\\Tain` rather than to
    // the user's application data — see `logging::component_log_path`. Decided
    // before the subscriber is built, because it decides which file it opens.
    let component = if args.iter().any(|a| a == service::RUN_FLAG) {
        Some("service".to_string())
    } else if args.iter().any(|a| a == agent::AGENT_FLAG) {
        Some(format!(
            "agent-{}",
            agent::desktop_from_args(&args).unwrap_or_else(|| "unknown".to_string())
        ))
    } else {
        None
    };
    if let Some(name) = &component
        && let Some(path) = logging::component_log_path(name)
    {
        // SAFETY: single-threaded here; nothing else has been started yet.
        unsafe { std::env::set_var("TAIN_LOG_FILE", &path) };
    }

    if let Some(path) = logging::init(has_console) {
        tracing::info!("Logging to {}", path.display());
    }

    // # Three ways this executable can start
    //
    // Handled before anything else opens a window, a socket or a credential
    // store, because none of these three wants any of that.
    //
    // The service modes exist so Tain stays a single portable executable: no
    // installer, no second download, and an exe handed to a friend still just
    // runs. See `service` for what the service is actually for.
    // The per-desktop agent, started by the service. Handled with the other
    // non-window modes: it opens no window, binds nothing, and must not be
    // mistaken for someone launching the application.
    if args.iter().any(|a| a == agent::AGENT_FLAG) {
        return agent::run(agent::desktop_from_args(&args));
    }

    if args.iter().any(|a| a == service::RUN_FLAG) {
        // Launched by the service control manager. Never returns until the
        // service is told to stop.
        if let Err(e) = service::run() {
            error!("Could not start as a service: {e:#}");
            return Err(e);
        }
        return Ok(());
    }

    if args.iter().any(|a| a == service::INSTALL_FLAG) {
        // The elevated half of the button in Settings: this process is the one
        // that was launched with administrator rights.
        return match service::install() {
            Ok(()) => {
                info!("Tain service installed");
                Ok(())
            }
            Err(e) => {
                error!("Could not install the Tain service: {e:#}");
                Err(e)
            }
        };
    }

    if args.iter().any(|a| a == service::UNINSTALL_FLAG) {
        return match service::uninstall() {
            Ok(()) => {
                info!("Tain service removed");
                Ok(())
            }
            Err(e) => {
                error!("Could not remove the Tain service: {e:#}");
                Err(e)
            }
        };
    }

    // Installed but not running is a state nobody asked for — Windows starts it
    // at boot, so reaching here stopped means it was stopped by hand or failed
    // once. Starting it needs administrator rights and often will not be
    // permitted, which is why this is quiet: Settings says plainly when the
    // service is installed and not running.
    service::start_if_stopped();

    // Before anything reads a setting or a credential. This application used to
    // be called Deskbridge, and the credential store holds the identity key
    // whose public half is inside every connection code this host has handed
    // out — filing it under a new name without moving it would silently mint a
    // new identity and break every code anyone had saved.
    migrate::from_deskbridge();

    // Unpack and load the ffmpeg runtime now, in the background, so the first
    // session does not pay for it and the log says up front whether hardware
    // decode is available.
    tain_core::warm_up_ffmpeg();

    // Before the window system. See `immersive::start_hook_early` — installed
    // after eframe is up, the keyboard hook is never invoked in this process,
    // though the same hook works in a standalone program on the same machine.
    gui::immersive::start_hook_early();

    let state = gui::AppState::persistent();
    #[cfg(debug_assertions)]
    gui::app::seed_demo_data(&state);

    // Binding waits for relay connectivity, so this can fail or stall on a bad
    // network. That must not stop the window from opening — the UI reports the
    // failure instead of the process dying before anything is visible.
    // # A window immediately, and hosting whenever the network allows
    //
    // This awaited `init_host_server` before opening the window. On a censored
    // or filtered connection the bind does not fail, it *hangs* — publishing to
    // the discovery service retries with a growing backoff and never gives up —
    // so `run_native` was never reached and no window ever appeared.
    //
    // The first attempt at fixing that put a deadline on the bind and carried on
    // without it. That was worse: on a network where binding legitimately takes
    // longer than the deadline, hosting was abandoned outright and never
    // retried, so the window opened and the application could not be connected
    // to at all. One broken behaviour traded for another.
    //
    // There is no deadline that is right, because "slow" and "never" are not
    // distinguishable here and the correct response to slow is to wait. So the
    // bind runs in the background and hands its channels over whenever it
    // finishes, however long that takes. The window opens at once; hosting
    // starts working the moment it is available, with no restart and nothing to
    // click.
    let prober = gui::init_prober(&state);

    let (handles_tx, handles_rx) = tokio::sync::oneshot::channel();
    let binding = state.clone();
    tokio::spawn(async move {
        match gui::init_host_server(&binding).await {
            Ok(handles) => {
                // The receiver is gone only if the window has already closed.
                let _ = handles_tx.send(handles);
            }
            Err(e) => {
                error!("Host server unavailable: {e:#}");
                *binding.host.error.lock().unwrap() = Some(format!("{e}"));
            }
        }
    });

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Tain")
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([800.0, 600.0])
            // The mark identifies the app in the native title bar and taskbar,
            // which is why the header no longer carries a logo row.
            .with_icon(gui::icon::window_icon(64)),
        ..Default::default()
    };

    eframe::run_native(
        "Tain",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            // Asked here because this callback runs on the render thread with
            // the GL context current, which is the only place the answer means
            // anything. See `gui::gl_interop`.
            #[cfg(target_os = "windows")]
            {
                // The event loop exists by now, so winit has already registered
                // for raw keyboard input. Undoing it here is what lets the
                // keyboard hook be called while a Tain window is in front —
                // see `gui::immersive::platform::release_keyboard_raw_input`.
                gui::immersive::release_keyboard_raw_input();
                gui::gl_interop::probe();
                // Kept for the sessions that will want to draw through it. This
                // is the only place eframe hands the context over.
                if let Some(gl) = cc.gl.clone() {
                    gui::gl_interop::remember_context(gl);
                }
            }
            // Required before any include_image! of the SVGs in assets/.
            egui_extras::install_image_loaders(&cc.egui_ctx);
            let mut app = gui::GuestApp::new(state);
            // Checking whether someone else is reachable does not depend on
            // this machine being reachable, so the prober starts straight away
            // rather than waiting on the host bind.
            app.attach_prober(prober);
            // Attached when the bind finishes, which may be after several
            // frames have already been drawn.
            app.attach_when_ready(handles_rx);
            app.apply_debug_flags();
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("Failed to start GUI: {e}"))?;

    Ok(())
}
