use anyhow::{Context, Result};

fn main() -> Result<()> {
    // We need to rerun the build script if any files in the cgroup_device_filter change.
    for entry in walkdir::WalkDir::new("cgroup_device_filter")
        .into_iter()
        .filter_entry(|entry| {
            entry
                .file_name()
                .to_str()
                .map(|s| s != "target")
                .unwrap_or(true)
        })
    {
        let entry = entry?;
        if entry.file_type().is_file() {
            println!(
                "cargo:rerun-if-changed={}",
                entry.path().to_str().context("file name not UTF-8")?
            );
        }
    }

    // Run cargo to compile the eBPF program.
    let status = std::process::Command::new("cargo")
        .current_dir("cgroup_device_filter")
        .args(["build", "--release"])
        .status()?;

    if !status.success() {
        anyhow::bail!("Failed to build eBPF program");
    }

    Ok(())
}
