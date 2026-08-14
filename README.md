# Katuxa Signage — desktop player

A Windows digital-signage player for the same fleet as the Android TV player in
`../exo-player`. It speaks the same protocol to the same management server
(`../sinage-server`), so a desktop screen and a TV stick are interchangeable as far as the
dashboard is concerned.

Built with [Tauri 2](https://v2.tauri.app): Rust owns config, downloads and the control
channel; the webview owns the `<video>` element and nothing else.

## Status

**Phase A of four is done.** What exists today: the kiosk shell, the local media store, the
loopback media server, and looping playback with a watchdog. It plays whatever `.mp4` is in
its media directory, forever.

| Phase | Contents | State |
|---|---|---|
| A | kiosk shell, media store, loopback media server, looping playback + watchdog | **done** |
| B | `config.json`, CLI provisioning, setup screen, promote handshake | not started |
| C | control channel, `assign`, resumable download + SHA-256, remote commands | not started |
| D | time sync, group drift correction, nightly restart, Windows CI | not started |

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

## Tests

```bash
cd src-tauri && cargo test     # media store, Range parsing, log rotation
pnpm test                      # frontend (arrives with the sync maths in Phase D)
```

## How this maps to the Android player

The two clients are deliberately the same program in two languages. The table is the map
between them; the constants (watchdog cadence, backoff ladders, retention rule) are copied
across rather than reinvented.

| Android | here |
|---|---|
| `PlayerController.kt` | `src/player.ts` |
| `data/MediaStore.kt` | `src-tauri/src/store.rs` |
| `data/LogStore.kt` | `src-tauri/src/logs.rs` |
| `PlayerService.kt` | `src-tauri/src/app.rs` (+ `socket.rs`, `download.rs` in Phase C) |
| lock task mode / device owner | fullscreen always-on-top window, close refused |
| HOME intent filter | `tauri-plugin-autostart` |
| `FLAG_KEEP_SCREEN_ON` | `SetThreadExecutionState`, `src-tauri/src/kiosk.rs` |
| `adb shell am start -S --es …` | a second launch, forwarded by `tauri-plugin-single-instance` (Phase B) |
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

## Not verified

Everything Windows-specific, because it cannot be exercised from a Mac and Tauri cannot
cross-compile to Windows. These are the Phase D CI job's first real job:

- `SetThreadExecutionState` actually holding the display awake over 30+ minutes
- autostart at logon, and the app surviving a reboot
- Alt+F4 and the window chrome being refused
- the file-locking handshake, which on macOS is hidden by Unix rename semantics
- WebView2 absent on a fresh Windows image
- audible autoplay under `--autoplay-policy=no-user-gesture-required` (on macOS the player
  falls back to muted and says so in the log)

Also untested anywhere: a 12-hour soak, and whether the seam at the loop point is visible
on a real screen. The Android player has the same open question.
