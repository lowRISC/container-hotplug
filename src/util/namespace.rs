//! Entering a container's namespaces to act on its behalf.
//!
//! To create a device node or a mount that the container can see, we must be in its mount namespace;
//! to send it a netlink message that it trusts, we must be in its network namespace with a
//! credential it recognises. Both are one-way operations for the thread that performs them, so
//! [`MntNamespace::with`] and [`NetNamespace::with`] run the work on a scoped thread that is then
//! discarded, leaving the rest of the process where it was.
//!
//! When the container has a user namespace, IDs must additionally be translated through it, since
//! the container's root is some other UID on the host. [`UserNamespace`] handles that, and is what
//! both of the above namespaces build on.

use std::fs::File;
use std::ops::Deref;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::Path;

use anyhow::{Context, Result};
use rustix::fs::{Gid, Uid};
use rustix::process::Pid;
use rustix::thread::{CapabilitiesSecureBits, LinkNameSpaceType, UnshareFlags};

/// `struct mount_attr`, the argument of `mount_setattr(2)`.
#[repr(C)]
#[derive(Default)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

/// Apply `attr` to `mount` itself, which must be a mount fd (e.g. from `open_tree`).
///
/// `mount_setattr` is not wrapped by rustix, so this is the raw syscall.
fn mount_setattr(mount: BorrowedFd, attr: &MountAttr) -> Result<()> {
    // SAFETY: `attr` is a valid `struct mount_attr` of the size we pass, and the path is an
    // empty C string used with `AT_EMPTY_PATH` to refer to `mount` itself.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            mount.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            attr as *const MountAttr,
            std::mem::size_of::<MountAttr>(),
        )
    };
    if ret != 0 {
        return Err(std::io::Error::last_os_error()).context("mount_setattr failed");
    }
    Ok(())
}

/// Make a detached mount idmapped with respect to a user namespace.
///
/// This makes files owned by UID/GID X on the host appear as owned by the ID that X maps to
/// inside `user_ns`, which is what allows a container user to access them.
///
/// `mount` must be a detached mount (e.g. as returned by `open_tree` with `OPEN_TREE_CLONE`);
/// an already attached mount cannot be idmapped.
///
/// Not all filesystems support idmapped mounts, in which case this fails with `EINVAL`.
pub fn idmap_mount(mount: BorrowedFd, user_ns: BorrowedFd) -> Result<()> {
    const MOUNT_ATTR_IDMAP: u64 = 0x0010_0000;

    mount_setattr(
        mount,
        &MountAttr {
            attr_set: MOUNT_ATTR_IDMAP,
            userns_fd: user_ns.as_raw_fd() as u64,
            ..Default::default()
        },
    )
}

/// Remove a mount from the propagation group it was cloned into.
///
/// `open_tree` with `OPEN_TREE_CLONE` is the new-mount-API equivalent of `mount --bind`, and
/// cloning a *shared* mount makes the clone a peer of the source rather than a private mount.
/// That is rarely what we want: any mount later made underneath the clone would then propagate
/// to every peer, including back into the mount namespace we cloned from, where nothing is
/// tracking it to clean it up.
///
/// Call this on the detached mount, before attaching it with `move_mount`.
pub fn make_mount_private(mount: BorrowedFd) -> Result<()> {
    // `MS_PRIVATE`, which `mount_setattr` takes in the `propagation` field.
    const MS_PRIVATE: u64 = 1 << 18;

    mount_setattr(
        mount,
        &MountAttr {
            propagation: MS_PRIVATE,
            ..Default::default()
        },
    )
}

/// A parsed `/proc/<pid>/{uid,gid}_map`.
pub struct IdMap {
    /// Ranges of `(id inside the namespace, id outside, length)`.
    map: Vec<(u32, u32, u32)>,
}

impl IdMap {
    /// Read and parse a map file.
    fn read(path: &Path) -> Result<Self> {
        Self::parse(&std::fs::read_to_string(path)?)
    }

