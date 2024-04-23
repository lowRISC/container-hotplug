use std::sync::{OnceLock, RwLock};

use log::Log;

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
