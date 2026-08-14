//! The control channel: what `PlayerService.kt` does on the Android side.
//!
//! One supervisor task owns the connection. It reads the server address out of the config
//! each time round, dials, runs a session until something ends it, and backs off before
//! trying again — 1s, 1s, 2s, 4s, 8s, 16s, 32s, 60s, the same ladder as `ControlSocket.kt`.
//! A configuration change interrupts the backoff, so re-provisioning a screen reconnects
//! immediately instead of waiting out a minute.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

use crate::app::AppState;
use crate::download::{self, Downloader};
use crate::protocol::{app_version, PlayerMessage, ServerMessage};
use crate::{ldebug, lerror, linfo, lwarn};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// Matches OkHttp's `pingInterval(20, SECONDS)` on the Android client.
const PING_INTERVAL: Duration = Duration::from_secs(20);
/// A half-open TCP connection never errors on its own; this is what notices.
const SILENCE_LIMIT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

const BASE_BACKOFF_MS: u64 = 1_000;
const MAX_BACKOFF_MS: u64 = 60_000;

/// The Android player reports every whole percent and the service forwards every tenth.
const PROGRESS_STEP: u8 = 10;

/// Resumable download failures are retried here. The Android player computes `resumable`
/// and then never reads it, so a failed download there waits for the server to re-send the
/// assign; this does not.
const DOWNLOAD_ATTEMPTS: u32 = 5;

/// Handle the rest of the app uses to talk to the control channel.
pub struct ControlHandle {
    outbound: mpsc::UnboundedSender<PlayerMessage>,
    config_epoch: watch::Sender<u64>,
    connected: Arc<AtomicBool>,
    downloading: Arc<AtomicBool>,
}

impl ControlHandle {
    /// Queues a message. Returns false when the socket is down — nothing is buffered,
    /// matching the Android client, so a screen that reconnects does not flush a minute of
    /// stale heartbeats at the server.
    pub fn send(&self, message: PlayerMessage) -> bool {
        if !self.connected.load(Ordering::Relaxed) {
            return false;
        }
        self.outbound.send(message).is_ok()
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// The identity or the server address changed. Reconnect, or re-announce if the
    /// address is the same.
    pub fn config_changed(&self) {
        self.config_epoch.send_modify(|epoch| *epoch += 1);
    }
}

pub fn start(app: &AppHandle) {
    let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
    let (epoch_tx, epoch_rx) = watch::channel(0u64);
    let connected = Arc::new(AtomicBool::new(false));
    let downloading = Arc::new(AtomicBool::new(false));

    app.manage(ControlHandle {
        outbound: outbound_tx,
        config_epoch: epoch_tx,
        connected: connected.clone(),
        downloading,
    });

    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        supervisor(handle, outbound_rx, epoch_rx, connected).await;
    });
}

async fn supervisor(
    app: AppHandle,
    mut outbound: mpsc::UnboundedReceiver<PlayerMessage>,
    mut epoch: watch::Receiver<u64>,
    connected: Arc<AtomicBool>,
) {
    let mut attempt: u32 = 0;

    loop {
        let url = control_url(&app);
        let Some(url) = url else {
            linfo!("No server configured — control channel idle");
            if epoch.changed().await.is_err() {
                return;
            }
            attempt = 0;
            continue;
        };

        linfo!("Connecting to {url}");
        match connect(&url).await {
            Ok(stream) => {
                attempt = 0;
                connected.store(true, Ordering::Relaxed);
                linfo!("Control channel open");
                run_session(&app, stream, &mut outbound, &mut epoch, &url).await;
                connected.store(false, Ordering::Relaxed);
                linfo!("Control channel closed");
            }
            Err(err) => {
                attempt = attempt.saturating_add(1);
                lwarn!("Connect to {url} failed: {err}");
            }
        }

        let wait = backoff_ms(attempt);
        ldebug!("Reconnecting in {wait}ms");
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(wait)) => {}
            changed = epoch.changed() => {
                if changed.is_err() { return; }
                // Somebody re-provisioned the screen. Do not make them wait out a minute
                // of backoff to find out whether they got it right.
                attempt = 0;
            }
        }
    }
}

