//! Headless provisioning from the command line.
//!
//! This is the desktop equivalent of the Android player's
//! `adb shell am start -S -n …/.setup.SetupActivity --es server … --es store … --es name …`,
//! and it works the same way: launching the app a second time with these arguments
//! re-provisions the instance that is already running. `tauri-plugin-single-instance`
//! forwards the arguments and exits the second process.
//!
//! ```text
//!     signage-desktop.exe --server 192.168.1.10:8080 --store 710 --name tv-entrada-01
//! ```
//!
//! Omitted flags mean "leave alone", matching `Prefs.setProvisioning`. There is no way to
//! clear a field from here.

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Provisioning {
    pub server: Option<String>,
    pub store: Option<String>,
    pub name: Option<String>,
}

impl Provisioning {
    pub fn is_empty(&self) -> bool {
        self.server.is_none() && self.store.is_none() && self.name.is_none()
    }
}

/// Parses `--flag value` and `--flag=value`. Anything else is ignored — the binary is also
/// launched by the OS at logon with arguments we do not control.
pub fn parse<I, S>(args: I) -> Provisioning
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = Provisioning::default();
    let mut args = args.into_iter().map(|a| a.as_ref().to_string()).peekable();

    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag.to_string(), Some(value.to_string())),
            None => (arg, None),
        };

        let slot = match flag.as_str() {
            "--server" => &mut out.server,
            "--store" => &mut out.store,
            "--name" => &mut out.name,
            _ => continue,
        };

        let value = match inline {
            Some(value) => Some(value),
            // Only consume the next argument if it is not itself a flag, so
            // `--store --name x` does not silently set the store to "--name".
            None => match args.peek() {
                Some(next) if !next.starts_with("--") => args.next(),
                _ => None,
            },
        };

        if let Some(value) = value {
            let value = value.trim().to_string();
            if !value.is_empty() {
                *slot = Some(value);
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn separate_and_inline_forms_both_work() {
        let parsed = parse([
            "signage-desktop",
            "--server",
            "192.168.1.10:8080",
            "--store=710",
            "--name",
            "tv-entrada-01",
        ]);
        assert_eq!(
            parsed,
            Provisioning {
                server: Some("192.168.1.10:8080".into()),
                store: Some("710".into()),
                name: Some("tv-entrada-01".into()),
            }
        );
    }

    #[test]
    fn omitted_flags_stay_none_so_they_leave_the_field_alone() {
        let parsed = parse(["signage-desktop", "--store", "704"]);
        assert_eq!(parsed.store.as_deref(), Some("704"));
        assert_eq!(parsed.server, None);
        assert_eq!(parsed.name, None);
    }

    #[test]
    fn a_flag_with_no_value_does_not_swallow_the_next_flag() {
        let parsed = parse(["signage-desktop", "--store", "--name", "tv-01"]);
        assert_eq!(parsed.store, None, "--store had no value");
        assert_eq!(parsed.name.as_deref(), Some("tv-01"));
    }

    #[test]
    fn unknown_arguments_are_ignored() {
        // The OS launches this at logon; we do not control what it appends.
        let parsed = parse(["signage-desktop", "-psn_0_12345", "--verbose"]);
        assert!(parsed.is_empty());
    }

    #[test]
    fn blank_values_are_not_treated_as_provisioning() {
        let parsed = parse(["signage-desktop", "--server=  ", "--store", "  "]);
        assert!(parsed.is_empty());
    }
}
