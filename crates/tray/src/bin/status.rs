//! Headless smoke-test client. Useful before the menu-bar UI exists.
//!
//! Usage:
//!     screentime-status              # human output
//!     screentime-status --json       # raw JSON
//!     SCREENTIMED_SOCKET=/tmp/x.sock screentime-status
//!
//! Exit codes: 0 ok, 2 transport / decode error, 3 daemon-side error.

use anyhow::Result;
use screentime_tray::{default_socket_path, fetch_status, format_remaining};

#[tokio::main]
async fn main() -> Result<()> {
    let json = std::env::args().any(|a| a == "--json");
    let socket = default_socket_path();

    let status = match fetch_status(&socket).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("screentime-status: {e:#}");
            std::process::exit(2);
        }
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        println!(
            "user      : {} (uid {})\n\
             state     : {:?}\n\
             daily     : {}\n\
             used      : {}\n\
             remaining : {}\n\
             resets_at : {}",
            status.username,
            status.uid,
            status.state,
            format_remaining(status.daily_limit_seconds as i64),
            format_remaining(status.used_seconds as i64),
            format_remaining(status.remaining_seconds),
            status.resets_at.format("%Y-%m-%d %H:%M:%S %z"),
        );
    }
    Ok(())
}
