# systemd units

Production install path for either twin: a dedicated unprivileged system
user, a hardened unit, no Docker. Docker is a dev/test convenience only: see [SECURITY.md](../../docs/SECURITY.md).

## Ferrous agent

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin ferrous
sudo mkdir -p /etc/ferrous
sudo cp ../../ferrous/config/ferrous.example.yaml /etc/ferrous/ferrous.yaml
# edit /etc/ferrous/ferrous.yaml: set allowed_paths, then run ferrite once to
# get its client pubkey (logged at startup) and paste it into clients[].pubkey
sudo chown -R ferrous:ferrous /etc/ferrous
sudo cp ferrous-agent.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now ferrous-agent
```

Edit `ReadWritePaths=` in the unit to match every path listed under
`allowed_paths` in `ferrous.yaml` (note that `ProtectSystem=strict` makes everything
else on disk read-only to the process, capability check or not).

## Ferrite

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin ferrite
sudo mkdir -p /etc/ferrite
sudo cp ../../ferrite/config/ferrite.example.yaml /etc/ferrite/ferrite.yaml
sudo chown -R ferrite:ferrite /etc/ferrite
sudo cp ferrite.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now ferrite
```

`/etc/ferrite` must stay in `ReadWritePaths=`: the Settings/Users panel and
the remote-connection editor write `ferrite.yaml` back to disk at runtime,
and Ferrite's own client identity key (for talking to Ferrous agents) lives
next to it.

## Verifying the sandbox

```bash
systemctl status ferrous-agent
systemctl status ferrite
journalctl -u ferrous-agent -f
journalctl -u ferrite -f
```

`systemd-analyze security ferrous-agent.service` (and the same for
`ferrite.service`) scores the unit's hardening and flags anything left wide
open.
