# Troubleshooting

Start with `mount-fs doctor` and `mount-fs providers`; most failures are host
prerequisites or a mismatch between the profile and the installed engine.

## Engine not found or installation fails

Symptoms:

```text
error: engine `s3fs-fuse` is not installed; run: mount-fs install s3fs-fuse
error: engine `qrypt` is not installed and no fallback engine is available
```

Checks and fixes:

```sh
mount-fs providers
mount-fs install s3fs-fuse --dry-run
mount-fs install s3fs-fuse
```

- Package-managed engines (`s3fs-fuse`) are installed through dnf/apt/pacman.
  `mount-fs doctor` reports the detected package manager and privilege
  escalation path (`sudo -n` or `pkexec`).
- Release-archive engines (`qrypt`, `aliyundrive-fuse`) install to
  `~/.local/bin` by default. Ensure that directory is on `PATH` in your shell
  **and** in the systemd user-manager environment used by persistent services.
- `s3fs-fuse` must be at least 1.96 (FUSE 3 and `-o region`). Check the version
  shown by `mount-fs providers`.
- `mount-fs install <engine> --method source` prints the source-build recipe;
  it only executes with `--yes`. Install the listed distro build dependencies
  first.
- If an engine was installed but `providers` still says `[WARN] not found`,
  run `command -v <engine>` and inspect `PATH`. A shell that can see the binary
  does not guarantee systemd can; see the systemd section below.

### Checksum rejection

Release archives are installed only when mount-fs has an **embedded pinned
sha256** for the exact engine version and architecture:

```text
error: <engine> <version> has no pinned checksum; refusing to install an unverified artifact
error: checksum mismatch for <archive>: expected ..., got ...
```

- Re-run `mount-fs install <engine> --dry-run` and verify the URL, version, and
  expected checksum.
- A mismatch means the downloaded artifact is not the one mount-fs was built
  to trust. Do not disable verification and do not install the file manually;
  wait for a mount-fs release that pins the new artifact, or use the distro
  package manager where available.
- Remove partial downloads and retry; mount-fs downloads into a temporary
  directory and deletes it on failure. Check network/proxy interference if the
  same URL fails repeatedly.
- Release downloads require `--yes`. This is deliberate: the archive is
  downloaded and a binary is installed.

## `allow_other` fails or other users get permission denied

`allow_other` is a FUSE mount option enforced by the kernel and
`fusermount3`. It requires the line `user_allow_other` in `/etc/fuse.conf`:

```sh
mount-fs doctor
grep -n user_allow_other /etc/fuse.conf
```

If it is missing:

```sh
# As root, doctor --fix appends the directive:
sudo "$(command -v mount-fs)" doctor --fix

# Or as a normal user, run the command doctor prints:
sudo sh -c 'echo user_allow_other >> /etc/fuse.conf'
```

Then remount with `--allow-other` (or set `allow_other = true` in the profile).
A warning during `mount` such as "allow_other requested but /etc/fuse.conf does
not set user_allow_other" means the mount may succeed for your user but other
users cannot access it. `mount-fs doctor --fix` only touches
`/etc/fuse.conf`; it does not change profiles or other system files.

## `fusermount3: mount failed: Operation not permitted`

This is an environment/permission problem, not a profile problem. Check the
basics first:

```sh
mount-fs doctor
ls -l /dev/fuse
command -v fusermount3
grep -E '^(NoNewPrivs|CapEff):' /proc/self/status
```

`fusermount3` is setuid/setgid in the normal desktop setup. A process with
`NoNewPrivs: 1` cannot gain that privilege, so the mount fails with `Operation
not permitted`. In a normal interactive shell, `NoNewPrivs` should be `0`. If it
is `1`, you are likely inside a sandbox, a service manager with
`NoNewPrivileges=yes`, or a container; run mount-fs from an unrestricted
session instead of trying to bypass the kernel restriction.

`CapEff` shows the effective capability mask. In a full user session, FUSE
mounts work through `fusermount3` without `CAP_SYS_ADMIN` in the calling
process. In a container, FUSE generally requires:

