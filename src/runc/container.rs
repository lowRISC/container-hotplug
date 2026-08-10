//! Manipulating a running container from the outside.

use std::fs::{File, Permissions};
use std::io::{BufRead, BufReader, Seek};
use std::os::fd::AsFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use rustix::fs::{FileType, Mode};
use rustix::mount::{
    FsMountFlags, FsOpenFlags, MountAttrFlags, MoveMountFlags, OpenTreeFlags, UnmountFlags,
};
use rustix::process::{Pid, Signal};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::Mutex;

use crate::cgroup::{Access, DeviceAccessController, DeviceType};

/// Watches a cgroup's `cgroup.events` file to detect when it becomes unpopulated.
///
/// The kernel signals changes to that file with `POLLPRI`, and reports `POLLERR` once the cgroup is
/// deleted, so a single fd covers both "all processes exited" and "cgroup gone".
struct CgroupEventNotifier {
    file: AsyncFd<File>,
}

impl CgroupEventNotifier {
    /// Open the `cgroup.events` file of `cgroup`.
    fn new(cgroup: &Path) -> Result<Self> {
        let file = AsyncFd::with_interest(
            File::open(cgroup.join("cgroup.events")).context("Cannot open cgroup.events")?,
            Interest::PRIORITY | Interest::ERROR,
        )?;
        Ok(Self { file })
    }

    /// Whether the cgroup currently contains any process.
    ///
    /// A deleted cgroup counts as unpopulated, so a container that has been torn down does not
    /// leave us waiting forever.
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

    /// Wait until the cgroup is no longer populated. Returns immediately if it already is not.
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

/// A handle on a container that `runc` has created.
///
/// The methods here are the operations [`crate::hotplug::HotPlug`] needs: granting and revoking
/// device access via the cgroup filter, and creating and removing device nodes, symlinks and sysfs
/// mounts inside the container's namespaces. Everything that touches the container's filesystem runs
/// on a throwaway thread that has entered its mount namespace, via
/// [`MntNamespace::with`](crate::util::namespace::MntNamespace::with).
///
/// Dropping this detaches the device filter's pin, so it should be kept alive for as long as the
/// container is running.
pub struct Container {
    // Uid and gid of the primary container user.
    // Note that they're inside the user namespace (if any).
    uid: u32,
    gid: u32,
    /// PID of the container's init process on the host, used to reach its namespaces.
    pid: Pid,
    /// Set once the container's cgroup becomes unpopulated. See [`Container::wait`].
    wait: tokio::sync::watch::Receiver<bool>,
    cgroup_device_filter: Mutex<DeviceAccessController>,
    /// Whether we have already complained about sysfs not supporting idmapped mounts.
    /// Used to warn once instead of once per device.
    sysfs_idmap_warned: AtomicBool,
}

impl Container {
    /// Take control of the container described by `config` and `state`.
    ///
    /// This has side effects on the live container, so it must be called after `runc create` has
    /// returned and before the container is started:
    ///
    /// * device filtering is taken over from the container manager
    ///   ([`DeviceAccessController::new`]), including deleting systemd's transient `DeviceAllow`
    ///   drop-ins, which systemd would otherwise reconcile back on a `daemon-reload` and undo our
    ///   filter;
    /// * a watcher is spawned for the container's cgroup, backing [`Container::wait`];
    /// * `/dev` is remounted if the container uses a user namespace ([`Container::remount_dev`]).
    ///
    /// Fails if the container is on cgroup v1, which is no longer supported.
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
            sysfs_idmap_warned: AtomicBool::new(false),
        };

        container.remount_dev()?;

