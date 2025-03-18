use std::fs::File;
use std::os::fd::AsFd;
use std::path::Path;

use anyhow::{Context, Result};
use rustix::fs::{Gid, Uid};
use rustix::process::Pid;
use rustix::thread::{CapabilitiesSecureBits, LinkNameSpaceType, UnshareFlags};

pub struct IdMap {
    map: Vec<(u32, u32, u32)>,
}

impl IdMap {
    fn read(path: &Path) -> Result<Self> {
        Self::parse(&std::fs::read_to_string(path)?)
    }

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

    fn translate(&self, id: u32) -> Option<u32> {
        for &(inside, outside, count) in self.map.iter() {
            if (inside..inside.checked_add(count)?).contains(&id) {
                return (id - inside).checked_add(outside);
            }
        }
        None
    }
}

pub struct MntNamespace {
    mnt_fd: File,
    uid_map: IdMap,
    gid_map: IdMap,
}

impl MntNamespace {
    /// Open the mount namespace of a process.
    pub fn of_pid(pid: Pid) -> Result<MntNamespace> {
        let mnt_fd = File::open(format!("/proc/{}/ns/mnt", pid.as_raw_nonzero()))?;
        let uid_map = IdMap::read(format!("/proc/{}/uid_map", pid.as_raw_nonzero()).as_ref())?;
        let gid_map = IdMap::read(format!("/proc/{}/gid_map", pid.as_raw_nonzero()).as_ref())?;
        Ok(MntNamespace {
            mnt_fd,
            uid_map,
            gid_map,
        })
    }

    /// Check if we're in an user namespace.
    pub fn in_user_ns(&self) -> bool {
        !(self.uid_map.map == &[(0, 0, u32::MAX)] && self.gid_map.map == &[(0, 0, u32::MAX)])
    }

    /// Translate user ID into a UID in the namespace.
    pub fn uid(&self, uid: u32) -> Result<u32> {
        Ok(self.uid_map.translate(uid).context("UID overflows")?)
    }

    /// Translate group ID into a GID in the namespace.
    pub fn gid(&self, gid: u32) -> Result<u32> {
        Ok(self.gid_map.translate(gid).context("GID overflows")?)
    }

    /// Enter the mount namespace.
    pub fn enter<T: Send, F: FnOnce() -> T + Send>(&self, f: F) -> Result<T> {
        // To avoid messing with rest of the process, we do everything in a new thread.
        // Use scoped thread to avoid 'static bound (we need to access fd).
        std::thread::scope(|scope| {
            scope
                .spawn(|| -> Result<T> {
                    // Unshare FS for this specific thread so we can switch to another namespace.
                    // Not doing this will cause EINVAL when switching to namespaces.
                    rustix::thread::unshare(UnshareFlags::FS)?;

                    // Switch this particular thread to the container's mount namespace.
                    rustix::thread::move_into_link_name_space(
                        self.mnt_fd.as_fd(),
                        Some(LinkNameSpaceType::Mount),
                    )?;

                    // If user namespace is used, we must act like the root user *inside*
                    // namespace to be able to create files properly (otherwise EOVERFLOW
                    // will be returned when creating file).
                    //
                    // Entering the user namespace turns out to be problematic.
                    // The reason seems to be this line [1]:
                    // which means `CAP_MKNOD` capability of the *init* namespace is needed.
                    // However task's associated security context is all relative to its current
                    // user namespace [2], so once you enter a user namespace there's no way of getting
                    // back `CAP_MKNOD` of the init namespace anymore.
                    // (Yes this means that even if CAP_MKNOD is granted to the container, you cannot
                    // create device nodes within it.)
                    //
                    // [1]: https://elixir.bootlin.com/linux/v6.11.1/source/fs/namei.c#L4073
                    // [2]: https://elixir.bootlin.com/linux/v6.11.1/source/include/linux/cred.h#L111

                    // By default `setuid` will drop capabilities when transitioning from root
                    // to non-root user. This bit prevents it so our code still have superpower.
                    rustix::thread::set_capabilities_secure_bits(
                        CapabilitiesSecureBits::NO_SETUID_FIXUP,
                    )?;

                    rustix::thread::set_thread_uid(Uid::from_raw(self.uid(0)?))?;
                    rustix::thread::set_thread_gid(Gid::from_raw(self.gid(0)?))?;

                    Ok(f())
                })
                .join()
                .map_err(|_| anyhow::anyhow!("work thread panicked"))?
        })
    }
}
