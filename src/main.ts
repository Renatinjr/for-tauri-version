/**
 * Wiring between the Rust side (which owns config, downloads and the control channel) and
 * the video element (which only the webview can touch).
 *
 * Rust drives: it emits `play` when a file is ready and `release` when it needs the
 * webview to let go of one. The frontend reports playback position back so the heartbeat
 * has something to send.
 */
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Player, type LogLevel } from "./player";

interface MediaRef {
  videoId: string;
  url: string;
}

interface Notice {
  title: string;
  detail: string;
}

interface Bootstrap {
  media: MediaRef | null;
  notice: Notice | null;
}

const video = document.querySelector<HTMLVideoElement>("#video")!;
const freeze = document.querySelector<HTMLCanvasElement>("#freeze")!;
const notice = document.querySelector<HTMLDivElement>("#notice")!;

/** Everything the frontend logs goes into the same rolling file as the Rust side, so
 *  `getLogs` from the dashboard returns one coherent story rather than half of one. */
function log(level: LogLevel, message: string): void {
  void invoke("ui_log", { level, message }).catch(() => undefined);
  if (import.meta.env.DEV) console.log(`[${level}] ${message}`);
}

const player = new Player(video, freeze, log);

function showNotice(n: Notice | null): void {
  if (!n) {
    notice.hidden = true;
    notice.replaceChildren();
    return;
  }
  const title = document.createElement("div");
  title.className = "title";
  title.textContent = n.title;
  const detail = document.createElement("div");
  detail.className = "detail";
  detail.textContent = n.detail;
  notice.replaceChildren(title, detail);
  notice.hidden = false;
}

function play(media: MediaRef): void {
  showNotice(null);
  player.start(media.url);
  // Tell Rust the webview now owns this file. Only after this does it dare delete the
  // one before it — on Windows a delete of an open file simply fails.
  void invoke("media_switched", { videoId: media.videoId }).catch((err: unknown) => {
    log("w", `media_switched failed: ${String(err)}`);
  });
}

async function main(): Promise<void> {
  await listen<MediaRef>("play", (event) => {
    log("i", `Now playing ${event.payload.videoId}`);
    play(event.payload);
  });

  await listen<Notice | null>("notice", (event) => showNotice(event.payload));

  // Rust is about to move or delete the file underneath us.
  await listen<null>("release", () => {
    log("i", "Releasing the current file at the backend's request");
    player.release();
    void invoke("media_released").catch(() => undefined);
  });

  await listen<null>("reload", () => {
    log("i", "Reload requested");
    player.stop();
    void invoke("bootstrap")
      .then((state) => {
        const s = state as Bootstrap;
        if (s.media) play(s.media);
      })
      .catch((err: unknown) => log("e", `Reload failed: ${String(err)}`));
  });

  const state = await invoke<Bootstrap>("bootstrap");
  if (state.media) {
    log("i", `Starting on local media ${state.media.videoId}`);
    play(state.media);
  } else {
    log("i", "Nothing to play at startup");
    showNotice(state.notice);
  }

  // The heartbeat needs a position, and the Android player sampled at the same cadence.
  window.setInterval(() => {
    const { playing, positionMs } = player.snapshot();
    void invoke("report_playback", { playing, positionMs }).catch(() => undefined);
  }, 2_000);
}

// ------------------------------------------------------------------ lockdown

// Nothing here is a security boundary — a determined person still has Task Manager. It
// exists so a customer leaning on the keyboard, or a cleaner with a mouse, cannot take
// the screen out of playback.
document.addEventListener("contextmenu", (e) => e.preventDefault());
document.addEventListener("dragstart", (e) => e.preventDefault());
document.addEventListener("selectstart", (e) => e.preventDefault());

const BLOCKED_KEYS = new Set(["F3", "F5", "F7", "F12"]);

let chordSince: number | null = null;
let chordTimer: number | null = null;

document.addEventListener("keydown", (e) => {
  // The escape hatch. Android had adb; a locked-down Windows box has nothing, so without
  // this a dev machine needs Task Manager to get its desktop back.
  if (e.ctrlKey && e.shiftKey && e.key.toLowerCase() === "q") {
    e.preventDefault();
    if (chordSince === null) {
      chordSince = Date.now();
      chordTimer = window.setTimeout(() => {
        log("w", "Exit chord held for 3s — quitting");
        void invoke("request_quit").catch(() => undefined);
      }, 3_000);
    }
    return;
  }

  if (BLOCKED_KEYS.has(e.key)) {
    e.preventDefault();
    return;
  }
  if (e.ctrlKey && ["r", "p", "f", "u", "s"].includes(e.key.toLowerCase())) {
    e.preventDefault();
  }
});

document.addEventListener("keyup", (e) => {
  if (e.key.toLowerCase() === "q" || e.key === "Control" || e.key === "Shift") {
    if (chordTimer !== null) window.clearTimeout(chordTimer);
    chordTimer = null;
    chordSince = null;
  }
});

void main().catch((err: unknown) => {
  log("e", `Startup failed: ${String(err)}`);
  showNotice({ title: "Falha ao iniciar", detail: String(err) });
});
