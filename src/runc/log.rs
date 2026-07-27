//! Logging in the format `runc` itself uses.

use std::io::{BufWriter, Write};
use std::sync::Mutex;
use std::time::SystemTime;

use log::{Level, Log};
use serde::Serialize;

/// One log record, with the field names logrus uses.
#[derive(Serialize)]
struct Message {
    level: &'static str,
    msg: String,
    time: String,
}

fn map_log_level(level: Level) -> &'static str {
    match level {
        Level::Error => "error",
        Level::Warn => "warning",
        Level::Info => "info",
        Level::Debug => "debug",
        Level::Trace => "trace",
    }
}

/// JSON logger compatible with logrus (the logging library used by runc and many go projects).
///
/// Used when we are invoked with `--log-format=json`, so that a container manager parsing `runc`'s
/// log file can read our messages too. Every record is flushed immediately, since the process may
/// `exec` or `fork` at any point.
pub struct JsonLogger {
    target: Mutex<BufWriter<Box<dyn Write + Send>>>,
}

impl JsonLogger {
    /// Log to `target`, typically the file named by `--log`.
    pub fn new(target: Box<dyn Write + Send>) -> Self {
        Self {
            target: Mutex::new(BufWriter::new(target)),
        }
    }
}

impl Log for JsonLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        let msg = Message {
            level: map_log_level(record.level()),
            msg: record.args().to_string(),
            time: humantime::format_rfc3339_seconds(SystemTime::now()).to_string(),
        };

        let mut target = self.target.lock().unwrap();
        let _ = serde_json::to_writer(&mut *target, &msg);
        let _ = target.flush();
    }

    fn flush(&self) {}
}
