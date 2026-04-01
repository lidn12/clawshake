//! Standalone clawshake-window binary.
//!
//! Thin shim — all logic lives in the `clawshake_window` library crate.

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "clawshake_window=info".into()),
        )
        .init();

    // Parse --port flag (default: 7475)
    let port = std::env::args()
        .position(|a| a == "--port")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(7475);

    if let Err(e) = clawshake_window::run(port) {
        eprintln!("clawshake-window error: {e}");
        std::process::exit(1);
    }
}
