use crate::dev::Device;

use anyhow::Result;
use async_stream::try_stream;
use std::io::ErrorKind::WouldBlock;
use std::path::PathBuf;
use udev::{Enumerator, EventType};

pub enum UdevEvent {
    Add(Device),
    Remove(udev::Device),
}

pub fn enumerate(hub_path: PathBuf) -> impl tokio_stream::Stream<Item = Result<Device>> {
    try_stream! {
        let mut enumerator = Enumerator::new()?;
        let devices = enumerator
            .scan_devices()?
            .filter(|device| device.syspath().starts_with(&hub_path))
            .map(|device| Device::from_udev(device));

        for device in devices {
            yield device;
        }
    }
}

pub fn monitor(hub_path: PathBuf) -> impl tokio_stream::Stream<Item = Result<UdevEvent>> {
    try_stream! {
        let socket = udev::MonitorBuilder::new()?.listen()?;
        let mut async_fd = tokio::io::unix::AsyncFd::new(socket)?;
        loop {
            let mut guard = async_fd.readable_mut().await?;
            if let Ok(Ok(event)) = guard.try_io(|socket| socket.get_ref().iter().next().ok_or_else(|| WouldBlock.into())) {
                match event.event_type() {
                    EventType::Add if event.syspath().starts_with(&hub_path) => {
                        yield UdevEvent::Add(Device::from_udev(event.device()));
                    }
                    EventType::Remove if event.syspath().starts_with(&hub_path) => {
                        yield UdevEvent::Remove(event.device());
                    }
                    _ => continue
                }
            };
        }
    }
}
