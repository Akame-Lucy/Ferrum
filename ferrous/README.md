# Ferrous

Ferrous is the server-side twin of [Ferrite](../ferrite/README.md). Phase 1 (minimal agent) is complete.

Ferrous is a lightweight, hardened agent that runs on a host and exposes that host's filesystem and shell to Ferrite and other clients over a purpose-built, authenticated, encrypted channel.

## Goals

- **Lightweight and low resource.** Written in Rust, single self-contained binary, does only what is needed.
- **One consistent protocol across every OS.** Windows, Linux, and macOS behave identically, avoiding the per-OS SSH server quirks that plague heterogeneous fleets.
- **Secure by design.** Authenticated, encrypted transport (Noise Protocol Framework or mutual TLS), public-key client identity, least-privilege capability scoping (which paths, which actions, shell or no shell), and no secrets baked into the binary.
- **Production-friendly.** VPN or localhost bound, systemd-managed, no Docker in production. Docker stays a dev and test convenience only.

## Non-goals

- Reimplementing a general-purpose SSH server. For hosts you do not own, use OpenSSH; Ferrite already talks to it directly. Ferrous is the premium path for hosts you control.

## Planned phases

1. Minimal agent for the twin: authenticated encrypted channel, filesystem operations, PTY, capability scoping. Ferrite gains a `ferrous` backend alongside its existing sftp, ftp, and local backends.
2. Hardening and production packaging (sandboxing, systemd, reverse-proxy examples), plus the security guide.
3. Public, versioned protocol specification, only if third-party clients become a goal.

## Status

Phase 1, complete:

- Noise_XX transport (`snow`), mutual public-key identity (both agent and
  client have a persistent keypair, generated on first run), per-client-identity
  capability scoping (`ferrous.yaml` or `--authorize-client`/`--allowed-path`
  CLI flags), filesystem operations (list, read, write, stat, delete, rename,
  create), Ferrite's `ferrous` backend and `agent_pubkey` pinning.
- PTY/shell over the agent (`portable-pty`, one shell process per session,
  scoped to the connecting client's capability), with a matching
  `FerrousPtySession` on the Ferrite side so the existing terminal feature
  works against `protocol: ferrous` remotes the same way it does over SSH.

Phase 2, complete:

- Privilege drop after bind on Unix (`--run-as-user`/`--run-as-group`,
  or `run_as_user`/`run_as_group` in `ferrous.yaml`), hardened systemd units
  for both twins (`packaging/systemd/`), reverse-proxy examples for Ferrite
  (`packaging/reverse-proxy/`), non-root Docker images, the
  [security guide](../docs/SECURITY.md), `config/ferrous.example.yaml`.
- Not covered: sandboxing beyond what the systemd unit's directives provide
  (no in-process seccomp/namespace isolation of spawned shells) - open-ended
  hardening, not a blocking gap.

See the [root README](../README.md) for the overall architecture.
