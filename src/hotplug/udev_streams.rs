use super::PluggableDevice;

use anyhow::Result;
use async_stream::try_stream;
use std::ops::Deref;
use std::path::PathBuf;
use tokio_stream::StreamExt;
use udev::{Device, Enumerator, EventType};

use crate::udev_ext::into_stream;

pub enum UdevEvent {
    Add(PluggableDevice),
    Remove(Device),
}

pub fn enumerate(hub_path: PathBuf) -> impl tokio_stream::Stream<Item = Result<PluggableDevice>> {
    try_stream! {
        let mut enumerator = Enumerator::new()?;
        let mut devices = enumerator
            .scan_devices()?
            .filter(|device| device.syspath().starts_with(&hub_path))
            .filter_map(|device| PluggableDevice::from_device(&device));

        while let Some(device) = devices.next() {
            yield device;
        }
    }
}

pub fn monitor(hub_path: PathBuf) -> impl tokio_stream::Stream<Item = Result<UdevEvent>> {
    try_stream! {
        let listener = udev::MonitorBuilder::new()?.listen()?;
        let listener = into_stream(listener)
            .filter_map(|event| event.ok())
            .filter(|event| event.syspath().starts_with(&hub_path));

        tokio::pin!(listener);
        while let Some(event) = listener.next().await {
            match event.event_type() {
                EventType::Add => {
                    if let Some(device) = PluggableDevice::from_device(event.deref()) {
                        yield UdevEvent::Add(device);
                    }
                }
                EventType::Remove => {
                    yield UdevEvent::Remove(event.device())
                }
                _ => continue
            }
        }
    }
}
