use std::fs::{File, Permissions};
use std::io::{BufRead, BufReader, Seek};
use std::os::fd::AsFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result, bail};
use rustix::fs::{FileType, Mode};
use rustix::mount::{FsMountFlags, FsOpenFlags, MountAttrFlags, MoveMountFlags, UnmountFlags};
use rustix::process::{Pid, Signal};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::Mutex;

use crate::cgroup::{Access, DeviceAccessController, DeviceType};

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
        let Ok(_) = file.seek(std::io::SeekFrom::Start(0)) else {
            return Ok(false);
        };
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else {
                // IO errors on cgroup.events file indicate that the cgroup has been deleted, so
                // it is no longer populated.
                return Ok(false);
            };
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
    // Uid and gid of the primary container user.
    // Note that they're inside the user namespace (if any).
    uid: u32,
    gid: u32,
    pid: Pid,
    wait: tokio::sync::watch::Receiver<bool>,
    cgroup_device_filter: Mutex<DeviceAccessController>,
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

        // runc configures systemd to also perform device filtering.
        // The removal of systemd's filtering is insufficient since after daemon-reload (or maybe
        // some other triggers as well), systemd will reconcile and add it back, which disrupts
        // container-hotplug's operation.
        // So we'll also go ahead and remove these configuration files. Ignore errors if any since
        // the cgroup might be handled by runc directly if `--cgroup-manager=cgroupfs` is used.
        let cgroup_name = state
            .cgroup_paths
            .unified
            .file_name()
            .context("cgroup doesn't have file name")?
            .to_str()
            .context("cgroup name is not UTF-8")?;
        let _ = std::fs::remove_file(format!(
            "/run/systemd/transient/{cgroup_name}.d/50-DeviceAllow.conf"
        ));
        let _ = std::fs::remove_file(format!(
            "/run/systemd/transient/{cgroup_name}.d/50-DevicePolicy.conf"
        ));

        anyhow::ensure!(
            state.cgroup_paths.devices.is_none(),
            "cgroupv1 is no longer supported"
        );

        let cgroup_device_filter = DeviceAccessController::new(&state.cgroup_paths.unified)?;

        let container = Self {
            uid: config.process.user.uid,
            gid: config.process.user.gid,
            pid: Pid::from_raw(state.init_process_pid.try_into()?).context("Invalid PID")?,
            wait: recv,
            cgroup_device_filter: Mutex::new(cgroup_device_filter),
        };

        container.remount_dev()?;

        Ok(container)
    }

    pub fn pid(&self) -> Pid {
        self.pid
    }

    /// Remount /dev inside the init namespace.
    ///
    /// When user namespace is used, the /dev created by runc will be mounted inside the user namespace,
    /// and will automatically gain SB_I_NODEV flag as a kernel security measure.
    ///
    /// This is doing no favour for us because that flag will cause device node within it to be unopenable.
    fn remount_dev(&self) -> Result<()> {
        let ns = crate::util::namespace::MntNamespace::of_pid(self.pid)?;
        if !ns.in_user_ns() {
            return Ok(());
        }

        log::info!("Remount /dev to allow device node access");

        // Create a tmpfs and mount in the init namespace.
        // Note that while we have "mounted" it, it is not associated with any mount point yet.
        // The actual mounting will happen after we moved into the mount namespace.
        let dev_fs = rustix::mount::fsopen("tmpfs", FsOpenFlags::empty())?;
        rustix::mount::fsconfig_create(dev_fs.as_fd())?;
        let dev_mnt = rustix::mount::fsmount(
            dev_fs.as_fd(),
            FsMountFlags::FSMOUNT_CLOEXEC,
            MountAttrFlags::empty(),
        )?;

        ns.with(|| -> Result<_> {
            // Don't interfere us setting the desired mode!
            rustix::process::umask(Mode::empty());

            // Move the existing mount elsewhere.
            std::fs::create_dir("/olddev")?;
            rustix::mount::mount_move("/dev", "/olddev")?;

            // Move to our newly created `/dev` mount.
            rustix::mount::move_mount(
                dev_mnt.as_fd(),
                "",
                rustix::fs::CWD,
                "/dev",
                MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
            )?;

            // Make sure the /dev is now owned by the container root not host root.
            std::os::unix::fs::chown("/dev", Some(ns.uid(0)?), Some(ns.gid(0)?))?;
            std::fs::set_permissions("/dev", Permissions::from_mode(0o755))?;

            for file in std::fs::read_dir("/olddev")? {
                let file = file?;
                let metadata = file.metadata()?;
                let new_path = Path::new("/dev").join(file.file_name());

                if file.file_name() == "console" {
                    // `console` is special, it's a file but it should be bind-mounted.
                    drop(
                        std::fs::OpenOptions::new()
                            .create(true)
                            .write(true)
                            .open(&new_path)?,
                    );
                    rustix::mount::mount_move(file.path(), new_path)?;
                } else if metadata.file_type().is_dir() {
                    // This is a mount point, e.g. pts, mqueue, shm.
                    std::fs::create_dir(&new_path)?;
                    rustix::mount::mount_move(file.path(), new_path)?;
                } else if metadata.file_type().is_symlink() {
                    // Recreate symlinks
                    let target = std::fs::read_link(file.path())?;
                    std::os::unix::fs::symlink(target, new_path)?;
                } else if metadata.file_type().is_char_device() {
                    // Recreate device
                    let dev = metadata.rdev();
                    rustix::fs::mknodat(
                        rustix::fs::CWD,
                        &new_path,
                        FileType::CharacterDevice,
                        Mode::from_raw_mode(metadata.mode()),
                        dev,
                    )?;

                    // The old file might be a bind mount. Try umount it.
                    let _ = rustix::mount::unmount(file.path(), UnmountFlags::DETACH);
                } else {
                    bail!("Unknown file present in /dev");
                }
            }

            // Now we have moved everything to the new /dev, obliterate the old one.
            rustix::mount::unmount("/olddev", UnmountFlags::DETACH)?;
            std::fs::remove_dir("/olddev")?;

            Ok(())
        })??;

        Ok(())
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

    pub async fn mknod(
        &self,
        node: &Path,
        ty: DeviceType,
        (major, minor): (u32, u32),
    ) -> Result<()> {
        let ns = crate::util::namespace::MntNamespace::of_pid(self.pid)?;
        ns.with(|| {
            if let Some(parent) = node.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::remove_file(node);
            rustix::fs::mknodat(
                rustix::fs::CWD,
                node,
                if ty == DeviceType::Character {
                    FileType::CharacterDevice
                } else {
                    FileType::BlockDevice
                },
                Mode::from(0o644),
                rustix::fs::makedev(major, minor),
            )?;
            std::os::unix::fs::chown(node, Some(ns.uid(self.uid)?), Some(ns.gid(self.gid)?))?;
            Ok(())
        })?
    }

    pub async fn symlink(&self, source: &Path, link: &Path) -> Result<()> {
        crate::util::namespace::MntNamespace::of_pid(self.pid)?.with(|| {
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
        crate::util::namespace::MntNamespace::of_pid(self.pid)?.with(|| {
            let _ = std::fs::remove_file(node);
        })
    }

    pub async fn device(
        &self,
        ty: DeviceType,
        (major, minor): (u32, u32),
        access: Access,
    ) -> Result<()> {
        self.cgroup_device_filter
            .lock()
            .await
            .set_permission(ty, major, minor, access)?;
        Ok(())
    }
}
