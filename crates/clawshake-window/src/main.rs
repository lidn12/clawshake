//! Minimal Tauri v2 app shell for clawshake.
//!
//! Opens a native window pointing at the broker's `/ui` endpoint.
//! No broker startup logic — assumes the broker is already running.
//!
//! Usage:
//!   clawshake-window                        # defaults to http://127.0.0.1:7475/ui
//!   clawshake-window --port 8080            # custom port

fn main() {
    // Parse --port flag (default: 7475)
    let port = std::env::args()
        .position(|a| a == "--port")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(7475);

    let url = format!("http://127.0.0.1:{port}/ui");

    tauri::Builder::default()
        .setup(move |app| {
            use tauri::WebviewUrl;
            let webview_url = WebviewUrl::External(url.parse().expect("valid URL"));

            tauri::WebviewWindowBuilder::new(app, "main", webview_url)
                .title("clawshake")
                .inner_size(1200.0, 800.0)
                .min_inner_size(600.0, 400.0)
                .build()?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to run clawshake-window");
}
