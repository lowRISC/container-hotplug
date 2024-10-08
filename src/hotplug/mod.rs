mod attached_device;
pub use attached_device::AttachedDevice;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_stream::try_stream;
use tokio_stream::StreamExt;

use super::Event;
use crate::cgroup::Access;
use crate::cli;
use crate::dev::{DeviceEvent, DeviceMonitor};
use crate::runc::Container;

pub struct HotPlug {
    pub container: Arc<Container>,
    symlinks: Vec<cli::Symlink>,
    monitor: DeviceMonitor,
    devices: HashMap<PathBuf, AttachedDevice>,
}

impl HotPlug {
    pub fn new(
        container: Arc<Container>,
        hub_path: PathBuf,
        symlinks: Vec<cli::Symlink>,
    ) -> Result<Self> {
        let monitor = DeviceMonitor::new(hub_path.clone())?;
        let devices = Default::default();

        Ok(Self {
            container,
            symlinks,
            monitor,
            devices,
        })
    }

    pub fn run(&mut self) -> impl tokio_stream::Stream<Item = Result<Event>> + '_ {
        try_stream! {
            while let Some(event) = self.monitor.try_read()? {
                if let Some(event) = self.process(event).await? {
                    yield event;
                }
            }

            yield Event::Initialized;

            while let Some(event) = self.monitor.try_next().await? {
                if let Some(event) = self.process(event).await? {
                    yield event;
                }
            }
        }
    }

    async fn process(&mut self, event: DeviceEvent) -> Result<Option<Event>> {
        match event {
            DeviceEvent::Add(device) => {
                let Some(devnode) = device.devnode() else {
                    return Ok(None);
                };

                let symlinks: Vec<_> = self
                    .symlinks
                    .iter()
                    .filter_map(|dev| dev.matches(&device))
                    .collect();

                self.container
                    .device(devnode.ty, devnode.devnum, Access::all())
                    .await?;
                self.container
                    .mknod(&devnode.path, devnode.ty, devnode.devnum)
                    .await?;
                for symlink in &symlinks {
                    self.container.symlink(&devnode.path, symlink).await?;
                }

                let syspath = device.syspath().to_owned();
                let device = AttachedDevice { device, symlinks };
                self.devices.insert(syspath, device.clone());

                Ok(Some(Event::Attach(device)))
            }
            DeviceEvent::Remove(device) => {
                let Some(device) = self.devices.remove(device.syspath()) else {
                    return Ok(None);
                };

                let devnode = device.devnode().unwrap();
                self.container
                    .device(devnode.ty, devnode.devnum, Access::empty())
                    .await?;
                self.container.rm(&devnode.path).await?;
                for symlink in &device.symlinks {
                    self.container.rm(symlink).await?;
                }

                Ok(Some(Event::Detach(device)))
            }
        }
    }
}
