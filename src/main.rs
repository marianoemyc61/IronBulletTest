#![windows_subsystem = "windows"]

mod ipc;

use std::sync::Arc;
use tokio::sync::Mutex;
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::{sink::SinkExt, stream::StreamExt};
use include_dir::{include_dir, Dir};

use ironbullet::config::{load_config, save_config};
use ipc::{AppState, IpcCmd};

// Webview checks removed

// Window handling removed

static GUI_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/gui/build");

fn mime_for(path: &str) -> &'static str {
    if path.ends_with(".html") { "text/html" }
    else if path.ends_with(".js") { "application/javascript" }
    else if path.ends_with(".css") { "text/css" }
    else if path.ends_with(".svg") { "image/svg+xml" }
    else if path.ends_with(".png") { "image/png" }
    else if path.ends_with(".ico") { "image/x-icon" }
    else if path.ends_with(".json") { "application/json" }
    else if path.ends_with(".woff2") { "font/woff2" }
    else if path.ends_with(".woff") { "font/woff" }
    else if path.ends_with(".ttf") { "font/ttf" }
    else { "application/octet-stream" }
}

// Evt enum removed

/// Check if we should run in CLI mode (any --config or --help arg present)
fn is_cli_mode() -> bool {
    std::env::args().any(|a| a == "--config" || a == "-c" || a == "--help" || a == "-h")
}

/// Attach to parent console on Windows so CLI output is visible.
/// Required because #![windows_subsystem = "windows"] hides the console.
#[cfg(target_os = "windows")]
fn attach_console() {
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(
            windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS,
        );
    }
}

#[cfg(not(target_os = "windows"))]
fn attach_console() {}

fn main() {
    if is_cli_mode() {
        attach_console();
        run_cli();
    } else {
        run_gui();
    }
}

fn run_cli() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match ironbullet::cli::parse_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {}", e);
            eprintln!("run with --help for usage");
            std::process::exit(1);
        }
    };

    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
    rt.block_on(async {
        if let Err(e) = ironbullet::cli::run(cli).await {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    });
}

use std::net::SocketAddr;
use std::borrow::Cow;
use axum::response::Html;
use axum::http::{header, StatusCode};

fn position_window() {}

#[tokio::main]
async fn run_gui() {
    // Clean up old binary from previous update
    if let Ok(exe) = std::env::current_exe() {
        let old = exe.with_extension("old.exe");
        if old.exists() {
            let _ = std::fs::remove_file(&old);
        }
    }

    let cfg = load_config();
    let state = Arc::new(Mutex::new(AppState::new()));

    // Axum router
    let app = Router::new()
        .route("/ws", get(ws_handler))
        .fallback(get(serve_gui))
        .with_state(state.clone());

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("Server running at http://{}", addr);

    // Open browser on startup
    let url = format!("http://{}", addr);
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd").args(["/c", "start", "", &url]).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(&url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(&url).spawn();

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn serve_gui(req: axum::extract::Request) -> impl IntoResponse {
    let uri = req.uri().path();
    let path = if uri.is_empty() || uri == "/" { "/index.html" } else { uri };
    let path = path.trim_start_matches('/');

    let (body, mime): (Cow<'static, [u8]>, &str) = match GUI_DIR.get_file(path) {
        Some(f) => (Cow::Borrowed(f.contents()), mime_for(path)),
        None => match GUI_DIR.get_file("index.html") {
            Some(f) => (Cow::Borrowed(f.contents()), "text/html"),
            None => (Cow::Borrowed(b"404 Not Found".as_ref()), "text/plain"),
        },
    };

    ([(header::CONTENT_TYPE, mime)], body)
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<Mutex<AppState>>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<Mutex<AppState>>) {
    let (mut sender, mut receiver) = socket.split();

    // Use a channel so arbitrary parts of the codebase can send IPC messages back to the frontend.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    // Forward messages from the channel to the websocket
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sender.send(Message::Text(msg)).await.is_err() {
                break; // Client disconnected
            }
        }
    });

    while let Some(Ok(Message::Text(text))) = receiver.next().await {
        if let Ok(cmd) = serde_json::from_str::<IpcCmd>(&text) {
            let tx_clone = tx.clone();
            let state_clone = state.clone();

            tokio::spawn(async move {
                // The eval_js callback used to wrap payloads in `window.__ipc_callback(...)`.
                // With WebSockets, we'll send the raw JSON and the frontend WS handler will process it.
                // However, for compatibility with hand-crafted JS strings inside ipc.rs, we can just send the string.
                // In Svelte, we'll `eval()` or safely parse depending on contents.
                ipc::handle_ipc_cmd(&cmd, &state_clone, move |js: String| {
                    let _ = tx_clone.send(js);
                });
            });
        }
    }
}
