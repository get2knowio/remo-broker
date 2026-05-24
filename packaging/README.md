# Packaging the `remo-broker` daemon

These files are the systemd-side of FR-023 (see
`specs/001-broker-daemon/spec.md`). They expect the broker binary at
`/usr/bin/remo-broker`; adjust `ExecStart=` in the unit if your
package puts it elsewhere.

## Files

| Path in repo | Install location | Purpose |
|---|---|---|
| `systemd/remo-broker.service` | `/etc/systemd/system/remo-broker.service` (or `/usr/lib/systemd/system/`) | The unit. `Type=notify`, full hardening directives. |
| `sysusers.d/remo-broker.conf` | `/usr/lib/sysusers.d/remo-broker.conf` | Creates the `remo-broker` system user/group. |
| `tmpfiles.d/remo-broker.conf` | `/usr/lib/tmpfiles.d/remo-broker.conf` | Ensures `/run/remo-broker` + `/var/log/remo-broker` exist with correct perms for ad-hoc (non-systemd) runs. |

## Quick install (manual, no .deb yet)

```bash
sudo cp packaging/systemd/remo-broker.service     /etc/systemd/system/
sudo cp packaging/sysusers.d/remo-broker.conf     /usr/lib/sysusers.d/
sudo cp packaging/tmpfiles.d/remo-broker.conf     /usr/lib/tmpfiles.d/

sudo systemd-sysusers                  # creates the remo-broker user
sudo systemd-tmpfiles --create         # creates the runtime/log dirs
sudo systemctl daemon-reload
```

## Provisioning the bootstrap token

The unit ships with `LoadCredential=bootstrap-token:/etc/remo-broker/bootstrap-token`
(plaintext on disk). Two paths:

### A. Plaintext (default)

```bash
sudo mkdir -p /etc/remo-broker
sudo install -m 0600 -o root -g root /dev/stdin /etc/remo-broker/bootstrap-token <<EOF
<paste the token Remo minted for this instance>
EOF
```

The file lives under `/etc` which `ProtectSystem=strict` makes read-only
to the daemon. Mode `0600` keeps it root-only.

### B. TPM2-sealed (preferred when available)

```bash
# Encrypt once, ship the .cred file:
sudo systemd-creds encrypt --name=bootstrap-token \
  /etc/remo-broker/bootstrap-token /etc/remo-broker/bootstrap-token.cred
sudo shred -u /etc/remo-broker/bootstrap-token
```

Then in `remo-broker.service`, comment out the `LoadCredential=` line and
uncomment `LoadCredentialEncrypted=`. systemd unseals the credential at
unit start; the plaintext exists only inside the per-unit credentials
tmpfs and is never written back to disk.

## Provisioning fnox-core config

The daemon talks to credential backends through fnox-core. By default
it calls `Fnox::discover()`, which walks for `fnox.toml` upward from
the working directory and merges parent/local/global configs (matches
the `fnox` CLI). Under systemd the working directory is typically `/`,
which means discover walks no config and returns an error — the daemon
will log a warning and start in degraded mode (admin/ping/info/cache
work; `get` returns `backend_error`).

For most installs you'll want to pass an explicit path. Append to
`ExecStart=` in the unit (or use a drop-in):

```
ExecStart=/usr/bin/remo-broker \
  --bootstrap-token-path %d/bootstrap-token \
  --fnox-config /etc/remo-broker/fnox.toml
```

`fnox.toml` syntax and provider configuration are documented at
<https://github.com/jdx/fnox>.

## Start, verify, troubleshoot

```bash
sudo systemctl enable --now remo-broker
sudo systemctl status remo-broker

# Stream logs:
sudo journalctl -u remo-broker -f

# Smoke-test the admin socket:
echo '{"op":"status"}' | sudo socat - UNIX-CONNECT:/run/remo-broker/admin.sock

# Tail the audit log:
sudo tail -f /var/log/remo-broker/audit.log
```

`systemctl status` will report `Active: active (running)` once the
daemon has bound its sockets and sent `READY=1`. If the unit fails:

- **`bootstrap token unavailable`** — the credential file is missing
  or empty. Re-check the file path under `/etc/remo-broker/`.
- **`fnox-core session could not be discovered`** — pass
  `--fnox-config /path/to/fnox.toml`. Without it the daemon starts
  in degraded mode (cache-only).
- **`failed to bind admin socket`** — a previous daemon left a stale
  socket file (FR-009 should handle this; if it's failing the
  underlying error in the journal will say why — usually a perms
  mismatch on `/run/remo-broker`).
- **Unit refuses to start at all** — `systemd-analyze verify
  /etc/systemd/system/remo-broker.service` will pinpoint syntax issues.

## Runtime dependency note

fnox-core pulls in `hidapi` transitively (for YubiKey / WebAuthn
providers via `ctap-hid-fido2`). `hidapi` dynamically links
`libudev`, so production hosts need it installed. On Debian / Ubuntu:

```bash
sudo apt-get install -y libudev1
```

(Almost always already present because `systemd` itself depends on
`libudev1`.)
