use crate::cli;
use crate::docker::Container;
use crate::tokio_ext::WithJoinHandleGuard;
use crate::udev_ext::{into_stream, DeviceExt};

use anyhow::{anyhow, bail, Error, Result};

use std::borrow::Cow;
use std::collections::HashMap;
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
    UdevAdd(Device, (u64, u64), PathBuf),
    UdevRemove(Device),
}

impl Event {
    pub fn from_udev(event_type: EventType, device: udev::Device) -> Option<Self> {
        match event_type {
            EventType::Add => {
                let devnum = device.device_number()?;
                let devnode = device.devnode()?.to_owned();
                Some(Self::UdevAdd(device, devnum, devnode))
            }
            EventType::Remove => Some(Self::UdevRemove(device)),
            _ => None,
        }
    }
}

pub enum HotPlugEvent {
    Add(Device, (u64, u64), PathBuf, Option<PathBuf>),
    Remove(Device, (u64, u64), PathBuf, Option<PathBuf>),
}

fn udev_task(hub: Device, tx: UnboundedSender<Event>) -> JoinHandle<Result<()>> {
    spawn(async move {
        let hub_path = hub.syspath();

        let listener = udev::MonitorBuilder::new()?.listen()?;
        let listener = into_stream(listener)
            .filter_map(|event| event.ok())
            .map(|event| (event.event_type(), event.device()));

        let mut enumerator = Enumerator::new()?;
        let existing = enumerator
            .scan_devices()?
            .map(|device| (EventType::Add, device));

        let events = iter(existing)
            .chain(listener)
            .filter(|(_, device)| device.syspath().starts_with(&hub_path))
            .filter_map(|(event_type, device)| Event::from_udev(event_type, device));

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

async fn allow_device(
    container: &Container,
    devnum: (u64, u64),
    devnode: &PathBuf,
    symlink: &Option<PathBuf>,
) -> Result<()> {
    container.device(devnum, (true, true, true)).await?;
    container.mknod(&devnode, devnum).await?;
    if let Some(symlink) = symlink {
        container.symlink(&devnode, &symlink).await?;
    }
    Ok(())
}

async fn deny_device(
    container: &Container,
    devnum: (u64, u64),
    devnode: &PathBuf,
    symlink: &Option<PathBuf>,
) -> Result<()> {
    container.device(devnum, (false, false, false)).await?;
    container.rm(&devnode).await?;
    if let Some(symlink) = symlink {
        container.rm(&symlink).await?;
    }
    Ok(())
}

pub fn hotplug<F, L>(
    container: Container,
    hub: Device,
    symlink_fn: F,
    log_fn: L,
) -> JoinHandle<Result<i64, Error>>
where
    F: Send + Clone + Fn(&udev::Device) -> Option<PathBuf> + 'static,
    L: Send + Clone + Fn(HotPlugEvent) -> () + 'static,
{
    let (tx, mut rx) = unbounded_channel::<Event>();
    spawn(async move {
        let _udev_guard = udev_task(hub, tx.clone()).guard();
        let _stop_guard = stop_task(container.clone(), tx.clone()).guard();
        let _sigs_guard = sigs_task(tx.clone()).guard();

        let mut devices = HashMap::<Cow<Path>, ((u64, u64), PathBuf, Option<PathBuf>)>::default();

        while let Some(event) = rx.recv().await {
            match event {
                Event::Signal(signal) => {
                    container.kill(signal.as_raw_value()).await?;
                }
                Event::ContainerStop(status) => {
                    return Ok(status);
                }
                Event::UdevAdd(device, devnum, devnode) => {
                    let syspath = device.syspath();
                    if let Some((devnum, devnode, symlink)) = devices.remove(syspath.into()) {
                        deny_device(&container, devnum, &devnode, &symlink).await?;
                        log_fn(HotPlugEvent::Remove(
                            device.clone(),
                            devnum,
                            devnode,
                            symlink,
                        ));
                    }

                    let symlink = symlink_fn(&device);
                    allow_device(&container, devnum, &devnode, &symlink).await?;
                    devices.insert(
                        device.syspath().to_owned().into(),
                        (devnum, devnode.clone(), symlink.clone()),
                    );

                    log_fn(HotPlugEvent::Add(device.clone(), devnum, devnode, symlink));
                }
                Event::UdevRemove(device) => {
                    let syspath = device.syspath();
                    if let Some((devnum, devnode, symlink)) = devices.remove(syspath.into()) {
                        deny_device(&container, devnum, &devnode, &symlink).await?;
                        log_fn(HotPlugEvent::Remove(
                            device.clone(),
                            devnum,
                            devnode,
                            symlink,
                        ));
                    }
                }
            }
        }

        bail!("Failed to monitor hotplug events");
    })
}

pub trait HotPlug {
    fn hotplug<L>(
        &self,
        hub: Device,
        symlinks: Vec<cli::Symlink>,
        log_fn: L,
    ) -> JoinHandle<Result<i64, Error>>
    where
        L: Send + Clone + Fn(HotPlugEvent) -> () + 'static;
}

impl HotPlug for Container {
    fn hotplug<L>(
        &self,
        hub: Device,
        symlinks: Vec<cli::Symlink>,
        log_fn: L,
    ) -> JoinHandle<Result<i64, Error>>
    where
        L: Send + Clone + Fn(HotPlugEvent) -> () + 'static,
    {
        hotplug(
            self.clone(),
            hub,
            move |device| {
                symlinks.iter().find_map(|dev| dev.matches(device))
            },
            log_fn,
        )
    }
}