/// 1s, 1s, 2s, 4s, 8s, 16s, 32s, 60s, 60s… — `ControlSocket.backoffMs` verbatim.
fn backoff_ms(attempt: u32) -> u64 {
    let shift = attempt.saturating_sub(1).min(6);
    (BASE_BACKOFF_MS << shift).min(MAX_BACKOFF_MS)
}

fn control_url(app: &AppHandle) -> Option<String> {
    app.state::<AppState>().config.snapshot().control_url()
}

type Stream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(url: &str) -> anyhow::Result<Stream> {
    let mut request = url.into_client_request()?;
    // Without this an ngrok free tunnel answers the handshake with its HTML interstitial.
    request.headers_mut().insert(
        "ngrok-skip-browser-warning",
        HeaderValue::from_static("true"),
    );
    let (stream, _response) =
        tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request)).await??;
    Ok(stream)
}

async fn run_session(
    app: &AppHandle,
    stream: Stream,
    outbound: &mut mpsc::UnboundedReceiver<PlayerMessage>,
    epoch: &mut watch::Receiver<u64>,
    url: &str,
) {
    let (mut sink, mut source) = stream.split();

    // Mark any config change that happened *before* this hello as already accounted for.
    // Without this, provisioning applied while the socket was still dialling fires
    // `epoch.changed()` the moment the session starts, and the screen sends a second hello
    // carrying exactly what the first one said — which brings back a second assign. The
    // Android player has a comment warning about the same trap.
    epoch.borrow_and_update();

    if let Err(err) = send(&mut sink, hello(app)).await {
        lwarn!("Could not announce: {err}");
        return;
    }

    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.tick().await; // the first tick is immediate; skip it
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await;
    let mut last_seen = Instant::now();

    loop {
        tokio::select! {
            incoming = source.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    last_seen = Instant::now();
                    handle_text(app, &text);
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_))) => {
                    last_seen = Instant::now();
                }
                Some(Ok(Message::Close(frame))) => {
                    linfo!("Server closed the connection: {frame:?}");
                    return;
                }
                Some(Ok(Message::Frame(_))) => {}
                Some(Err(err)) => {
                    lwarn!("Control channel error: {err}");
                    return;
                }
                None => return,
            },

            Some(message) = outbound.recv() => {
                if let Err(err) = send(&mut sink, message).await {
                    lwarn!("Send failed: {err}");
                    return;
                }
            }

            _ = heartbeat.tick() => {
                if let Err(err) = send(&mut sink, heartbeat_message(app)).await {
                    lwarn!("Heartbeat failed: {err}");
                    return;
                }
            }

            _ = ping.tick() => {
                if last_seen.elapsed() > SILENCE_LIMIT {
                    lwarn!(
                        "No traffic for {}s — treating the connection as dead",
                        last_seen.elapsed().as_secs()
                    );
                    return;
                }
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    return;
                }
            }

            changed = epoch.changed() => {
                if changed.is_err() { return; }
                if control_url(app).as_deref() != Some(url) {
                    linfo!("Server address changed — reconnecting");
                    return;
                }
                // Same server, new identity. Re-announce so the dashboard moves this
                // screen to its new store and pushes that store's campaign.
                linfo!("Re-announcing after a provisioning change");
                if let Err(err) = send(&mut sink, hello(app)).await {
                    lwarn!("Re-announce failed: {err}");
                    return;
                }
            }
        }
    }
}

async fn send(
    sink: &mut futures_util::stream::SplitSink<Stream, Message>,
    message: PlayerMessage,
) -> anyhow::Result<()> {
    let text = serde_json::to_string(&message)?;
    ldebug!("-> {text}");
    sink.send(Message::Text(text.into())).await?;
    Ok(())
}

fn hello(app: &AppHandle) -> PlayerMessage {
    let state = app.state::<AppState>();
    let config = state.config.snapshot();
    PlayerMessage::Hello {
        device_id: config.device_id,
        store_id: config.store_id,
        device_name: config.device_name,
        app_version: app_version(),
        current_video_id: state.active_video_id(),
    }
}

