# mount-fs usage guide

`mount-fs` is a Linux CLI that mounts cloud drives by orchestrating existing FUSE
engines. This guide covers install, host checks, profiles, S3/Aliyun examples,
the mount lifecycle, persistent systemd services, and the end-to-end smoke test.

For fixes to common failures, see [troubleshooting.md](troubleshooting.md).

## Requirements

- Linux with FUSE support (`/dev/fuse`, `fusermount3`).
- Rust 1.85 or newer to build from source.
- `s3fs-fuse` for S3 profiles; `aliyundrive-fuse` or `qrypt` for Aliyun Drive.
- `allow_other` mounts additionally require `user_allow_other` in
  `/etc/fuse.conf`.

## Install

From a release archive:

```sh
tar xzf mount-fs-<version>-<target>.tar.gz
install -m 0755 mount-fs-<version>-<target>/mount-fs ~/.local/bin/mount-fs
```

Make sure `~/.local/bin` is on `PATH`. From source:

```sh
cargo build --release --locked
install -m 0755 target/release/mount-fs ~/.local/bin/mount-fs
```

## Quickstart (S3)

```sh
# 1. Inspect the host and available engines.
mount-fs doctor
mount-fs providers

# 2. Install the S3 engine (dnf/apt/pacman, depending on the distro).
mount-fs install s3fs-fuse

# 3. Create a profile. Paths may start with ~/.
mount-fs config profile add photos \
  --provider s3 \
  --engine s3fs-fuse \
  --mountpoint ~/mnt/photos \
  --bucket my-photos \
  --region us-east-1

# 4. Store credentials. You are prompted on stdin; secrets never go in argv.
mount-fs config login s3 --profile photos

# 5. Validate and mount.
mount-fs config validate photos
mount-fs mount photos
mount-fs status photos
ls ~/mnt/photos
```

## Host checks: `doctor` and `providers`

`mount-fs doctor` reports the distro, package manager, `/dev/fuse`,
`fusermount3`, `/etc/fuse.conf`, privilege escalation, and active FUSE mounts.
It is advisory: a `[WARN]` explains what is missing but does not block commands
that do not need it.

`mount-fs doctor --fix` performs one safe, minimal fix: when run as root it
appends `user_allow_other` to `/etc/fuse.conf` if needed. As a normal user it
prints the exact `sudo` command instead of modifying system files.

`mount-fs providers` lists each provider and engine, whether the binary was
found and its version, capability flags (`read_write`, `allow_other`,
`foreground`), the token kind (`aws-key`, `legacy-web`, `openapi`), and whether
credentials are temporary or persistent.

Install a single engine with:

```sh
mount-fs install s3fs-fuse
mount-fs install qrypt
mount-fs install aliyundrive-fuse
```

Use `--dry-run` to see the package/download/source-build plan without changing
the system. Release archives require `--yes` because they download and execute
an installation; source builds require `--method source --yes`.

## Profiles and configuration

The default config file is `$XDG_CONFIG_HOME/mount-fs/config.toml`
(`~/.config/mount-fs/config.toml` when unset). Pass `--config PATH` before the
subcommand to use another file.

```sh
mount-fs config init                 # write a commented starter config
mount-fs config list                 # profile names and mountpoints
mount-fs config show [PROFILE]       # redacted effective configuration
mount-fs config validate [PROFILE]   # provider/bucket/engine checks
mount-fs config get <key>            # secrets are always redacted
mount-fs config set general.allow_other true
```

Profiles are managed with `config profile`:

```sh
mount-fs config profile add NAME --provider s3|aliyun --mountpoint PATH [flags]
mount-fs config profile set NAME region=us-west-2 path_style=true
mount-fs config profile remove NAME
```

Non-secret profile fields that can be changed with `config profile set` include
`mountpoint`, `engine`, `bucket`, `endpoint`, `region`, `drive_id`,
`path_style`, `allow_other`, and `read_only`. Secret-bearing keys are refused
from argv; use `config login`, which reads from stdin or an interactive prompt.
`config show` and `config get` never print stored secrets.

Stored credential files are created under
`$XDG_CONFIG_HOME/mount-fs/credentials/` with mode `0600`; the directory is
`0700`. The profile records that path in `credentials_ref`.

## S3 examples

All S3 profiles use the `s3fs-fuse` engine. Non-AWS endpoints require a region
for SigV4, and most S3-compatible services need path-style requests.

