//! Shared code for the `capture` tool, the `tone` test helper and the integration tests.
//!
//! * [`wasapi`]: thin wrappers over WASAPI (device lookup, render/capture streams,
//!   process-loopback activation).
//! * [`procs`]: process enumeration and "root process of a name" resolution.
//! * [`dsp`]: tiny signal-analysis helpers (Goertzel level meter).

pub mod config;
pub mod dsp;
pub mod engine;
pub mod output;
pub mod procs;
pub mod timeline;
pub mod wasapi;
