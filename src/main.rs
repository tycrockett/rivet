//! Runs the Rust application directly on the machine's native backend.

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(target_os = "linux")]
use nsh_platform_linux as platform;
#[cfg(target_os = "macos")]
use nsh_platform_macos as platform;
#[cfg(target_os = "windows")]
use nsh_platform_windows as platform;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn main() -> impl std::process::Termination {
    // The pty daemon (`ptyd.rs`) is this executable, started by the app and
    // outliving it: no window, no logger.
    if std::env::args().nth(1).as_deref() == Some("--ptyd") {
        return match rivet::ptyd::run() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("rivet: pty daemon: {e}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    rivet::adopt_legacy_data_dir();
    env_logger::init();
    // No system title bar: the strip and the columns run to the window's
    // top, the traffic lights over the strip's top, centered in a bar as
    // tall as the columns' tab rows (Bolt's `titleBarStyle: "hiddenInset"`).
    let window = nsh_core::desktop::MainWindowOptions::from_json(&serde_json::json!({
        "titleBar": { "style": "hidden", "height": 36 }
    }))
    .expect("the window options are valid");
    // `nsh.json`'s capabilities, as `nsh run`'s generated code declares
    // them: what an undeclared app gets, plus trusted input for web tabs
    // (`browse click` / `type`), which nothing grants by default.
    let manifest: serde_json::Value = serde_json::from_str(include_str!("../nsh.json")).expect("nsh.json is valid JSON");
    let capabilities = nsh_core::access::Capabilities::from_json(&manifest["capabilities"]).expect("nsh.json's capabilities are valid");
    platform::run_app(
        platform::AppSource::Rust(platform::Root::of::<rivet::App>().main_window(window).capabilities(capabilities)),
        platform::AppOptions {
            title: "Rivet".to_owned(),
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            app_id: "dev.rivet.app".to_owned(),
            size: (1280, 800),
            ..Default::default()
        },
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!(
    "Run this binary on macOS, Windows or Linux; use the TSX entry with nsh run for iOS and Android."
);