### AWS S3

```sh
mount-fs config profile add backup \
  --provider s3 \
  --engine s3fs-fuse \
  --mountpoint ~/mnt/backup \
  --bucket my-backup-bucket \
  --region us-east-1

mount-fs config login s3 --profile backup
mount-fs mount backup
```

Omitting `--endpoint` lets `s3fs` use the AWS default endpoint for the region.

### MinIO

```sh
mount-fs config profile add minio \
  --provider s3 \
  --engine s3fs-fuse \
  --mountpoint ~/mnt/minio \
  --bucket photos \
  --endpoint https://minio.example.com \
  --region us-east-1 \
  --path-style

mount-fs config login s3 --profile minio
mount-fs mount minio
```

Use the MinIO access key as the AWS access key id and the MinIO secret key as
the AWS secret access key. If MinIO uses a self-signed certificate, configure
trust for the system/`s3fs` environment rather than disabling verification.

### Aliyun OSS (S3-compatible endpoint)

```sh
mount-fs config profile add oss \
  --provider s3 \
  --engine s3fs-fuse \
  --mountpoint ~/mnt/oss \
  --bucket my-oss-bucket \
  --endpoint https://oss-cn-hangzhou.aliyuncs.com \
  --region oss-cn-hangzhou \
  --path-style

mount-fs config login s3 --profile oss
mount-fs mount oss
```

Use an OSS AccessKey ID/secret. Keep `--path-style` and a region matching the
OSS endpoint.

### Ad-hoc S3 mount

A profile is not required for a one-off S3 mount:

```sh
mount-fs mount s3 \
  --mountpoint ~/mnt/tmp \
  --bucket photos \
  --endpoint https://minio.example.com \
  --region us-east-1 \
  --engine s3fs-fuse
```

Credentials are then read from `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`, or
prompted interactively. Prefer `config login` for persistent use.

## Aliyun Drive examples

Aliyun Drive has two incompatible credential systems:

- **Legacy web token** — used by `aliyundrive-fuse`, which is archived upstream,
  read-only, and accepts a token from the old web session.
- **OpenAPI refresh token + drive id** — used by `qrypt` (read/write) and by the
  `aliyundrive-webdav` engine, which runs a loopback WebDAV server bridged to FUSE
  through `rclone mount` (requires `rclone`). These tokens cannot be used with
  `aliyundrive-fuse`, and vice versa.

Check `mount-fs providers` to see each engine's `token_kind` and capability
flags.

### Read-only: `aliyundrive-fuse`

```sh
mount-fs install aliyundrive-fuse

mount-fs config profile add pan \
  --provider aliyun \
  --engine aliyundrive-fuse \
  --mountpoint ~/mnt/pan \
  --read-only

# Paste a legacy web refresh token when prompted.
mount-fs config login aliyun --profile pan
mount-fs mount pan
```

`aliyundrive-fuse` is read-only. It cannot satisfy a read/write mount; use
`qrypt` for writes. Its upstream project is archived and may eventually stop
working with Aliyun API changes.

### Read/write: `qrypt`

`qrypt` needs an OpenAPI refresh token and the numeric `drive_id`:

```sh
mount-fs install qrypt

mount-fs config profile add pan-rw \
  --provider aliyun \
  --engine qrypt \
  --mountpoint ~/mnt/pan-rw \
  --drive-id <drive-id>

mount-fs config login aliyun --profile pan-rw --drive-id <drive-id>
mount-fs mount pan-rw
```

`qrypt` runs in the foreground; `mount-fs mount` supervises it in the
background (or use `--foreground` to keep it attached to the terminal).

### QR login: `aliyundrive-webdav` (no drive_id)

`aliyundrive-webdav` needs only an OpenAPI refresh token and can obtain one via
a scannable QR code. It runs a loopback WebDAV server bridged to FUSE by
`rclone mount`, and needs no `drive_id`:

```sh
mount-fs config profile add aliyun --provider aliyun --engine aliyundrive-webdav --mountpoint ~/mnt/aliyun
mount-fs config login aliyun --profile aliyun --qr     # scan the QR in your terminal
mount-fs mount aliyun
```

This flow requires both `aliyundrive-webdav` and `rclone` to be installed. The
QR is printed by the upstream tool; mount-fs stops echoing before the
`refresh_token:` block so the token is never shown or logged, then stores it in
a `0600` credential file and sets the profile's `credentials_ref`.

