use udev::Device;

pub trait DevNum {
    fn device_number(&self) -> Option<(u64, u64)>;
}

impl DevNum for Device {
    fn device_number(&self) -> Option<(u64, u64)> {
        self.devnum().map(|devnum| {
            (
                (devnum & 0xfff00) >> 8,
                (devnum & 0xff) | ((devnum >> 12) & 0xfff00),
            )
        })
    }
}
