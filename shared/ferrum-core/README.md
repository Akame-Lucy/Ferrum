# ferrum-core

Internal shared crate: not published, not meant to be depended on outside this
monorepo. Holds what Ferrite and Ferrous both need so the two can't silently
drift apart:

- `traits.rs` : `RemoteFilesystem`, the trait every Ferrite backend (SFTP,
  FTP, WebDAV, S3, local, Ferrous) implements, plus the shared `RemoteError`,
  `FileEntry`/`FileMeta`/`SearchMatch` types.
- `config.rs` : `Config`, `RemoteConfig`, `User`/`Role`/`PathGrant` (Ferrite's
  multi-user permission model), and YAML load/save.
- `protocol.rs` : `Capability` (Ferrous's per-client path/read-write/shell
  scoping) and the `FerrousRequest`/`FerrousResponse` wire enum.
- `noise.rs` : the shared Noise_XX session: both `ferrous-agent` (responder)
  and Ferrite's `FerrousBackend`/`FerrousPtySession` (initiator) use the exact
  same handshake and framing code, so there's only one implementation to keep
  correct.
- `identity.rs` : persistent keypair load-or-generate, used by both binaries
  to create/reuse their `*_identity.key`.

If you're changing anything here, check both `ferrite/crates/ferrite-core`
and `ferrous/src` for call sites; that's the whole point of this crate
existing.
