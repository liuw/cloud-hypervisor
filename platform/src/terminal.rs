// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause
//
// Cross-platform terminal state management.
//
// Provides an opaque `TerminalState` type that can save and restore terminal
// settings. On Unix, this wraps `libc::termios`. On Windows, it wraps console
// mode flags.

use std::io;

/// Opaque terminal state that can be saved and restored.
#[derive(Debug, Clone)]
pub struct TerminalState {
    #[cfg(unix)]
    inner: libc::termios,
    #[cfg(target_os = "windows")]
    inner: u32, // CONSOLE_MODE value
}

/// Save the current terminal state for the given file descriptor (Unix) or
/// standard input handle (Windows).
#[cfg(unix)]
pub fn save_terminal_state(fd: i32) -> io::Result<TerminalState> {
    // SAFETY: zeroed termios is valid, tcgetattr writes into it.
    let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
    let ret = unsafe { libc::tcgetattr(fd, &mut termios) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(TerminalState { inner: termios })
}

#[cfg(target_os = "windows")]
pub fn save_terminal_state(_fd: i32) -> io::Result<TerminalState> {
    use windows::Win32::System::Console::{CONSOLE_MODE, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE};

    // SAFETY: Getting the standard input handle and its console mode.
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }
        .map_err(|e| io::Error::other(format!("{e}")))?;
    let mut mode = CONSOLE_MODE::default();
    // SAFETY: handle is valid, mode is a valid output pointer.
    unsafe {
        GetConsoleMode(handle, &mut mode)
            .map_err(|e| io::Error::other(format!("{e}")))?;
    }
    Ok(TerminalState { inner: mode.0 })
}

/// Restore a previously saved terminal state.
#[cfg(unix)]
pub fn restore_terminal_state(fd: i32, state: &TerminalState) -> io::Result<()> {
    // SAFETY: state.inner is a valid termios struct obtained from tcgetattr.
    let ret = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &state.inner) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn restore_terminal_state(_fd: i32, state: &TerminalState) -> io::Result<()> {
    use windows::Win32::System::Console::{CONSOLE_MODE, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode};

    // SAFETY: Restoring the console mode to a previously saved value.
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }
        .map_err(|e| io::Error::other(format!("{e}")))?;
    // SAFETY: handle is valid, mode is a valid saved value.
    unsafe {
        SetConsoleMode(handle, CONSOLE_MODE(state.inner))
            .map_err(|e| io::Error::other(format!("{e}")))?;
    }
    Ok(())
}

/// Set the terminal to raw mode for the given file descriptor.
#[cfg(unix)]
pub fn set_raw_mode(fd: i32) -> io::Result<()> {
    // SAFETY: zeroed termios is valid, tcgetattr writes into it, cfmakeraw
    // modifies it, tcsetattr applies it.
    let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
    let ret = unsafe { libc::tcgetattr(fd, &mut termios) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe { libc::cfmakeraw(&mut termios) };
    let ret = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn set_raw_mode(_fd: i32) -> io::Result<()> {
    use windows::Win32::System::Console::{
        CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
        SetConsoleMode,
    };

    // SAFETY: Getting the standard input handle and modifying its console mode.
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }
        .map_err(|e| io::Error::other(format!("{e}")))?;
    let mut mode = CONSOLE_MODE::default();
    // SAFETY: handle is valid, mode is a valid output pointer.
    unsafe {
        GetConsoleMode(handle, &mut mode)
            .map_err(|e| io::Error::other(format!("{e}")))?;
    }
    // Disable line input, echo, and processed input; enable VT input.
    let raw_mode = CONSOLE_MODE(
        (mode.0 & !(ENABLE_LINE_INPUT.0 | ENABLE_ECHO_INPUT.0 | ENABLE_PROCESSED_INPUT.0))
            | ENABLE_VIRTUAL_TERMINAL_INPUT.0,
    );
    // SAFETY: handle is valid, raw_mode is a valid console mode value.
    unsafe {
        SetConsoleMode(handle, raw_mode)
            .map_err(|e| io::Error::other(format!("{e}")))?;
    }
    Ok(())
}

/// Check if the given file descriptor is a terminal.
#[cfg(unix)]
pub fn is_terminal(fd: i32) -> bool {
    // SAFETY: isatty is always safe to call.
    unsafe { libc::isatty(fd) == 1 }
}

#[cfg(target_os = "windows")]
pub fn is_terminal(_fd: i32) -> bool {
    use windows::Win32::System::Console::{CONSOLE_MODE, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE};

    // SAFETY: Checking if stdin is a console.
    let Ok(handle) = (unsafe { GetStdHandle(STD_INPUT_HANDLE) }) else {
        return false;
    };
    let mut mode = CONSOLE_MODE::default();
    // SAFETY: handle is valid.
    unsafe { GetConsoleMode(handle, &mut mode) }.is_ok()
}
