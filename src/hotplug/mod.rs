use crate::cli;
use crate::docker::Container;
use crate::udev_ext::{into_stream, DeviceExt, DeviceSummary};

use async_stream::try_stream;
use async_trait::async_trait;

use anyhow::{bail, Context, Error, Result};

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Display;
use std::path::{Path, PathBuf};

use tokio_stream::StreamExt;

use udev::{Device, Enumerator, EventType};

#[derive(Debug)]
enum Event {
    Started,
    Stopped(i64),
    UdevAdd(Device, (u64, u64), PathBuf),
    UdevRemove(Device),
}

impl Event {
    pub fn from_udev(event_type: EventType, device: udev::Device) -> Option<Self> {
        match event_type {
            EventType::Add => {
                let (_, devnum, devnode) = device.summary()?;
                Some(Self::UdevAdd(device, devnum, devnode))
            }
            EventType::Remove => Some(Self::UdevRemove(device)),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct HotplugDeviceSummary(PathBuf, (u64, u64), PathBuf, Option<PathBuf>);

impl HotplugDeviceSummary {
    fn as_tuple(&self) -> (&PathBuf, (u64, u64), &PathBuf, Option<&PathBuf>) {
        (&self.0, self.1, &self.2, self.3.as_ref())
    }
}

impl Display for HotplugDeviceSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (_syspath, (major, minor), devnode, symlink) = self.as_tuple();
        if let Some(symlink) = symlink {
            write!(
                f,
                "{major:0>3}:{minor:0>3} [{}, {}]",
                devnode.display(),
                symlink.display()
            )
        } else {
            write!(f, "{major:0>3}:{minor:0>3} [{}]", devnode.display())
        }
    }
}

pub enum HotPlugEvent {
    RequiredMissing(Vec<DeviceSummary>),
    Add(HotplugDeviceSummary),
    ReAdd(HotplugDeviceSummary, HotplugDeviceSummary),
    Remove(HotplugDeviceSummary),
    RequiredRemoved(HotplugDeviceSummary),
}

fn udev_stream(hub: Device) -> impl tokio_stream::Stream<Item = Result<Event>> {
    let hub_path = hub.syspath().to_owned();
    let map_fn = move |event_type: EventType, device: Device| {
        if !device.syspath().starts_with(&hub_path) {
            None
        } else {
            Event::from_udev(event_type, device)
        }
    };

    let monitor_stream = {
        let map_fn = map_fn.clone();
        try_stream! {
            let listener = udev::MonitorBuilder::new()?.listen()?;
            let listener = into_stream(listener)
                .filter_map(|event| event.ok())
                .filter_map(|event| map_fn(event.event_type(), event.device()));

            tokio::pin!(listener);
            while let Some(event) = listener.next().await {
                yield event;
            }
        }
    };

    let start_stream = {
        let map_fn = map_fn.clone();
        try_stream! {
            let mut enumerator = Enumerator::new()?;
            let mut devices = enumerator
                .scan_devices()?
                .filter_map(|device| map_fn(EventType::Add, device));

            while let Some(event) = devices.next() {
                yield event;
            }

            yield Event::Started;
        }
    };

    start_stream.chain(monitor_stream)
}

fn stop_stream(container: Container) -> impl tokio_stream::Stream<Item = Result<Event>> {
    try_stream! {
        let status = container.wait().await?;
        yield Event::Stopped(status);
    }
}

async fn allow_device(container: &Container, summary: &HotplugDeviceSummary) -> Result<()> {
    let (_, devnum, devnode, symlink) = summary.as_tuple();
    container.device(devnum, (true, true, true)).await?;
    container.mknod(&devnode, devnum).await?;
    if let Some(symlink) = symlink {
        container.symlink(&devnode, &symlink).await?;
    }
    Ok(())
}

async fn deny_device(container: &Container, summary: &HotplugDeviceSummary) -> Result<()> {
    let (_, devnum, devnode, symlink) = summary.as_tuple();
    container.device(devnum, (false, false, false)).await?;
    container.rm(&devnode).await?;
    if let Some(symlink) = symlink {
        container.rm(&symlink).await?;
    }
    Ok(())
}

pub async fn hotplug<F, S, L>(
    container: Container,
    dev: Device,
    hub: Device,
    symlink_fn: F,
    start_cb: S,
    log_cb: L,
) -> Result<i64, Error>
where
    F: Send + Clone + Fn(&udev::Device) -> Option<PathBuf> + 'static,
    S: Send + Clone + Fn(&Container) -> () + 'static,
    L: Send + Clone + Fn(HotPlugEvent) -> () + 'static,
{
    let stream = tokio_stream::empty()
        .merge(udev_stream(hub.clone()))
        .merge(stop_stream(container.clone()));

    let mut devices = HashMap::<Cow<Path>, HotplugDeviceSummary>::default();

    let required = HashMap::<Cow<Path>, DeviceSummary>::from([
        (
            hub.syspath().into(),
            hub.summary()
                .context("Failed to obtain basic information about required device")?,
        ),
        (
            dev.syspath().into(),
            dev.summary()
                .context("Failed to obtain basic information about required device")?,
        ),
    ]);

    tokio::pin!(stream);
    while let Some(event) = stream.next().await {
        match event {
            Err(err) => {
                container.remove(true).await?;
                return Err(err.into());
            }
            Ok(Event::Stopped(status)) => {
                return Ok(status);
            }
            Ok(Event::Started) => {
                let missing: Vec<_> = required
                    .iter()
                    .filter_map(|(syspath, summary)| {
                        if devices.contains_key(syspath) {
                            None
                        } else {
                            Some(summary)
                        }
                    })
                    .cloned()
                    .collect();

                if missing.is_empty() {
                    start_cb(&container);
                } else {
                    log_cb(HotPlugEvent::RequiredMissing(missing));
                    container.remove(true).await?;
                }
            }
            Ok(Event::UdevAdd(device, devnum, devnode)) => {
                let syspath = device.syspath();
                let summary =
                    HotplugDeviceSummary(syspath.to_owned(), devnum, devnode, symlink_fn(&device));

                let old = devices.remove(syspath.into());
                if let Some(old) = &old {
                    log_cb(HotPlugEvent::ReAdd(summary.clone(), old.clone()));
                    deny_device(&container, old).await?;
                } else {
                    log_cb(HotPlugEvent::Add(summary.clone()));
                }

                allow_device(&container, &summary).await?;
                devices.insert(syspath.to_owned().into(), summary);
            }
            Ok(Event::UdevRemove(device)) => {
                let syspath = device.syspath();
                if let Some(summary) = devices.remove(syspath.into()) {
                    if required.contains_key(syspath.into()) {
                        log_cb(HotPlugEvent::RequiredRemoved(summary));
                        container.remove(true).await?;
                        continue;
                    } else {
                        log_cb(HotPlugEvent::Remove(summary.clone()));
                        deny_device(&container, &summary).await?;
                    }
                }
            }
        }
    }

    bail!("Failed to monitor hotplug events");
}

#[async_trait]
pub trait HotPlug {
    async fn hotplug<L, S>(
        &self,
        dev: Device,
        hub: Device,
        symlinks: Vec<cli::Symlink>,
        start_cb: S,
        log_cb: L,
    ) -> Result<i64, Error>
    where
        L: Send + Clone + Fn(HotPlugEvent) -> () + 'static,
        S: Send + Clone + Fn(&Container) -> () + 'static;
}

#[async_trait]
impl HotPlug for Container {
    async fn hotplug<L, S>(
        &self,
        dev: Device,
        hub: Device,
        symlinks: Vec<cli::Symlink>,
        start_cb: S,
        log_cb: L,
    ) -> Result<i64, Error>
    where
        L: Send + Clone + Fn(HotPlugEvent) -> () + 'static,
        S: Send + Clone + Fn(&Container) -> () + 'static,
    {
        let status = hotplug(
            self.clone(),
            dev,
            hub,
            move |device| symlinks.iter().find_map(|dev| dev.matches(device)),
            start_cb,
            log_cb,
        )
        .await?;
        Ok(status)
    }
}
