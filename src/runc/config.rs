//! The OCI `config.json` of the bundle being created.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// The user the container's entrypoint runs as.
///
/// These IDs are relative to the container's user namespace, if it has one, so they must be
/// translated before use on the host — see [`crate::util::namespace::UserNamespace`].
#[non_exhaustive]
#[derive(Debug, Deserialize)]
pub struct User {
    pub uid: u32,
    pub gid: u32,
}

#[non_exhaustive]
#[derive(Debug, Deserialize)]
pub struct Process {
    pub user: User,
}

/// OCI config.
///
/// Only config that we need are implemented here.
/// Ref: <https://github.com/opencontainers/runtime-spec/blob/main/config.md>
#[non_exhaustive]
#[derive(Debug, Deserialize)]
pub struct Config {
    pub process: Process,
    /// Free-form annotations, which is how the container manager passes us the
    /// `org.lowrisc.hotplug.*` configuration. See [the crate docs](crate) for the ones we read.
    #[serde(default)]
    pub annotations: HashMap<String, String>,
}

impl Config {
    /// Parse from the contents of a `config.json`.
    pub fn from_str(s: &str) -> Result<Self> {
        serde_json::from_str(s).context("Cannot parse config.json")
    }

    /// Read and parse a `config.json` at `path`.
    pub fn from_config(path: &Path) -> Result<Self> {
        Self::from_str(&std::fs::read_to_string(path).context("Cannot read config.json")?)
    }

    /// Read and parse the `config.json` of an OCI bundle directory.
    pub fn from_bundle(bundle: &Path) -> Result<Self> {
        let config = bundle.join("config.json");
        Self::from_config(&config)
    }
}
