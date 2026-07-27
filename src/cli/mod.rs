//! Parsers for the values of the `org.lowrisc.hotplug.*` annotations.
//!
//! Both types implement [`FromStr`](std::str::FromStr) and [`Display`](std::fmt::Display), where
//! `Display` round-trips back to the accepted syntax. Parse errors are user-facing: they quote the
//! offending input and state the expected form.

pub mod device;
pub mod symlink;

pub use device::DeviceRef;
pub use symlink::Symlink;
