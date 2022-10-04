use crate::docker::Container;
use crate::tokio_ext::WithJoinHandleGuard;
use crate::udev_device_ext::DevNum;

use anyhow::{anyhow, bail, Error, Result};

use std::borrow::{BorrowMut, Cow};
use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::path::PathBuf;

use tokio::spawn;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tokio_util::task::LocalPoolHandle;

use tokio_udev::{AsyncMonitorSocket, Device, Enumerator, EventType};

#[derive(Debug)]
enum HotPlugEvent {
    ContainerStop(i64),
    UdevAdd((u64, u64), PathBuf, Option<PathBuf>),
    UdevRemove((u64, u64)),
}

impl HotPlugEvent {
    pub fn from_udev<D, F>(event_type: EventType, device: D, symlink_fn: &mut F) -> Option<Self>
    where
        D: Deref<Target = Device>,
        F: Send + Clone + Fn(&Device) -> Option<PathBuf>,
    {
        let devnum = device.device_number()?;
        let devnode = device.devnode()?.to_owned();
        let symlink = symlink_fn(device.deref());
        match event_type {
            EventType::Add | EventType::Change => Some(Self::UdevAdd(devnum, devnode, symlink)),
            EventType::Remove => Some(Self::UdevRemove(devnum)),
            _ => None,
        }
    }
}

fn udev_task<F>(
    root_path: PathBuf,
    tx: UnboundedSender<HotPlugEvent>,
    symlink_fn: F,
) -> JoinHandle<Result<()>>
where
    F: Send + Clone + Fn(&Device) -> Option<PathBuf> + 'static,
{
    const POOL_SIZE: usize = 1;
    LocalPoolHandle::new(POOL_SIZE).spawn_pinned(move || async move {
        let listener = tokio_udev::MonitorBuilder::new()?.listen()?;
        let listener = {
            let mut symlink_fn = symlink_fn.clone();
            AsyncMonitorSocket::new(listener)?
                .filter_map(|event| event.ok())
                .filter(|event| event.syspath().starts_with(&root_path))
                .filter_map(move |event| {
                    HotPlugEvent::from_udev(event.event_type(), event, symlink_fn.borrow_mut())
                })
        };

        let mut enumerator = Enumerator::new()?;
        let existing = {
            let mut symlink_fn = symlink_fn.clone();
            enumerator
                .scan_devices()?
                .filter(|device| device.syspath().starts_with(&root_path))
                .filter_map(move |device| {
                    HotPlugEvent::from_udev(EventType::Add, &device, symlink_fn.borrow_mut())
                })
        };

        let existing = tokio_stream::iter(existing);

        let mut events = existing.chain(listener);
        while let Some(event) = events.next().await {
            tx.send(event)?;
        }

        Err::<(), Error>(anyhow!("Failed to read udev event"))
    })
}

fn stop_task(
    container: Container,
    tx: tokio::sync::mpsc::UnboundedSender<HotPlugEvent>,
) -> JoinHandle<Result<()>> {
    spawn(async move {
        let status = container.wait().await?;
        tx.send(HotPlugEvent::ContainerStop(status))?;
        Ok::<(), Error>(())
    })
}

pub fn hotplug<F>(
    container: Container,
    root_path: PathBuf,
    symlink_fn: F,
) -> JoinHandle<Result<i64, Error>>
where
    F: Send + Clone + Fn(&Device) -> Option<PathBuf> + 'static,
{
    let (tx, mut rx) = unbounded_channel::<HotPlugEvent>();
    spawn(async move {
        let _udev_guard = udev_task(root_path, tx.clone(), symlink_fn).guard();
        let _stop_guard = stop_task(container.clone(), tx.clone()).guard();

        let mut devices = HashMap::<(u64, u64), HashSet<PathBuf>>::default();

        while let Some(event) = rx.recv().await {
            match event {
                HotPlugEvent::ContainerStop(status) => {
                    return Ok(status);
                }
                HotPlugEvent::UdevAdd(devnum, devnode, symlink) => {
                    let nodes = devices.entry(devnum).or_default();
                    container.device(devnum, (true, true, true)).await?;
                    nodes.insert(devnode.clone());
                    container.mknod(&devnode, devnum).await?;
                    println!("Attaching {:?}", devnode);
                    if let Some(symlink) = symlink {
                        nodes.insert(symlink.clone());
                        container.symlink(&devnode, &symlink).await?;
                        println!("Attaching {:?}", symlink);
                    }
                }
                HotPlugEvent::UdevRemove(devnum) => {
                    let nodes = devices.entry(devnum).or_default();
                    container.device(devnum, (false, false, false)).await?;
                    for node in nodes.drain() {
                        container.rm(&node).await?;
                        println!("Dettaching {:?}", node);
                    }
                }
            }
        }
        bail!("Failed to monitor hotplug events");
    })
}

pub trait HotPlug {
    fn hotplug<S, P, L>(&self, hub: &Device, symlinks: L) -> JoinHandle<Result<i64, Error>>
    where
        S: Into<String>,
        P: Into<PathBuf>,
        L: IntoIterator<Item = ((S, S, S), P)>;
}

impl HotPlug for Container {
    fn hotplug<S, P, L>(&self, hub: &Device, symlinks: L) -> JoinHandle<Result<i64, Error>>
    where
        S: Into<String>,
        P: Into<PathBuf>,
        L: IntoIterator<Item = ((S, S, S), P)>,
    {
        let link_map: HashMap<(Cow<str>, Cow<str>, Cow<str>), PathBuf> = symlinks
            .into_iter()
            .map(|((v, p, i), s)| {
                (
                    (v.into().into(), p.into().into(), i.into().into()),
                    s.into(),
                )
            })
            .collect();

        hotplug(self.clone(), hub.syspath().to_owned(), move |device| {
            let vid = device.property_value("ID_VENDOR_ID")?.to_str()?.into();
            let pid = device.property_value("ID_MODEL_ID")?.to_str()?.into();
            let interface = device
                .property_value("ID_USB_INTERFACE_NUM")?
                .to_str()?
                .into();
            link_map.get(&(vid, pid, interface)).cloned()
        })
    }
}
