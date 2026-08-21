//! Host mode implementation.
//!
//! - Creates an iroh endpoint and listens for incoming guest connections
//! - Generates master/guest passcodes on demand
//! - Manages multiple concurrent sessions with independent approval control

use anyhow::Result;
use tain_core::network::HostServer;
use tain_core::passcode::PasscodeType;
use std::io::{self, BufRead, Write};
use tracing::*;

/// Run the host server.
pub async fn run_host(
    generate_master: bool,
    generate_guest: bool,
    show_ticket: bool,
    _interactive: bool,
) -> Result<()> {
    // Create and start the iroh-based host server.
    let server = HostServer::bind().await?;

    if show_ticket {
        println!("Host Endpoint Ticket:\n{}", server.endpoint_id());
        return Ok(());
    }

    // Generate requested passcodes and exit (non-interactive one-shot mode).
    if generate_master {
        let code = server.generate_master_passcode().await?;
        println!("Generated MASTER passcode: {}", code);
        println!("Share this with the remote user — they get immediate full control.");
        return Ok(());
    }

    if generate_guest {
        let code = server.generate_guest_passcode().await?;
        println!("Generated GUEST passcode: {}", code);
        println!("Guests connecting with this code will need host approval before input is forwarded.");
        return Ok(());
    }

    // Interactive mode — run the host server and manage passcodes via CLI.
    info!("Host server running. Press Ctrl+C to stop.");
    info!("Commands: generate-master, generate-guest, list-codes, revoke <code>, approve <secret> <true|false>, quit");

    let stdin = io::stdin();
    loop {
        print!("rdp-host> ");
        io::stdout().flush()?;

        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 { break; } // EOF.

        let args: Vec<&str> = line.trim().split_whitespace().collect();
        if args.is_empty() { continue; }

        match args.as_slice() {
            ["generate-master"] => {
                match server.generate_master_passcode().await {
                    Ok(code) => println!("Generated: {}", code),
                    Err(e) => error!("Failed to generate master passcode: {}", e),
                }
            }
            ["generate-guest"] => {
                match server.generate_guest_passcode().await {
                    Ok(code) => println!("Generated: {}", code),
                    Err(e) => error!("Failed to generate guest passcode: {}", e),
                }
            }
            ["list-codes"] | ["codes"] => {
                let codes = server.list_passcodes().await;
                if codes.is_empty() {
                    println!("No active passcodes.");
                } else {
                    for (encoded, typ) in &codes {
                        let label = match typ { PasscodeType::Master => "MASTER", PasscodeType::Guest => "GUEST" };
                        println!("  {} — {}", encoded, label);
                    }
                }
            }
            ["revoke", code] => {
                match server.revoke_passcode(code).await {
                    Ok(()) => println!("Revoked: {}", code),
                    Err(e) => error!("Failed to revoke '{}': {}", code, e),
                }
            }
            ["approve", secret_hex, approved_str] => {
                let approved = match approved_str.to_lowercase().as_str() {
                    "true" | "1" | "yes" => true,
                    "false" | "0" | "no" => false,
                    _ => { println!("Invalid approval value '{}'. Use true/false.", approved_str); continue; }
                };

                if let Ok(secret_bytes) = hex_decode(secret_hex) {
                    match server.approve_guest(secret_bytes, approved).await {
                        Ok(true) => println!("Approval updated for {}: {}", secret_hex, if approved { "APPROVED" } else { "DENIED" }),
                        Ok(false) => println!("No pending approval request for this passcode secret"),
                        Err(e) => error!("Failed to update approval: {}", e),
                    }
                } else {
                    warn!("Invalid secret hex: {}", secret_hex);
                }
            }
            ["quit"] | ["exit"] | ["q"] => {
                info!("Shutting down host server...");
                server.shutdown().await?;
                break;
            }
            _ => println!("Unknown command. Use: generate-master, generate-guest, list-codes, revoke <code>, approve <secret> <true|false>, quit"),
        }
    }

    Ok(())
}

/// Decode a 64-character hex string to [u8; 32].
fn hex_decode(hex: &str) -> Result<[u8; 32], String> {
    if hex.len() != 64 {
        return Err(format!("Expected 64-char hex, got {} chars", hex.len()));
    }

    let mut bytes = [0u8; 32];
    for i in 0..32 {
        bytes[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|e| format!("Invalid hex at position {}: {}", i, e))?;
    }

    Ok(bytes)
}
