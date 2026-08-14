//! Rotating log file, plus the macros everything else logs through.
//!
//! A port of `data/LogStore.kt`. Two files of 2.5 MB, so `getLogs` from the dashboard has
//! roughly the last 5 MB to draw on, and a screen that has been up for months does not
//! quietly fill its disk with its own diagnostics.
//!
//! Writes go through a dedicated thread. Nothing that logs should ever block on disk —
//! least of all the thread driving playback.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;
use std::thread;

/// Two of these is the spec's "last ~5MB".
const MAX_FILE_BYTES: u64 = 2_500_000;

/// One frame's worth; enough to see what went wrong without flooding the socket.
pub const DEFAULT_TAIL_BYTES: usize = 64 * 1024;

static SINK: OnceLock<Sink> = OnceLock::new();

struct Sink {
    tx: Sender<String>,
    dir: PathBuf,
}

/// Installs the log file. Safe to call twice; the second call is ignored.
pub fn init(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let (tx, rx) = mpsc::channel::<String>();
    let writer_dir = dir.to_path_buf();

    thread::Builder::new()
        .name("log-writer".into())
        .spawn(move || {
            for line in rx {
                if let Err(err) = append(&writer_dir, &line) {
                    // Nowhere left to report this but the console.
                    eprintln!("log write failed: {err}");
                }
            }
        })?;

    let _ = SINK.set(Sink {
        tx,
        dir: dir.to_path_buf(),
    });
    Ok(())
}

fn current(dir: &Path) -> PathBuf {
    dir.join("signage.log")
}

fn previous(dir: &Path) -> PathBuf {
    dir.join("signage.1.log")
}

fn append(dir: &Path, line: &str) -> std::io::Result<()> {
    rotate_if_needed(dir)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(current(dir))?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")
}

fn rotate_if_needed(dir: &Path) -> std::io::Result<()> {
    let path = current(dir);
    let size = match fs::metadata(&path) {
        Ok(meta) => meta.len(),
        Err(_) => return Ok(()),
    };
    if size < MAX_FILE_BYTES {
        return Ok(());
    }
    let prev = previous(dir);
    let _ = fs::remove_file(&prev);
    fs::rename(&path, &prev)
}

/// Called by the macros. Formatting matches the Android player line for line, so the two
/// platforms' logs read the same in the dashboard.
pub fn write(level: char, message: &str) {
    let stamp = chrono::Local::now().format("%m-%d %H:%M:%S%.3f");
    let line = format!("{stamp} {level} {message}");

    if cfg!(debug_assertions) {
        eprintln!("{line}");
    }

    if let Some(sink) = SINK.get() {
        // A full channel or a dead writer must not take the app down.
        let _ = sink.tx.send(line);
    } else if !cfg!(debug_assertions) {
        eprintln!("{line}");
    }
}

/// The tail of the rolling log, for the `getLogs` command.
///
/// Returns `(text, truncated)`. The cut is made at the first newline at or after the
/// budget, so the reply never opens mid-line.
pub fn tail(max_bytes: usize) -> (String, bool) {
    let Some(sink) = SINK.get() else {
        return (String::new(), false);
    };
    let mut text = read_lossy(&previous(&sink.dir));
    text.push_str(&read_lossy(&current(&sink.dir)));

    if text.len() <= max_bytes {
        return (text, false);
    }
    let start = text.len() - max_bytes;
    let cut = text[start..]
        .find('\n')
        .map(|i| start + i + 1)
        .unwrap_or(start);
    (text[cut..].to_string(), true)
}

fn read_lossy(path: &Path) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    // Only ever read the last chunk we could possibly need; these files are megabytes.
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let want = MAX_FILE_BYTES.min(len);
    if file.seek(SeekFrom::End(-(want as i64))).is_err() {
        return String::new();
    }
    let mut buf = Vec::with_capacity(want as usize);
    if file.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[macro_export]
macro_rules! linfo {
    ($($arg:tt)*) => { $crate::logs::write('I', &format!($($arg)*)) };
}

#[macro_export]
macro_rules! lwarn {
    ($($arg:tt)*) => { $crate::logs::write('W', &format!($($arg)*)) };
}

#[macro_export]
macro_rules! lerror {
    ($($arg:tt)*) => { $crate::logs::write('E', &format!($($arg)*)) };
}

#[macro_export]
macro_rules! ldebug {
    ($($arg:tt)*) => { $crate::logs::write('D', &format!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_cuts_at_a_line_boundary() {
        // Reproduce the cutting rule directly; the sink is a process-wide singleton and
        // installing it from a test would race the other tests.
        let text = "first line\nsecond line\nthird line\n";
        let max_bytes = 15;
        let start = text.len() - max_bytes;
        let cut = text[start..]
            .find('\n')
            .map(|i| start + i + 1)
            .unwrap_or(start);
        assert_eq!(&text[cut..], "third line\n");
    }

    #[test]
    fn rotation_moves_current_to_previous() {
        let dir = std::env::temp_dir().join(format!("signage-logs-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        fs::write(current(&dir), vec![b'x'; MAX_FILE_BYTES as usize + 1]).unwrap();
        rotate_if_needed(&dir).unwrap();

        assert!(!current(&dir).exists(), "current should have been renamed");
        assert!(previous(&dir).exists(), "previous should now hold the old file");

        fs::remove_dir_all(&dir).ok();
    }
}
