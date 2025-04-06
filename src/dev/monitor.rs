//! Wrapper around udev to monitor device events.
//!
//! Provides async support and convience methods to monitor devices and retrieve device properties.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::collections::hash_map::Entry;
use std::path::PathBuf;
use std::rc::Rc;
use std::task::{Poll, ready};

use anyhow::Result;
use tokio::io::unix::AsyncFd;
use udev::{Enumerator, EventType};

use super::Device;

pub enum DeviceEvent {
    Add(Device),
    Remove(Device),
}

pub struct DeviceMonitor {
    /// Root path for devices to monitor. This is usually a USB hub.
    root: PathBuf,
    /// Udev monitor socket.
    // Use `Rc` to avoid lifecycle issues in async stream impl.
    socket: Rc<AsyncFd<udev::MonitorSocket>>,
    /// All devices seen so far. This stores devnode and devnum which
    /// may not be available when the device is removed.
    seen: HashMap<PathBuf, Device>,
    /// Enumerated devices that are available when the monitor is started.
    /// Initial reads are from this list.
    enumerated: VecDeque<Device>,
}

impl DeviceMonitor {
    /// Create a new device monitor.
    ///
    /// Devices that are already plugged will each generate an `Add` event immediately.
    pub fn new(root: PathBuf) -> Result<Self> {
        // Create a socket before enumerating devices to avoid missing events.
        let socket = Rc::new(AsyncFd::new(udev::MonitorBuilder::new()?.listen()?)?);

        // Process all devices that are already plugged.
        let mut enumerator = Enumerator::new()?;
        let enumerated = enumerator
            .scan_devices()?
            .filter(|device| device.syspath().starts_with(&root))
            .map(Device::from_udev)
            .collect::<VecDeque<_>>();

        let mut seen = HashMap::new();
        for device in &enumerated {
            seen.insert(device.syspath().to_owned(), device.clone());
        }

        Ok(Self {
            root,
            socket,
            seen,
            enumerated,
        })
    }

    pub fn try_read(&mut self) -> Result<Option<DeviceEvent>> {
        if let Some(device) = self.enumerated.pop_front() {
            return Ok(Some(DeviceEvent::Add(device)));
        }

        loop {
            let Some(event) = self.socket.get_ref().iter().next() else {
                return Ok(None);
            };

            match event.event_type() {
                EventType::Add if event.syspath().starts_with(&self.root) => {
                    match self.seen.entry(event.syspath().to_owned()) {
                        Entry::Occupied(occupied) => {
                            log::info!("Device already seen: {}", occupied.key().display());
                        }
                        Entry::Vacant(entry) => {
                            let device = Device::from_udev(event.device());
                            entry.insert(device.clone());
                            return Ok(Some(DeviceEvent::Add(device)));
                        }
                    }
                }
                EventType::Remove => {
                    if let Some(device) = self.seen.remove(event.syspath()) {
                        return Ok(Some(DeviceEvent::Remove(device)));
                    }
                }
                _ => continue,
            }
        }
    }
}

impl tokio_stream::Stream for DeviceMonitor {
    type Item = Result<DeviceEvent>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        if !self.enumerated.is_empty() {
            return Poll::Ready(self.try_read().transpose());
        }

        let fd = self.socket.clone();
        loop {
            let mut guard = ready!(fd.poll_read_ready(cx))?;

            match self.try_read() {
                Ok(Some(v)) => break Poll::Ready(Some(Ok(v))),
                Err(err) => break Poll::Ready(Some(Err(err))),
                Ok(None) => {
                    guard.clear_ready();
                    continue;
                }
            }
        }
    }
}