        Ok(container)
    }

    /// PID of the container's init process, on the host.
    pub fn pid(&self) -> Pid {
        self.pid
    }

    /// Remount /dev inside the init namespace.
    ///
    /// When user namespace is used, the /dev created by runc will be mounted inside the user namespace,
    /// and will automatically gain SB_I_NODEV flag as a kernel security measure.
    ///
    /// This is doing no favour for us because that flag will cause device node within it to be unopenable.
    ///
    /// The replacement is a fresh tmpfs created in the initial namespace, into which everything from
    /// the old `/dev` is moved: submounts (`pts`, `shm`, `mqueue`, and the bind-mounted `console`)
    /// are moved across, symlinks recreated, and device nodes re-`mknod`ed. It is owned by the
    /// container's root user, so the container can create entries under it.
    ///
    /// Does nothing if the container does not use a user namespace.
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
                    // Only an empty file is needed as the bind-mount target.
                    drop(std::fs::File::create(&new_path)?);
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

    /// Send a signal to the container's init process.
    pub async fn kill(&self, signal: Signal) -> Result<()> {
        rustix::process::kill_process(self.pid, signal)?;
        Ok(())
    }

    /// Wait until every process in the container's cgroup has exited.
    ///
    /// This is deliberately about the cgroup, not just the init process: it stays true once reached,
    /// so it can be awaited repeatedly, and it does not race with `runc delete`.
    pub async fn wait(&self) -> Result<()> {
        self.wait
            .clone()
            .wait_for(|state| *state)
            .await
            .context("Failed to wait for container")?;
        Ok(())
    }

    /// Create a device node inside the container.
    ///
    /// `node` is a path in the container's mount namespace — the same path the device has on the
    /// host. Parent directories are created as needed and any existing file at `node` is replaced.
    /// The node is mode `0644` and owned by the container's primary user, so an unprivileged
    /// entrypoint can use it.
    ///
    /// Access is still governed by the cgroup filter, so pair this with [`Container::device`].
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

    /// Give the container read-write access to a device's sysfs directory.
    ///
    /// sysfs is not namespaced (other than the `net` subsystem), so the directory is already
    /// visible inside the container. However runc mounts `/sys` read-only, so writing to any
    /// attribute is rejected. To allow writes for this device only, we clone the host's
    /// (read-write) sysfs mount and overmount the device's directory with it.
    ///
    /// Note that a device's children appear as subdirectories of its sysfs directory, so binding
    /// a hub also covers everything connected to it.
    pub async fn bind_sysfs(&self, syspath: &Path) -> Result<()> {
        let ns = crate::util::namespace::MntNamespace::of_pid(self.pid)?;

        // Clone the mount while we're still in the initial mount namespace, where `/sys` is
        // writable. The clone is detached and is attached by `move_mount` below.
        let tree = rustix::mount::open_tree(
            rustix::fs::CWD,
            syspath,
            OpenTreeFlags::OPEN_TREE_CLONE | OpenTreeFlags::OPEN_TREE_CLOEXEC,
        )
        .with_context(|| format!("Cannot clone sysfs mount for {}", syspath.display()))?;

        // `/sys` is typically a shared mount (systemd makes `/` rshared at boot), and cloning a
        // shared mount yields a peer of the source rather than a private mount. Since device
        // syspaths nest inside each other, the clone for a device becomes the parent mount of
        // the clones for its children, and each of those would propagate a copy to every peer of
        // the group -- including the host's own `/sys`. Those copies outlive the container, so
        // the host's mount table grows by roughly one entry per device per container start,
        // until it reaches `fs.mount-max` and every further mount fails with `ENOSPC`.
        //
        // Detaching the clone from the peer group stops that. Nothing is lost by it: sysfs
        // device directories are dentries within a single superblock rather than submounts, so
        // nothing under `/sys/devices` relies on propagation to become visible.
        crate::util::namespace::make_mount_private(tree.as_fd()).with_context(|| {
            format!("Cannot make sysfs mount private for {}", syspath.display())
        })?;

        // The attributes are owned by the host root, so a container using a user namespace
        // cannot write to them even through a read-write mount. Idmap the mount to fix up the
        // ownership.
        //
        // sysfs does not support idmapped mounts (as of Linux 6.x it does not set
        // `FS_ALLOW_IDMAP`, so this fails with `EINVAL`), but attempt it anyway so that we
        // benefit if it gains support. Failure is not fatal: the mount is still read-write, it
        // is only DAC that then stands in the way.
        if ns.in_user_ns()
            && let Err(err) = crate::util::namespace::idmap_mount(tree.as_fd(), ns.as_fd())
            && !self.sysfs_idmap_warned.swap(true, Ordering::Relaxed)
        {
            log::warn!(
                "Cannot idmap sysfs mount for {}: {err:#}. \
                 Writing to sysfs attributes requires a container user that maps to host root.",
                syspath.display()
            );
        }

        ns.with(|| -> Result<()> {
            // The mount point must already exist: we cannot create it, as the container's `/sys`
            // is read-only (and it is absent altogether if the container has no `/sys` mounted).
            if !syspath.is_dir() {
                bail!("{} does not exist in the container", syspath.display());
            }

            rustix::mount::move_mount(
                tree.as_fd(),
                "",
                rustix::fs::CWD,
                syspath,
                MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
            )?;
            Ok(())
        })?
        .with_context(|| format!("Cannot mount sysfs directory {}", syspath.display()))
    }

    /// Revert [`Container::bind_sysfs`].
    pub async fn unbind_sysfs(&self, syspath: &Path) -> Result<()> {
        crate::util::namespace::MntNamespace::of_pid(self.pid)?.with(|| {
            // The device is usually gone by the time we get here, in which case the mount is
            // already detached, so ignore errors.
            let _ = rustix::mount::unmount(syspath, UnmountFlags::DETACH);
        })
    }

    /// Create a symlink at `link` pointing to `source`, inside the container.
    ///
    /// Parent directories are created as needed and any existing file at `link` is replaced.
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

    /// Remove a file inside the container, used to undo [`Container::mknod`] and
    /// [`Container::symlink`].
    ///
    /// A file that is already gone is not an error: the container is free to delete entries under
    /// `/dev` itself.
    pub async fn rm(&self, node: &Path) -> Result<()> {
        crate::util::namespace::MntNamespace::of_pid(self.pid)?.with(|| {
            let _ = std::fs::remove_file(node);
        })
    }

    /// Set the container's access to a device, by major/minor number.
    ///
    /// An empty `access` revokes it. See [`DeviceAccessController::set_permission`] for what this
    /// does and does not affect.
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
