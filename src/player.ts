/**
 * The video element and everything that keeps it playing — a port of
 * `PlayerController.kt` from the Android player onto HTML5 media.
 *
 * The contract is the same: point it at a file and it loops forever. If it stops looping
 * for any reason, notice within fifteen seconds and rebuild, backing off so a genuinely
 * broken file does not spin the CPU.
 */

export type LogLevel = "i" | "w" | "e";
export type Logger = (level: LogLevel, message: string) => void;

export interface PlaybackSnapshot {
  playing: boolean;
  positionMs: number;
}

/** Watchdog cadence. Everything below is counted in ticks of this. */
const TICK_MS = 5_000;

/** Three consecutive identical positions — 15s — before calling it a stall. */
const STALL_TICKS = 3;

/** Local files should never buffer for 20s. */
const BUFFERING_TICKS = 4;

const BASE_BACKOFF_MS = 1_000;
const MAX_BACKOFF_MS = 30_000;
const HEALTHY_RESET_MS = 60_000;

/** `HTMLMediaElement.HAVE_FUTURE_DATA` — enough decoded to keep going. */
const HAVE_FUTURE_DATA = 3;

export class Player {
  private src: string | null = null;
  private running = false;

  /** Where to pick up after a rebuild, so a glitch does not restart the loop. */
  private resumeAtMs = 0;

  private lastPositionMs = -1;
  private stalledTicks = 0;

  private rebuildPending = false;
  private rebuildStreak = 0;
  private rebuildCount = 0;
  private lastRebuildAt = 0;

  private tickTimer: number | null = null;
  private rebuildTimer: number | null = null;

  constructor(
    private readonly video: HTMLVideoElement,
    private readonly freeze: HTMLCanvasElement,
    private readonly log: Logger,
  ) {
    // A media error is the one fault worth reacting to immediately rather than waiting
    // for the next tick; everything else the watchdog will catch.
    this.video.addEventListener("error", () => {
      if (!this.running) return;
      const code = this.video.error?.code ?? 0;
      this.rebuild(`media error ${mediaErrorName(code)}`);
    });
  }

  /**
   * Play [src]. A *different* source starts at zero; the *same* source resumes where it
   * was, which is what makes a watchdog rebuild invisible.
   */
  start(src: string): void {
    const changed = this.src !== src;
    this.src = src;
    if (changed) {
      this.resumeAtMs = 0;
      this.rebuildStreak = 0;
    }
    this.running = true;
    this.stalledTicks = 0;
    this.lastPositionMs = -1;
    this.clearTimer("rebuild");
    this.rebuildPending = false;
    this.build();
    this.clearTimer("tick");
    this.tickTimer = window.setInterval(() => this.checkProgress(), TICK_MS);
  }

  stop(): void {
    this.running = false;
    this.clearTimer("tick");
    this.clearTimer("rebuild");
    this.resumeAtMs = Math.round(this.video.currentTime * 1000);
    this.teardown();
  }

  /**
   * Releases the current file so the process that owns it can move or delete it. Windows
   * refuses to touch a file the webview still has open, which is the whole reason the
   * swap is a handshake rather than a rename.
   */
  release(): void {
    this.captureFreezeFrame();
    this.teardown();
  }

  snapshot(): PlaybackSnapshot {
    return {
      playing: this.running && !this.video.paused && !this.video.ended,
      positionMs: Math.round(this.video.currentTime * 1000),
    };
  }

  get durationMs(): number | null {
    const d = this.video.duration;
    return Number.isFinite(d) && d > 0 ? Math.round(d * 1000) : null;
  }

  get positionMs(): number {
    return Math.round(this.video.currentTime * 1000);
  }

  get rebuilds(): number {
    return this.rebuildCount;
  }

  // ---------------------------------------------------------------- internals

  private build(): void {
    const src = this.src;
    if (!src) return;

    const v = this.video;
    v.loop = true;
    v.controls = false;
    v.preload = "auto";
    v.disablePictureInPicture = true;
    // Autoplay policy: WebView2 refuses to start audible playback without a gesture
    // unless the runtime was launched with --autoplay-policy=no-user-gesture-required,
    // which main.rs does. Nothing here is muted, because store campaigns may have sound.
    v.src = src;

    const onLoaded = () => {
      v.removeEventListener("loadeddata", onLoaded);
      if (this.resumeAtMs > 0) v.currentTime = this.resumeAtMs / 1000;
      this.clearFreezeFrame();
      this.log("i", `Playing ${src} from ${this.resumeAtMs}ms`);
    };
    v.addEventListener("loadeddata", onLoaded);

    v.load();
    void this.tryPlay();
  }