> QR login yields an **OpenAPI** refresh token. It is not usable with
> `aliyundrive-fuse` (legacy web token) or with `qrypt` unless you also supply
> the matching `drive_id`.

If an Aliyun profile requests an engine that is not installed and
`general.engine_auto_fallback` is `true`, mount-fs may fall back to another
installed Aliyun engine. Because token kinds differ, verify which engine is
selected with `mount-fs providers` and `mount-fs status <profile>`.

## Mount lifecycle

```sh
mount-fs mount <profile> [--foreground] [--ro|--rw] [--allow-other] [--dry-run]
mount-fs umount <profile-or-mountpoint> [--force]
mount-fs list [--json]
mount-fs status <profile> [--json]
mount-fs logs <profile> [--follow]
```

- `mount` is idempotent. If the profile is already healthy it reports the
  current state instead of spawning a second engine.
- `--dry-run` prints the engine command without executing it; temporary
  credentials are not persisted.
- `--ro` / `--rw` override the profile's `read_only` setting.
- `--allow-other` requires `user_allow_other` in `/etc/fuse.conf`.
- `umount --force` uses a lazy unmount (`fusermount3 -uz`) for hung or stale
  mounts. Without `--force` it uses `fusermount3 -u`.
- `list` and `status` reconcile the saved state against
  `/proc/self/mountinfo`, so externally mounted FUSE filesystems and stale
  entries are reported too. `--json` is available for both.
- `logs` reads the engine log for a background mount; `--follow` tails it.

## Persistent mounts with systemd (user services)

Generate a user `.service` that starts a foreground mount and survives login
(within the user manager's lifetime):

```sh
# Credentials must live in a persistent 0600 file, not $XDG_RUNTIME_DIR.
mount-fs config login s3 --profile backup

mount-fs service generate backup
systemctl --user daemon-reload
systemctl --user enable --now mount-fs-backup.service
systemctl --user status mount-fs-backup.service
```

The unit is written to `$XDG_CONFIG_HOME/systemd/user/mount-fs-<profile>.service`
and uses:

- `ExecStart=<mount-fs> mount <profile> --foreground`
- `ExecStopPost=-fusermount3 -u <mountpoint>`
- `Restart=on-failure`

Useful options and commands:

```sh
mount-fs service generate backup --dry-run   # render without writing
mount-fs service generate backup --force     # overwrite an existing unit
mount-fs service generate backup --exec /usr/local/bin/mount-fs
mount-fs service list [--json]
mount-fs service remove backup
systemctl --user disable --now mount-fs-backup.service
```

systemd user services do **not** inherit your interactive shell environment.
Credentials referenced by the profile must exist on disk, and the engine binary
must be on the user manager's `PATH` (or installed system-wide). See
[troubleshooting.md](troubleshooting.md#systemd-services-do-not-see-shell-environment).

## End-to-end S3 smoke test

The repository includes a self-contained S3 test that starts a local S3 server
with `rclone serve s3`, mounts it through `s3fs`, reads/writes a file, checks
`list`/`status --json`, unmounts, and verifies cleanup:

```sh
scripts/e2e-s3.sh
```

It requires `rclone`, `s3fs`, `fusermount3`, `/dev/fuse`, and an environment
where FUSE mounts are permitted (`NoNewPrivs: 0`). If the environment cannot
mount FUSE, it prints a `SKIP:` line with the reason and exits successfully. It
is not part of `cargo test`. To test a specific binary:

```sh
MOUNT_FS=target/release/mount-fs scripts/e2e-s3.sh
```

## Security notes

- Secrets are read from stdin/interactive prompts, never from `mount-fs` argv.
- Credential files are `0600`; the credentials directory is `0700`.
  `mount-fs` refuses group/world-readable credential files.
- `s3fs` receives a temporary `passwd_file` in the runtime directory and never
  gets secrets on its command line.
- Aliyun tokens are written to private engine work/config files; the token is
  passed to engines through files or a redacted environment variable.
- `config show`/`config get` redact secret-bearing keys. `config set` refuses to
  set secrets from argv.
- Temporary credential artifacts are removed on unmount and rollback. Do not
  place persistent credentials in `$XDG_RUNTIME_DIR`.
