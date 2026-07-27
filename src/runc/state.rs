//! The `state.json` `runc` writes for a container it has created.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Where the container's cgroups live, keyed by controller.
#[non_exhaustive]
#[derive(Debug, Deserialize)]
pub struct CgroupPaths {
    /// The cgroup v2 unified hierarchy path, e.g.
    /// `/sys/fs/cgroup/system.slice/docker-<id>.scope`. This is the cgroup we attach our device
    /// filter to and whose `cgroup.events` we watch to tell when the container has stopped.
    #[serde(rename = "")]
    pub unified: PathBuf,
    /// The cgroup v1 `devices` controller path. Its presence means the container is on cgroup v1,
    /// which we no longer support, so [`crate::runc::Container::new`] rejects it.
    pub devices: Option<PathBuf>,
}

/// runc `libcontainer` states.
///
/// Only states that we need are implemented here.
/// Ref: <https://github.com/opencontainers/runc/blob/6a2813f16ad4e3be44903f6fb499c02837530ad5/libcontainer/container_linux.go#L52>
#[non_exhaustive]
#[derive(Debug, Deserialize)]
pub struct State {
    /// PID, on the host, of the container's init process. Used to reach the container's mount, user
    /// and network namespaces via `/proc`.
    pub init_process_pid: u32,
    pub cgroup_paths: CgroupPaths,
}

impl State {
    /// Parse from the contents of a `state.json`.
    pub fn from_str(s: &str) -> Result<Self> {
        serde_json::from_str(s).context("Cannot parse state.json")
    }

    /// Read and parse a `state.json` at `path`.
    pub fn from_state(path: &Path) -> Result<Self> {
        Self::from_str(&std::fs::read_to_string(path).context("Cannot read state.json")?)
    }

    /// Read the state of container `id` from `runc`'s state directory (`--root`).
    ///
    /// Only valid once `runc create` has returned; before that the file does not exist.
    pub fn from_root_and_id(root: &Path, id: &str) -> Result<Self> {
        Self::from_state(&root.join(format!("{}/state.json", id)))
    }
}
