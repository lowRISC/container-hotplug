//! Interfacing with `runc`.
//!
//! We stand in for `runc`, so we must speak its interfaces:
//!
//! * [`cli`] — enough of `runc`'s command line to recognise `create` and forward everything else.
//! * [`config`] — the parts of the OCI `config.json` in the bundle that we read.
//! * [`state`] — the parts of `runc`'s `state.json` that tell us where the container's cgroup is and
//!   what its init process is.
//! * [`log`] — logrus-compatible JSON output, for when `runc` is asked to log that way.
//!
//! [`Container`] then builds on those to manipulate the container `runc` created.

pub mod cli;
pub mod config;
pub mod log;
pub mod state;

mod container;
pub use container::Container;
