# mount-fs

`mount-fs` is a Linux command-line tool for mounting cloud drives — Aliyun
Drive and S3-compatible object storage — on the local filesystem by
orchestrating existing FUSE tools (`s3fs-fuse`, `aliyundrive-fuse`, `qrypt`,
`aliyundrive-webdav`).

Rather than reimplementing cloud storage clients, `mount-fs` manages their
detection, installation, configuration, and lifecycle behind one consistent
CLI. It also detects and installs missing runtime dependencies (FUSE 3,
`rclone`, …) either explicitly or on demand while mounting.

## Status

Both providers are usable end to end:

- **S3** via `s3fs-fuse` (AWS S3, MinIO, Ceph RGW, Aliyun OSS, …).
- **Aliyun Drive** via `aliyundrive-webdav` (QR/OpenAPI token, read/write, no
  `drive_id`), `qrypt` (OpenAPI token + `drive_id`, read/write), or
  `aliyundrive-fuse` (legacy web token, read-only).

Environment diagnostics (`doctor`, safe `doctor --fix`), engine and dependency
installation (`providers`, `install`, `deps`), profile and credential
management (`config ...`), the mount lifecycle (`mount`, `umount`, `list`,
`status`, `logs`), systemd user-service generation (`service ...`) for
persistent mounts, and source/release packaging are all implemented and tested.

Secrets are never accepted through `mount-fs`'s own argv: they come from
stdin/interactive prompts (or a pre-existing `0600` file) and are referenced by
a profile's `credentials_ref`.

## Supported drives and engines

| Provider | Engine | Read/write | Credential | Notes |
| --- | --- | --- | --- | --- |
| S3 | `s3fs-fuse` | r/w | access key + secret | Packaged in Fedora/RHEL (EPEL); non-AWS endpoints use `url`, path-style and `region`/`sigv4`. |
| Aliyun | `aliyundrive-webdav` | r/w | OpenAPI refresh token | Two-process bridge: loopback WebDAV server + `rclone mount`. No `drive_id`; supports `drive_type`. |
| Aliyun | `qrypt` | r/w | OpenAPI refresh token + `drive_id` | Single process, foreground FUSE 3 mount. |
| Aliyun | `aliyundrive-fuse` | read-only | legacy web refresh token | Archived upstream; no `drive_id`. |

Use `mount-fs providers` to see which engines are installed and each engine's
`token_kind` and capabilities.

## Requirements

- Linux with FUSE 3 (`/dev/fuse`, `fusermount3`).
- Rust 1.85 or newer to build from source.

Runtime dependencies such as `fuse3` and `rclone` (for the webdav bridge) are
detected by `mount-fs deps` and can be installed automatically.

## Build and install

```sh
cargo build --release --locked
install -m 0755 target/release/mount-fs ~/.local/bin/mount-fs
```

### Packaging

```sh
bash scripts/package.sh --locked
```

This writes `dist/mount-fs-<version>-<target>.tar.gz` and a `.sha256` checksum
(binary, README, LICENSE, `dev-docs/`, and user docs). Tagged builds (`v*`) run
the same script in `.github/workflows/release.yml` and attach the archives to a
GitHub Release.

## Quick start

### S3 with `s3fs-fuse`

```sh
mount-fs config profile add bucket \
  --provider s3 \
  --engine s3fs-fuse \
  --mountpoint ~/mnt/bucket \
  --bucket mybucket \
  --endpoint https://minio.example.com \
  --region us-east-1 \
  --path-style

# Credentials are read from stdin and stored in a 0600 file (never argv).
mount-fs config login s3 --profile bucket
mount-fs mount bucket
```

### Aliyun Drive with the QR flow (webdav, no `drive_id`)

```sh
mount-fs config profile add aliyun \
  --provider aliyun \
  --engine aliyundrive-webdav \
  --mountpoint ~/mnt/aliyun \
  --drive-type resource          # resource (资源库) | backup (备份盘) | default

# Opens the QR login in your terminal and stores the token (never printed).
mount-fs config login aliyun --profile aliyun --qr
mount-fs mount aliyun
```

### Aliyun Drive with `qrypt` (read/write, needs `drive_id`)

```sh
mount-fs config profile add pan \
  --provider aliyun \
  --engine qrypt \
  --mountpoint ~/mnt/pan \
  --drive-id <DRIVE_ID>

# qrypt has no interactive login: provide the OpenAPI refresh token on stdin.
mount-fs config login aliyun --profile pan
mount-fs mount pan
```

`aliyundrive-fuse` (read-only, legacy web token) is also selectable with
`--engine aliyundrive-fuse`.

## Dependency management

```sh
mount-fs doctor                     # environment + dependency status
mount-fs deps                       # /dev/fuse, fusermount3, rclone, ...
mount-fs deps install --engine aliyundrive-webdav   # install missing packages
mount-fs install s3fs-fuse --no-deps                 # engine only, skip deps
```

`mount --auto-install` (or `general.auto_install = true`) installs a missing
engine and its runtime dependencies before mounting. Auto-install never compiles
from source, and `--dry-run` prints the exact install plan without changing the
system.

## Mount lifecycle

- `mount <profile>` is idempotent: an already-mounted profile is a no-op.
- `umount <profile|path>` unmounts and cleans state/credentials; `--force`
  escalates (lazy unmount, process termination) for stuck mounts.
- Stale/broken FUSE mountpoints left by a crash are detected and lazy-unmounted
  before a new mount.
- `list`, `status [--json]`, and `logs [--follow]` inspect managed mounts.

## Persistent mounts (systemd)

Generate a user `.service` for a profile (credentials must live in a persistent
`0600` `credentials_ref`, not `$XDG_RUNTIME_DIR`):

```sh
mount-fs service generate aliyun
systemctl --user daemon-reload
systemctl --user enable --now mount-fs-aliyun.service
```

`mount-fs service list [--json]` lists generated units and
`mount-fs service remove aliyun` deletes one. The unit uses
`ExecStopPost=-fusermount3 -u <mountpoint>` and start limiting so a broken mount
does not restart forever. To mount before login, enable lingering:
`sudo loginctl enable-linger "$USER"`.

## Security

- Secrets are never passed through `mount-fs`'s argv; `config get`/`config show`
  redact secret values.
- Credentials are stored in `0600` files (backend files are `0700`), and
  temporary credential artifacts are removed on unmount and swept on startup.
- Release archives are verified against pinned SHA-256 checksums before install.

## End-to-end tests

On a host with FUSE (and the relevant engines), run a self-contained test:

```sh
scripts/e2e-s3.sh        # rclone-served S3 + s3fs-fuse
scripts/e2e-aliyun.sh    # Aliyun; ALIYUN_ENGINE=aliyundrive-webdav|qrypt
```

They print `SKIP:` with a reason when FUSE mounting is unavailable. The Aliyun
script reads `ALIYUN_REFRESH_TOKEN` (and `ALIYUN_DRIVE_ID` for qrypt) from the
environment; tokens are never placed in argv.

## Documentation

- [Usage guide](docs/usage.md) — quickstart, S3 and Aliyun profiles, mounting,
  persistent services, and the end-to-end smoke tests.
- [Troubleshooting](docs/troubleshooting.md) — installation, FUSE permission
  errors, stale mounts, Aliyun tokens, credentials, and systemd environment
  issues.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT. See [LICENSE](LICENSE).
