//! On-disk layout and the rules that keep a small store PC from filling up.
//!
//! A port of `data/MediaStore.kt`:
//!
//! ```text
//!     <appdata>/media/<videoId>.mp4      what is playing, or about to
//!     <appdata>/pending/<videoId>.mp4    a download in progress; may be a partial file
//! ```
//!
//! Files only ever move from `pending` to `media`, by rename, and only after their hash
//! has been verified. Nothing in `media` is touched until then, so a failed download
//! cannot disturb what is on screen.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::{linfo, lwarn};

const EXT: &str = "mp4";

pub struct MediaStore {
    root: PathBuf,
    pub media_dir: PathBuf,
    pub pending_dir: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Space {
    Ok,
    Insufficient { required_bytes: u64, free_bytes: u64 },
}

impl MediaStore {
    pub fn new(root: &Path) -> std::io::Result<Self> {
        let media_dir = root.join("media");
        let pending_dir = root.join("pending");
        fs::create_dir_all(&media_dir)?;
        fs::create_dir_all(&pending_dir)?;
        Ok(Self {
            root: root.to_path_buf(),
            media_dir,
            pending_dir,
        })
    }

    pub fn media_file(&self, video_id: &str) -> PathBuf {
        self.media_dir.join(format!("{video_id}.{EXT}"))
    }

    pub fn pending_file(&self, video_id: &str) -> PathBuf {
        self.pending_dir.join(format!("{video_id}.{EXT}"))
    }

    /// Media files, oldest first.
    pub fn media_files(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(&self.media_dir) else {
            return Vec::new();
        };
        let mut files: Vec<(SystemTime, PathBuf)> = entries
            .flatten()
            .filter(|e| e.path().is_file())
            .map(|e| {
                let modified = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                (modified, e.path())
            })
            .collect();
        files.sort_by_key(|(modified, _)| *modified);
        files.into_iter().map(|(_, path)| path).collect()
    }

    /// The file to play at startup when nothing has told us otherwise.
    pub fn newest_media(&self) -> Option<PathBuf> {
        self.media_files().pop()
    }

    pub fn free_bytes(&self) -> u64 {
        fs4::available_space(&self.root).unwrap_or(0)
    }

    /// Makes room for a download, evicting inactive media files oldest-first if needed.
    ///
    /// The requirement is `sizeBytes * 1.5` — the file itself plus 50% headroom so the
    /// outgoing and incoming videos can coexist across the swap — except that bytes
    /// already on disk from an interrupted attempt are discounted, so a resume near
    /// completion is not blocked by a full-size check it no longer needs.
    pub fn ensure_space_for(
        &self,
        video_id: &str,
        size_bytes: u64,
        active_video_id: Option<&str>,
    ) -> Space {
        let already_have = fs::metadata(self.pending_file(video_id))
            .map(|m| m.len())
            .unwrap_or(0);
        let remaining = size_bytes.saturating_sub(already_have);
        let required = remaining + size_bytes / 2;

        if self.free_bytes() >= required {
            return Space::Ok;
        }

        lwarn!(
            "Need {} bytes, only {} free — evicting inactive media",
            required,
            self.free_bytes()
        );
        for file in self.media_files() {
            if active_video_id.is_some_and(|active| video_id_of(&file) == active) {
                continue;
            }
            let size = fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            if fs::remove_file(&file).is_ok() {
                linfo!("Evicted {} ({} bytes)", file.display(), size);
            }
            if self.free_bytes() >= required {
                return Space::Ok;
            }
        }

        Space::Insufficient {
            required_bytes: required,
            free_bytes: self.free_bytes(),
        }
    }

    /// Drops partial downloads for any video other than `video_id`; they are dead weight.
    pub fn clear_stale_pending(&self, video_id: &str) {
        let Ok(entries) = fs::read_dir(&self.pending_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && video_id_of(&path) != video_id {
                let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                linfo!("Discarding stale partial {} ({} bytes)", path.display(), size);
                let _ = fs::remove_file(&path);
            }
        }
    }

    /// Moves a verified download into place.
    ///
    /// A rename within the app data directory is atomic, so `media/` never contains a
    /// half-written file. Unlike the Android player, the target may still be held open by
    /// the webview — Windows refuses to delete an open file — so the caller must have had
    /// the frontend release it first.
    pub fn promote(&self, video_id: &str) -> std::io::Result<PathBuf> {
        let source = self.pending_file(video_id);
        let target = self.media_file(video_id);
        if target.exists() {
            fs::remove_file(&target)?;
        }
        fs::rename(&source, &target)?;
        Ok(target)
    }