    /// Parse the contents of a map file: one `inside outside count` triple per line.
    fn parse(content: &str) -> Result<Self> {
        let mut map = Vec::new();
        for line in content.lines() {
            let mut words = line.split_ascii_whitespace();
            let inside = words.next().context("unexpected id_map")?.parse()?;
            let outside = words.next().context("unexpected id_map")?.parse()?;
            let count = words.next().context("unexpected id_map")?.parse()?;
            map.push((inside, outside, count));
        }
        Ok(Self { map })
    }

    /// Translate an ID inside the namespace to the corresponding ID outside it.
    ///
    /// Returns [`None`] if the ID is not mapped.
    fn translate(&self, id: u32) -> Option<u32> {
        for &(inside, outside, count) in self.map.iter() {
            if (inside..inside.checked_add(count)?).contains(&id) {
                return (id - inside).checked_add(outside);
            }
        }
        None
    }
}

/// A process's user namespace, together with its ID mappings.
///
/// A container may well not have one, in which case the mappings are the identity and
/// [`UserNamespace::in_user_ns`] is false.
pub struct UserNamespace {
    user_fd: File,
    uid_map: IdMap,
    gid_map: IdMap,
}

impl UserNamespace {
    /// Open the user namespace of a process.
    pub fn of_pid(pid: Pid) -> Result<Self> {
        let user_fd = File::open(format!("/proc/{}/ns/user", pid.as_raw_nonzero()))?;
        let uid_map = IdMap::read(format!("/proc/{}/uid_map", pid.as_raw_nonzero()).as_ref())?;
        let gid_map = IdMap::read(format!("/proc/{}/gid_map", pid.as_raw_nonzero()).as_ref())?;
        Ok(Self {
            user_fd,
            uid_map,
            gid_map,
        })
    }

    /// File descriptor referring to the user namespace.
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.user_fd.as_fd()
    }

    /// Check if we're in an user namespace.
    ///
    /// Determined from the mappings rather than the namespace itself: a namespace that maps every ID
    /// to itself is indistinguishable from none for our purposes.
    pub fn in_user_ns(&self) -> bool {
        !(self.uid_map.map == [(0, 0, u32::MAX)] && self.gid_map.map == [(0, 0, u32::MAX)])
    }

    /// Translate a UID inside the namespace to the corresponding UID on the host.
    ///
    /// So `uid(0)` is the host UID that the container's root user is mapped to.
    pub fn uid(&self, uid: u32) -> Result<u32> {
        self.uid_map.translate(uid).context("UID overflows")
    }

    /// Translate a GID inside the namespace to the corresponding GID on the host.
    pub fn gid(&self, gid: u32) -> Result<u32> {
        self.gid_map.translate(gid).context("GID overflows")
    }

    /// "Enter" the user namespace.
    ///
    /// This operation is not reversible.
    ///
    /// This does not actually enter the user namespace, but rather just switch to become the root
    /// user inside the namespace.
    ///
    /// Entering the user namespace turns out to be problematic.
    /// The reason seems to be this line [1]:
    /// which means `CAP_MKNOD` capability of the *init* namespace is needed.
    /// However task's associated security context is all relative to its current
    /// user namespace [2], so once you enter a user namespace there's no way of getting
    /// back `CAP_MKNOD` of the init namespace anymore.
    /// (Yes this means that even if CAP_MKNOD is granted to the container, you cannot
    /// create device nodes within it.)
    ///
    /// [1]: https://elixir.bootlin.com/linux/v6.11.1/source/fs/namei.c#L4073
    /// [2]: https://elixir.bootlin.com/linux/v6.11.1/source/include/linux/cred.h#L111
    pub fn enter(&self) -> Result<()> {
        // By default `setuid` will drop capabilities when transitioning from root
        // to non-root user. This bit prevents it so our code still have superpower.
        rustix::thread::set_capabilities_secure_bits(CapabilitiesSecureBits::NO_SETUID_FIXUP)?;

        rustix::thread::set_thread_uid(Uid::from_raw(self.uid(0)?))?;
        rustix::thread::set_thread_gid(Gid::from_raw(self.gid(0)?))?;
        Ok(())
    }
}

/// A process's mount namespace.
///
/// Derefs to the [`UserNamespace`] of the same process, since acting inside a mount namespace
/// generally also needs its ID mappings.
pub struct MntNamespace {
    mnt_fd: File,
    user_ns: UserNamespace,
}