- `/dev/fuse` passed into the container,
- `CAP_SYS_ADMIN` in the container,
- an AppArmor/SELinux profile that permits mounting,
- and a container startup that does not force `NoNewPrivs: 1`.

For example (Docker):

```sh
docker run --rm -it \
  --device /dev/fuse \
  --cap-add SYS_ADMIN \
  --security-opt apparmor:unconfined \
  -v "$PWD:/work" \
  fedora:latest
```

Rootless containers and CI sandboxes may still be unable to mount FUSE even
with those flags. This is why `scripts/e2e-s3.sh` prints `SKIP:` instead of
failing in such environments. On the host, also confirm the kernel module is
available:

```sh
lsmod | grep '^fuse'
sudo modprobe fuse
```

## Hung, stale, or stuck mounts

Symptoms include `status` reporting `hung`/`stale`, `ls` blocking on the
mountpoint, or unmount failing with "target is busy".

```sh
mount-fs list
mount-fs status <profile>
mount-fs logs <profile>
mount-fs umount <profile> --force
```

`--force` uses a lazy unmount (`fusermount3 -uz`). If the mountpoint is still
present after that:

```sh
fusermount3 -uz /path/to/mountpoint
sudo umount -l /path/to/mountpoint
```

Check whether the engine process is still alive:

```sh
mount-fs status <profile> --json
ps -ef | grep -E 's3fs|qrypt|aliyundrive'
```

If the kernel mount is gone but state remains, re-run `mount-fs list` (it
reconciles state against `/proc/self/mountinfo`) and `mount-fs umount
<profile>` to clean the stale record. Avoid removing credential or state files
manually while an engine is still running. Reboot only as a last resort.

## Aliyun token problems and engine token-kind incompatibility

The Aliyun engines do **not** share tokens:

| Engine | Token kind | Writes | Extra requirement |
| --- | --- | --- | --- |
| `aliyundrive-fuse` | legacy web token | no (read-only) | archived upstream |
| `qrypt` | OpenAPI refresh token | yes | `drive_id` |
| `aliyundrive-webdav` | OpenAPI refresh token | yes | `rclone`; runs a loopback WebDAV server + `rclone mount` |

Common causes:

- **Wrong token type.** A web token pasted for `qrypt` (or an OpenAPI token
  pasted for `aliyundrive-fuse`) is rejected or fails during engine startup.
  Check `mount-fs providers` for each engine's `token_kind`, then re-login for
  the engine you actually use:
  ```sh
  # Legacy web token (aliyundrive-fuse):
  mount-fs config login aliyun --profile <profile>

  # OpenAPI refresh token + drive id (qrypt):
  mount-fs config login aliyun --profile <profile> --drive-id <drive-id>
  ```
- **Missing `drive_id`.** `qrypt` requires a non-empty drive id. Add it when
  creating the profile (`--drive-id`) or during `config login aliyun
  --drive-id`. `mount-fs config profile set <profile> drive_id=<id>` also works.
- **Expired/revoked token.** Log in again with a freshly issued token. For
  `aliyundrive-fuse`, mount-fs persists a token rotated by the engine on
  unmount; if the token was invalidated upstream, that cannot help.
- **Archived/read-only engine.** `aliyundrive-fuse` is archived and read-only.
  It may stop working when Aliyun changes its legacy API. For a writable mount,
  install `qrypt` and create a profile with `--engine qrypt --drive-id ...`.
- **Non-interactive sessions.** If stdin is not a terminal, mount-fs cannot
  prompt. Set `ALIYUN_REFRESH_TOKEN` (or `REFRESH_TOKEN` for engine
  compatibility) or, preferably, use a `0600` `credentials_ref` created by
  `config login`. A profile's `credentials_ref` takes precedence over the
  environment.
- **Systemd.** Environment variables from your shell do not reach the systemd
  user manager; use a persistent credential file.

Inspect engine output with:

```sh
mount-fs logs <profile>
mount-fs mount <profile> --dry-run
```

