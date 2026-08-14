//! The wire protocol, mirroring `data/Messages.kt` on the Android side and
//! `src/common/protocol.ts` on the server.
//!
//! Three conventions from the Kotlin serializer have to survive the port exactly, because
//! the same server talks to both clients:
//!
//! - `classDiscriminator = "type"` — the tag is an inline `"type"` key, not a wrapper.
//! - Variant and field names are **camelCase** (`timeResponse`, `downloadProgress`).
//! - `explicitNulls = false` — a null field is **omitted**, never written as `null`.
//!   Hence `skip_serializing_if` on every `Option` going out.
//!
//! Unknown keys are ignored on the way in, and an unknown `type` parses to
//! [`ServerMessage::Unknown`] rather than failing, so the server can add messages without
//! taking the fleet down.

use serde::{Deserialize, Serialize};

/// Server → player.
#[derive(Debug, Clone, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ServerMessage {
    /// Make this video the active one.
    Assign {
        video_id: String,
        url: String,
        sha256: String,
        size_bytes: u64,
        /// Null when the screen is standalone.
        #[serde(default)]
        group_id: Option<String>,
        /// Null unless the group is synchronised. Honoured in Phase D.
        #[serde(default)]
        start_epoch_ms: Option<i64>,
    },
    /// Rebuild the player from the current file — a remote "try turning it off and on".
    Reload,
    /// Put the device name on screen so a human can find screen 11 in a store.
    Identify,
    Reboot,
    GetLogs,
    /// Re-provisions a screen from the dashboard. Omitted fields mean "leave alone".
    Configure {
        #[serde(default)]
        store_id: Option<String>,
        #[serde(default)]
        device_name: Option<String>,
    },
    /// Answer to a [`PlayerMessage::TimeRequest`]; `client_send_ms` is echoed untouched.
    TimeResponse {
        client_send_ms: i64,
        server_ms: i64,
    },
    #[serde(other)]
    Unknown,
}

/// Player → server.
#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PlayerMessage {
    Hello {
        device_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        store_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        device_name: Option<String>,
        app_version: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        current_video_id: Option<String>,
    },
    Heartbeat {
        device_id: String,
        /// Repeated from the hello so a dashboard that missed it can still group screens.
        #[serde(skip_serializing_if = "Option::is_none")]
        store_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        current_video_id: Option<String>,
        playing: bool,
        position_ms: i64,
        free_bytes: u64,
        app_version: String,
        uptime_ms: u64,
    },
    DownloadProgress {
        video_id: String,
        percent: u8,
    },
    Error {
        code: String,
        detail: String,
    },
    #[allow(dead_code)] // Phase D: the clock probe.
    TimeRequest {
        client_send_ms: i64,
    },
    Logs {
        device_id: String,
        /// Tail of the rotating log; whole files are too big for one frame.
        text: String,
        truncated: bool,
    },
}

/// Sent in `hello` and every `heartbeat`.
///
/// The server stores this verbatim on the device document and does not otherwise know what
/// platform a screen is, so the `desktop-` prefix is what distinguishes these from the
/// Android screens in the dashboard — with no server change.
pub fn app_version() -> String {
    format!("desktop-{}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_omits_null_fields_rather_than_writing_them() {
        let hello = PlayerMessage::Hello {
            device_id: "pc-abc123".into(),
            store_id: None,
            device_name: None,
            app_version: "desktop-0.1.2".into(),
            current_video_id: None,
        };
        assert_eq!(
            serde_json::to_string(&hello).unwrap(),
            r#"{"type":"hello","deviceId":"pc-abc123","appVersion":"desktop-0.1.2"}"#
        );
    }

    #[test]
    fn a_full_heartbeat_matches_the_kotlin_shape() {
        let beat = PlayerMessage::Heartbeat {
            device_id: "pc-abc123".into(),
            store_id: Some("710".into()),
            current_video_id: Some("vid_2026_08_promo".into()),
            playing: true,
            position_ms: 14_300,
            free_bytes: 2_147_483_648,
            app_version: "desktop-0.1.2".into(),
            uptime_ms: 918_000,
        };
        assert_eq!(
            serde_json::to_string(&beat).unwrap(),
            r#"{"type":"heartbeat","deviceId":"pc-abc123","storeId":"710","currentVideoId":"vid_2026_08_promo","playing":true,"positionMs":14300,"freeBytes":2147483648,"appVersion":"desktop-0.1.2","uptimeMs":918000}"#
        );
    }

    #[test]
    fn small_messages_are_byte_for_byte_what_the_android_tests_pin() {
        assert_eq!(
            serde_json::to_string(&PlayerMessage::DownloadProgress {
                video_id: "v1".into(),
                percent: 42,
            })
            .unwrap(),
            r#"{"type":"downloadProgress","videoId":"v1","percent":42}"#
        );
        assert_eq!(
            serde_json::to_string(&PlayerMessage::Error {
                code: "HASH_MISMATCH".into(),
                detail: "bad".into(),
            })
            .unwrap(),
            r#"{"type":"error","code":"HASH_MISMATCH","detail":"bad"}"#
        );
        assert_eq!(
            serde_json::to_string(&PlayerMessage::TimeRequest {
                client_send_ms: 1000
            })
            .unwrap(),
            r#"{"type":"timeRequest","clientSendMs":1000}"#
        );
    }

    #[test]
    fn assign_parses_and_tolerates_fields_we_do_not_know() {
        let json = r#"{"type":"assign","videoId":"v1","url":"http://h/media/v1.mp4",
            "sha256":"abc","sizeBytes":123,"playlist":["a","b"],"somethingNew":true}"#;
        let ServerMessage::Assign {
            video_id,
            size_bytes,
            group_id,
            start_epoch_ms,
            ..
        } = serde_json::from_str(json).unwrap()
        else {
            panic!("expected an assign");
        };
        assert_eq!(video_id, "v1");
        assert_eq!(size_bytes, 123);
        assert_eq!(group_id, None, "absent is the same as null");
        assert_eq!(start_epoch_ms, None);
    }

    #[test]
    fn bare_commands_are_just_the_discriminator() {
        assert!(matches!(
            serde_json::from_str::<ServerMessage>(r#"{"type":"reload"}"#).unwrap(),
            ServerMessage::Reload
        ));
        assert!(matches!(
            serde_json::from_str::<ServerMessage>(r#"{"type":"getLogs"}"#).unwrap(),
            ServerMessage::GetLogs
        ));
    }

    #[test]
    fn configure_distinguishes_omitted_from_present() {
        let ServerMessage::Configure {
            store_id,
            device_name,
        } = serde_json::from_str(r#"{"type":"configure","storeId":"704"}"#).unwrap()
        else {
            panic!("expected a configure");
        };
        assert_eq!(store_id.as_deref(), Some("704"));
        assert_eq!(device_name, None, "omitted means leave the name alone");
    }

    #[test]
    fn an_unknown_message_type_does_not_fail_the_parse() {
        // The server must be able to add messages without taking the fleet offline.
        assert!(matches!(
            serde_json::from_str::<ServerMessage>(r#"{"type":"somethingFromTheFuture"}"#).unwrap(),
            ServerMessage::Unknown
        ));
    }
}
