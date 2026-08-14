/**
 * The provisioning screen — a port of `setup/SetupActivity.kt`.
 *
 * Server, store, screen name, and whether the screen may be closed. It opens on every
 * launch, prefilled from the last session, so whoever is standing at the machine can see
 * and change where it points without knowing a shortcut. Ctrl+Shift+S opens it again later.
 *
 * When there is something to fall back to it counts itself down and continues — a screen
 * coming back from a power cut at 04:00 must not sit here waiting for a human. Any key or
 * click stops the countdown.
 *
 * Validation lives in Rust so the command line and this form cannot drift apart; this file
 * only shows what comes back.
 */
import { invoke } from "@tauri-apps/api/core";
import type { Logger } from "./player";

export interface ConfigView {
  deviceId: string;
  deviceName: string | null;
  storeId: string | null;
  server: string | null;
  kiosk: boolean;
}

export interface ShowOptions {
  /** False when the screen has nothing to fall back to, so there is nothing to continue to. */
  cancellable: boolean;
  /** Continue on its own after this many milliseconds. Null waits indefinitely. */
  autoContinueMs?: number | null;
}

const FORCED_HINT =
  "Esta tela ainda não sabe a qual servidor e loja pertence. Informe o endereço do servidor e o número da loja.";
const OPTIONAL_HINT =
  "Confira para onde esta tela aponta. Alterar estes dados reconecta a tela e carrega a campanha da nova loja.";

export class SetupScreen {
  private readonly root = document.querySelector<HTMLDivElement>("#setup")!;
  private readonly form = document.querySelector<HTMLFormElement>("#setup-form")!;
  private readonly hint = document.querySelector<HTMLParagraphElement>("#setup-hint")!;
  private readonly server = document.querySelector<HTMLInputElement>("#setup-server")!;
  private readonly store = document.querySelector<HTMLInputElement>("#setup-store")!;
  private readonly name = document.querySelector<HTMLInputElement>("#setup-name")!;
  private readonly kiosk = document.querySelector<HTMLInputElement>("#setup-kiosk")!;
  private readonly error = document.querySelector<HTMLParagraphElement>("#setup-error")!;
  private readonly cancel = document.querySelector<HTMLButtonElement>("#setup-cancel")!;
  private readonly save = document.querySelector<HTMLButtonElement>("#setup-save")!;
  private readonly quit = document.querySelector<HTMLButtonElement>("#setup-quit")!;
  private readonly deviceId = document.querySelector<HTMLSpanElement>("#setup-device-id")!;

  private cancellable = false;

  private countdownTimer: number | null = null;
  private countdownEndsAt = 0;

  constructor(
    private readonly log: Logger,
    private readonly onSaved: (config: ConfigView) => void,
    private readonly onContinue: () => void,
  ) {
    this.form.addEventListener("submit", (event) => {
      event.preventDefault();
      void this.submit();
    });
    this.cancel.addEventListener("click", () => this.continue());
    // The only visible way out: the window is borderless, so it has no close button, and
    // Ctrl+Shift+Q is not something anybody discovers by looking.
    this.quit.addEventListener("click", () => {
      this.log("i", "Quit from the setup screen");
      void invoke("request_quit").catch(() => undefined);
    });
    this.form.addEventListener("keydown", (event) => {
      if (event.key === "Escape" && this.cancellable) {
        event.preventDefault();
        this.continue();
      }
    });

    // Somebody is here and typing. Whatever the countdown was going to do, they are the
    // ones deciding now.
    const interrupt = () => this.stopCountdown();
    this.root.addEventListener("keydown", interrupt);
    this.root.addEventListener("pointerdown", interrupt);
  }

  get visible(): boolean {
    return !this.root.hidden;
  }

  /** Open because the screen had nowhere else to go, rather than because somebody asked. */
  get forced(): boolean {
    return this.visible && !this.cancellable;
  }

  show(config: ConfigView, options: ShowOptions): void {
    this.cancellable = options.cancellable;
    this.hint.textContent = options.cancellable ? OPTIONAL_HINT : FORCED_HINT;
    this.cancel.hidden = !options.cancellable;
    this.showError(null);

    this.server.value = config.server ?? "";
    this.store.value = config.storeId ?? "";
    this.name.value = config.deviceName ?? "";
    this.kiosk.checked = config.kiosk;
    this.deviceId.textContent = config.deviceId;

    this.root.hidden = false;
    document.body.classList.add("setup-open");

    // Land on the first thing that still needs typing, so an operator with only a
    // keyboard can fill this in without hunting.
    const firstEmpty = [this.server, this.store, this.name].find((f) => f.value === "");
    (firstEmpty ?? this.server).focus();

    this.stopCountdown();
    if (options.cancellable && options.autoContinueMs) {
      this.startCountdown(options.autoContinueMs);
    }
  }

  hide(): void {
    this.stopCountdown();
    this.root.hidden = true;
    document.body.classList.remove("setup-open");
  }

  /**
   * Called when the config changed underneath us — a second launch with `--store`, or a
   * `configure` from the dashboard once Phase C lands.
   */
  refresh(config: ConfigView, needsProvisioning: boolean): void {
    if (needsProvisioning) {
      if (!this.visible) this.show(config, { cancellable: false });
      return;
    }
    if (this.visible && !this.cancellable) {
      // It was forced open because the screen was unprovisioned. It no longer is.
      this.log("i", "Provisioned elsewhere — closing setup");
      this.hide();
    }
  }

  // ------------------------------------------------------------- countdown

  private startCountdown(ms: number): void {
    this.countdownEndsAt = Date.now() + ms;
    this.paintCountdown();
    this.countdownTimer = window.setInterval(() => {
      if (Date.now() >= this.countdownEndsAt) {
        this.log("i", "Setup timed out — continuing with the stored settings");
        this.continue();
        return;
      }
      this.paintCountdown();
    }, 250);
  }

  private paintCountdown(): void {
    const left = Math.max(0, Math.ceil((this.countdownEndsAt - Date.now()) / 1000));
    this.cancel.textContent = `Continuar (${left})`;
  }

  private stopCountdown(): void {
    if (this.countdownTimer === null) return;
    window.clearInterval(this.countdownTimer);
    this.countdownTimer = null;
    this.cancel.textContent = "Continuar";
  }

  private continue(): void {
    this.hide();
    this.onContinue();
  }

  private async submit(): Promise<void> {
    this.stopCountdown();
    this.save.disabled = true;
    this.showError(null);
    try {
      const config = await invoke<ConfigView>("save_provisioning", {
        server: this.server.value,
        storeId: this.store.value,
        deviceName: this.name.value,
        kiosk: this.kiosk.checked,
      });
      this.hide();
      this.onSaved(config);
    } catch (err) {
      // Rust returns a human-readable string for the validation failures.
      this.showError(String(err));
    } finally {
      this.save.disabled = false;
    }
  }

  private showError(message: string | null): void {
    if (message === null) {
      this.error.hidden = true;
      this.error.textContent = "";
      return;
    }
    this.error.textContent = message;
    this.error.hidden = false;
  }
}
