//! Keeping the display on.
//!
//! The desktop stand-in for Android's `FLAG_KEEP_SCREEN_ON`. The rest of that player's
//! device-owner machinery has no equivalent here and none is attempted: the window is
//! fullscreen and always-on-top, but it closes when asked. Locking a machine down to one
//! app is the OS's job — Assigned Access on Windows Enterprise — not this program's.

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
