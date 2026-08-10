pub mod config;
pub mod identity;
pub mod noise;
pub mod protocol;
pub mod traits;

pub use config::{
    AuthConfig, Config, PathGrant, ProtocolType, RemoteConfig, Role, ServerAuthConfig, TlsConfig,
    User,
};
pub use identity::{from_hex, load_or_generate_keypair, parse_public_key, to_hex, KEY_LEN};
pub use noise::NoiseSession;
pub use protocol::{Capability, FerrousRequest, FerrousResponse};
pub use traits::{BoxStream, FileEntry, FileMeta, RemoteError, RemoteFilesystem, SearchMatch};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const VERSION_CODENAME: &str = "Rusty Nail";
pub const FULL_VERSION_INFO: &str = concat!("v", env!("CARGO_PKG_VERSION"), " \"Rusty Nail\"");

