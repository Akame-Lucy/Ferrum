# Ferrite

Ferrite is a self-hosted, single-binary remote file browser, editor, and terminal server written in Rust.

## Key Features

- **High-Performance Backend**: Built with Tokio and Axum for fast, concurrent remote operations.
- **Multi-Protocol Support**: Unified remote filesystem access across SFTP, FTPS/FTP, WebDAV, S3-compatible object storage, local filesystem, and Ferrous agent.
- **In-Browser Monaco Editor**: Code editing with syntax highlighting, multi-tab support, and Ctrl+S saving.
- **Real Interactive Terminal Mode**: Full xterm.js terminal always connected to a remote PTY shell, over SSH or, for `protocol: ferrous` remotes, directly to a Ferrous agent. The session is locked like PuTTY: it stays connected while you browse other remotes or switch to the editor, and only changes when you click "Reconnect SSH".
- **Real-Time Collaboration**: Multi-session live document editing over WebSockets.
- **Diff-Before-Save**: SHA-256 integrity verification preventing accidental overwrites of externally modified files.
- **Remote Search**: Deep file name and full-text content searching across remote file structures with instant editor line navigation.
- **Plugin System**: Real-time event streaming WebSocket API (`/ws/events`) for external linters and formatters.
- **Embedded Frontend**: Ships as a single standalone executable containing all web assets.
- **Multi-User Accounts**: Admin-managed users, each scoped to specific remotes/folders with read or read-write grants (the Users panel under Settings). Non-admin users only ever see what they're granted; admins bypass all scoping.
- **Security First**: Default localhost binding (`127.0.0.1`), Argon2id password hashing, HTTP-only session cookies, rate-limited login, TOTP support, native TLS 1.3, and secret redaction.

## Installation and Build

### Building from Source

Prerequisites: Rust stable toolchain installed.

```bash
cargo build --release
```

The compiled binary will be located at `target/release/ferrite`.

#### First-time build on Windows

On Windows the SSH/SFTP client (`libssh2`) is compiled against OpenSSL so it supports modern key exchange and host keys (`curve25519`, `ecdh`, `ed25519`). OpenSSL is built from source during the first build, which needs two extra tools on PATH:

1. Install the tools (once):
   ```bash
   winget install NASM.NASM
   winget install StrawberryPerl.StrawberryPerl
   ```
   If the Strawberry Perl MSI cannot elevate, download the portable edition, unzip it, and add its `perl\bin` folder to PATH instead.
2. **Open a fresh PowerShell** afterwards so the new PATH entries are picked up (a shell opened before installing will not see them).
3. Build:
   ```bash
   cargo build --release
   ```

Notes for the first build:

- The first build compiles OpenSSL from source and takes roughly 10-12 minutes. Subsequent builds reuse cached OpenSSL and are fast.
- Do **not** run two `cargo` commands against this project at the same time (for example a second `cargo run` while the first build is going). Concurrent builds corrupt the OpenSSL build directory and cause `LNK1104` / "perl not found" errors. Let one build finish before starting another.
- Build from PowerShell or cmd, not Git Bash. Git Bash puts its own incompatible `perl` first on PATH.

On Linux and macOS none of this applies; the client links against system OpenSSL automatically (`build-essential`/Xcode command line tools and `pkg-config`/`libssl-dev` are enough).

### Running Locally

1. Start the Ferrite server:
   ```bash
   cargo run --bin ferrite -- serve
   ```
2. Access the web interface at `http://127.0.0.1:8080`.


---


## Production Security & Setup Guide

