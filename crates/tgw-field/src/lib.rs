//! `tgw-field` — field-client library: pacer, persistent queue, delivery loop, vitals
//! capture. The binary (`main.rs`) is a thin clap shell over these modules; they are a
//! library so the workspace integration test can drive the sender in-process.
//!
//! OWNER: Muaz (`muaz/core`). Error style: `anyhow` (binary-side code).

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod pacer;
pub mod queue;
pub mod sender;
pub mod vitals;