fn heartbeat_message(app: &AppHandle) -> PlayerMessage {
    let state = app.state::<AppState>();
    let config = state.config.snapshot();
    let playback = state.playback();
    PlayerMessage::Heartbeat {
        device_id: config.device_id,
        store_id: config.store_id,
        current_video_id: state.active_video_id(),
        playing: playback.playing,
        position_ms: playback.position_ms,
        free_bytes: state.store.free_bytes(),
        app_version: app_version(),
        uptime_ms: state.uptime_ms(),
    }
}

fn handle_text(app: &AppHandle, text: &str) {
    ldebug!("<- {text}");
    // An unparseable frame is logged and dropped. It must never close the connection:
    // one bad message from the dashboard would otherwise take a store offline.
    let message: ServerMessage = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(err) => {
            lwarn!(
                "Ignoring unparseable message ({err}): {}",
                text.chars().take(200).collect::<String>()
            );
            return;
        }
    };
    handle_message(app, message);
}

fn handle_message(app: &AppHandle, message: ServerMessage) {
    match message {
        ServerMessage::Assign {
            video_id,
            url,
            sha256,
            size_bytes,
            group_id,
            start_epoch_ms,
        } => on_assign(
            app,
            video_id,
            url,
            sha256,
            size_bytes,
            group_id,
            start_epoch_ms,
        ),

        ServerMessage::Reload => {
            linfo!("Reload requested");
            let _ = app.emit("reload", ());
        }

        ServerMessage::Identify => {
            let name = app
                .state::<AppState>()
                .config
                .snapshot()
                .display_name()
                .to_string();
            linfo!("Identify requested — showing {name}");
            let _ = app.emit("identify", name);
        }

        ServerMessage::GetLogs => {
            let device_id = app.state::<AppState>().config.snapshot().device_id;
            let (text, truncated) = crate::logs::tail(crate::logs::DEFAULT_TAIL_BYTES);
            linfo!(
                "Sending {} bytes of log (truncated: {truncated})",
                text.len()
            );
            if let Some(control) = app.try_state::<ControlHandle>() {
                control.send(PlayerMessage::Logs {
                    device_id,
                    text,
                    truncated,
                });
            }
        }

        ServerMessage::Configure {
            store_id,
            device_name,
        } => {
            linfo!("Configure from the dashboard: store={store_id:?} name={device_name:?}");
            crate::app::apply_remote_configure(app, store_id.as_deref(), device_name.as_deref());
        }

        ServerMessage::Reboot => {
            // The Android player reboots the stick, which it can do as device owner.
            // Restarting the app is the proportionate equivalent: rebooting a shop's PC
            // from a dashboard is a much bigger hammer than whoever clicked it expects.
            lwarn!("Reboot requested — restarting the app");
            app.restart();
        }

        // Phase D turns these into a clock offset.
        ServerMessage::TimeResponse {
            client_send_ms,
            server_ms,
        } => ldebug!("timeResponse: sent {client_send_ms}, server {server_ms}"),

        ServerMessage::Unknown => ldebug!("Ignoring a message type this build does not know"),
    }
}

// ------------------------------------------------------------------ downloads

#[allow(clippy::too_many_arguments)]
fn on_assign(
    app: &AppHandle,
    video_id: String,
    url: String,
    sha256: String,
    size_bytes: u64,
    group_id: Option<String>,
    start_epoch_ms: Option<i64>,
) {
    let Some(video_id) = crate::store::sanitize_video_id(&video_id).map(str::to_string) else {
        lerror!("Refusing assign: {video_id:?} is not a usable file name");
        report_error(app, "HTTP_ERROR", format!("unusable videoId {video_id:?}"));
        return;
    };

    let state = app.state::<AppState>();

    if state.active_video_id().as_deref() == Some(video_id.as_str())
        && state.store.media_file(&video_id).exists()
    {
        linfo!("Already playing {video_id} — ignoring assign");
        return;
    }

    let Some(control) = app.try_state::<ControlHandle>() else {
        return;
    };
    // One download at a time, and a new assign arriving mid-download is dropped rather
    // than queued — the same as the Android player. The server re-sends on the next
    // heartbeat if the store's campaign really did change.
    if control
        .downloading
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        lwarn!("A download is already running — ignoring assign for {video_id}");
        return;
    }

    let request = download::Request {
        video_id,
        url,
        sha256,
        size_bytes,
    };
    let handle = app.clone();
    let downloading = control.downloading.clone();

    tauri::async_runtime::spawn(async move {
        run_download(&handle, request, group_id, start_epoch_ms).await;
        downloading.store(false, Ordering::SeqCst);
    });
}