`--dry-run` shows the engine command and confirms which profile/engine/mountpoint
would be used, without printing secrets.

## Read-only engine with `--rw`

If a profile requests a writable mount but the selected engine cannot write,
mount-fs refuses:

```text
error: engine `aliyundrive-fuse` is read-only; use --ro or --engine qrypt for a writable mount
```

`mount-fs providers` prints `read_write=true/false` for every engine. Either:

```sh
mount-fs mount <profile> --ro
```

or install `qrypt` and use a writable profile:

```sh
mount-fs install qrypt
mount-fs config profile set <profile> engine=qrypt drive_id=<drive-id>
mount-fs config login aliyun --profile <profile> --drive-id <drive-id>
mount-fs mount <profile> --rw
```

Remember that changing the engine means changing the token kind; a legacy web
token cannot authenticate `qrypt`.

## Credential and permissions errors

`mount-fs` refuses credential files that other users can read:

```text
error: credential file ... is group/world accessible (mode 644); expected 0600
```

Fix permissions:

```sh
chmod 700 ~/.config/mount-fs/credentials
chmod 600 ~/.config/mount-fs/credentials/*
```

`config login` creates private files and directories; prefer it over manual
editing. If you copy a credential file, preserve `0600`. The runtime directory
(`$XDG_RUNTIME_DIR/mount-fs`, fallback `/tmp/mount-fs-<uid>`) is also private
(`0700`) and is used for temporary `s3fs` passwd files and engine work dirs.

Other checks:

- The profile's `credentials_ref` must point to a file that exists and contains
  the expected format (`ACCESS:SECRET` for S3; a refresh token for Aliyun).
- `mount-fs config logout s3 --profile backup` (or `aliyun`) removes the stored
  file and clears `credentials_ref`; log in again afterward.
- Secrets are never accepted via `mount-fs` argv or `config set`. Put
  credentials through `config login`/stdin. `config show` and `config get`
  redact secret values.
- If `~` paths in the config are not expanded as expected, check that the
  profile was written through `config profile add/set` or the TOML loader; paths
  beginning with `~/` are expanded when the config is loaded.
- Do not run `mount-fs` with a different `XDG_CONFIG_HOME` than the one used for
  `config login`, or it will look in a different config/credential directory.

## systemd services do not see shell environment

A generated user unit starts a new process from the systemd user manager. It
does not inherit variables exported in your interactive shell (`AWS_ACCESS_KEY_ID`,
`ALIYUN_REFRESH_TOKEN`, custom `PATH`, etc.). This is intentional and is why
`service generate` warns when a profile has no `credentials_ref`.

Fix:

```sh
# Store durable credentials in the same config tree the service uses.
mount-fs config login s3 --profile backup
mount-fs config validate backup

mount-fs service generate backup --force
systemctl --user daemon-reload
systemctl --user restart mount-fs-backup.service
```

Also verify the engine binary is visible to the manager:

```sh
systemctl --user show-environment | grep '^PATH='
command -v s3fs qrypt aliyundrive-fuse
```

If an engine is in `~/.local/bin` but not in the manager's `PATH`, either add
that directory to the manager environment (for example with
`systemctl --user import-environment PATH` in your session startup) or install
the engine system-wide. Check the unit and its logs:

```sh
systemctl --user cat mount-fs-backup.service
systemctl --user status mount-fs-backup.service
journalctl --user -u mount-fs-backup.service -n 100
mount-fs logs backup
```

If the service starts before the network is usable, it may fail and restart.
The generated unit orders after `network-online.target`; for cloud endpoints
that need VPN/DNS setup, adjust the unit or start it manually after the network
is ready.

## Still stuck?

Collect:

```sh
mount-fs version
mount-fs doctor
mount-fs providers
mount-fs config validate <profile>
mount-fs status <profile> --json
mount-fs logs <profile>
grep -E '^(NoNewPrivs|CapEff):' /proc/self/status
```

Include the command that failed and the final `error:` line. Never paste
refresh tokens, access keys, or the contents of credential files into an issue.
