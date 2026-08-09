# Ferrum

Ferrum is a self-hosted platform for secure, web-based access to remote files and shells over SSH, SFTP, FTP, and more. It is built as two twin components that share one iron root (Ferrum is Latin for iron; ferrite and ferrous are both forms of it).

- **Ferrite** (`ferrite/`): the client and control plane. A single binary that serves a web UI (Monaco or Vim editor, file browser, and an SSH terminal) and connects outward to remote hosts over SFTP, FTP, WebDAV, S3, the local filesystem, and Ferrous.
- **Ferrous** (`ferrous/`): the server-side agent. A lightweight, hardened daemon you install on a host to expose its filesystem and shell to Ferrite and other clients over a purpose-built, authenticated, encrypted channel (Noise_XX, mutual public-key identity, per-client capability scoping).

Both talk to `shared/ferrum-core`, the crate holding what they have in common: the `RemoteFilesystem` trait every backend implements, config types, and the Noise transport + identity handling both twins use to talk to each other.

## Quick start

Try Ferrite alone first - it works standalone against local files with zero configuration:

```bash
cd ferrite
cargo run --bin ferrite -- serve
```

Open `http://127.0.0.1:8080`. See [ferrite/README.md](ferrite/README.md) for connecting to real SFTP/FTP/S3/WebDAV remotes, enabling login, and the full config reference.

To also run a Ferrous agent (the premium path for hosts you control; one consistent protocol instead of per-OS SSH quirks):

```bash
cd ferrous
cargo run --bin ferrous-agent -- --authorize-client <hex-pubkey-ferrite-logged-at-startup> --allowed-path /some/dir
```

See [ferrous/README.md](ferrous/README.md) for the config-file form and capability scoping.

## Repository layout

```
ferrum/
  ferrite/            client + web UI + control plane
    crates/           ferrite-core (backends), ferrite-server (API/web), ferrite-pty (SSH PTY)
    frontend/          the web UI (vanilla JS, Monaco/xterm)
  ferrous/            server agent
  shared/ferrum-core/ code both twins depend on (traits, config, Noise transport)
  packaging/          systemd units, reverse-proxy examples (see docs/SECURITY.md)
  docs/               security model, contributing guide, code of conduct
```

This is a monorepo for development only. Each twin ships as its own standalone binary, so users install just what they need: Ferrite alone, Ferrous alone, or both. Cloning the repo is for contributors; end users download a single binary per twin from releases.

## Status

- **Ferrite**: functional. See [ferrite/README.md](ferrite/README.md) to build and run.
- **Ferrous**: Phase 1 (minimal agent) and Phase 2 (hardening/packaging) complete: encrypted mutual-auth transport, filesystem operations, PTY/shell, capability scoping, systemd units, reverse-proxy examples. See [ferrous/README.md](ferrous/README.md) for what's left (in-process sandboxing beyond systemd, the public protocol spec).

## Security model in one line

Security lives in keys and architecture, not in hiding code. With the full source public, an attacker still gains nothing without the user's keys: no secrets in the binary, standard audited encryption in transit, app login with hashed credentials, mutual key auth between Ferrite and Ferrous, and VPN or localhost binding in front of everything. Full guide: [docs/SECURITY.md](docs/SECURITY.md).

## Contributing

Issues and pull requests welcome. [docs/CONTRIBUTING.md](docs/CONTRIBUTING.md)
is the full guide: the checks a PR has to pass, the Conventional Commits title
format (the PR title becomes the commit message on `main`), code style, and how
to add a backend. Participation is covered by our
[Code of Conduct](docs/CODE_OF_CONDUCT.md).

`main` is protected: no direct pushes, and the merge button stays disabled
until CI, Clippy, the dependency/license policy, and the PR guards all report
green.

### Getting started

1. Clone the repo and run `cargo check --workspace` from the root to verify everything compiles.
2. Copy the example configs to create your local configs:
   ```bash
   cp ferrite/config/ferrite.example.yaml ferrite/ferrite.yaml
   cp ferrous/config/ferrous.example.yaml ferrous/ferrous.yaml
   ```
   These files are gitignored (they hold real credentials/keys). Edit them as needed.
3. On **Windows**, the first build compiles OpenSSL from source (~10 min). See [ferrite/README.md](ferrite/README.md#first-time-build-on-windows) for prerequisites (NASM + Perl).
4. Before opening a PR, run the same checks CI does - they are listed in [docs/CONTRIBUTING.md](docs/CONTRIBUTING.md#the-checks-your-pr-has-to-pass).

### Things to know

- This is a Cargo workspace spanning `shared/ferrum-core`, `ferrite/crates/*`, and `ferrous`. `ferrite/` also has its own nested workspace so it can be built and released independently; if you touch shared code, check both.
- Never run two `cargo` commands against the same workspace concurrently, as it corrupts the OpenSSL build directory on Windows.

## Author

Created by **Akame Lucy**.

## License

MIT, see [LICENSE](LICENSE).
