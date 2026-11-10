//! rg-core — clean-room, IDM-style multi-segment resumable HTTP download engine.

pub mod engine;
pub mod error;
pub mod ftp;
pub mod limiter;
pub mod manifest;
pub mod naming;
pub mod net;
pub mod probe;

pub use engine::{Download, Progress, Stats, DEFAULT_CONNECTIONS, DEFAULT_USER_AGENT};
pub use error::{Error, Result};
pub use manifest::{parts_dir_for, Manifest, Segment};
pub use net::{Credentials, NetworkConfig, ProxyConfig, ProxyMode, ResolvedProxy};
pub use naming::{
    clean_title, default_capture_name, extension_for_content_type,
    filename_from_content_disposition, infer_output_path, is_generic_title, is_web_page,
    sanitize_file_stem, sanitize_filename, unique_file_name, NamingContext,
};
