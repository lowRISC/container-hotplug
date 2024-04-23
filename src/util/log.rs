use std::os::unix::net::UnixDatagram;
use std::sync::{OnceLock, RwLock};

use anyhow::{Context, Result};
use log::{Level, Log};
use rustix::thread::Pid;

pub struct ReplaceableLogger {
    logger: RwLock<Box<dyn Log>>,
}

impl Log for ReplaceableLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.logger.read().unwrap().enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        self.logger.read().unwrap().log(record)
    }

    fn flush(&self) {
        self.logger.read().unwrap().flush()
    }
}

impl ReplaceableLogger {
    pub fn new(logger: Box<dyn Log>) -> Self {
        Self {
            logger: RwLock::new(logger),
        }
    }

    pub fn replace(&self, logger: Box<dyn Log>) {
        // Replace and drop in two stages to ensure the old logger is dropped outside the lock.
        let logger = core::mem::replace(&mut *self.logger.write().unwrap(), logger);
        drop(logger);
    }
}

static LOGGER: OnceLock<ReplaceableLogger> = OnceLock::new();

pub fn global_replace(logger: Box<dyn Log>) {
    let mut logger = Some(logger);
    let this = LOGGER.get_or_init(|| ReplaceableLogger::new(logger.take().unwrap()));
    if let Some(logger) = logger {
        this.replace(logger);
    }
    let _ = log::set_logger(this);
}

pub struct SyslogLogger {
    socket: UnixDatagram,
    exe: String,
    pid: Pid,
}

impl SyslogLogger {
    pub fn new() -> Result<Self> {
        let socket = UnixDatagram::unbound()?;
        socket.connect("/dev/log")?;
        let exe = std::env::current_exe()?
            .file_name()
            .context("cannot get process name")?
            .to_str()
            .context("process name is not UTF-8")?
            .to_owned();
        let pid = rustix::process::getpid();
        Ok(Self { socket, exe, pid })
    }
}

impl Log for SyslogLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        let severity = match record.level() {
            Level::Error => 3,
            Level::Warn => 4,
            Level::Info => 5,
            Level::Debug => 6,
            Level::Trace => 7,
        };
        const FACILITY: u8 = 1; // indicates user-level messages.
        let priority = FACILITY << 3 | severity;

        let msg = format!(
            "<{priority}>{exe}[{pid}]: {msg}",
            exe = self.exe,
            pid = self.pid.as_raw_nonzero(),
            msg = record.args()
        );
        let _ = self.socket.send(msg.as_bytes());
    }

    fn flush(&self) {}
}