async fn run_download(
    app: &AppHandle,
    request: download::Request,
    group_id: Option<String>,
    start_epoch_ms: Option<i64>,
) {
    let downloader = match Downloader::new() {
        Ok(downloader) => downloader,
        Err(err) => {
            lerror!("Could not build the HTTP client: {err}");
            return;
        }
    };

    linfo!(
        "Downloading {} ({} bytes) from {}",
        request.video_id,
        request.size_bytes,
        request.url
    );

    // Re-downloading the same videoId means promote has to delete the file the webview is
    // playing, and Windows refuses to delete an open file. Ask for it back first.
    if active_video_id(app).as_deref() == Some(request.video_id.as_str()) {
        release_current_file(app).await;
    }

    for attempt in 1..=DOWNLOAD_ATTEMPTS {
        let mut last_reported: Option<u8> = None;
        let result = {
            let state = app.state::<AppState>();
            let active = state.active_video_id();
            let store = &state.store;
            downloader
                .download(store, &request, active.as_deref(), |percent| {
                    if percent % PROGRESS_STEP == 0 && last_reported != Some(percent) {
                        last_reported = Some(percent);
                        report_progress(app, &request.video_id, percent);
                    }
                })
                .await
        };

        match result {
            Ok(file) => {
                linfo!("{} verified and in place", request.video_id);
                let state = app.state::<AppState>();
                if let Err(err) = state.config.set_current_video(
                    Some(&request.video_id),
                    group_id.as_deref(),
                    start_epoch_ms,
                ) {
                    lwarn!("Could not remember the current video: {err}");
                }
                state.play(app, &file);
                return;
            }
            Err(failure) => {
                lwarn!(
                    "Download of {} failed [{}]: {} (attempt {attempt} of {DOWNLOAD_ATTEMPTS})",
                    request.video_id,
                    failure.code,
                    failure.detail
                );
                report_error(app, failure.code, failure.detail.clone());

                if !failure.resumable || attempt == DOWNLOAD_ATTEMPTS {
                    lerror!(
                        "Giving up on {}; the screen keeps what it had",
                        request.video_id
                    );
                    return;
                }
                tokio::time::sleep(Duration::from_millis(backoff_ms(attempt))).await;
            }
        }
    }
}

fn active_video_id(app: &AppHandle) -> Option<String> {
    app.state::<AppState>().active_video_id()
}

fn released_seq(app: &AppHandle) -> u64 {
    app.state::<AppState>().released_seq()
}

/// Asks the webview to let go of the file it is playing, and waits for it to say it has.
async fn release_current_file(app: &AppHandle) {
    let before = released_seq(app);
    linfo!("Asking the screen to release its file before replacing it");
    let _ = app.emit("release", ());

    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if released_seq(app) != before {
            return;
        }
    }
    lwarn!("The screen did not confirm it released the file; going ahead anyway");
}

fn report_progress(app: &AppHandle, video_id: &str, percent: u8) {
    if let Some(control) = app.try_state::<ControlHandle>() {
        control.send(PlayerMessage::DownloadProgress {
            video_id: video_id.to_string(),
            percent,
        });
    }
}

/// `NETWORK_ERROR` and `HTTP_ERROR` are load-bearing: the server matches on them to print
/// its "PUBLIC_URL is unreachable from the players" diagnostic.
fn report_error(app: &AppHandle, code: &str, detail: String) {
    if let Some(control) = app.try_state::<ControlHandle>() {
        control.send(PlayerMessage::Error {
            code: code.to_string(),
            detail,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::backoff_ms;

    #[test]
    fn the_backoff_ladder_matches_the_android_client() {
        // attempt 0 is "a session that did open and then ended" — retry promptly.
        let ladder: Vec<u64> = (0..10).map(backoff_ms).collect();
        assert_eq!(
            ladder,
            vec![1_000, 1_000, 2_000, 4_000, 8_000, 16_000, 32_000, 60_000, 60_000, 60_000]
        );
    }
}
