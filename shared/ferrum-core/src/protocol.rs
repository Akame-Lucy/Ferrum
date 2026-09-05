use crate::paths::is_within;
use crate::traits::{FileEntry, FileMeta, SearchMatch};
use serde::{Deserialize, Serialize};

/// Size of one file-transfer chunk on the wire. Files of any size move as a
/// sequence of these, so nothing about a file's size is bounded by the
/// transport's per-message limit.
pub const TRANSFER_CHUNK: usize = 1024 * 1024;

/// The largest chunk an agent will serve or accept in one request. A client
/// asking for more is not this client.
pub const MAX_TRANSFER_CHUNK: usize = 4 * TRANSFER_CHUNK;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    pub allowed_paths: Vec<String>,
    pub read_only: bool,
    pub allow_shell: bool,
    /// Shell binary to spawn for this client instead of the platform default.
    /// Point it at a restricted shell (`rbash`, a dedicated menu script) to
    /// narrow what a session can do beyond its working directory.
    #[serde(default)]
    pub shell: Option<String>,
}

impl Default for Capability {
    fn default() -> Self {
        Self {
            allowed_paths: vec!["/".to_string(), "C:\\".to_string()],
            read_only: false,
            allow_shell: true,
            shell: None,
        }
    }
}

impl Capability {
    /// Whether `path` falls under one of the allowed roots.
    ///
    /// Both sides are lexically normalized first, so `.` and `..` segments
    /// cannot be used to climb out of a root, and a `/`-boundary is required
    /// so an allowed path of `/foo` does not also cover `/foobar`. Symlinks
    /// are the agent's problem: see `ferrous::server::resolve_real`.
    pub fn is_path_allowed(&self, path: &str) -> bool {
        if self.allowed_paths.is_empty() {
            return true;
        }
        self.allowed_paths.iter().any(|allowed| is_within(allowed, path))
    }
}

/// Serializes byte payloads as base64 strings instead of JSON number arrays.
/// A number array costs ~3.5 bytes per byte and is slow to parse; base64 is
/// 1.33 and fast, which matters once a payload is a whole file chunk.
pub mod bytes_b64 {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        STANDARD.decode(text.as_bytes()).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum FerrousRequest {
    /// Always the first message on a connection. Carries the client's
    /// protocol number and release so a mismatch is reported in words on
    /// both sides rather than as a parse error on one of them.
    Hello { protocol_version: u32, version: String },
    ListDir { path: String },
    Stat { path: String },
    Delete { path: String },
    Rename { from: String, to: String },
    CreateFile { path: String },
    CreateDir { path: String },
    Search { path: String, query: String, content_search: bool },
    /// Up to `len` bytes of `path` starting at `offset`. The reply's `eof`
    /// flag, not a short read, says whether the file is exhausted.
    ReadChunk { path: String, offset: u64, len: u32 },
    /// Writes `data` at `offset`. The first chunk of a fresh write sets
    /// `truncate` so whatever was there before is discarded; later chunks
    /// extend the file in place. An empty `data` with `truncate` creates an
    /// empty file.
    WriteChunk {
        path: String,
        offset: u64,
        #[serde(with = "bytes_b64")]
        data: Vec<u8>,
        truncate: bool,
    },
    PtyOpen { cols: u32, rows: u32 },
    PtyInput {
        #[serde(with = "bytes_b64")]
        data: Vec<u8>,
    },
    PtyResize { cols: u32, rows: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum FerrousResponse {
    /// The agent's answer to `Hello`: its own protocol number and release.
    Hello { protocol_version: u32, version: String },
    ListDir { entries: Vec<FileEntry> },
    Stat { meta: FileMeta },
    Search { matches: Vec<SearchMatch> },
    Chunk {
        #[serde(with = "bytes_b64")]
        data: Vec<u8>,
        eof: bool,
    },
    PtyOutput {
        #[serde(with = "bytes_b64")]
        data: Vec<u8>,
    },
    Success,
    Error { message: String },
    /// Distinct from `Error`: the request was well-formed but the client's
    /// capability doesn't cover it, so callers can surface a clean "access
    /// denied" instead of a generic failure.
    Forbidden { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(paths: &[&str]) -> Capability {
        Capability {
            allowed_paths: paths.iter().map(|s| s.to_string()).collect(),
            read_only: false,
            allow_shell: false,
            shell: None,
        }
    }

    #[test]
    fn allows_inside_and_denies_outside() {
        let c = cap(&["/srv/data"]);
        assert!(c.is_path_allowed("/srv/data"));
        assert!(c.is_path_allowed("/srv/data/a/b.txt"));
        assert!(!c.is_path_allowed("/srv/other"));
        assert!(!c.is_path_allowed("/srv/database"));
    }

    #[test]
    fn denies_traversal_out_of_an_allowed_root() {
        let c = cap(&["/srv/data"]);
        assert!(!c.is_path_allowed("/srv/data/../../etc/shadow"));
        assert!(!c.is_path_allowed("/srv/data/..\\..\\etc\\shadow"));
        assert!(!c.is_path_allowed("/srv/data/./../data2"));
        assert!(c.is_path_allowed("/srv/data/sub/../inside"));
    }

    #[test]
    fn windows_roots_work_with_either_separator() {
        let c = cap(&["C:\\srv\\data"]);
        assert!(c.is_path_allowed("C:/srv/data/x"));
        assert!(c.is_path_allowed("C:\\srv\\data\\x"));
        assert!(!c.is_path_allowed("C:\\srv\\data\\..\\..\\Windows"));
    }

    #[test]
    fn root_grant_and_empty_list_allow_everything() {
        assert!(cap(&["/"]).is_path_allowed("/etc/shadow"));
        assert!(cap(&[]).is_path_allowed("/etc/shadow"));
    }

    #[test]
    fn byte_payloads_round_trip_as_base64() {
        let req = FerrousRequest::WriteChunk {
            path: "/x".into(),
            offset: 0,
            data: vec![0, 255, 10, 13, 34],
            truncate: true,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"data\":\"AP8KDSI=\""), "got {json}");
        match serde_json::from_str::<FerrousRequest>(&json).unwrap() {
            FerrousRequest::WriteChunk { data, truncate, .. } => {
                assert_eq!(data, vec![0, 255, 10, 13, 34]);
                assert!(truncate);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn hello_round_trips_with_the_current_protocol_number() {
        let req = FerrousRequest::Hello { protocol_version: crate::PROTOCOL_VERSION, version: crate::VERSION.into() };
        let json = serde_json::to_string(&req).unwrap();
        match serde_json::from_str::<FerrousRequest>(&json).unwrap() {
            FerrousRequest::Hello { protocol_version, version } => {
                assert_eq!(protocol_version, crate::PROTOCOL_VERSION);
                assert_eq!(version, crate::VERSION);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn capability_without_shell_field_still_parses() {
        let yaml = "allowed_paths: ['/srv']\nread_only: true\nallow_shell: false\n";
        let c: Capability = serde_yaml::from_str(yaml).unwrap();
        assert!(c.shell.is_none());
        assert!(c.read_only);
    }
}
