//! Shared state and the commands the webview calls.
//!
//! The split follows the Android player: everything that is not the video element itself
//! lives on this side. The frontend is a display and a set of media-element controls; it
//! owns no state that matters if it is reloaded.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::cli::Provisioning;
use crate::config::ConfigStore;
use crate::media_server::MediaServer;
use crate::store::{self, MediaStore};
use crate::{lerror, linfo, lwarn};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaRef {
    pub video_id: String,
    pub url: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Notice {
    pub title: String,
    pub detail: String,
}

/// What the setup screen is allowed to see. Deliberately not the whole `Config` — the
/// group and sync fields are none of the frontend's business.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigView {
    pub device_id: String,
    pub device_name: Option<String>,
    pub store_id: Option<String>,
    pub server: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Bootstrap {
    pub media: Option<MediaRef>,
    pub notice: Option<Notice>,
    pub config: ConfigView,
    pub needs_provisioning: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackSnapshot {
    pub playing: bool,
    pub position_ms: i64,
}

pub struct AppState {
    pub store: MediaStore,
    pub media_server: MediaServer,
    pub config: ConfigStore,
    started_at: Instant,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    active_video_id: Option<String>,
    playback: PlaybackSnapshot,
    /// Bumped every time the webview confirms it has let go of its file. Phase C waits on
    /// this before promoting over a file Windows would otherwise refuse to touch.
    released_seq: u64,
}

impl AppState {
    pub fn new(store: MediaStore, media_server: MediaServer, config: ConfigStore) -> Self {
        Self {
            store,
            media_server,
            config,
            started_at: Instant::now(),
            inner: Mutex::new(Inner::default()),
        }
    }

    pub fn uptime_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    pub fn active_video_id(&self) -> Option<String> {
        self.lock().active_video_id.clone()
    }

    pub fn playback(&self) -> PlaybackSnapshot {
        self.lock().playback
    }

    pub fn released_seq(&self) -> u64 {
        self.lock().released_seq
    }

    pub fn config_view(&self) -> ConfigView {
        let config = self.config.snapshot();
        ConfigView {
            device_id: config.device_id,
            device_name: config.device_name,
            store_id: config.store_id,
            server: config.server,
        }
    }

    /// What is on disk to play right now, if anything.
    pub fn local_media(&self) -> Option<MediaRef> {
        let file = self.store.newest_media()?;
        self.media_ref(&file)
    }

    pub fn media_ref(&self, file: &Path) -> Option<MediaRef> {
        Some(MediaRef {
            video_id: store::video_id_of(file),
            url: self.media_server.url_for(file)?,
        })
    }

    /// Whether the setup screen should be showing.
    ///
    /// Derived, never latched. It used to be set once at boot in the Android player, which
    /// meant a provisioned, playing screen threw the setup form over the video on every
    /// activity recreation (fixed there in `eed8f17`). Do not reintroduce that.
    pub fn needs_provisioning(&self) -> bool {
        self.config.snapshot().server.is_none() && self.local_media().is_none()
    }

    /// Points the screen at a file. The frontend answers with `media_switched`, and only
    /// then is the previous file safe to delete.
    pub fn play(&self, app: &AppHandle, file: &Path) {
        let Some(media) = self.media_ref(file) else {
            lerror!("Cannot build a URL for {}", file.display());
            return;
        };
        linfo!("Handing {} to the screen", media.video_id);
        if let Err(err) = app.emit("play", media) {
            lerror!("Could not tell the screen to play: {err}");
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock means another thread panicked while holding it. The state here
        // is a few scalars; carrying on with it is strictly better than taking the screen
        // down over a lock flag.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Applies command-line provisioning, from this process's own arguments or from a second
/// launch forwarded by the single-instance plugin.
///
/// No validation, matching the Android player's intent path — `provision.ps1` and an
/// operator at a keyboard are different audiences, and a script that wants to set only the
/// store should be able to.
pub fn apply_cli_provisioning(app: &AppHandle, provisioning: &Provisioning) {
    if provisioning.is_empty() {
        return;
    }
    let state = app.state::<AppState>();
    match state.config.set_provisioning(
        provisioning.server.as_deref(),
        provisioning.name.as_deref(),
        provisioning.store.as_deref(),
    ) {
        Ok(false) => linfo!("Command-line provisioning matched what was already stored"),
        Ok(true) => {
            let config = state.config.snapshot();
            linfo!(
                "Provisioned from the command line: server={:?} store={:?} name={:?}",
                config.server,
                config.store_id,
                config.device_name
            );
            announce_config(app, &state);
        }
        Err(err) => lerror!("Could not save command-line provisioning: {err}"),
    }
}

/// Tells the frontend the identity changed, so the setup form and the prompt stay honest.
fn announce_config(app: &AppHandle, state: &AppState) {
    let payload = ConfigChanged {
        config: state.config_view(),
        needs_provisioning: state.needs_provisioning(),
    };
    if let Err(err) = app.emit("config-changed", payload) {
        lwarn!("Could not tell the screen about the new config: {err}");
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigChanged {
    config: ConfigView,
    needs_provisioning: bool,
}

// ------------------------------------------------------------------- commands

#[tauri::command]
pub fn bootstrap(state: State<'_, AppState>) -> Bootstrap {
    let media = state.local_media();
    match &media {
        Some(media) => linfo!("Local media on disk: {}", media.video_id),
        None => linfo!("No local media yet"),
    }

    let needs_provisioning = state.needs_provisioning();
    let notice = if media.is_some() {
        None
    } else if needs_provisioning {
        // The setup screen covers this case, so no notice behind it.
        None
    } else {
        Some(Notice {
            title: "Sem campanha".into(),
            detail: "A tela está configurada e aguardando o servidor enviar um vídeo.".into(),
        })
    };

    Bootstrap {
        media,
        notice,
        config: state.config_view(),
        needs_provisioning,
    }
}

/// Validation for the setup form, matching `SetupActivity.bindUi`.
///
/// The store is required for the same reason it is on Android: without one the screen can
/// only ever be addressed individually, which defeats the point of campaigns. The name is
/// optional and falls back to the device id.
///
/// The command line deliberately does *not* go through this — a provisioning script that
/// wants to move only the store should be able to.
pub fn validate_provisioning(server: &str, store_id: &str) -> Result<(), String> {
    if server.is_empty() {
        return Err("Informe o endereço do servidor.".into());
    }
    if store_id.is_empty() {
        return Err("Informe o número da loja — as campanhas são enviadas por loja.".into());
    }
    Ok(())
}

/// Saves what the operator typed into the setup screen.
#[tauri::command]
pub fn save_provisioning(
    app: AppHandle,
    state: State<'_, AppState>,
    server: String,
    store_id: String,
    device_name: String,
) -> Result<ConfigView, String> {
    let server = server.trim();
    let store_id = store_id.trim();
    let device_name = device_name.trim();

    validate_provisioning(server, store_id)?;

    state
        .config
        .set_provisioning(
            Some(server),
            if device_name.is_empty() {
                None
            } else {
                Some(device_name)
            },
            Some(store_id),
        )
        .map_err(|err| format!("Não foi possível salvar: {err}"))?;

    let config = state.config.snapshot();
    linfo!(
        "Provisioned from the setup screen: server={:?} store={:?} name={:?}",
        config.server,
        config.store_id,
        config.device_name
    );
    announce_config(&app, &state);
    Ok(state.config_view())
}

/// The webview now holds this file open. Anything it replaced can be deleted.
#[tauri::command]
pub fn media_switched(state: State<'_, AppState>, video_id: String) {
    state.lock().active_video_id = Some(video_id.clone());
    state.store.retain_only(&video_id);
}

/// The webview has let go of its file, so a promote or delete over it can proceed.
#[tauri::command]
pub fn media_released(state: State<'_, AppState>) {
    state.lock().released_seq += 1;
    linfo!("Screen released its file");
}

#[tauri::command]
pub fn report_playback(state: State<'_, AppState>, playing: bool, position_ms: i64) {
    state.lock().playback = PlaybackSnapshot {
        playing,
        position_ms,
    };
}

#[tauri::command]
pub fn ui_log(level: String, message: String) {
    let level = match level.as_str() {
        "e" => 'E',
        "w" => 'W',
        "d" => 'D',
        _ => 'I',
    };
    crate::logs::write(level, &format!("[ui] {message}"));
}

/// The escape hatch, held for three seconds by a human standing at the machine.
///
/// `exit` does not go through the window's close request, which is refused on purpose —
/// otherwise this would be ignored along with Alt+F4.
#[tauri::command]
pub fn request_quit(app: AppHandle) {
    lwarn!("Exit requested from the keyboard");
    app.exit(0);
}

/// Where everything this app owns lives. Windows puts it under `%APPDATA%`.
pub fn data_dir(app: &AppHandle) -> anyhow::Result<PathBuf> {
    let dir = app.path().app_data_dir()?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::validate_provisioning;

    #[test]
    fn the_server_is_required() {
        assert!(validate_provisioning("", "710").is_err());
        assert!(validate_provisioning("192.168.1.10:8080", "710").is_ok());
    }

    #[test]
    fn the_store_is_required_because_campaigns_are_sent_per_store() {
        let err = validate_provisioning("192.168.1.10:8080", "").unwrap_err();
        assert!(err.contains("loja"), "got {err}");
    }
}