This section covers Ferrite specifically. For the full picture across both
twins (Ferrous's mutual key auth, systemd hardening, and production security): see [SECURITY.md](../docs/SECURITY.md).

### 1. Default Binding and App Authentication

By default, Ferrite binds strictly to `127.0.0.1:8080`. Never expose an unauthenticated Ferrite server directly to the public internet (`0.0.0.0`).

To enable application-level login, update `ferrite.yaml`:

```yaml
bind: "127.0.0.1:8080"

server_auth:
  enabled: true
  username: "admin"
  password_hash: "$argon2id$v=19$m=19456,t=2,p=1$..." # Argon2id hash
  totp_secret: "JBSWY3DPEHPK3PXP" # Optional 2FA secret key
```

### 2. Reverse Proxy & TLS 1.3 Termination

Run Ferrite behind a reverse proxy like Caddy or Nginx for TLS termination and WebSockets support. The snippets below are a quick reference; `packaging/reverse-proxy/{nginx.conf,Caddyfile}` at the repo root have complete, commented versions (upload body size limits, long-lived websocket timeouts, HSTS).

#### Caddy Example (`Caddyfile`):

```caddy
ferrite.example.com {
    tls user@example.com
    encode gzip zstd

    reverse_proxy 127.0.0.1:8080 {
        header_up X-Forwarded-Proto {scheme}
        header_up Host {host}
    }
}
```

#### Nginx Example (`nginx.conf`):

```nginx
server {
    listen 443 ssl http2;
    server_name ferrite.example.com;

    ssl_certificate /etc/letsencrypt/live/ferrite.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/ferrite.example.com/privkey.pem;
    ssl_protocols TLSv1.3;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "Upgrade";
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
    }
}
```

### 3. Native TLS Option

Ferrite also supports native TLS termination directly without a reverse proxy:

```bash
ferrite serve --bind 0.0.0.0:8443 --tls-cert /path/to/cert.pem --tls-key /path/to/key.pem
```

### 4. VPN and Firewall Hardening

- **WireGuard / Tailscale**: Bind Ferrite to your private VPN IP (e.g. `100.x.y.z` or `10.x.y.z`).
- **Firewall Rules (`ufw`)**:
  ```bash
  sudo ufw default deny incoming
  sudo ufw allow in on tailscale0 to any port 8080
  sudo ufw enable
  ```

### 5. Remote Authentication Security

Prefer SSH public key authentication over plain passwords for SFTP and SSH terminal remotes:

```yaml
remotes:
  - name: production-vps
    protocol: sftp
    host: 192.168.1.100
    port: 2222
    username: admin
    auth:
      private_key_path: "~/.ssh/id_ed25519"
      passphrase: null
```

---

## What Each Protocol Browses

Ferrite is a client. Each remote shows the filesystem of the machine you point it at:

- `sftp` / `ftps` / `ftp` / `webdav` / `s3` / `ferrous`: a **remote** server reached over the network.
- `local`: the **machine Ferrite itself runs on**, read directly with no network server.

---

## Detailed Configuration Reference

Configuration is defined in `ferrite.yaml`:

```yaml
bind: "127.0.0.1:8080"

server_auth:
  enabled: true
  username: "admin"
  password_hash: "$argon2id$v=19$m=19456,t=2,p=1$..."

remotes:
  - name: internal-sftp
    protocol: sftp
    host: 192.168.1.100
    port: 22
    username: alex
    auth:
      private_key_path: "~/.ssh/id_ed25519"

  - name: cloud-ftps
    protocol: ftps
    host: 203.0.113.15
    port: 21
    username: ftpuser
    auth:
      password: "ftppassword"

  - name: ferrous-agent
    protocol: ferrous
    host: 127.0.0.1
    port: 9090
    username: agent
    auth:
      password: ""
    # Pins the agent's identity key (it logs this at startup). Without a pin
    # Ferrite trusts whatever key answers on first connect; see SECURITY.md.
    agent_pubkey: "<hex public key printed by ferrous-agent at startup>"

users:
  # The first admin is normally created via the Users panel (Settings ->
  # Manage Users) once server_auth.enabled is true, not hand-written here.
  # Standard users only see the remotes/paths listed in their permissions;
  # admins bypass permissions entirely.
  - username: "alex"
    password_hash: "$argon2id$v=19$m=19456,t=2,p=1$..."
    role: "user"
    enabled: true
    allow_shell: false
    permissions:
      - remote: "internal-sftp"
        path: "/var/www"
        write: true
```

---

## API and WebSocket Endpoints

Auth:

- `POST /api/auth/login` : App login (Argon2id + TOTP).
- `POST /api/auth/logout` : Clear session cookie.
- `GET /api/auth/status` : Auth requirements, login status, role, and permission grants for the current session.

Settings and users (admin-only where noted):

- `GET/POST /api/settings` : Per-browser UI settings (theme, etc).
- `POST /api/settings/password` : Change the current user's own password.
- `GET /api/users` / `POST /api/users` *(admin)* : List / create users.
- `PUT /api/users/:username` / `DELETE /api/users/:username` *(admin)* : Update role/permissions/password, or remove a user.

Remotes and files (`remote=<name>&path=<path>` query params unless noted):

- `GET /api/remotes` : List remotes visible to the current user.
- `POST /api/remotes` *(admin)* : Create or update a remote connection.
- `GET /api/files` : List directory entries.
- `GET /api/file` : Stream raw file content.
- `PUT /api/file` : Write content to a file.
- `POST /api/file` : Create a new empty file.
- `POST /api/directory` : Create a new directory.
- `DELETE /api/entry` : Delete a file or directory.
- `POST /api/rename` : Rename or move an entry.
- `POST /api/copy` : Copy an entry.
- `GET /api/download` : Download a single file.
- `POST /api/download/zip` : Download multiple entries as a zip.
- `POST /api/upload` : Multipart file upload.
- `GET /api/search` : Filename/content search within one remote.
- `GET /api/search/global` : Full-text search across the local workspace (uses `rg`).
- `GET /api/zip/inspect` / `GET /api/zip/extract` : Inspect or extract a single entry from a `.zip` without downloading it whole.

Git (operates on the local filesystem directly, independent of the selected remote):

- `GET /api/git/status`, `/diff`, `/blame`, `/history`, `/branches` : Read-only git info for a path.
- `POST /api/git/commit`, `/stage`, `/unstage`, `/revert`, `/checkout` : Mutating git operations.

WebSockets:

- `WS /ws/terminal?cols=<c>&rows=<r>` : Interactive terminal, over SSH or a `protocol: ferrous` remote depending on the `terminal:` config.
- `WS /ws/collab?remote=<name>&path=<path>` : Real-time collaborative document synchronization.
- `WS /ws/events` : Real-time system event stream for external plugins (see [PLUGINS.md](PLUGINS.md)).