impl Deref for MntNamespace {
    type Target = UserNamespace;

    fn deref(&self) -> &UserNamespace {
        &self.user_ns
    }
}

impl MntNamespace {
    /// Open the mount namespace of a process.
    pub fn of_pid(pid: Pid) -> Result<MntNamespace> {
        let mnt_fd = File::open(format!("/proc/{}/ns/mnt", pid.as_raw_nonzero()))?;
        let user_ns = UserNamespace::of_pid(pid)?;
        Ok(MntNamespace { mnt_fd, user_ns })
    }

    /// Enter the mount namespace.
    ///
    /// This operation is not reversible.
    pub fn enter(&self) -> Result<()> {
        // Unshare FS for this specific thread so we can switch to another namespace.
        // Not doing this will cause EINVAL when switching to namespaces.
        // SAFETY: The safety requirement only concerns `UnshareFlags::FILES`, which would
        // detach this thread's file descriptor table; we only unshare the filesystem context.
        unsafe { rustix::thread::unshare_unsafe(UnshareFlags::FS)? };

        // Switch this particular thread to the container's mount namespace.
        rustix::thread::move_into_link_name_space(
            self.mnt_fd.as_fd(),
            Some(LinkNameSpaceType::Mount),
        )?;

        // If user namespace is used, we must act like the root user *inside*
        // namespace to be able to create files properly (otherwise EOVERFLOW
        // will be returned when creating file).
        self.user_ns.enter()?;
        Ok(())
    }

    /// Execute `f` inside the mount namespace, as the container's root user.
    ///
    /// Paths seen by `f` are the container's, so `/dev/ttyACM0` means the container's device node,
    /// not the host's. Errors from entering the namespace, and a panic in `f`, are reported as the
    /// outer [`Err`]; `f`'s own result is returned as-is.
    pub fn with<T: Send, F: FnOnce() -> T + Send>(&self, f: F) -> Result<T> {
        // To avoid messing with rest of the process, we do everything in a new thread.
        // Use scoped thread to avoid 'static bound (we need to access fd).
        std::thread::scope(|scope| {
            scope
                .spawn(|| -> Result<T> {
                    self.enter()?;
                    Ok(f())
                })
                .join()
                .map_err(|_| anyhow::anyhow!("work thread panicked"))?
        })
    }
}

/// A process's network namespace.
pub struct NetNamespace {
    net_fd: File,
    user_ns: UserNamespace,
}

impl NetNamespace {
    /// Open the network namespace of a process.
    pub fn of_pid(pid: Pid) -> Result<NetNamespace> {
        let net_fd = File::open(format!("/proc/{}/ns/net", pid.as_raw_nonzero()))?;
        let user_ns = UserNamespace::of_pid(pid)?;
        Ok(NetNamespace { net_fd, user_ns })
    }

    /// Enter the network namespace.
    ///
    /// This operation is not reversible.
    pub fn enter(&self) -> Result<()> {
        // Switch this particular thread to the container's network namespace.
        rustix::thread::move_into_link_name_space(
            self.net_fd.as_fd(),
            Some(LinkNameSpaceType::Network),
        )?;

        // Similar to mount namespace, we also want to behave as container root.
        // This is so that SCM credentials are seen properly.
        self.user_ns.enter()?;
        Ok(())
    }

    /// Execute `f` inside the network namespace, as the container's root user.
    ///
    /// Being the container's root user matters as much as the namespace itself here: it is what makes
    /// libudev in the container accept the SCM credentials on messages we send. See
    /// [`crate::hotplug::UdevSender`].
    pub fn with<T: Send, F: FnOnce() -> T + Send>(&self, f: F) -> Result<T> {
        // To avoid messing with rest of the process, we do everything in a new thread.
        // Use scoped thread to avoid 'static bound (we need to access fd).
        std::thread::scope(|scope| {
            scope
                .spawn(|| -> Result<T> {
                    self.enter()?;
                    Ok(f())
                })
                .join()
                .map_err(|_| anyhow::anyhow!("work thread panicked"))?
        })
    }
}
