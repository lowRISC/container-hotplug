mod attached_device;
mod kobject_uevent;
pub use attached_device::AttachedDevice;
pub use kobject_uevent::UdevSender;

use std::collections::{HashMap, HashSet};
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
    /// Whether the sysfs directory of attached devices should be made writable.
    sysfs: bool,
    monitor: DeviceMonitor,
    devices: HashMap<PathBuf, AttachedDevice>,
    /// Devices whose sysfs directory we have bind-mounted, so we know what to unmount.
    sysfs_bound: HashSet<PathBuf>,
    udev_sender: UdevSender,
}

impl HotPlug {
    pub fn new(
        container: Arc<Container>,
        hub_path: Vec<PathBuf>,
        symlinks: Vec<cli::Symlink>,
        sysfs: bool,
    ) -> Result<Self> {
        let monitor = DeviceMonitor::new(hub_path)?;
        let devices = Default::default();

        let udev_sender = UdevSender::new(crate::util::namespace::NetNamespace::of_pid(
            container.pid(),
        )?)?;

        Ok(Self {
            container,
            symlinks,
            sysfs,
            monitor,
            devices,
            sysfs_bound: Default::default(),
            udev_sender,
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

                if self.sysfs {
                    // Don't fail the attachment if this doesn't work; the device itself is
                    // usable without writable sysfs attributes.
                    match self.container.bind_sysfs(&syspath).await {
                        Ok(()) => {
                            self.sysfs_bound.insert(syspath.clone());
                        }
                        Err(err) => log::warn!("Cannot make sysfs writable: {err:#}"),
                    }
                }

                self.udev_sender.send(device.udev(), "add")?;

                let device = AttachedDevice { device, symlinks };
                self.devices.insert(syspath, device.clone());

                Ok(Some(Event::Attach(device)))
            }
            DeviceEvent::Remove(device) => {
                let Some(device) = self.devices.remove(device.syspath()) else {
                    return Ok(None);
                };

                if self.sysfs_bound.remove(device.syspath()) {
                    self.container.unbind_sysfs(device.syspath()).await?;
                }

                let devnode = device.devnode().unwrap();
                self.container
                    .device(devnode.ty, devnode.devnum, Access::empty())
                    .await?;
                self.container.rm(&devnode.path).await?;
                for symlink in &device.symlinks {
                    self.container.rm(symlink).await?;
                }

                self.udev_sender.send(device.udev(), "remove")?;

                Ok(Some(Event::Detach(device)))
            }
        }
    }
}
