use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const MANIFEST_VERSION: u32 = 1;

/// One download segment. `end` is inclusive. `pos` is the next byte to write.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub id: u32,
    pub start: u64,
    pub end: u64,
    pub pos: u64,
    #[serde(default)]
    pub done: bool,
    /// Runtime-only claim flag; never persisted.
    #[serde(skip)]
    pub claimed: bool,
}

impl Segment {
    pub fn new(id: u32, start: u64, end: u64) -> Self {
        Self {
            id,
            start,
            end,
            pos: start,
            done: false,
            claimed: false,
        }
    }

    pub fn remaining(&self) -> u64 {
        if self.done || self.pos > self.end {
            0
        } else {
            self.end - self.pos + 1
        }
    }
}

/// Resume metadata persisted next to the part files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub url: String,
    pub output: String,
    pub total: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub connections: usize,
    pub segments: Vec<Segment>,
    /// Stable identity used for resume when the request URL is signed and
    /// changes on every re-resolve (e.g. YouTube's `googlevideo.com` URLs).
    #[serde(default)]
    pub resume_key: Option<String>,
}

impl Manifest {
    pub fn load(path: &Path) -> std::io::Result<Option<Manifest>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Directory that holds `manifest.json` plus `part_XXXX` files for an output file.
pub fn parts_dir_for(output: &Path) -> PathBuf {
    let mut s = output.as_os_str().to_os_string();
    s.push(".rgparts");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trip_preserves_resume_position() {
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            url: "http://127.0.0.1:8765/file".to_string(),
            output: "out.bin".to_string(),
            total: 1_000_000,
            etag: Some("\"fixture\"".to_string()),
            last_modified: Some("Thu, 01 Jan 1970 00:00:00 GMT".to_string()),
            connections: 8,
            segments: vec![Segment {
                id: 3,
                start: 500_000,
                end: 999_999,
                pos: 625_000,
                done: false,
                claimed: true,
            }],
            resume_key: Some("page|format".to_string()),
        };

        let encoded = serde_json::to_vec(&manifest).expect("manifest serializes");
        let decoded: Manifest = serde_json::from_slice(&encoded).expect("manifest parses");
        assert_eq!(decoded.version, MANIFEST_VERSION);
        assert_eq!(decoded.segments[0].pos, 625_000);
        assert_eq!(decoded.segments[0].remaining(), 375_000);
        assert_eq!(decoded.resume_key.as_deref(), Some("page|format"));
        // Runtime claims must not leak into persisted resume state.
        assert!(!decoded.segments[0].claimed);
    }

    #[test]
    fn parts_directory_appends_suffix_to_output() {
        assert_eq!(
            parts_dir_for(Path::new("downloads/file.bin")),
            PathBuf::from("downloads/file.bin.rgparts")
        );
    }
}
