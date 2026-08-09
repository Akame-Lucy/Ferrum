<!--
The PR title becomes the commit message on main (squash merge), so it must
follow Conventional Commits - `fix(ferrite-core): ...`, `feat(ferrous): ...`,
`docs: ...`. The "PR title guard" check enforces this.
-->

## What this changes

<!-- One or two sentences. What is different after this merges? -->

## Why

<!-- The problem, bug, or gap. Link issues with "Closes #123". -->

## How it was verified

<!-- Delete what does not apply; add the real commands you ran. -->

- [ ] `cargo check --workspace --all-targets --locked`
- [ ] `cargo test --workspace --locked`
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] Exercised manually against a real backend (which one: )

## Security review

<!--
Ferrum brokers filesystem and shell access to remote hosts, so answer these
honestly even when the answer is "none".
-->

- Touches authentication, sessions, or credential handling: **no / yes ->**
- Touches path handling, `PathGrant`s, or `Capability` scoping: **no / yes ->**
- Touches the Noise transport or identity keys: **no / yes ->**
- Adds a new dependency: **no / yes ->** (what it does, why a dependency beats
  writing it, and its license)

## Notes for the reviewer

<!-- Anything non-obvious: tradeoffs taken, alternatives rejected, follow-ups
     you intend to file separately. -->
