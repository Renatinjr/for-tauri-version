//! The desktop stand-ins for Android's device-owner machinery.
//!
//! The Android player relied on lock task mode, a HOME intent filter and
//! `FLAG_KEEP_SCREEN_ON`. None of those exist here. What replaces them:
//!
//! | Android | here |
//! |---|---|
//! | lock task mode | a borderless, always-on-top fullscreen window whose close request is refused |
//! | HOME intent filter | `tauri-plugin-autostart`, which writes the `Run` key |
//! | `FLAG_KEEP_SCREEN_ON` | [`keep_awake`] |
//! | `adb` as the escape hatch | Ctrl+Shift+Q held for three seconds |
//!
//! A true equivalent of device owner — Shell Launcher or Assigned Access, where the app
//! replaces Explorer — needs Windows Enterprise and an IT policy. It is documented in the
//! README as the optional extra step, not attempted here.

/// Stops Windows blanking the display or sleeping the machine.
///
/// `SetThreadExecutionState` applies to the calling thread and lasts until that thread
/// resets it or exits, so the request is made from a thread that then parks forever. A
/// store screen that goes dark at 22:00 because nobody moved a mouse is the single most
/// visible way this app can fail.
pub fn keep_awake() {
    #[cfg(windows)]
    {
        use windows::Win32::System::Power::{
            SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
        };

        let spawned = std::thread::Builder::new()
            .name("keep-awake".into())
            .spawn(|| {
                // SAFETY: a plain flag-setting Win32 call with no pointers involved.
                let previous = unsafe {
                    SetThreadExecutionState(
                        ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED,
                    )
                };
                if previous.0 == 0 {
                    crate::logs::write('W', "SetThreadExecutionState was refused");
                } else {
                    crate::logs::write('I', "Display sleep and system sleep inhibited");
                }
                loop {
                    std::thread::park();
                }
            });

        if let Err(err) = spawned {
            crate::logs::write('E', &format!("Could not hold the display awake: {err}"));
        }
    }

    #[cfg(not(windows))]
    {
        // macOS is the development machine, not a target. Letting it sleep is correct.
        crate::logs::write('I', "Not Windows — leaving display sleep alone");
    }
}
