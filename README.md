# Katuxa Signage — desktop player

A Windows digital-signage player for the same fleet as the Android TV player in
`../exo-player`. It speaks the same protocol to the same management server
(`../sinage-server`), so a desktop screen and a TV stick are interchangeable as far as the
dashboard is concerned.

Built with [Tauri 2](https://v2.tauri.app): Rust owns config, downloads and the control
channel; the webview owns the `<video>` element and nothing else.

## Status

**Phases A, B and C of four are done.** A screen now connects to the management server,
announces itself, takes its store's campaign, downloads and verifies it, and plays it on
loop — and answers `reload`, `identify`, `getLogs`, `configure` and `reboot` from the
dashboard.

What is left is Phase D: clock sync and group drift correction, so several screens can hold
a video wall in step, plus the nightly restart.

| Phase | Contents | State |
|---|---|---|
| A | kiosk shell, media store, loopback media server, looping playback + watchdog | **done** |
| B | `config.json`, setup screen, CLI provisioning | **done** |
| C | control channel, `assign`, resumable download + SHA-256, remote commands | **done** |
| D | time sync, group drift correction, nightly restart | not started |

## Running it during development

```bash
pnpm install
SIGNAGE_WINDOWED=1 pnpm tauri dev
```

`SIGNAGE_WINDOWED=1` is important on a development machine. Without it the window is
fullscreen, always on top, and refuses to close — which is correct in a store and
intolerable at a desk. It also skips registering the app for autostart.

Give it something to play by dropping an `.mp4` into the media directory:

| Platform | Media directory |
|---|---|
| Windows | `%APPDATA%\com.katuxa.signage\media` |
| macOS | `~/Library/Application Support/com.katuxa.signage/media` |

The newest file wins, and everything else in `media/` is deleted once the screen has
switched — retention is one file, as on Android.

The rolling log sits next to it in `logs/signage.log`, and carries both the Rust and the
webview side of the story.

## Provisioning a screen

A screen needs three things: a server address, a store number, and optionally a name. There
are two ways to give it them, and they write the same `config.json`.

**The setup screen**, which opens on **every launch**, prefilled from the last session, so
whoever is standing at the machine can see and change where it points without knowing a
shortcut. Ctrl+Shift+S opens it again later; Esc continues past it.

That would be fatal on its own — fifteen screens that come back from a power cut and then
sit on a form are fifteen screens showing nothing until somebody drives out. So when there
is something to fall back to, the form counts down and **continues by itself after ten
seconds**. Any key or click stops the countdown, because somebody is evidently there. A
screen with no server *and* nothing to play has nowhere to continue to, so that one waits
indefinitely — that case is still derived from state, recomputed whenever either changes,
never latched. (The Android player latched it once at boot and threw the setup form over a
playing video on every activity recreation. Do not reintroduce that.)

The form also carries the **kiosk** switch and the only visible way to quit — the window is
borderless, so it has no close button of its own.

**The command line**, which is the equivalent of `adb shell am start -S --es …`:

```bash
signage-desktop.exe --server 192.168.1.10:8080 --store 710 --name pc-entrada-01 --kiosk
```

Run against a screen that is already running, this re-provisions it in place:
`tauri-plugin-single-instance` forwards the arguments to the live instance and the second
process exits without opening a window. Omitted flags mean *leave alone*, so
`--store 704` on its own moves the store and keeps the server and name — the same
convention as `Prefs.setProvisioning`.

`--kiosk` / `--no-kiosk` is the same switch as the checkbox on the form, so a machine can be
locked down on its way into a shop without anybody hunting for it.

The two paths differ in one way, on purpose: the form requires a server and a store,
because a screen without a store can only ever be addressed individually and that defeats
the point of campaigns. The command line validates nothing, because a provisioning script
that wants to set only one field should be able to.

The device id is minted once, on first run, as `pc-` plus ten hex characters — `pc-` rather
than the Android player's `tv-` so the two are distinguishable in the dashboard with no
server change. It must stay stable: the server terminates any older socket claiming the
same id, so two screens sharing one would kill each other in a reconnect loop.

## The control channel

One supervisor task owns the connection to `<server>/ws`. It reads the address out of the
config each time round, dials, runs a session, and backs off before trying again — 1s, 1s,
2s, 4s, 8s, 16s, 32s, 60s, the ladder from `ControlSocket.kt`. A configuration change
interrupts the backoff, so re-provisioning a screen reconnects at once rather than waiting
out a minute to find out whether you typed the address correctly.

The scheme is derived, never stored: `https://…` becomes `wss://`, `http://` and a bare
`host:port` become `ws://`. Getting that wrong is the nastiest failure in the system,
because a screen pointed at `ws://` on an HTTPS tunnel fails in a way that looks like a
cabling problem.

On connect it sends `hello`, and every 30 seconds a `heartbeat` carrying what it is playing,
where it is in the file, and how much disk is left. It pings every 20 seconds and treats 60
seconds of silence as a dead connection — a half-open TCP socket never errors on its own.

An `assign` downloads the video with a resumable `Range` request, replays anything already
on disk through the SHA-256 hasher so a resumed transfer can still be verified whole, checks
the digest against what the server said, and only then moves the file into place. Progress
goes back every 10%. Failures are reported with the same codes as the Android player, and
`NETWORK_ERROR`/`HTTP_ERROR` are load-bearing: the server matches on them to print its
"PUBLIC_URL is unreachable from the players" diagnostic.

Two departures from the Android player, both deliberate:

- **Resumable failures are retried**, up to five times on the same backoff ladder. Android
  computes `resumable` and then never reads it, so a failed download there waits for the
  server to send another assign.
- **`reboot` restarts the app, not the PC.** Android reboots the stick, which it can do as
  device owner. Rebooting a shop's computer from a dashboard is a much bigger hammer than
  whoever clicked it is expecting.

Kept as-is even though it is arguably a bug, because the two clients must behave the same
under the same server message: a second `assign` arriving mid-download is **dropped, not
queued**.

## Tests

```bash
cd src-tauri && cargo test                        # media store, config, CLI, Range parsing
cd src-tauri && cargo clippy --all-targets -- -D warnings
cd src-tauri && cargo fmt --check
pnpm build                                        # typecheck + bundle
pnpm test                                         # frontend (sync maths arrive in Phase D)
```

CI runs exactly these, on Windows.

## Building the Windows installers

Tauri cannot cross-compile to Windows — the bundler needs the Windows toolchain, WiX and
NSIS, and `tauri-build` will not even `cargo check` for the target without a Windows
resource compiler. So `.github/workflows/windows.yml` is the only place the shipped binary
is produced.

| Trigger | What happens |
|---|---|
| push to `main`, or a pull request | tests only |
| **Actions → windows → Run workflow** | tests, then installers as a downloadable artifact |
| a `v*` tag | tests, installers, and a **draft** GitHub release with both attached |

The manual run is the one to reach for normally: no tag, no release, just a
`katuxa-signage-windows` artifact holding both installers, kept for 30 days. It takes a
`debug_bundle` option that skips the optimised build — faster, larger, and it keeps the
console window, which is useful the first time something misbehaves on real hardware.

Two installers come out on purpose: the NSIS `.exe` is the one to hand somebody, the `.msi`
is the one IT can push with Group Policy across fifteen stores.

Tagging checks that the tag agrees with `version` in both `Cargo.toml` and
`tauri.conf.json` and fails if it does not — Windows keys upgrades off that version, so an
installer that misreports it is worse than a failed build. Bump both, then tag.

**The installers are unsigned.** SmartScreen will warn on first run ("More info" → "Run
anyway"). Signing needs a code-signing certificate; until there is one, that warning is
part of the install.

**This repository has no GitHub remote yet**, so none of the above runs. Push it to GitHub
first — the workflow needs no secrets, only `contents: write`, which it declares itself.

## How this maps to the Android player

The two clients are deliberately the same program in two languages. The table is the map
between them; the constants (watchdog cadence, backoff ladders, retention rule) are copied
across rather than reinvented.

| Android | here |
|---|---|
| `PlayerController.kt` | `src/player.ts` |
| `data/MediaStore.kt` | `src-tauri/src/store.rs` |
| `data/LogStore.kt` | `src-tauri/src/logs.rs` |
| `data/Prefs.kt` | `src-tauri/src/config.rs` |
| `setup/SetupActivity.kt` | `src/setup.ts` + `src-tauri/src/cli.rs` |
| `PlayerService.kt` | `src-tauri/src/app.rs` (+ `socket.rs`, `download.rs` in Phase C) |
| lock task mode / device owner | fullscreen always-on-top window; close refused **only when kiosk mode is on**, which is off by default |
| HOME intent filter | `tauri-plugin-autostart` |
| `FLAG_KEEP_SCREEN_ON` | `SetThreadExecutionState`, `src-tauri/src/kiosk.rs` |
| `adb shell am start -S --es …` | a second launch, forwarded by `tauri-plugin-single-instance` |
| `adb` as the escape hatch | Ctrl+Shift+Q held for three seconds |

Two things are done differently on purpose, and both are Windows consequences:

**Media is served over `127.0.0.1`, not `asset://`.** Tauri's asset protocol reads a whole
file into memory when there is no `Range` header and has open bugs seeking large media.
`src-tauri/src/media_server.rs` streams instead, with the same Range semantics as the
management server's own `/media` route.

**Swapping a file is a handshake, not a rename.** Unix lets you delete a file that is still
open; Windows does not. So the backend hands the frontend a new file, waits for it to
confirm via `media_switched`, and only then deletes the old one — and tolerates the delete
failing, because a leftover video costs disk, not correctness.

## Verified

On macOS, against a real 10-second campaign video:

- Plays and loops for 75+ seconds with no watchdog rebuild — the watchdog is silent only
  while the position advances on every 5-second tick, so this is evidence of continuous
  playback across roughly seven loop points.
- The media server matches the management server's Range contract exactly: `200` with
  `Content-Length` for a plain GET, `206` with `Content-Range` for `bytes=N-` and
  `bytes=N-M`, `416` with `bytes */size` past the end, and a malformed `Range` ignored
  rather than rejected. Served bytes hash identically to the file on disk.
- Path traversal (`..%2f..%2f`), a wrong token, and a missing file all return 404.
- Newest-file-wins and retention: with two videos present, the newer one played and the
  older was deleted.
- Provisioning, end to end: a wiped screen opens the setup form by itself; a second launch
  with `--server --store --name` re-provisions the running instance and the form closes
  itself; `--store 704` alone moves the store and leaves the server and name intact; and a
  provisioned screen with a video on disk never opens the form at all.
- `config.json` survives a restart, keeps its device id, and is replaced rather than fatal
  when corrupt.
- The setup screen opens on every launch, prefilled, counts down, and continues on its own
  after ten seconds into playback.
- `--kiosk` and `--no-kiosk` toggle the lock on a running screen and the choice persists.

Against the real NestJS server, over an `https://` ngrok tunnel — so `wss://` and TLS were
exercised, not just plain `ws://` on a LAN:

- A freshly wiped screen connects, sends exactly **one** `hello`, receives **one** `assign`,
  downloads 4 MB, verifies the SHA-256, promotes and plays — reporting progress at every
  10%.
- The server lists it as `desktop-0.1.2`, online, in store 710, with the name it was given.
- Heartbeats carry real values: playing, position, free bytes, uptime.
- `identify` shows the name, `reload` rebuilds the player and **resumes at 7198 ms** rather
  than restarting the loop, `getLogs` puts 4489 bytes into the dashboard.
- `configure` from the dashboard moves the screen to another store, and it re-announces with
  the new store so that store's campaign follows it.

## Not verified

Everything Windows-specific, because it cannot be exercised from a Mac and Tauri cannot
cross-compile to Windows. The CI workflow exists now but has never run — the repository has
no remote — so this list is still entirely open, starting with whether the Windows-only
branch of `kiosk.rs` compiles at all:

- `SetThreadExecutionState` actually holding the display awake over 30+ minutes
- autostart at logon, and the app surviving a reboot
- Alt+F4 and the window chrome being refused
- the file-locking handshake, which on macOS is hidden by Unix rename semantics
- WebView2 absent on a fresh Windows image
- audible autoplay under `--autoplay-policy=no-user-gesture-required` (on macOS the player
  falls back to muted and says so in the log)

Also untested anywhere: a 12-hour soak, and whether the seam at the loop point is visible
on a real screen. The Android player has the same open question.

Untested on the control channel specifically: a genuinely interrupted download resuming
across a restart (the local tunnel completes in a second), a full disk, and anything
involving more than one screen. Group sync is Phase D and not written — a screen currently
ignores `groupId` and `startEpochMs` beyond storing them.

Two things verified only by reading, not by running. This machine grants the shell neither
screen-recording nor accessibility permission, so the UI cannot be seen or clicked from a
test:

- The setup screen's **appearance** — layout, contrast, focus order. Only its behaviour has
  been exercised: that it opens, counts down, validates, saves and closes when it should.
- **Closing.** That the window honours a close request with kiosk off, refuses it with
  kiosk on, and that the "Sair" button quits. The flag itself is unit-tested and the branch
  is eight lines, but nothing has actually pressed the button.
