use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{bail, ensure, Error, Result};

#[derive(Clone)]
pub struct Symlink(String, PathBuf);

fn is_hex4(val: &str) -> bool {
    val.len() == 4 && val.chars().all(|c| c.is_ascii_hexdigit())
}

fn is_number(val: &str) -> bool {
    val.len() >= 1 && val.chars().all(|c| c.is_ascii_digit())
}

impl FromStr for Symlink {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<_> = s.split("=").collect();

        ensure!(
            parts.len() == 2,
            "Symlink format should be `<PREFIX>:<DEVICE>=<PATH>`, found `{s}`"
        );

        let dev = parts[0];
        let path = parts[1];

        ensure!(
            path.starts_with("/") && !path.ends_with("/"),
            "Symlink PATH should be an absolute file path, found `{path}`."
        );

        let path = PathBuf::from(path);

        let mut parts = dev.split(":");
        let prefix = parts.next().unwrap();
        let parts: Vec<_> = parts.collect();
        let dev = parts.join(":");

        match prefix {
            "usb" => {
                ensure!(parts.len() == 3, "Symlink DEVICE format for usb should be `<VID>:<PID>:<INTERFACE>`, found `{dev}`.");

                let vid = parts[0];
                let pid = parts[1];
                let ifc = parts[2];

                ensure!(
                    is_hex4(vid),
                    "USB symlink VID should be a 4 digit hex number, found `{vid}`"
                );
                ensure!(
                    is_hex4(pid),
                    "USB symlink PID should be a 4 digit hex number, found `{pid}`"
                );
                ensure!(
                    is_number(ifc),
                    "USB symlink INTERFACE should be a number, found `{ifc}`"
                );

                let vid = vid.to_ascii_lowercase();
                let pid = pid.to_ascii_lowercase();

                Ok(Symlink(format!("usb:{vid}:{pid}:{ifc:0>2}"), path))
            }
            _ => {
                bail!("Symlink PREFIX should be `usb`, found `{prefix}`");
            }
        }
    }
}

impl Symlink {
    pub fn id(&self) -> &str {
        &self.0
    }
    pub fn path(&self) -> &Path {
        &self.1
    }
}
