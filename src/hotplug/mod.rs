mod pluggable_device;
mod plugged_device;
mod udev_streams;

use crate::cli;
use crate::docker::Container;

use async_stream::try_stream;

use anyhow::{Context, Result};
use futures::stream::LocalBoxStream;

use std::collections::HashMap;
use std::path::PathBuf;

use tokio_stream::StreamExt;

use udev::Device;

pub use pluggable_device::PluggableDevice;
pub use plugged_device::PluggedDevice;

use self::udev_streams::UdevEvent;

#[derive(Clone)]
pub enum Event {
    Add(PluggedDevice),
    Remove(PluggedDevice),
}

pub struct HotPlug {
    pub container: Container,
    pub device: PluggableDevice,
    pub hub_path: PathBuf,
    symlinks: Vec<cli::Symlink>,
    monitor: LocalBoxStream<'static, Result<UdevEvent>>,
    devices: HashMap<PathBuf, PluggedDevice>,
}

impl HotPlug {
    pub fn new(
        container: Container,
        device: PluggableDevice,
        hub_path: PathBuf,
        symlinks: Vec<cli::Symlink>,
    ) -> Result<Self> {
        let monitor = udev_streams::monitor(hub_path.clone());
        let monitor = Box::pin(monitor);

        let devices = Default::default();

        Ok(Self {
            container,
            device,
            hub_path,
            symlinks,
            monitor,
            devices,
        })
    }

    fn find_symlink(&self, device: &PluggableDevice) -> Option<PathBuf> {
        self.symlinks
            .iter()
            .find_map(|dev| dev.matches(device.device()))
    }

    pub fn start(&mut self) -> impl tokio_stream::Stream<Item = Result<Event>> + '_ {
        try_stream! {
            let dev = self.device.clone();
            let dev = self.allow_device(&dev).await?;
            yield Event::Add(dev);

            let enumerate = udev_streams::enumerate(self.hub_path.clone());

            tokio::pin!(enumerate);
            while let Some(device) = enumerate.next().await {
                let device = device?;
                let device = self.allow_device(&device).await?;
                yield Event::Add(device);
            }
        }
    }

    pub fn run(&mut self) -> impl tokio_stream::Stream<Item = Result<Event>> + '_ {
        try_stream! {
            while let Some(event) = self.monitor.next().await {
                match event? {
                    UdevEvent::Add(device) => {
                        if let Some(_plugged) = self.deny_device(device.device()).await? {
                            // yield Event::Remove(plugged);
                        }
                        let device = self.allow_device(&device).await?;
                        yield Event::Add(device);
                    }
                    UdevEvent::Remove(device) => {
                        if let Some(plugged) = self.deny_device(&device).await? {
                            yield Event::Remove(plugged);
                        }
                    }
                }
            }
        }
    }

    async fn allow_device(&mut self, device: &PluggableDevice) -> Result<PluggedDevice> {
        let device = device.clone();
        let symlink = self.find_symlink(&device);
        let device = PluggedDevice { device, symlink };
        self.container
            .device(device.devnum(), (true, true, true))
            .await?;
        let devnode = device.devnode().to_str().context("devnode is not UTF-8")?;
        self.container.mknod(devnode, device.devnum()).await?;
        if let Some(symlink) = device.symlink() {
            self.container
                .symlink(devnode, symlink.to_str().context("symlink is not UTF-8")?)
                .await?;
        }
        let syspath = device.syspath().to_owned();
        self.devices.insert(syspath, device.clone());
        Ok(device)
    }

    async fn deny_device(&mut self, device: &Device) -> Result<Option<PluggedDevice>> {
        let syspath = device.syspath().to_owned();
        if let Some(device) = self.devices.remove(&syspath) {
            self.container
                .device(device.devnum(), (false, false, false))
                .await?;
            self.container
                .rm(device.devnode().to_str().context("devnode is not UTF-8")?)
                .await?;
            if let Some(symlink) = device.symlink() {
                self.container
                    .rm(symlink.to_str().context("devnode is not UTF-8")?)
                    .await?;
            }
            Ok(Some(device))
        } else {
            Ok(None)
        }
    }
}
