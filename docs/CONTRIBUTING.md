# Contributing to Ferrum

Ferrum brokers filesystem and shell access to remote hosts. A bug here is not a
cosmetic glitch - it is somebody's server. That shapes how changes land: small
diffs, explicit verification, and a bias toward boring, auditable mechanisms
over clever ones.

## Getting set up

You need a stable Rust toolchain (install via [rustup](https://rustup.rs)).
Windows additionally needs [NASM](https://www.nasm.us/) on `PATH` for the
crypto dependencies - `choco install nasm`.

```bash
git clone https://github.com/Akame-Lucy/Ferrum.git
cd Ferrum
cargo check --workspace --all-targets --locked
```

Run Ferrite standalone against your local filesystem, no config needed:

```bash
cargo run --bin ferrite -- serve
```

## The checks your PR has to pass

These four run on every pull request and gate the merge button. Run them
locally first - it is faster than a CI round trip:

```bash
cargo check --workspace --all-targets --locked
```

```bash
cargo test --workspace --locked
```

```bash
cargo clippy --workspace --all-targets --locked -- -D warnings
```

```bash
cargo deny --workspace --all-features check
```

The last one needs `cargo install cargo-deny --locked` once. It enforces
[deny.toml](../deny.toml): no known-vulnerable or yanked dependencies, no licenses
outside the permissive allowlist, no crates from unexpected registries.

Two more checks run only on GitHub, against PR metadata rather than code:

- **Protected-path guard** - fails if the PR touches a path that must never be
  committed (`ferrite.yaml`, `*_identity.key`, `*.pem`, `target/`, ...). If it
  fires, you almost certainly used `git add -f` on something. Remove it with
  `git rm --cached <file>`.
- **PR title guard** - see below.
- **PR size guard** - blocks files over 5 MB. Binary assets belong in release
  archives; git keeps every blob forever.

## Commits and PR titles

Ferrum squash-merges, so **the PR title becomes the commit message on `main`**
verbatim. It has to follow [Conventional
Commits](https://www.conventionalcommits.org/):

```
fix(ferrite-core): stop SFTP reads from blocking the runtime
feat(ferrous): add per-client capability expiry
docs: document branch protection setup
```

Types: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`,
`ci`, `chore`, `revert`, `security`. Scope is optional and is usually the crate
(`ferrite-core`, `ferrite-server`, `ferrite-pty`, `ferrous`, `ferrum-core`).
Append `!` for a breaking change. Keep it under 72 characters, no trailing
period.

Individual commits inside the PR are yours to organize as you like - they get
squashed away.

## Branching

`main` is protected: no direct pushes, no force-pushes, PRs only.

```bash
git checkout -b fix/sftp-blocking-read
```

Prefix with the change type (`fix/`, `feat/`, `docs/`, `ci/`). One logical
change per branch - a PR that fixes a bug *and* refactors a module is two PRs.

## Code style

Match the surrounding code rather than importing your own conventions. Beyond
that:

- **Comments explain why, not what.** The code already says what it does. A
  comment earns its place by recording the constraint, the tradeoff, or the
  non-obvious failure it prevents.
- **No `unwrap()` on anything reachable from a request.** Ferrite runs as a
  long-lived server; a panic in a handler is a denial of service. Use the
  crate's `RemoteError` variants.
- **Never log credentials, tokens, key material, or full remote paths at info
  level.** Debug level is acceptable for paths; secrets never are.
- **New dependencies need a justification** in the PR description: what it
  does, why it beats writing it, and its license. Every dependency is
  statically linked into the release binaries.

The tree is not currently `rustfmt`-clean, so there is no formatting check yet.
Do not reformat files you are not otherwise touching - a diff that mixes real
changes with whitespace churn cannot be reviewed.

## Adding a backend

Backends implement `RemoteFilesystem` in `shared/ferrum-core`. Look at
`ferrite/crates/ferrite-core/src/webdav.rs` for a compact reference
implementation. A new backend needs, at minimum: path handling that cannot
escape its configured root, an auth path that does not log secrets, and
`stat`/`list_dir`/`read_file`/`write_file` that map remote failures onto
`RemoteError` rather than panicking.

## Security issues

Do not open a public issue for a vulnerability. Use [private
reporting](https://github.com/Akame-Lucy/Ferrum/security/advisories/new).
[SECURITY.md](SECURITY.md) covers the deployment model and what is in scope.

## Licence

Contributions are accepted under the [MIT License](../LICENSE), the same terms as
the project.
