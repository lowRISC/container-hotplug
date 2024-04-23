use std::fs::File;
use std::io::{BufRead, BufReader, Seek};
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};
use rustix::fs::{FileType, Gid, Mode, Uid};
use rustix::process::{Pid, Signal};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::Mutex;

use crate::cgroup::{
    Access, DeviceAccessController, DeviceAccessControllerV1, DeviceAccessControllerV2, DeviceType,
};

struct CgroupEventNotifier {
    file: AsyncFd<File>,
}

impl CgroupEventNotifier {
    fn new(cgroup: &Path) -> Result<Self> {
        let file = AsyncFd::with_interest(
            File::open(cgroup.join("cgroup.events")).context("Cannot open cgroup.events")?,
            Interest::PRIORITY | Interest::ERROR,
        )?;
        Ok(Self { file })
    }

    fn populated(&mut self) -> Result<bool> {
        let file = self.file.get_mut();
        file.seek(std::io::SeekFrom::Start(0))
            .context("Cannot seek to start")?;
        for line in BufReader::new(file).lines() {
            let line = line.context("Cannot read line")?;
            if line.starts_with("populated ") {
                return Ok(line.ends_with('1'));
            }
        }
        bail!("Cannot find populated field");
    }

    pub async fn wait(&mut self) -> Result<()> {
        if !self.populated()? {
            return Ok(());
        }

        loop {
            self.file
                .ready(Interest::PRIORITY | Interest::ERROR)
                .await?
                .clear_ready();

            if !self.populated()? {
                return Ok(());
            }
        }
    }
}

pub struct Container {
    uid: Uid,
    gid: Gid,
    pid: Pid,
    wait: tokio::sync::watch::Receiver<bool>,
    cgroup_device_filter: Mutex<Box<dyn DeviceAccessController + Send>>,
}

impl Container {
    pub fn new(config: &super::config::Config, state: &super::state::State) -> Result<Self> {
        let (send, recv) = tokio::sync::watch::channel(false);
        let mut notifier = CgroupEventNotifier::new(&state.cgroup_paths.unified)?;
        tokio::task::spawn(async move {
            if notifier.wait().await.is_ok() {
                send.send_replace(true);
            }
        });

        let cgroup_device_filter: Box<dyn DeviceAccessController + Send> =
            if let Some(device_cgroup) = &state.cgroup_paths.devices {
                Box::new(DeviceAccessControllerV1::new(device_cgroup)?)
            } else {
                Box::new(DeviceAccessControllerV2::new(&state.cgroup_paths.unified)?)
            };

        ensure!(config.process.user.uid != u32::MAX && config.process.user.gid != u32::MAX);

        Ok(Self {
            uid: unsafe { Uid::from_raw(config.process.user.uid) },
            gid: unsafe { Gid::from_raw(config.process.user.gid) },
            pid: Pid::from_raw(state.init_process_pid.try_into()?).context("Invalid PID")?,
            wait: recv,
            cgroup_device_filter: Mutex::new(cgroup_device_filter),
        })
    }

    pub async fn kill(&self, signal: Signal) -> Result<()> {
        rustix::process::kill_process(self.pid, signal)?;
        Ok(())
    }

    pub async fn wait(&self) -> Result<()> {
        self.wait
            .clone()
            .wait_for(|state| *state)
            .await
            .context("Failed to wait for container")?;
        Ok(())
    }

    pub async fn mknod(&self, node: &Path, (major, minor): (u32, u32)) -> Result<()> {
        crate::util::namespace::MntNamespace::of_pid(self.pid)?.enter(|| {
            if let Some(parent) = node.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::remove_file(node);
            rustix::fs::mknodat(
                rustix::fs::CWD,
                node,
                FileType::CharacterDevice,
                Mode::from(0o644),
                rustix::fs::makedev(major, minor),
            )?;
            if !self.uid.is_root() {
                rustix::fs::chown(node, Some(self.uid), Some(self.gid))?;
            }
            Ok(())
        })?
    }

    pub async fn symlink(&self, source: &Path, link: &Path) -> Result<()> {
        crate::util::namespace::MntNamespace::of_pid(self.pid)?.enter(|| {
            if let Some(parent) = link.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::remove_file(link);
            std::os::unix::fs::symlink(source, link)?;
            // No need to chown symlink. Permission is determined by the target.
            Ok(())
        })?
    }

    pub async fn rm(&self, node: &Path) -> Result<()> {
        crate::util::namespace::MntNamespace::of_pid(self.pid)?.enter(|| {
            let _ = std::fs::remove_file(node);
        })
    }

    pub async fn device(&self, (major, minor): (u32, u32), access: Access) -> Result<()> {
        self.cgroup_device_filter.lock().await.set_permission(
            DeviceType::Character,
            major,
            minor,
            access,
        )?;
        Ok(())
    }
}
