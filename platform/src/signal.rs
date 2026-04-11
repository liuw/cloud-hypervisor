// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause
//
// Cross-platform signal/termination handler.
//
// Provides a high-level API for installing a termination handler that signals
// an EventFd when the process receives a termination request (SIGTERM/SIGINT
// on Unix, Ctrl+C/Close on Windows).

use std::io;
use std::sync::Arc;
use std::thread;

use crate::EventFd;

/// Install a termination handler that writes to `exit_evt` when the process
/// receives a termination signal.
///
/// On Unix, this handles SIGTERM and SIGINT via signal_hook.
/// On Windows, this handles CTRL_C_EVENT and CTRL_CLOSE_EVENT via
/// SetConsoleCtrlHandler.
///
/// Returns a join handle for the handler thread (Unix) or Ok(None) (Windows,
/// where the handler runs on a system thread).
#[cfg(unix)]
pub fn install_termination_handler(
    exit_evt: Arc<EventFd>,
) -> io::Result<Option<thread::JoinHandle<()>>> {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = Signals::new([SIGTERM, SIGINT])
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("signal setup: {e}")))?;

    let handle = thread::Builder::new()
        .name("signal-handler".to_string())
        .spawn(move || {
            for sig in signals.forever() {
                log::info!("Received signal {sig}, triggering exit");
                if let Err(e) = exit_evt.write(1) {
                    log::error!("Failed to signal exit event: {e}");
                }
            }
        })
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("spawn: {e}")))?;

    Ok(Some(handle))
}

#[cfg(target_os = "windows")]
pub fn install_termination_handler(
    exit_evt: Arc<EventFd>,
) -> io::Result<Option<thread::JoinHandle<()>>> {
    use std::sync::OnceLock;

    use windows::Win32::System::Console::SetConsoleCtrlHandler;

    use windows::core as windows_core;

    static EXIT_EVT: OnceLock<Arc<EventFd>> = OnceLock::new();
    EXIT_EVT
        .set(exit_evt)
        .map_err(|_| io::Error::new(io::ErrorKind::AlreadyExists, "handler already installed"))?;

    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    const CTRL_CLOSE_EVENT: u32 = 2;

    // SAFETY: SetConsoleCtrlHandler registers a callback for console control
    // events. The callback is safe to call from any thread.
    unsafe extern "system" fn handler(ctrl_type: u32) -> windows_core::BOOL {
        if ctrl_type == CTRL_C_EVENT
            || ctrl_type == CTRL_CLOSE_EVENT
            || ctrl_type == CTRL_BREAK_EVENT
        {
            log::info!("Received console control event {ctrl_type}, triggering exit");
            if let Some(evt) = EXIT_EVT.get() {
                let _ = evt.write(1);
            }
            return true.into();
        }
        false.into()
    }

    // SAFETY: Registering a valid handler function.
    unsafe {
        SetConsoleCtrlHandler(Some(handler), true).map_err(|e| {
            io::Error::other(format!("SetConsoleCtrlHandler: {e}"))
        })?;
    }

    Ok(None)
}
