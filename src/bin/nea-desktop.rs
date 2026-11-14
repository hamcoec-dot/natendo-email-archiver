//! Natendo Email Archiver desktop app: the web UI in a native window.
//!
//! Build with `cargo build --release --features desktop`. Uses the OS web
//! view (WebView2 on Windows, WKWebView on macOS, WebKitGTK on Linux), so the
//! app stays small. The UI talks to the same local API as `nea serve`, on a
//! random port bound to 127.0.0.1.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::path::PathBuf;

use anyhow::{Context, Result};
use nea::archive::{Archive, OpenOptions};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::{Icon, WindowBuilder};
use wry::WebViewBuilder;

const ICON_PNG: &[u8] = include_bytes!("../../assets/icons/icon-256.png");

fn default_archive() -> PathBuf {
    if let Some(a) = std::env::args_os().nth(1) {
        return PathBuf::from(a);
    }
    if let Some(a) = std::env::var_os("NEA_ARCHIVE") {
        return PathBuf::from(a);
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
    match home {
        Some(h) if h.join("Documents").is_dir() => h.join("Documents").join("Natendo Archive"),
        Some(h) => h.join("Natendo Archive"),
        None => PathBuf::from("Natendo Archive"),
    }
}

fn icon() -> Option<Icon> {
    let decoder = png::Decoder::new(std::io::Cursor::new(ICON_PNG));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    if info.color_type != png::ColorType::Rgba {
        return None;
    }
    Icon::from_rgba(buf, info.width, info.height).ok()
}

fn error_box(msg: &str) {
    eprintln!("{msg}");
    #[cfg(windows)]
    {
        // Without a console, show the error where the user can see it.
        let _ = std::process::Command::new("mshta")
            .arg(format!("javascript:alert({});close()", serde_json::to_string(msg).unwrap_or_default()))
            .status();
    }
}

fn main() {
    if let Err(e) = run() {
        error_box(&format!("Natendo Email Archiver could not start:\n{e:#}"));
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let root = default_archive();
    let opts = OpenOptions { create: true, ..Default::default() };
    let archive = Archive::open(&root, &opts).with_context(|| format!("opening archive {}", root.display()))?;
    let serve_opts = OpenOptions { create: false, ..opts };

    let std_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    std_listener.set_nonblocking(true)?;
    let port = std_listener.local_addr()?.port();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(std_listener).expect("listener");
            if let Err(e) = nea::server::serve_with(archive, serve_opts, listener, false).await {
                eprintln!("server stopped: {e:#}");
            }
        });
    });

    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("Natendo Email Archiver")
        .with_window_icon(icon())
        .with_inner_size(tao::dpi::LogicalSize::new(1360.0, 860.0))
        .with_min_inner_size(tao::dpi::LogicalSize::new(720.0, 480.0))
        .build(&event_loop)?;
    let url = format!("http://127.0.0.1:{port}/");
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    let builder = WebViewBuilder::new(&window);
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let builder = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        WebViewBuilder::new_gtk(window.default_vbox().context("no GTK container")?)
    };
    let _webview = builder
        .with_url(&url)
        // Links in mail open in the user's browser, not inside the app.
        .with_new_window_req_handler(|u| {
            if u.starts_with("http://") || u.starts_with("https://") || u.starts_with("mailto:") {
                open_external(&u);
            }
            false
        })
        .with_navigation_handler(move |u| u.starts_with(&format!("http://127.0.0.1:{port}/")) || u == "about:blank" || u.starts_with("about:srcdoc"))
        .build()?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent { event: WindowEvent::CloseRequested, .. } = event {
            *control_flow = ControlFlow::Exit;
        }
    });
}

fn open_external(url: &str) {
    #[cfg(windows)]
    let _ = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}
