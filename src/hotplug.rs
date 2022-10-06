use crate::docker::Container;
use crate::tokio_ext::WithJoinHandleGuard;
use crate::udev_device_ext::DevNum;

use anyhow::{anyhow, bail, Error, Result};

use std::borrow::BorrowMut;
use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::os::unix::prelude::AsRawFd;
use std::path::PathBuf;

use tokio::signal::ctrl_c;
use tokio::signal::unix::{signal, SignalKind};
use tokio::spawn;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::task::JoinHandle;

use udev::{Device, Enumerator, EventType};

#[derive(Debug)]
enum Event {
    Signal(SignalKind),
    ContainerStop(i64),
    UdevAdd((u64, u64), PathBuf, Option<PathBuf>),
    UdevRemove((u64, u64)),
}

impl Event {
    pub fn from_udev<D, F>(event_type: EventType, device: D, symlink_fn: &mut F) -> Option<Self>
    where
        D: Deref<Target = Device>,
        F: Send + Clone + Fn(&Device) -> Option<PathBuf>,
    {
        let devnum = device.device_number()?;
        let devnode = device.devnode()?.to_owned();
        let symlink = symlink_fn(device.deref());
        match event_type {
            EventType::Add => Some(Self::UdevAdd(devnum, devnode, symlink)),
            EventType::Remove => Some(Self::UdevRemove(devnum)),
            _ => None,
        }
    }
}

pub enum HotPlugEvent {
    Add((u64, u64), Vec<PathBuf>),
    Remove((u64, u64), Vec<PathBuf>),
}

fn udev_task<F>(root_path: PathBuf, tx: UnboundedSender<Event>, symlink_fn: F) -> ()
where
    F: Send + Clone + Fn(&Device) -> Option<PathBuf> + 'static,
{
    std::thread::spawn(move || -> Result<(), anyhow::Error> {
        let socket = udev::MonitorBuilder::new()?.listen()?;
        let poll_fd = nix::poll::PollFd::new(socket.as_raw_fd(), nix::poll::PollFlags::POLLIN);

        let mut listener = {
            let mut symlink_fn = symlink_fn.clone();
            socket
                .filter(|event| event.syspath().starts_with(&root_path))
                .filter_map(move |event| {
                    Event::from_udev(event.event_type(), event, symlink_fn.borrow_mut())
                })
        };

        let mut enumerator = Enumerator::new()?;
        let existing = {
            let mut symlink_fn = symlink_fn.clone();
            enumerator
                .scan_devices()?
                .filter(|device| device.syspath().starts_with(&root_path))
                .filter_map(move |device| {
                    Event::from_udev(EventType::Add, &device, symlink_fn.borrow_mut())
                })
        };

        for event in existing {
            tx.send(event)?;
        }

        loop {
            while let Some(event) = listener.next() {
                tx.send(event)?;
            }
            nix::poll::poll(&mut [poll_fd], -1)?;
        }
    });
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

fn ctrl_c_task(tx: tokio::sync::mpsc::UnboundedSender<Event>) -> JoinHandle<Result<()>> {
    spawn(async move {
        while let Ok(_) = ctrl_c().await {
            tx.send(Event::Signal(SignalKind::interrupt()))?;
        }
        Err::<(), Error>(anyhow!("Failed to listen for ctrl+c signal"))
    })
}

fn sighup_task(tx: tokio::sync::mpsc::UnboundedSender<Event>) -> JoinHandle<Result<()>> {
    spawn(async move {
        let mut stream = signal(SignalKind::hangup())?;
        while let Some(_) = stream.recv().await {
            tx.send(Event::Signal(SignalKind::hangup()))?;
        }
        Err::<(), Error>(anyhow!("Failed to listen for sighup signal"))
    })
}

pub fn hotplug<F, L>(
    container: Container,
    root_path: PathBuf,
    symlink_fn: F,
    log_fn: L,
) -> JoinHandle<Result<i64, Error>>
where
    F: Send + Clone + Fn(&Device) -> Option<PathBuf> + 'static,
    L: Send + Clone + Fn(HotPlugEvent) -> () + 'static,
{
    let (tx, mut rx) = unbounded_channel::<Event>();
    spawn(async move {
        /*let _udev_guard =*/
        udev_task(root_path, tx.clone(), symlink_fn); //.guard();
        let _stop_guard = stop_task(container.clone(), tx.clone()).guard();
        let _ctrl_c_guard = ctrl_c_task(tx.clone()).guard();
        let _sighup_guard = sighup_task(tx.clone()).guard();

        let mut devices = HashMap::<(u64, u64), HashSet<PathBuf>>::default();

        while let Some(event) = rx.recv().await {
            match event {
                Event::Signal(_) => {
                    container.clone().remove(true).await?;
                }
                Event::ContainerStop(status) => {
                    return Ok(status);
                }
                Event::UdevAdd(devnum, devnode, symlink) => {
                    let nodes = devices.entry(devnum).or_default();
                    container.device(devnum, (true, true, true)).await?;
                    if nodes.insert(devnode.clone()) {
                        container.mknod(&devnode, devnum).await?;
                    }
                    if let Some(symlink) = symlink {
                        if nodes.insert(symlink.clone()) {
                            container.symlink(&devnode, &symlink).await?;
                        }
                    }
                    log_fn(HotPlugEvent::Add(devnum, nodes.iter().cloned().collect()))
                }
                Event::UdevRemove(devnum) => {
                    if let Some(nodes) = devices.remove(&devnum) {
                        container.device(devnum, (false, false, false)).await?;
                        for node in nodes.iter() {
                            container.rm(&node).await?;
                        }
                        log_fn(HotPlugEvent::Remove(
                            devnum,
                            nodes.iter().cloned().collect(),
                        ));
                    }
                }
            }
        }
        bail!("Failed to monitor hotplug events");
    })
}

pub trait HotPlug {
    fn hotplug<S, P, L, C>(
        &self,
        hub: &Device,
        symlinks: L,
        log_fn: C,
    ) -> JoinHandle<Result<i64, Error>>
    where
        S: Into<String>,
        P: Into<PathBuf>,
        L: IntoIterator<Item = (S, P)>,
        C: Send + Clone + Fn(HotPlugEvent) -> () + 'static;
}

impl HotPlug for Container {
    fn hotplug<S, P, L, C>(
        &self,
        hub: &Device,
        symlinks: L,
        log_fn: C,
    ) -> JoinHandle<Result<i64, Error>>
    where
        S: Into<String>,
        P: Into<PathBuf>,
        L: IntoIterator<Item = (S, P)>,
        C: Send + Clone + Fn(HotPlugEvent) -> () + 'static,
    {
        let link_map: HashMap<String, PathBuf> = symlinks
            .into_iter()
            .map(|(dev, path)| (dev.into(), path.into()))
            .collect();

        hotplug(
            self.clone(),
            hub.syspath().to_owned(),
            move |device| {
                let vid = device.property_value("ID_VENDOR_ID")?.to_str()?;
                let pid = device.property_value("ID_MODEL_ID")?.to_str()?;
                let interface = device.property_value("ID_USB_INTERFACE_NUM")?.to_str()?;
                let key = format!("usb:{vid}:{pid}:{interface}");
                link_map.get(&key).cloned()
            },
            log_fn,
        )
    }
}
