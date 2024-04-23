use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

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
/// Ref: https://github.com/opencontainers/runtime-spec/blob/main/config.md
#[non_exhaustive]
#[derive(Debug, Deserialize)]
pub struct Config {
    pub process: Process,
    #[serde(default)]
    pub annotations: HashMap<String, String>,
}

impl Config {
    pub fn from_str(s: &str) -> Result<Self> {
        serde_json::from_str(s).context("Cannot parse config.json")
    }

    pub fn from_config(path: &Path) -> Result<Self> {
        Self::from_str(&std::fs::read_to_string(path).context("Cannot read config.json")?)
    }

    pub fn from_bundle(bundle: &Path) -> Result<Self> {
        let config = bundle.join("config.json");
        Self::from_config(&config)
    }
}
