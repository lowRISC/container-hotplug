use std::path::PathBuf;

pub type DeviceSummary = (PathBuf, (u64, u64), PathBuf);

pub trait DeviceExt {
    fn device_number(&self) -> Option<(u64, u64)>;
    fn summary(&self) -> Option<DeviceSummary>;
}

impl DeviceExt for udev::Device {
    fn device_number(&self) -> Option<(u64, u64)> {
        self.devnum().map(|devnum| {
            (
                (devnum & 0xfff00) >> 8,
                (devnum & 0xff) | ((devnum >> 12) & 0xfff00),
            )
        })
    }

    fn summary(&self) -> Option<DeviceSummary> {
        let devnum = self.device_number()?;
        let devnode = self.devnode()?.to_owned();
        Some((self.syspath().to_owned(), devnum, devnode))
    }
}

pub fn into_stream(
    socket: udev::MonitorSocket,
) -> impl futures_core::Stream<Item = anyhow::Result<udev::Event>> + Send {
    async_stream::try_stream! {
        let mut async_fd = tokio::io::unix::AsyncFd::new(socket)?;
        loop {
            let mut guard = async_fd.readable_mut().await?;
            match guard.try_io(|socket| {
                socket
                    .get_mut()
                    .next()
                    .ok_or(std::io::Error::from(std::io::ErrorKind::WouldBlock))
            }) {
                Ok(event) => yield event?,
                Err(_would_block) => continue,
            };
        };
    }
}
