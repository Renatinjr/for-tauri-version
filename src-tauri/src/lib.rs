pub mod app;
pub mod cli;
pub mod config;
pub mod kiosk;
pub mod logs;
pub mod media_server;
pub mod store;

use tauri::{Manager, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

use crate::app::AppState;
use crate::config::ConfigStore;
use crate::store::MediaStore;
// `linfo!`/`lwarn!` are `#[macro_export]`ed, which places them in this module already.

pub fn run() {
    // Must be set before the webview is created. WebView2 otherwise refuses to start
    // audible playback without a user gesture, and there is nobody in the store to
    // provide one. Muting instead would be wrong: campaigns may have sound.
    #[cfg(windows)]
    std::env::set_var(
        "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
        "--autoplay-policy=no-user-gesture-required",
    );

    tauri::Builder::default()
        // A second launch is how re-provisioning works — the same shape as
        // `adb shell am start -S --es …` on the Android side. The running instance takes
        // the new arguments; the new process exits immediately.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            let forwarded = argv.get(1..).unwrap_or(&[]);
            linfo!("Second launch forwarded: {forwarded:?}");
            app::apply_cli_provisioning(app, &cli::parse(forwarded));
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            app::bootstrap,
            app::save_provisioning,
            app::media_switched,
            app::media_released,
            app::report_playback,
            app::ui_log,
            app::request_quit,
        ])
        .on_window_event(|_window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if std::env::var_os("SIGNAGE_WINDOWED").is_some() {
                    return;
                }
                // Alt+F4 and the window chrome must not be able to end a campaign. The
                // deliberate way out is the Ctrl+Shift+Q chord, which calls `exit`
                // directly and so is not caught here.
                api.prevent_close();
                lwarn!("Close request refused — use Ctrl+Shift+Q to exit");
            }
        })
        .setup(|tauri_app| {
            let handle = tauri_app.handle().clone();
            let dir = app::data_dir(&handle)?;

            logs::init(&dir.join("logs"))?;
            linfo!("--- signage-desktop {} starting ---", env!("CARGO_PKG_VERSION"));
            linfo!("Data directory: {}", dir.display());

            let store = MediaStore::new(&dir)?;
            let media_server = media_server::spawn(store.media_dir.clone())?;
            let config = ConfigStore::load(&dir)?;
            linfo!("Device id {}", config.snapshot().device_id);
            tauri_app.manage(AppState::new(store, media_server, config));

            // Provisioning passed to *this* process. A second launch takes the same path
            // through the single-instance plugin above.
            app::apply_cli_provisioning(&handle, &cli::parse(std::env::args().skip(1)));

            kiosk::keep_awake();

            // Developing against a kiosk window means it eats your screen and refuses to
            // close. `SIGNAGE_WINDOWED=1 pnpm tauri dev` gives back a normal window; the
            // shipped app never sets it.
            let windowed = std::env::var_os("SIGNAGE_WINDOWED").is_some();
            if windowed {
                if let Some(window) = tauri_app.get_webview_window("main") {
                    lwarn!("SIGNAGE_WINDOWED is set — running unlocked");
                    // Only these two. `set_resizable` and `set_decorations` reach
                    // `NSWindow makeFirstResponder` in wry and segfault on a window that
                    // was created undecorated — a dev-mode-only crash, but a crash.
                    let _ = window.set_fullscreen(false);
                    let _ = window.set_always_on_top(false);
                }
            }

            // A store screen must come back on its own after a power cut. This is the
            // stand-in for the Android player registering itself as HOME. Skipped in
            // windowed mode so a dev machine does not quietly acquire a logon item.
            if windowed {
                lwarn!("Windowed mode — not registering autostart");
            } else {
                let autostart = handle.autolaunch();
                match autostart.is_enabled() {
                    Ok(true) => linfo!("Autostart already registered"),
                    Ok(false) => match autostart.enable() {
                        Ok(()) => linfo!("Registered for autostart at logon"),
                        Err(err) => lwarn!("Could not register autostart: {err}"),
                    },
                    Err(err) => lwarn!("Could not read the autostart state: {err}"),
                }
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the signage player");
}
