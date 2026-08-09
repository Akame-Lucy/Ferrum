# Securing a Ferrum deployment

Security here lives in keys and architecture, not in hiding code (Kerckhoffs's
principle): with the full source public, an attacker should gain nothing
without your keys. That means layering standard, audited mechanisms rather
than inventing anything novel: defense in depth across network, transport,
identity, and resource:

| Layer       | Mechanism                                             | Protects against |
|-------------|--------------------------------------------------------|-------------------|
| Network     | VPN (WireGuard/Tailscale) or localhost-only binding    | Port scanning, exposure to the open internet |
| Transport   | TLS 1.2+ (native or reverse proxy)                      | Traffic interception, tampering |
| Identity    | Ferrite app login (argon2id, sessions, TOTP); Ferrous mutual Noise_XX key auth | Unauthenticated access |
| Resource    | Ferrite per-user `PathGrant`s; Ferrous per-client `Capability` | Lateral movement once inside |

No single layer is load-bearing on its own. Skipping any one of them (running
Ferrite open on `0.0.0.0` with auth disabled "because it's behind a VPN
anyway", say) removes a layer the others were assuming was there.

## Network: keep management surface off the open internet

- Bind Ferrite and Ferrous to `127.0.0.1` or a VPN interface, never
  `0.0.0.0`, unless a reverse proxy in front of it is your actual public
  entry point (see below). Both already default to `127.0.0.1`.
- Put a VPN (WireGuard, Tailscale) between you and any host running Ferrite
  or Ferrous. Ferrous in particular should almost never be reachable from
  the open internet (it's the "premium path" for hosts you control, and its
  whole value proposition (mutual key identity, per-client capability
  scoping) is easiest to reason about when the network itself is already
  restricted to people you trust).
- Firewall everything else. If you do expose Ferrite publicly (via a
  reverse proxy), only the proxy's TLS port should be reachable from outside;
  Ferrite's own port and every Ferrous agent's port should not be.

## Transport: TLS everywhere

- Either enable Ferrite's native TLS (`tls.enabled` + `cert_path`/`key_path`
  in `ferrite.yaml`), or terminate TLS at a reverse proxy: see
  `packaging/reverse-proxy/{nginx.conf,Caddyfile}` for working examples,
  including the websocket-upgrade passthrough the terminal, collaborative
  editing, and live-events features need.
- Ferrous doesn't use TLS; its transport is Noise_XX (see below), which is
  already an authenticated, encrypted channel appropriate for that use case.

## Identity

**Ferrite** ships app login: argon2id password hashing, httpOnly session
cookies, login rate limiting, optional TOTP. Enable it
(`server_auth.enabled: true`) for anything not purely local. Prefer the
multi-user system (the Users panel under Settings) over sharing one admin
login: create one account per person, and see Resource below for scoping
what each one can touch.

**Ferrous** authenticates both directions with public keys, SSH-host-key
style, over `Noise_XX`:

- Both the agent and every Ferrite instance that talks to it generate a
  persistent keypair on first run (`ferrous_identity.key`,
  `ferrite_identity.key`) and log the public half at startup.
- The agent only accepts connections from client keys explicitly listed in
  its config (`ferrous.yaml`'s `clients:`, or `--authorize-client`): an
  unrecognized key is rejected before any request is processed, full stop.
- Ferrite should pin the agent's public key via `agent_pubkey` in
  `ferrite.yaml`'s remote entry. Without a pin, Ferrite trusts whatever key
  answers on that host:port on first connect (logged as a warning) - fine
  for a quick local test, not for anything that matters. With a pin, a
  changed key (agent reinstalled, or something impersonating it) is refused
  outright with a clear error, the same value SSH's host-key checking
  provides.
- If an agent's identity key is ever compromised or you're unsure, delete
  `ferrous_identity.key` and restart it to rotate: every Ferrite pinning the
  old key will then (correctly) refuse to reconnect until you update
  `agent_pubkey` to the new one out of band.

Prefer SSH key auth over passwords for any SFTP/SSH remote Ferrite connects
to (`auth.private_key`/`private_key_path` instead of `auth.password`), and
disable password auth in that host's own `sshd_config` where you control it.

## Resource: least privilege on both twins

- **Ferrite users**: give each account only the `PathGrant`s (remote + path +
  read/write) they actually need, via the Users panel. Admins bypass this
  entirely, so keep the admin role to the people who should be able to touch
  every remote and manage other accounts.
- **Ferrous clients**: each authorized client key gets its own `Capability`
  (including `allowed_paths`, `read_only`, `allow_shell`). Scope these narrowly per
  Ferrite instance that connects, the same way you'd scope an SSH
  `authorized_keys` `command=`/`restrict` entry.
- **systemd**: run both as dedicated unprivileged system users with the
  sandboxing directives in `packaging/systemd/` (`ProtectSystem=strict`,
  `NoNewPrivileges`, capability bounding set cleared, etc). See that
  directory's README for install steps. `ReadWritePaths=` must list every
  path the process is actually allowed to touch, or the OS-level sandbox
  will block operations the app-level capability check already approved.
- **Docker Deployment**: A multi-stage production `Dockerfile` is provided at the root for container deployments. For bare-metal Linux hosts, the systemd path in `packaging/systemd/` is recommended for native isolation and service management.

## Known gaps, honestly

- **`ferrite.yaml` stores remote passwords in plaintext.** They're redacted
  from logs and from `Debug` output, but the file on disk is not encrypted
  at rest. Prefer key-based auth for remotes where you can, and restrict the
  file's permissions to the service user (the systemd setup above does this
  by construction).
- **The frontend loads Monaco and xterm.js from a CDN** (see the
  `Content-Security-Policy` in `ferrite-server/src/main.rs`, which
  explicitly allow-lists those origins). Fine for most deployments; if
  you're airgapped or want zero third-party origins in scope, vendor those
  assets and tighten the CSP accordingly.
- **Large file reads are buffered in memory** rather than fully streamed in
  every code path. Fine for typical files; be mindful with very large ones
  until this is addressed.

If you find an actual vulnerability, please open an issue describing it
rather than exploiting it: this is a self-hosted tool, but the same
courtesy applies.
