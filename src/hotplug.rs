use crate::cli;
use crate::docker::Container;
use crate::tokio_ext::WithJoinHandleGuard;
use crate::udev_ext::{into_stream, DeviceExt, DeviceSummary};

use anyhow::{anyhow, bail, Error, Result, Context};

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Display;
use std::path::{Path, PathBuf};

use tokio::signal::unix::{signal, SignalKind};
use tokio::spawn;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::task::JoinHandle;
use tokio_stream::{iter, StreamExt};

use udev::{Device, Enumerator, EventType};

#[derive(Debug)]
enum Event {
    Signal(SignalKind),
    ContainerStop(i64),
    Started,
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
            write!(f, "{major:0>3}:{minor:0>3} [{}, {}]", devnode.display(), symlink.display())
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

fn udev_task(hub: Device, tx: UnboundedSender<Event>) -> JoinHandle<Result<()>> {
    spawn(async move {
        let hub_path = hub.syspath();

        let map_fn = |event_type: EventType, device: Device| {
            if !device.syspath().starts_with(&hub_path) {
                None
            } else {
                Event::from_udev(event_type, device)
            }
        };

        let listener = udev::MonitorBuilder::new()?.listen()?;
        let listener = into_stream(listener)
            .filter_map(|event| event.ok())
            .filter_map(|event| map_fn(event.event_type(), event.device()));

        let mut enumerator = Enumerator::new()?;
        let existing = enumerator
            .scan_devices()?
            .filter_map(|device| map_fn(EventType::Add, device));

        let events = iter(existing)
            .chain(tokio_stream::once(Event::Started))
            .chain(listener);

        tokio::pin!(events);
        loop {
            while let Some(event) = events.next().await {
                tx.send(event)?;
            }
        }
    })
}

fn stop_task(
    container: Container,
    tx: tokio::sync::mpsc::UnboundedSender<Event>,
) -> JoinHandle<Result<()>> {
    spawn(async move {
        let status = container.wait().await?;
        tx.send(Event::ContainerStop(status))?;
        Ok::<(), Error>(())
    })
}

fn signal_stream(kind: SignalKind) -> impl tokio_stream::Stream<Item = Result<SignalKind>> {
    async_stream::try_stream! {
        let sig_kind = SignalKind::hangup();
        let mut sig_stream = signal(kind)?;
        while let Some(_) = sig_stream.recv().await {
            yield sig_kind;
        }
    }
}

fn sigs_task(tx: tokio::sync::mpsc::UnboundedSender<Event>) -> JoinHandle<Result<()>> {
    spawn(async move {
        let stream = tokio_stream::empty()
            .merge(signal_stream(SignalKind::alarm()))
            .merge(signal_stream(SignalKind::hangup()))
            .merge(signal_stream(SignalKind::interrupt()))
            .merge(signal_stream(SignalKind::quit()))
            .merge(signal_stream(SignalKind::terminate()))
            .merge(signal_stream(SignalKind::user_defined1()))
            .merge(signal_stream(SignalKind::user_defined2()))
            ;

        tokio::pin!(stream);
        while let Some(signal) = stream.next().await {
            println!("Received signal {signal:?}");
            tx.send(Event::Signal(signal?))?;
        }

        Err::<(), Error>(anyhow!("Failed to listen for signals"))
    })
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

pub fn hotplug<F, S, L>(
    container: Container,
    dev: Device,
    hub: Device,
    symlink_fn: F,
    start_cb: S,
    log_cb: L,
) -> JoinHandle<Result<i64, Error>>
where
    F: Send + Clone + Fn(&udev::Device) -> Option<PathBuf> + 'static,
    S: Send + Clone + Fn(&Container) -> () + 'static,
    L: Send + Clone + Fn(HotPlugEvent) -> () + 'static,
{
    let (tx, mut rx) = unbounded_channel::<Event>();
    spawn(async move {
        let _udev_guard = udev_task(hub.clone(), tx.clone()).guard();
        let _stop_guard = stop_task(container.clone(), tx.clone()).guard();
        let _sigs_guard = sigs_task(tx.clone()).guard();

        let mut devices = HashMap::<Cow<Path>, HotplugDeviceSummary>::default();

        let required = HashMap::<Cow<Path>, DeviceSummary>::from([
            (hub.syspath().into(), hub.summary().context("Failed to obtain basic information about required device")?),
            (dev.syspath().into(), dev.summary().context("Failed to obtain basic information about required device")?),
        ]);

        while let Some(event) = rx.recv().await {
            match event {
                Event::Signal(signal) => {
                    container.kill(signal.as_raw_value()).await?;
                }
                Event::ContainerStop(status) => {
                    return Ok(status);
                }
                Event::Started => {
                    let missing: Vec<_> = required
                        .iter()
                        .filter_map(|(syspath, summary)| {
                            if devices.contains_key(syspath) {
                                None
                            } else {
                                Some(summary)
                            }
                        }).cloned().collect();

                    if missing.is_empty() {
                        start_cb(&container);
                    } else {
                        log_cb(HotPlugEvent::RequiredMissing(missing));
                        container.remove(true).await?;
                    }
                }
                Event::UdevAdd(device, devnum, devnode) => {
                    let syspath = device.syspath();
                    let summary = HotplugDeviceSummary(syspath.to_owned(), devnum, devnode, symlink_fn(&device));

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
                Event::UdevRemove(device) => {
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
    })
}

pub trait HotPlug {
    fn hotplug<L, S>(
        &self,
        dev: Device,
        hub: Device,
        symlinks: Vec<cli::Symlink>,
        start_cb: S,
        log_cb: L,
    ) -> JoinHandle<Result<i64, Error>>
    where
        L: Send + Clone + Fn(HotPlugEvent) -> () + 'static,
        S: Send + Clone + Fn(&Container) -> () + 'static;
}

impl HotPlug for Container {
    fn hotplug<L, S>(
        &self,
        dev: Device,
        hub: Device,
        symlinks: Vec<cli::Symlink>,
        start_cb: S,
        log_cb: L,
    ) -> JoinHandle<Result<i64, Error>>
    where
        L: Send + Clone + Fn(HotPlugEvent) -> () + 'static,
        S: Send + Clone + Fn(&Container) -> () + 'static,
    {
        hotplug(
            self.clone(),
            dev,
            hub,
            move |device| {
                symlinks.iter().find_map(|dev| dev.matches(device))
            },
            start_cb,
            log_cb,
        )
    }
}
