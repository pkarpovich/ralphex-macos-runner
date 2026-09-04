//! A native runner daemon for ralphex-farm.
//!
//! The crate ships two binaries: `ralphex-macos-runner`, the daemon that claims
//! jobs from the farm and runs ralphex in an existing checkout, and `rxd`, the
//! local client that opens a ticketless run and streams its output to a
//! terminal.

pub mod agent;
pub mod config;
pub mod ipc;
pub mod job;
pub mod logstream;
pub mod paths;
pub mod pr;
pub mod protocol;
pub mod service;
