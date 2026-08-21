//! Everything a screen remembers across restarts.
//!
//! A port of `data/Prefs.kt`. DataStore becomes a JSON file written temp-then-rename, so a
//! power cut in the middle of a write cannot leave a screen with a half-parsed identity.
//!
//! Note the two opposite null conventions, kept deliberately because the Android player
//! has them and the two clients must behave identically under the same server message:
//! [`ConfigStore::set_provisioning`] treats `None` as *leave alone*, while
//! [`ConfigStore::set_current_video`] treats `None` as *clear*.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::linfo;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    /// Stable and unique per machine. The server terminates any older socket claiming the
    /// same id, so two screens sharing one would kill each other in a reconnect loop.
    pub device_id: String,
    pub device_name: Option<String>,
    pub store_id: Option<String>,
    /// Bare `host:port` or a full URL. The control URL is derived, never stored.
    pub server: Option<String>,
    pub current_video_id: Option<String>,
    /// None means the screen is standalone.
    pub group_id: Option<String>,
    /// Persisted on purpose: `(now - origin) % duration` stays valid indefinitely, so a
    /// screen rejoins its group after a restart instead of drifting until the next assign.
    pub sync_origin_ms: Option<i64>,
}

impl Config {
    /// Where the control socket should connect.
    ///
    /// The scheme matters and is easy to get wrong: an `https://` server behind a tunnel
    /// needs `wss://`, and pointing `ws://` at it fails in a way that looks like a network
    /// fault rather than a configuration one.
    pub fn control_url(&self) -> Option<String> {
        let raw = self.server.as_deref()?.trim().trim_end_matches('/');
        if raw.is_empty() {
            return None;
        }
        let lower = raw.to_ascii_lowercase();
        // Slicing `raw`, not `lower`, so the host keeps whatever casing it was given.
        let url = if lower.starts_with("ws://") || lower.starts_with("wss://") {
            format!("{raw}/ws")
        } else if lower.starts_with("https://") {
            format!("wss://{}/ws", &raw[8..])
        } else if lower.starts_with("http://") {
            format!("ws://{}/ws", &raw[7..])
        } else {
            format!("ws://{raw}/ws")
        };
        Some(url)
    }

    pub fn display_name(&self) -> &str {
        self.device_name.as_deref().unwrap_or(&self.device_id)
    }
}

pub struct ConfigStore {
    path: PathBuf,
    inner: Mutex<Config>,
}

impl ConfigStore {
    /// Reads the config, minting a device id on first run.
    ///
    /// An unreadable or corrupt file is replaced rather than being allowed to stop the
    /// screen: a signage box with no config still has to boot and play what it has.
    pub fn load(dir: &Path) -> std::io::Result<Self> {
        let path = dir.join("config.json");
        let mut config = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<Config>(&text).unwrap_or_else(|err| {
                crate::logs::write(
                    'E',
                    &format!("config.json is unreadable ({err}) — starting fresh"),
                );
                Config::default()
            }),
            Err(_) => Config::default(),
        };

        if config.device_id.is_empty() {
            // `pc-` rather than the Android player's `tv-`, so the two are distinguishable
            // in the dashboard without a server change.
            config.device_id = format!("pc-{}", &uuid::Uuid::new_v4().simple().to_string()[..10]);
            linfo!("Minted device id {}", config.device_id);
        }

