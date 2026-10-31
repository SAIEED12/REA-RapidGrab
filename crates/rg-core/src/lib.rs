//! rg-core — clean-room, IDM-style multi-segment resumable HTTP download engine.

pub mod engine;
pub mod error;
pub mod limiter;
pub mod manifest;
pub mod probe;

pub use engine::{Download, Progress, Stats, DEFAULT_CONNECTIONS, DEFAULT_USER_AGENT};
pub use error::{Error, Result};
pub use manifest::{parts_dir_for, Manifest, Segment};