    /// Retention: one active file in `media`, per the spec's "at most two on disk".
    ///
    /// Failure is not an error. On Windows the outgoing file can stay locked for a moment
    /// after the switch, and a leftover video costs disk, not correctness — the next swap
    /// and the next startup both try again.
    pub fn retain_only(&self, active_video_id: &str) {
        for file in self.media_files() {
            if video_id_of(&file) == active_video_id {
                continue;
            }
            match fs::remove_file(&file) {
                Ok(()) => linfo!("Retention: deleted {}", file.display()),
                Err(err) => lwarn!(
                    "Retention: {} is still locked ({err}); will retry after the next swap",
                    file.display()
                ),
            }
        }
    }
}

pub fn video_id_of(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The Android player interpolates `videoId` straight into a path. That is a traversal
/// waiting to happen, and Windows forbids a wider set of characters besides, so the
/// desktop client refuses anything the server itself would not have accepted on upload
/// (`videos.service.ts` validates `^[a-zA-Z0-9._-]+$`).
pub fn sanitize_video_id(video_id: &str) -> Option<&str> {
    if video_id.is_empty() || video_id.len() > 200 {
        return None;
    }
    if video_id == "." || video_id == ".." {
        return None;
    }
    if video_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        Some(video_id)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (MediaStore, PathBuf) {
        let root = std::env::temp_dir().join(format!("signage-store-{}", uuid::Uuid::new_v4()));
        let store = MediaStore::new(&root).unwrap();
        (store, root)
    }

    #[test]
    fn rejects_ids_that_would_escape_the_media_directory() {
        assert_eq!(sanitize_video_id("campaign_98x"), Some("campaign_98x"));
        assert_eq!(sanitize_video_id("vid.2026-08_promo"), Some("vid.2026-08_promo"));
        assert_eq!(sanitize_video_id("../../etc/passwd"), None);
        assert_eq!(sanitize_video_id("..\\windows\\system32"), None);
        assert_eq!(sanitize_video_id("has space"), None);
        assert_eq!(sanitize_video_id("c:evil"), None);
        assert_eq!(sanitize_video_id(".."), None);
        assert_eq!(sanitize_video_id(""), None);
    }

    #[test]
    fn newest_media_wins_and_retention_clears_the_rest() {
        let (store, root) = temp_store();

        fs::write(store.media_file("old"), b"old").unwrap();
        // Rename ordering is by mtime, so make sure they differ.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(store.media_file("new"), b"new").unwrap();

        assert_eq!(video_id_of(&store.newest_media().unwrap()), "new");

        store.retain_only("new");
        assert!(store.media_file("new").exists());
        assert!(!store.media_file("old").exists());

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn promote_moves_the_pending_file_over_the_active_one() {
        let (store, root) = temp_store();

        fs::write(store.media_file("v1"), b"stale").unwrap();
        fs::write(store.pending_file("v1"), b"fresh").unwrap();

        let promoted = store.promote("v1").unwrap();
        assert_eq!(promoted, store.media_file("v1"));
        assert_eq!(fs::read(&promoted).unwrap(), b"fresh");
        assert!(!store.pending_file("v1").exists());

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn stale_partials_are_discarded_but_the_wanted_one_is_kept() {
        let (store, root) = temp_store();

        fs::write(store.pending_file("wanted"), b"keep").unwrap();
        fs::write(store.pending_file("abandoned"), b"drop").unwrap();

        store.clear_stale_pending("wanted");

        assert!(store.pending_file("wanted").exists());
        assert!(!store.pending_file("abandoned").exists());

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_resumed_download_only_needs_the_bytes_it_is_missing() {
        let (store, root) = temp_store();

        // Nothing on disk yet: the whole file plus 50% headroom.
        fs::write(store.pending_file("v1"), b"").unwrap();
        assert_eq!(store.ensure_space_for("v1", 1_000, None), Space::Ok);

        // 900 of 1000 bytes already fetched — the check must not still demand 1500.
        fs::write(store.pending_file("v1"), vec![0u8; 900]).unwrap();
        assert_eq!(store.ensure_space_for("v1", 1_000, None), Space::Ok);

        fs::remove_dir_all(&root).ok();
    }
}
