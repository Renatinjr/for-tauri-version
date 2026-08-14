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

#[derive(Debug, Serialize)]
pub struct Bootstrap {
    pub media: Option<MediaRef>,
    pub notice: Option<Notice>,
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
    pub fn new(store: MediaStore, media_server: MediaServer) -> Self {
        Self {
            store,
            media_server,
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

    /// Points the screen at a file. The frontend answers with `media_switched`, and only
    /// then is the previous file safe to delete.
    pub fn play(&self, app: &AppHandle, file: &Path) {
        let video_id = store::video_id_of(file);
        let Some(url) = self.media_server.url_for(file) else {
            lerror!("Cannot build a URL for {}", file.display());
            return;
        };
        linfo!("Handing {video_id} to the screen");
        if let Err(err) = app.emit("play", MediaRef { video_id, url }) {
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

// ------------------------------------------------------------------- commands

#[tauri::command]
pub fn bootstrap(state: State<'_, AppState>) -> Bootstrap {
    match state.store.newest_media() {
        Some(file) => {
            let video_id = store::video_id_of(&file);
            linfo!("Local media on disk: {video_id}");
            Bootstrap {
                media: state.media_server.url_for(&file).map(|url| MediaRef {
                    video_id,
                    url,
                }),
                notice: None,
            }
        }
        None => {
            linfo!("No local media yet");
            Bootstrap {
                media: None,
                notice: Some(Notice {
                    title: "Sem campanha".into(),
                    detail: format!(
                        "Nenhum vídeo disponível. Coloque um arquivo .mp4 em {} ou provisione a tela com um servidor.",
                        state.store.media_dir.display()
                    ),
                }),
            }
        }
    }
}

/// The webview now holds this file open. Anything it replaced can be deleted.
#[tauri::command]
pub fn media_switched(state: State<'_, AppState>, video_id: String) {
    {
        let mut inner = state.lock();
        inner.active_video_id = Some(video_id.clone());
    }
    state.store.retain_only(&video_id);
}

/// The webview has let go of its file, so a promote or delete over it can proceed.
#[tauri::command]
pub fn media_released(state: State<'_, AppState>) {
    let mut inner = state.lock();
    inner.released_seq += 1;
    drop(inner);
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
