//! Headless smoke-test client. Useful before the menu-bar UI exists.
//!
//! Usage:
//!     konstantin-status                  # one-shot, human output
//!     konstantin-status --json           # one-shot, raw JSON
//!     konstantin-status --watch          # subscribe, stream updates
//!     konstantin-status --watch --json   # subscribe, one JSON object per line
//!     SCREENTIMED_SOCKET=/tmp/x.sock konstantin-status
//!
//! Exit codes: 0 ok, 2 transport / decode error, 3 daemon-side error.

use anyhow::Result;
use konstantin_proto::UserStatus;
use konstantin_tray::{default_socket_path, fetch_status, format_remaining, Subscription};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let json = args.iter().any(|a| a == "--json");
    let watch = args.iter().any(|a| a == "--watch" || a == "-w");
    let socket = default_socket_path();

    if watch {
        let mut sub = match Subscription::open(&socket).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("konstantin-status: {e:#}");
                std::process::exit(2);
            }
        };
        loop {
            match sub.next_update().await {
                Ok(Some(status)) => {
                    if json {
                        // One JSON object per line — friendly for `jq`,
                        // pipes, and trivial parsers.
                        println!("{}", serde_json::to_string(&status)?);
                    } else {
                        print_status_compact(&status);
                    }
                }
                Ok(None) => {
                    eprintln!("konstantin-status: daemon closed connection");
                    return Ok(());
                }
                Err(e) => {
                    eprintln!("konstantin-status: {e:#}");
                    std::process::exit(2);
                }
            }
        }
    }

    let status = match fetch_status(&socket).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("konstantin-status: {e:#}");
            std::process::exit(2);
        }
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print_status_block(&status);
    }
    Ok(())
}

fn print_status_block(status: &UserStatus) {
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

/// One status update per line — readable when watching a stream.
fn print_status_compact(status: &UserStatus) {
    let now = chrono::Local::now().format("%H:%M:%S");
    println!(
        "[{}] {} state={:?} used={} remaining={}",
        now,
        status.username,
        status.state,
        format_remaining(status.used_seconds as i64),
        format_remaining(status.remaining_seconds),
    );
}
