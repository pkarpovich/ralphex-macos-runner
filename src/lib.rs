//! A native runner daemon for ralphex-farm.
//!
//! The crate ships two binaries: `ralphex-macos-runner`, the daemon that claims
//! jobs from the farm and runs ralphex in an existing checkout, and `rxd`, the
//! local client that opens a ticketless run and streams its output to a
//! terminal.
//!
//! A run writes to a pseudo-terminal, so [`ansi::plain`] separates the two
//! views of its output: the farm's log and the completion tail carry plain
//! text, while the replay history and the lines a live client follows keep the
//! escape sequences the run wrote.

pub mod agent;
pub mod ansi;
pub mod config;
pub mod ipc;
pub mod job;
pub mod logstream;
pub mod paths;
pub mod pr;
pub mod protocol;
pub mod service;
