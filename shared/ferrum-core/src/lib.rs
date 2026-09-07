pub mod config;
pub mod identity;
pub mod noise;
pub mod paths;
pub mod protocol;
pub mod traits;

pub use config::{
    expand_env, AuthConfig, Config, Limits, PathGrant, ProtocolType, RemoteConfig, Role,
    ServerAuthConfig, TlsConfig, User,
};
pub use identity::{from_hex, load_or_generate_keypair, parse_public_key, to_hex, KEY_LEN};
pub use noise::{NoiseSession, MAX_MESSAGE_LEN};
pub use paths::{is_within, normalize_path};
pub use protocol::{Capability, FerrousRequest, FerrousResponse, MAX_TRANSFER_CHUNK, TRANSFER_CHUNK};
pub use traits::{
    modified_secs, BoxStream, FileEntry, FileMeta, RemoteError, RemoteFilesystem, SearchMatch,
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Version of the Ferrite <-> Ferrous wire protocol. Exchanged in the first
/// message after the Noise handshake; a peer speaking a different number is
/// refused with a message naming both releases. Bump it whenever the
/// framing or the request/response set changes incompatibly.
///
/// History: 1 = one Noise message per request, byte arrays as JSON numbers
/// (releases up to 0.1.1). 2 = chunked framing, base64 payloads, chunked
/// file transfer, the Hello exchange (0.2.0).
pub const PROTOCOL_VERSION: u32 = 2;
/// The release codename, defined once as a macro because `concat!` only
/// accepts literals. Change it here and both constants below follow.
macro_rules! codename {
    () => {
        "Lodestone"
    };
}

pub const VERSION_CODENAME: &str = codename!();
pub const FULL_VERSION_INFO: &str = concat!("v", env!("CARGO_PKG_VERSION"), " \"", codename!(), "\"");

