use std::fs::File;
use std::os::fd::AsFd;

use anyhow::Result;
use rustix::process::Pid;
use rustix::thread::{LinkNameSpaceType, UnshareFlags};

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
                    Ok(f())
                })
                .join()
                .map_err(|_| anyhow::anyhow!("work thread panicked"))?
        })
    }
}
