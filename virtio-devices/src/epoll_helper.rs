// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.
//
// Copyright © 2020 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};
#[cfg(target_os = "windows")]
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use log::info;
use thiserror::Error;
use platform::{EventFd, EventPoll, PollEvent};

pub struct EpollHelper {
    pause_evt: EventFd,
    poll: EventPoll,
}

#[derive(Error, Debug)]
pub enum EpollHelperError {
    #[error("Failed to create Fd")]
    CreateFd(#[source] std::io::Error),
    #[error("Failed to epoll_ctl")]
    Ctl(#[source] std::io::Error),
    #[error("IO error")]
    IoError(#[source] std::io::Error),
    #[error("Failed to epoll_wait")]
    Wait(#[source] std::io::Error),
    #[error("Failed to get virtio-queue index")]
    QueueRingIndex(#[source] virtio_queue::Error),
    #[error("Failed to handle virtio device events")]
    HandleEvent(#[source] anyhow::Error),
    #[error("Failed to handle timeout")]
    HandleTimeout(#[source] anyhow::Error),
}

pub const EPOLL_HELPER_EVENT_PAUSE: u16 = 0;
pub const EPOLL_HELPER_EVENT_KILL: u16 = 1;
pub const EPOLL_HELPER_EVENT_LAST: u16 = 15;

pub trait EpollHelperHandler {
    // Handle one event at a time.
    fn handle_event(
        &mut self,
        helper: &mut EpollHelper,
        event: &PollEvent,
    ) -> Result<(), EpollHelperError>;

    // Called when epoll_wait times out (only if timeout != -1).
    fn handle_timeout(&mut self, _helper: &mut EpollHelper) -> Result<(), EpollHelperError> {
        Ok(())
    }

    // Called with the full list of events before individual dispatch.
    fn event_list(
        &mut self,
        _helper: &mut EpollHelper,
        _events: &[PollEvent],
    ) -> Result<(), EpollHelperError> {
        Ok(())
    }
}

impl EpollHelper {
    pub fn new(
        kill_evt: &EventFd,
        pause_evt: &EventFd,
    ) -> std::result::Result<Self, EpollHelperError> {
        let mut poll = EventPoll::new().map_err(EpollHelperError::CreateFd)?;

        let mut helper = Self {
            pause_evt: pause_evt.try_clone().unwrap(),
            poll,
        };

        #[cfg(unix)]
        {
            helper.add_event(kill_evt.as_raw_fd(), EPOLL_HELPER_EVENT_KILL)?;
            helper.add_event(pause_evt.as_raw_fd(), EPOLL_HELPER_EVENT_PAUSE)?;
        }
        #[cfg(target_os = "windows")]
        {
            helper.add_event(kill_evt.as_raw_handle(), EPOLL_HELPER_EVENT_KILL)?;
            helper.add_event(pause_evt.as_raw_handle(), EPOLL_HELPER_EVENT_PAUSE)?;
        }
        Ok(helper)
    }

    // ── Unix API (RawFd + epoll::Events) ────────────────────────────────

    #[cfg(unix)]
    pub fn add_event(&mut self, fd: RawFd, id: u16) -> std::result::Result<(), EpollHelperError> {
        self.add_event_custom(fd, id, epoll::Events::EPOLLIN)
    }

    #[cfg(unix)]
    pub fn add_event_custom(
        &mut self,
        fd: RawFd,
        id: u16,
        evts: epoll::Events,
    ) -> std::result::Result<(), EpollHelperError> {
        self.poll
            .add_event_raw(fd, id.into(), evts)
            .map_err(EpollHelperError::Ctl)
    }

    #[cfg(unix)]
    pub fn mod_event_custom(
        &mut self,
        fd: RawFd,
        id: u16,
        evts: epoll::Events,
    ) -> std::result::Result<(), EpollHelperError> {
        self.poll
            .mod_event_raw(fd, id.into(), evts)
            .map_err(EpollHelperError::Ctl)
    }

    #[cfg(unix)]
    pub fn del_event_custom(
        &mut self,
        fd: RawFd,
        id: u16,
        evts: epoll::Events,
    ) -> std::result::Result<(), EpollHelperError> {
        self.poll
            .del_event_raw(fd, evts)
            .map_err(EpollHelperError::Ctl)
    }

    // ── Windows API (RawHandle) ─────────────────────────────────────────

    #[cfg(target_os = "windows")]
    pub fn add_event(
        &mut self,
        handle: RawHandle,
        id: u16,
    ) -> std::result::Result<(), EpollHelperError> {
        self.poll
            .add_event_raw(handle, id.into())
            .map_err(EpollHelperError::Ctl)
    }

    #[cfg(target_os = "windows")]
    pub fn del_event(
        &mut self,
        handle: RawHandle,
    ) -> std::result::Result<(), EpollHelperError> {
        self.poll
            .del_event_raw(handle)
            .map_err(EpollHelperError::Ctl)
    }

    // ── Cross-platform run loop ─────────────────────────────────────────

    pub fn run(
        &mut self,
        paused: &AtomicBool,
        paused_sync: &Barrier,
        handler: &mut dyn EpollHelperHandler,
    ) -> std::result::Result<(), EpollHelperError> {
        self.run_with_timeout(paused, paused_sync, handler, -1, false)
    }

    #[cfg(not(fuzzing))]
    pub fn run_with_timeout(
        &mut self,
        paused: &AtomicBool,
        paused_sync: &Barrier,
        handler: &mut dyn EpollHelperHandler,
        timeout: i32,
        enable_event_list: bool,
    ) -> std::result::Result<(), EpollHelperError> {
        const EPOLL_EVENTS_LEN: usize = 100;
        let mut events = vec![PollEvent { data: 0, raw_events: 0 }; EPOLL_EVENTS_LEN];

        while paused.load(Ordering::SeqCst) {
            thread::park();
        }

        loop {
            let num_events = match self.poll.wait(timeout, &mut events[..]) {
                Ok(res) => res,
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(EpollHelperError::Wait(e));
                }
            };

            if num_events == 0 {
                handler.handle_timeout(self)?;
                continue;
            }

            if enable_event_list {
                handler.event_list(self, &events[..num_events])?;
            }

            for event in events.iter().take(num_events) {
                let ev_type = event.data as u16;

                match ev_type {
                    EPOLL_HELPER_EVENT_KILL => {
                        info!("KILL_EVENT received, stopping epoll loop");
                        return Ok(());
                    }
                    EPOLL_HELPER_EVENT_PAUSE => {
                        info!("PAUSE_EVENT received, pausing epoll loop");
                        paused_sync.wait();
                        while paused.load(Ordering::SeqCst) {
                            thread::park();
                        }
                        let _ = self.pause_evt.read();
                    }
                    _ => {
                        handler.handle_event(self, event)?;
                    }
                }
            }
        }
    }

    #[cfg(fuzzing)]
    pub fn run_with_timeout(
        &mut self,
        paused: &AtomicBool,
        paused_sync: &Barrier,
        handler: &mut dyn EpollHelperHandler,
        _timeout: i32,
        _enable_event_list: bool,
    ) -> std::result::Result<(), EpollHelperError> {
        const EPOLL_EVENTS_LEN: usize = 100;
        let mut events = vec![PollEvent { data: 0, raw_events: 0 }; EPOLL_EVENTS_LEN];

        loop {
            let num_events = match self.poll.wait(0, &mut events[..]) {
                Ok(res) => res,
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(EpollHelperError::Wait(e));
                }
            };

            if num_events == 0 {
                return Ok(());
            }

            for event in events.iter().take(num_events) {
                let ev_type = event.data as u16;

                match ev_type {
                    EPOLL_HELPER_EVENT_KILL => {
                        info!("KILL_EVENT received, stopping epoll loop");
                        return Ok(());
                    }
                    EPOLL_HELPER_EVENT_PAUSE => {
                        info!("PAUSE_EVENT received, pausing epoll loop");
                        paused_sync.wait();
                        while paused.load(Ordering::SeqCst) {
                            thread::park();
                        }
                        let _ = self.pause_evt.read();
                    }
                    _ => {
                        handler.handle_event(self, event)?;
                    }
                }
            }
        }
    }
}

#[cfg(unix)]
impl AsRawFd for EpollHelper {
    fn as_raw_fd(&self) -> RawFd {
        self.poll.as_raw_fd()
    }
}
