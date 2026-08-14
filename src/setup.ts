/**
 * The provisioning screen — a port of `setup/SetupActivity.kt`.
 *
 * Three fields: server, store, screen name. It appears by itself when a screen has neither
 * a server nor anything to play, and can be opened deliberately with Ctrl+Shift+S to move
 * an already-provisioned screen to another store.
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
}

const FORCED_HINT =
  "Esta tela ainda não sabe a qual servidor e loja pertence. Informe o endereço do servidor e o número da loja.";
const OPTIONAL_HINT =
  "Alterar estes dados reconecta a tela e carrega a campanha da nova loja!.";

export class SetupScreen {
  private readonly root = document.querySelector<HTMLDivElement>("#setup")!;
  private readonly form =
    document.querySelector<HTMLFormElement>("#setup-form")!;
  private readonly hint =
    document.querySelector<HTMLParagraphElement>("#setup-hint")!;
  private readonly server =
    document.querySelector<HTMLInputElement>("#setup-server")!;
  private readonly store =
    document.querySelector<HTMLInputElement>("#setup-store")!;
  private readonly name =
    document.querySelector<HTMLInputElement>("#setup-name")!;
  private readonly error =
    document.querySelector<HTMLParagraphElement>("#setup-error")!;
  private readonly cancel =
    document.querySelector<HTMLButtonElement>("#setup-cancel")!;
  private readonly save =
    document.querySelector<HTMLButtonElement>("#setup-save")!;
  private readonly deviceId =
    document.querySelector<HTMLSpanElement>("#setup-device-id")!;

  /** False when the screen has nothing to fall back to, so there is nothing to cancel to. */
  private cancellable = false;

  constructor(
    private readonly log: Logger,
    private readonly onSaved: (config: ConfigView) => void,
  ) {
    this.form.addEventListener("submit", (event) => {
      event.preventDefault();
      void this.submit();
    });
    this.cancel.addEventListener("click", () => this.hide());
    this.form.addEventListener("keydown", (event) => {
      if (event.key === "Escape" && this.cancellable) {
        event.preventDefault();
        this.hide();
      }
    });
  }

  get visible(): boolean {
    return !this.root.hidden;
  }

  show(config: ConfigView, cancellable: boolean): void {
    this.cancellable = cancellable;
    this.hint.textContent = cancellable ? OPTIONAL_HINT : FORCED_HINT;
    this.cancel.hidden = !cancellable;
    this.showError(null);

    this.server.value = config.server ?? "";
    this.store.value = config.storeId ?? "";
    this.name.value = config.deviceName ?? "";
    this.deviceId.textContent = config.deviceId;

    this.root.hidden = false;
    document.body.classList.add("setup-open");

    // Land on the first thing that still needs typing, so an operator with only a
    // keyboard can fill this in without hunting.
    const firstEmpty = [this.server, this.store, this.name].find(
      (f) => f.value === "",
    );
    (firstEmpty ?? this.server).focus();
  }

  hide(): void {
    this.root.hidden = true;
    document.body.classList.remove("setup-open");
  }

  /**
   * Called when the config changed underneath us — a second launch with `--store`, or a
   * `configure` from the dashboard once Phase C lands.
   */
  refresh(config: ConfigView, needsProvisioning: boolean): void {
    if (needsProvisioning) {
      if (!this.visible) this.show(config, false);
      return;
    }
    if (this.visible && !this.cancellable) {
      // It was forced open because the screen was unprovisioned. It no longer is.
      this.log("i", "Provisioned elsewhere — closing setup");
      this.hide();
    }
  }

  private async submit(): Promise<void> {
    this.save.disabled = true;
    this.showError(null);
    try {
      const config = await invoke<ConfigView>("save_provisioning", {
        server: this.server.value,
        storeId: this.store.value,
        deviceName: this.name.value,
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
