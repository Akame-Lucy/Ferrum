pub mod config;
pub mod ferrous_client;
pub mod ferrous_pty;
pub mod ftp;
pub mod local;
pub mod s3;
pub mod sftp;
pub mod traits;
pub mod webdav;

pub use config::{AuthConfig, Config, PathGrant, ProtocolType, RemoteConfig, Role, User};
pub use ferrous_client::FerrousBackend;
pub use ferrous_pty::FerrousPtySession;
pub use ftp::FtpBackend;
pub use local::LocalBackend;
pub use s3::S3Backend;
pub use sftp::SftpBackend;
pub use traits::{FileEntry, FileMeta, RemoteError, RemoteFilesystem, SearchMatch};
pub use webdav::WebDavBackend;
