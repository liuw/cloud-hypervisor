// Copyright © 2024 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0
//
// Windows serial manager: reads from stdin and feeds bytes to the serial device.
//
// This is the Windows equivalent of serial_manager.rs. On Unix, the serial
// manager uses epoll to multiplex stdin/PTY/socket inputs. On Windows, we
// use a simple reader thread on stdin with an EventFd for kill signaling.

use std::io::Read;
use std::sync::{Arc, Mutex};
use std::{io, result, thread};

use devices::legacy::Serial;
use log::{error, info};
use platform::{EFD_NONBLOCK, EventFd};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    /// Cannot create EventFd.
    #[error("Error creating EventFd")]
    EventFd(#[source] io::Error),

    /// Cannot spawn SerialManager thread.
    #[error("Error spawning SerialManager thread")]
    SpawnSerialManager(#[source] io::Error),
}
pub type Result<T> = result::Result<T, Error>;

/// Manages serial console input on Windows.
///
/// Spawns a reader thread that reads from stdin and queues bytes to the
/// serial device. The thread is stopped via `kill_evt` on drop.
pub struct SerialManager {
    serial: Arc<Mutex<Serial>>,
    kill_evt: EventFd,
    handle: Option<thread::JoinHandle<()>>,
}

impl SerialManager {
    /// Create a new serial manager for the given serial device.
    ///
    /// Returns `None` if stdin is not a terminal (e.g., redirected from a file).
    pub fn new(serial: Arc<Mutex<Serial>>) -> Result<Option<Self>> {
        if !platform::is_terminal(0) {
            return Ok(None);
        }

        let kill_evt = EventFd::new(EFD_NONBLOCK).map_err(Error::EventFd)?;

        Ok(Some(SerialManager {
            serial,
            kill_evt,
            handle: None,
        }))
    }

    /// Start the stdin reader thread.
    pub fn start_thread(&mut self, exit_evt: EventFd) -> Result<()> {
        if self.handle.is_some() {
            return Ok(());
        }

        let serial = self.serial.clone();
        let kill_evt = self.kill_evt.try_clone().map_err(Error::EventFd)?;

        let thread = thread::Builder::new()
            .name("serial-manager".to_string())
            .spawn(move || {
                let stdin = std::io::stdin();
                let mut buf = [0u8; 64];

                loop {
                    // Check kill signal (non-blocking)
                    if kill_evt.read().is_ok() {
                        info!("KILL_EVENT received, stopping serial manager");
                        return;
                    }

                    // Read from stdin (blocking — will wake when input arrives)
                    match stdin.lock().read(&mut buf) {
                        Ok(0) => {
                            info!("stdin EOF, stopping serial manager");
                            return;
                        }
                        Ok(count) => {
                            // Replace "\n" with "\r" to match Unix behavior (#1170)
                            if count == 1 && buf[0] == 0x0a {
                                buf[0] = 0x0d;
                            }

                            if let Err(e) = serial.lock().unwrap().queue_input_bytes(&buf[..count])
                            {
                                error!("Error queuing serial input: {e}");
                            }
                        }
                        Err(e) => {
                            if e.kind() == io::ErrorKind::Interrupted {
                                continue;
                            }
                            error!("Error reading stdin: {e}");
                            exit_evt.write(1).ok();
                            return;
                        }
                    }
                }
            })
            .map_err(Error::SpawnSerialManager)?;

        self.handle = Some(thread);
        Ok(())
    }
}

impl Drop for SerialManager {
    fn drop(&mut self) {
        self.kill_evt.write(1).ok();
        if let Some(handle) = self.handle.take() {
            handle.join().ok();
        }
    }
}