  /**
   * WebView2 is launched with `--autoplay-policy=no-user-gesture-required`, so audible
   * autoplay works on the target platform. On anything else — a dev machine, or a Windows
   * build where that flag did not take — the browser refuses, and a silent screen is very
   * much better than a black one. So fall back to muted and say so in the log.
   */
  private async tryPlay(): Promise<void> {
    const v = this.video;
    try {
      await v.play();
      return;
    } catch (err) {
      this.log("w", `play() refused: ${String(err)}`);
    }
    if (v.muted) return;
    v.muted = true;
    try {
      await v.play();
      this.log("w", "Autoplay is blocked here — playing muted");
    } catch (err) {
      // The watchdog retries on every tick while we are paused.
      this.log("e", `play() refused even muted: ${String(err)}`);
    }
  }

  private teardown(): void {
    const v = this.video;
    v.pause();
    v.removeAttribute("src");
    // load() after clearing src is what actually makes the element let go of the file.
    v.load();
  }

  private checkProgress(): void {
    if (!this.running || this.rebuildPending || !this.src) return;

    const v = this.video;

    if (v.ended) {
      // `loop` is set, so the video reaching its end means the loop broke.
      this.rebuild("unexpected end of media");
      return;
    }

    if (v.paused) {
      // A screen must never sit paused. Usually this is a refused autoplay, so ask again
      // before treating it as a fault.
      this.stalledTicks++;
      void this.tryPlay();
      if (this.stalledTicks >= STALL_TICKS) {
        this.rebuild(`still paused after ${(STALL_TICKS * TICK_MS) / 1000}s`);
      }
      return;
    }

    if (v.readyState < HAVE_FUTURE_DATA) {
      this.stalledTicks++;
      if (this.stalledTicks >= BUFFERING_TICKS) {
        this.rebuild(`buffering for ${(BUFFERING_TICKS * TICK_MS) / 1000}s on a local file`);
      }
      return;
    }

    const positionMs = Math.round(v.currentTime * 1000);
    if (positionMs === this.lastPositionMs) {
      this.stalledTicks++;
      if (this.stalledTicks >= STALL_TICKS) {
        this.rebuild(`position frozen at ${positionMs}ms`);
      }
      return;
    }

    this.stalledTicks = 0;
    this.lastPositionMs = positionMs;
    this.forgetRebuildStreakIfHealthy();
  }

  private forgetRebuildStreakIfHealthy(): void {
    if (this.rebuildStreak === 0) return;
    if (Date.now() - this.lastRebuildAt < HEALTHY_RESET_MS) return;
    this.log("i", `Healthy for ${HEALTHY_RESET_MS / 1000}s — clearing rebuild streak`);
    this.rebuildStreak = 0;
  }

  private rebuild(reason: string): void {
    if (this.rebuildPending || !this.running) return;
    this.rebuildPending = true;
    this.rebuildCount++;
    this.rebuildStreak++;
    this.lastRebuildAt = Date.now();
    this.resumeAtMs = Math.round(this.video.currentTime * 1000);
    this.stalledTicks = 0;
    this.lastPositionMs = -1;

    this.captureFreezeFrame();
    this.teardown();

    const delay = this.backoffMs();
    this.log(
      "w",
      `Rebuilding player: ${reason} (streak ${this.rebuildStreak}, retrying in ${delay}ms, resume at ${this.resumeAtMs}ms)`,
    );
    this.clearTimer("rebuild");
    this.rebuildTimer = window.setTimeout(() => {
      this.rebuildTimer = null;
      this.rebuildPending = false;
      this.build();
    }, delay);
  }

  /** 1s, 2s, 4s, 8s, 16s, 30s, 30s… */
  private backoffMs(): number {
    const shift = Math.min(Math.max(this.rebuildStreak - 1, 0), 5);
    return Math.min(BASE_BACKOFF_MS << shift, MAX_BACKOFF_MS);
  }

  private captureFreezeFrame(): void {
    const v = this.video;
    if (!v.videoWidth || !v.videoHeight) return;
    try {
      this.freeze.width = v.videoWidth;
      this.freeze.height = v.videoHeight;
      const ctx = this.freeze.getContext("2d");
      if (!ctx) return;
      ctx.drawImage(v, 0, 0, v.videoWidth, v.videoHeight);
      v.classList.add("hidden");
    } catch (err) {
      // Never let a failed screenshot stop a rebuild.
      this.log("w", `Could not hold the last frame: ${String(err)}`);
    }
  }

  private clearFreezeFrame(): void {
    this.video.classList.remove("hidden");
  }

  private clearTimer(which: "tick" | "rebuild"): void {
    if (which === "tick" && this.tickTimer !== null) {
      window.clearInterval(this.tickTimer);
      this.tickTimer = null;
    }
    if (which === "rebuild" && this.rebuildTimer !== null) {
      window.clearTimeout(this.rebuildTimer);
      this.rebuildTimer = null;
    }
  }
}

function mediaErrorName(code: number): string {
  switch (code) {
    case 1:
      return "ABORTED";
    case 2:
      return "NETWORK";
    case 3:
      return "DECODE";
    case 4:
      return "SRC_NOT_SUPPORTED";
    default:
      return `UNKNOWN(${code})`;
  }
}
