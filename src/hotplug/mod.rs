mod attached_device;

use crate::cgroup::Access;
use crate::cli;
use crate::dev::DeviceMonitor;
use crate::docker::Container;

use anyhow::Result;
use async_stream::try_stream;
use std::path::PathBuf;
use std::{collections::HashMap, sync::Arc};
use tokio_stream::StreamExt;

pub use crate::dev::{Device, DeviceEvent};
pub use attached_device::AttachedDevice;

#[derive(Clone)]
pub enum Event {
    Attach(AttachedDevice),
    Detach(AttachedDevice),
}

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

    fn find_symlinks(&self, device: &Device) -> Vec<PathBuf> {
        self.symlinks
            .iter()
            .filter_map(|dev| dev.matches(device))
            .collect()
    }

    pub fn start(&mut self) -> impl tokio_stream::Stream<Item = Result<Event>> + '_ {
        try_stream! {
            while let Some(event) = self.monitor.try_read()? {
                match event {
                    DeviceEvent::Add(device) => {
                        if device.devnode().is_none() {
                            continue;
                        }
                        let device = self.allow_device(&device).await?;
                        yield Event::Attach(device);
                    }
                    DeviceEvent::Remove(device) => {
                        if let Some(plugged) = self.deny_device(device.udev()).await? {
                            yield Event::Detach(plugged);
                        }
                    }
                }
            }
        }
    }

    pub fn run(&mut self) -> impl tokio_stream::Stream<Item = Result<Event>> + '_ {
        try_stream! {
            while let Some(event) = self.monitor.try_next().await? {
                match event {
                    DeviceEvent::Add(device) => {
                        if device.devnode().is_none() {
                            continue;
                        }
                        let device = self.allow_device(&device).await?;
                        yield Event::Attach(device);
                    }
                    DeviceEvent::Remove(device) => {
                        if let Some(plugged) = self.deny_device(device.udev()).await? {
                            yield Event::Detach(plugged);
                        }
                    }
                }
            }
        }
    }

    async fn allow_device(&mut self, device: &Device) -> Result<AttachedDevice> {
        let device = device.clone();
        let symlinks = self.find_symlinks(&device);
        let device = AttachedDevice { device, symlinks };
        let devnode = device.devnode().unwrap();
        self.container.device(devnode.devnum, Access::all()).await?;
        self.container.mknod(&devnode.path, devnode.devnum).await?;
        for symlink in &device.symlinks {
            self.container.symlink(&devnode.path, symlink).await?;
        }
        let syspath = device.syspath().to_owned();
        self.devices.insert(syspath, device.clone());
        Ok(device)
    }

    async fn deny_device(&mut self, device: &udev::Device) -> Result<Option<AttachedDevice>> {
        let syspath = device.syspath().to_owned();
        if let Some(device) = self.devices.remove(&syspath) {
            let devnode = device.devnode().unwrap();
            self.container
                .device(devnode.devnum, Access::empty())
                .await?;
            self.container.rm(&devnode.path).await?;
            for symlink in &device.symlinks {
                self.container.rm(symlink).await?;
            }
            Ok(Some(device))
        } else {
            Ok(None)
        }
    }
}
