# mount-fs

`mount-fs` is a Linux command-line tool for mounting cloud drives — Aliyun
Drive and S3-compatible object storage — by orchestrating existing FUSE tools
such as `s3fs-fuse`, `aliyundrive-fuse`, and `qrypt`.

Rather than reimplementing cloud storage clients, `mount-fs` manages their
installation, configuration, and lifecycle behind a single consistent CLI.

## Status

**M6 — release-ready.** The S3 and Aliyun providers are usable end to end:
environment diagnostics (`doctor`, including a safe `--fix`), engine detection
and installation (`providers`, `install`), profile and credential management
(`config ...`), the mount lifecycle (`mount`, `umount`, `list`, `status`,
`logs`), systemd user-service generation (`service ...`) for persistent mounts,
cross-distro CI, and source/release archive packaging.

Secrets are never accepted through `mount-fs`'s own argv: they come from
stdin/interactive prompts and are stored in `0600` credential files referenced
by a profile's `credentials_ref`.

## Documentation

- [Usage guide](docs/usage.md) — quickstart, S3 and Aliyun profiles, mounting,
  persistent services, and the end-to-end smoke test.
- [Troubleshooting](docs/troubleshooting.md) — installation, FUSE permission
  errors, stale mounts, Aliyun tokens, credentials, and systemd environment
  issues.

## Capabilities

- Detect, install, and uninstall supported mount engines (`install`,
  `uninstall`, `providers`).
- Configure providers and credentials (`config init|list|get|set|login|logout`).
- Manually invoked and unattended mounts (`mount`, `umount`, `list`,
  `status`, `logs`, `service`).
- Preflight host diagnostics (`doctor`).

## Persistent mounts (systemd)

Generate a user `.service` for a profile (credentials must live in a persistent
`0600` `credentials_ref`, not `$XDG_RUNTIME_DIR`):

```sh
mount-fs service generate bucket
systemctl --user daemon-reload
systemctl --user enable --now mount-fs-bucket.service
```

`mount-fs service list [--json]` lists generated units and
`mount-fs service remove bucket` deletes one. The unit uses
`ExecStopPost=-fusermount3 -u <mountpoint>` so an already-unmounted target does
not fail the unit.

## End-to-end test

On a host with FUSE and `rclone`/`s3fs`, run the self-contained S3 smoke test:

```sh
scripts/e2e-s3.sh
```

It prints `SKIP:` with a reason when FUSE mounting is unavailable.

## Requirements

- Linux with FUSE support (`/dev/fuse`).
- Rust 1.85 or newer to build from source.

## Build

```sh
cargo build --release --locked
```

The binary is written to `target/release/mount-fs`.

## Packaging

Build a self-contained source/binary archive (binary, README, LICENSE,
`dev-docs/`, and user documentation) plus its sha256 checksum:

```sh
bash scripts/package.sh --locked
```

Artifacts are written to `dist/mount-fs-<version>-<target>.tar.gz` and
`dist/mount-fs-<version>-<target>.tar.gz.sha256`. Tagged builds (`v*`) run the
same script in `.github/workflows/release.yml` and attach the archives to a
GitHub Release. Builds for `x86_64-unknown-linux-gnu` are authoritative; the
`aarch64-unknown-linux-gnu` cross build is best-effort.

Install from a release archive with:

```sh
tar xzf mount-fs-<version>-<target>.tar.gz
install -m 0755 mount-fs-<version>-<target>/mount-fs ~/.local/bin/mount-fs
```

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