        let store = Self {
            path,
            inner: Mutex::new(config),
        };
        store.persist()?;
        Ok(store)
    }

    pub fn snapshot(&self) -> Config {
        self.lock().clone()
    }

    /// Applies provisioning. **`None` means leave the field alone** — there is no way to
    /// clear one, matching `Prefs.setProvisioning`.
    ///
    /// Returns true when something actually changed, so callers can avoid re-announcing
    /// over a no-op.
    pub fn set_provisioning(
        &self,
        server: Option<&str>,
        device_name: Option<&str>,
        store_id: Option<&str>,
    ) -> std::io::Result<bool> {
        let changed = {
            let mut config = self.lock();
            let before = config.clone();
            if let Some(value) = server {
                config.server = Some(value.to_string());
            }
            if let Some(value) = device_name {
                config.device_name = Some(value.to_string());
            }
            if let Some(value) = store_id {
                config.store_id = Some(value.to_string());
            }
            *config != before
        };
        if changed {
            self.persist()?;
        }
        Ok(changed)
    }

    /// Records what is playing. **`None` means clear**, unlike [`Self::set_provisioning`].
    pub fn set_current_video(
        &self,
        video_id: Option<&str>,
        group_id: Option<&str>,
        sync_origin_ms: Option<i64>,
    ) -> std::io::Result<()> {
        {
            let mut config = self.lock();
            config.current_video_id = video_id.map(str::to_string);
            config.group_id = group_id.map(str::to_string);
            config.sync_origin_ms = sync_origin_ms;
        }
        self.persist()
    }

    /// Temp file then rename, so a crash mid-write leaves the previous config intact
    /// rather than a truncated one.
    fn persist(&self) -> std::io::Result<()> {
        let config = self.snapshot();
        let text = serde_json::to_string_pretty(&config)?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &self.path)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Config> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_server(server: &str) -> Config {
        Config {
            server: Some(server.to_string()),
            ..Config::default()
        }
    }

    #[test]
    fn a_bare_host_and_port_gets_plain_ws() {
        assert_eq!(
            with_server("192.168.1.10:8080").control_url().as_deref(),
            Some("ws://192.168.1.10:8080/ws")
        );
    }

    #[test]
    fn https_becomes_wss() {
        assert_eq!(
            with_server("https://tunnel.example.com")
                .control_url()
                .as_deref(),
            Some("wss://tunnel.example.com/ws")
        );
    }

    #[test]
    fn http_becomes_ws() {
        assert_eq!(
            with_server("http://10.0.0.4:8080").control_url().as_deref(),
            Some("ws://10.0.0.4:8080/ws")
        );
    }

    #[test]
    fn an_explicit_websocket_scheme_is_left_alone() {
        assert_eq!(
            with_server("wss://tunnel.example.com")
                .control_url()
                .as_deref(),
            Some("wss://tunnel.example.com/ws")
        );
        assert_eq!(
            with_server("ws://10.0.0.4:8080").control_url().as_deref(),
            Some("ws://10.0.0.4:8080/ws")
        );
    }

    #[test]
    fn surrounding_whitespace_and_trailing_slashes_go() {
        assert_eq!(
            with_server("  http://10.0.0.4:8080/  ")
                .control_url()
                .as_deref(),
            Some("ws://10.0.0.4:8080/ws")
        );
    }

    #[test]
    fn the_scheme_match_is_case_insensitive_but_the_host_keeps_its_casing() {
        assert_eq!(
            with_server("HTTPS://Tunnel.Example.COM")
                .control_url()
                .as_deref(),
            Some("wss://Tunnel.Example.COM/ws")
        );
    }

    #[test]
    fn an_unprovisioned_screen_has_no_control_url() {
        assert_eq!(Config::default().control_url(), None);
        assert_eq!(with_server("   ").control_url(), None);
    }

    #[test]
    fn provisioning_leaves_omitted_fields_alone_but_current_video_clears_them() {
        let dir = std::env::temp_dir().join(format!("signage-cfg-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let store = ConfigStore::load(&dir).unwrap();

        store
            .set_provisioning(Some("10.0.0.4:8080"), Some("tv-entrada-01"), Some("710"))
            .unwrap();
        // Only the store moves; the name and the server must survive.
        store.set_provisioning(None, None, Some("704")).unwrap();

        let config = store.snapshot();
        assert_eq!(config.server.as_deref(), Some("10.0.0.4:8080"));
        assert_eq!(config.device_name.as_deref(), Some("tv-entrada-01"));
        assert_eq!(config.store_id.as_deref(), Some("704"));

        store
            .set_current_video(Some("v1"), Some("entrada"), Some(42))
            .unwrap();
        store.set_current_video(None, None, None).unwrap();
        let config = store.snapshot();
        assert_eq!(config.current_video_id, None);
        assert_eq!(config.group_id, None);
        assert_eq!(config.sync_origin_ms, None);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_device_id_is_minted_once_and_then_survives_reloads() {
        let dir = std::env::temp_dir().join(format!("signage-cfg-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let first = ConfigStore::load(&dir).unwrap().snapshot().device_id;
        let second = ConfigStore::load(&dir).unwrap().snapshot().device_id;

        assert!(first.starts_with("pc-"), "got {first}");
        assert_eq!(first.len(), 13);
        assert_eq!(first, second, "the id must not change on restart");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupt_config_is_replaced_rather_than_fatal() {
        let dir = std::env::temp_dir().join(format!("signage-cfg-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.json"), "{not json at all").unwrap();

        let store = ConfigStore::load(&dir).unwrap();
        assert!(store.snapshot().device_id.starts_with("pc-"));

        fs::remove_dir_all(&dir).ok();
    }
}
