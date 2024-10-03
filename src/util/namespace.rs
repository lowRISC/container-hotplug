use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;

use anyhow::Result;
use rustix::fs::{Gid, Uid};
use rustix::process::Pid;
use rustix::thread::{CapabilitiesSecureBits, LinkNameSpaceType, UnshareFlags};

pub struct MntNamespace {
    fd: File,
}

impl MntNamespace {
    /// Open the mount namespace of a process.
    pub fn of_pid(pid: Pid) -> Result<MntNamespace> {
        let path = format!("/proc/{}/ns/mnt", pid.as_raw_nonzero());
        let fd = File::open(path)?;
        Ok(MntNamespace { fd })
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
                        self.fd.as_fd(),
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
                    // (Yes this means that even if CAP_MKNOD is granted to the container, you canot
                    // create device nodes within it.)
                    //
                    // https://elixir.bootlin.com/linux/v6.11.1/source/fs/namei.c#L4073
                    // https://elixir.bootlin.com/linux/v6.11.1/source/include/linux/cred.h#L111
                    let metadata = std::fs::metadata("/")?;
                    let uid = metadata.uid();
                    let gid = metadata.gid();

                    // By default `setuid` will drop capabilities when transitioning from root
                    // to non-root user. This bit prevents it so our code still have superpower.
                    rustix::thread::set_capabilities_secure_bits(
                        CapabilitiesSecureBits::NO_SETUID_FIXUP,
                    )?;

                    rustix::thread::set_thread_uid(unsafe { Uid::from_raw(uid) })?;
                    rustix::thread::set_thread_gid(unsafe { Gid::from_raw(gid) })?;

                    Ok(f())
                })
                .join()
                .map_err(|_| anyhow::anyhow!("work thread panicked"))?
        })
    }
}
