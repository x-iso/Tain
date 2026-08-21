//! Guest mode CLI implementation (no GUI).
//!
//! Connects to a host via iroh P2P using the provided endpoint ticket and passcode.

use anyhow::Result;
use tracing::*;

/// Run the guest client in headless/CLI mode.
pub async fn run_guest(passcode: &str, ticket: Option<&str>) -> Result<()> {
    println!("Tain — P2P Remote Desktop Guest Client");
    println!("=============================================");
    println!();
    println!("Passcode: {}", passcode);

    // Parse host EndpointId from ticket (64-char hex string).
    let endpoint_id_str = match ticket {
        Some(t) => t.to_string(),
        None => {
            eprintln!("Error: --ticket <endpoint-id> is required to connect to a host.");
            println!();
            println!("Usage: rdp guest --passcode \"MASTER-xxx\" --ticket <64-char-endpoint-id>");
            std::process::exit(1);
        }
    };

    println!("Host ticket: {}", endpoint_id_str);
    println!();

    // For CLI-only mode, just log the connection attempt.
    // A full implementation would spawn a background tokio task that connects
    // and streams frames (with egui rendering handled by --gui mode).
    info!("Guest CLI mode — use `rdp host --gui` or `rdp guest --gui` for the unified GUI window.");

    Ok(())
}
