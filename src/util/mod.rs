//! Assorted helpers.
//!
//! * [`namespace`] — entering a container's namespaces to act on its behalf.
//! * [`log`] — a logger that can be swapped out after installation.
//! * [`escape`] — undoing udev's escaping of device strings.

pub mod escape;
pub mod log;
pub mod namespace;
