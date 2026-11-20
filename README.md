# mount-fs

`mount-fs` is a Linux command-line tool for mounting cloud drives — Aliyun
Drive and S3-compatible object storage — by orchestrating existing FUSE tools
such as `s3fs-fuse`, `aliyundrive-fuse`, and `qrypt`.

Rather than reimplementing cloud storage clients, `mount-fs` manages their
installation, configuration, and lifecycle behind a single consistent CLI.

## Status

**M4 — configuration and credential UX.** The S3 and Aliyun providers are
usable end to end: environment diagnostics (`doctor`), engine detection and
installation (`providers`, `install`), profile and credential management
(`config profile`, `show`, `validate`, `login`, `logout`), and the mount
lifecycle (`mount`, `umount`, `list`, `status`, `logs`) with state
reconciliation and clean rollback. systemd integration and packaging (M5–M6)
are still pending.

Secrets are never accepted through `mount-fs`'s own argv: they come from
stdin/interactive prompts and are stored in `0600` credential files referenced
by a profile's `credentials_ref`.

## Planned capabilities

- Detect, install, and uninstall supported mount engines (`install`,
  `uninstall`, `providers`).
- Configure providers and credentials (`config init|list|get|set|login|logout`).
- Manually invoked and (later) unattended mounts (`mount`, `umount`, `list`,
  `status`, `logs`).
- Preflight host diagnostics (`doctor`).

## Requirements

- Linux with FUSE support (`/dev/fuse`).
- Rust 1.85 or newer to build from source.

## Build

```sh
cargo build --release
```

The binary is written to `target/release/mount-fs`.

## Run

```sh
# Show usage
./target/release/mount-fs --help

# Run host diagnostics
./target/release/mount-fs doctor
```

### Create a profile and log in

```sh
# Add an S3 profile
mount-fs config profile add bucket \
  --provider s3 \
  --engine s3fs-fuse \
  --mountpoint ~/mnt/bucket \
  --bucket mybucket \
  --endpoint https://minio.example.com \
  --region us-east-1 \
  --path-style

# Store credentials (prompted on stdin; never via argv)
mount-fs config login s3 --profile bucket

# Inspect / validate / mount
mount-fs config show bucket
mount-fs config validate bucket
mount-fs mount bucket
```

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT. See [LICENSE](LICENSE).
