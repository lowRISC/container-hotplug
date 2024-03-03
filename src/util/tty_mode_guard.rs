// Copyright lowRISC contributors.
// Licensed under the Apache License, Version 2.0, see LICENSE for details.
// SPDX-License-Identifier: Apache-2.0

use std::io::{IsTerminal, Result};
use std::ops::{Deref, DerefMut};
use std::os::fd::{AsFd, BorrowedFd};

use rustix::termios::{self, Termios};

/// TTY mode guard over any file descriptor.
///
/// The fd is converted into selected terminal mode and restored to its original state on drop, if it's a terminal.
/// For non-terminal fds, nothing will be performed.
pub struct TtyModeGuard<T: AsFd> {
    termios: Option<Termios>,
    fd: T,
}

impl<T: AsFd> Deref for TtyModeGuard<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.fd
    }
}

impl<T: AsFd> DerefMut for TtyModeGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.fd
    }
}

impl<T: AsFd> AsFd for TtyModeGuard<T> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl<T: AsFd> TtyModeGuard<T> {
    pub fn new(fd: T, mode: impl FnOnce(&mut Termios)) -> Result<Self> {
        let termios = if fd.as_fd().is_terminal() {
            let termios = termios::tcgetattr(&fd)?;

            let mut new_termios = termios.clone();
            mode(&mut new_termios);
            termios::tcsetattr(&fd, termios::OptionalActions::Now, &new_termios)?;

            Some(termios)
        } else {
            None
        };

        Ok(Self { termios, fd })
    }
}

impl<T: AsFd> Drop for TtyModeGuard<T> {
    fn drop(&mut self) {
        if let Some(termios) = self.termios.as_ref() {
            termios::tcsetattr(&self.fd, termios::OptionalActions::Now, termios).unwrap();
        }
    }
}
