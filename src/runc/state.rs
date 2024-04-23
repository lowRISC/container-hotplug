use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[non_exhaustive]
#[derive(Debug, Deserialize)]
pub struct CgroupPaths {
    #[serde(rename = "")]
    pub unified: PathBuf,
    pub devices: Option<PathBuf>,
}

/// runc `libcontainer` states.
///
/// Only states that we need are implemented here.
/// Ref: https://github.com/opencontainers/runc/blob/6a2813f16ad4e3be44903f6fb499c02837530ad5/libcontainer/container_linux.go#L52
#[non_exhaustive]
#[derive(Debug, Deserialize)]
pub struct State {
    pub init_process_pid: u32,
    pub cgroup_paths: CgroupPaths,
}

impl State {
    pub fn from_str(s: &str) -> Result<Self> {
        serde_json::from_str(s).context("Cannot parse state.json")
    }

    pub fn from_state(path: &Path) -> Result<Self> {
        Self::from_str(&std::fs::read_to_string(path).context("Cannot read state.json")?)
    }

    pub fn from_root_and_id(root: &Path, id: &str) -> Result<Self> {
        Self::from_state(&root.join(format!("{}/state.json", id)))
    }
}
