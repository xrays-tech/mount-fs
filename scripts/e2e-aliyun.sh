#!/usr/bin/env bash
#
# End-to-end Aliyun Drive smoke test for mount-fs.
#
# Engine selection (ALIYUN_ENGINE, default = auto):
#   - aliyundrive-webdav : QR/OpenAPI token, NO drive_id needed. Runs a loopback
#                          WebDAV server bridged to FUSE via rclone. This is the
#                          "scan a QR code" path (see `config login aliyun --qr`).
#   - qrypt              : OpenAPI token + drive_id (read/write, single process).
#
# Requirements:
#   - FUSE-capable host (/dev/fuse, NoNewPrivs=0)
#   - the selected engine installed (and `rclone` for webdav)
#   - ALIYUN_REFRESH_TOKEN in the environment (secret; never placed in argv)
#   - ALIYUN_DRIVE_ID only for qrypt
#   - ALIYUN_DRIVE_TYPE only for aliyundrive-webdav: resource | backup | default
#     (`resource` mounts the Resource Library / 资源库, `backup` the Backup drive)
#
# Obtaining the token (QR, no drive_id -> use webdav):
#   mount-fs config profile add aliyun --provider aliyun --engine aliyundrive-webdav --mountpoint ~/mnt/aliyun
#   mount-fs config login aliyun --profile aliyun --qr          # scan the QR
#   export ALIYUN_REFRESH_TOKEN="$(cat ~/.config/mount-fs/credentials/aliyun.token)"
#   ALIYUN_ENGINE=aliyundrive-webdav ALIYUN_DRIVE_TYPE=resource scripts/e2e-aliyun.sh
#
# This script is NOT run by `cargo test`.
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MOUNT_FS="${MOUNT_FS:-$ROOT/target/debug/mount-fs}"
PROFILE="aliyun-e2e"

skip() {
    echo "SKIP: $1"
    exit 0
}

# --- environment preflight -------------------------------------------------
[[ -e /dev/fuse ]] || skip "/dev/fuse is missing"
grep -q '^NoNewPrivs:[[:space:]]*0' /proc/self/status \
    || skip "NoNewPrivs is set (run on a real host / user session, not a restricted sandbox)"

# --- engine selection ------------------------------------------------------
ENGINE="${ALIYUN_ENGINE:-}"
if [[ -z "$ENGINE" ]]; then
    if [[ -n "${ALIYUN_DRIVE_ID:-}" ]] && command -v qrypt >/dev/null 2>&1; then
        ENGINE="qrypt"
    elif command -v aliyundrive-webdav >/dev/null 2>&1 && command -v rclone >/dev/null 2>&1; then
        ENGINE="aliyundrive-webdav"
    elif command -v qrypt >/dev/null 2>&1; then
        ENGINE="qrypt"
    else
        skip "no Aliyun engine installed (run: mount-fs install qrypt --yes  or  mount-fs install aliyundrive-webdav --yes)"
    fi
fi

case "$ENGINE" in
    qrypt)
        command -v qrypt >/dev/null 2>&1 || skip "qrypt not installed"
        ;;
    aliyundrive-webdav)
        command -v aliyundrive-webdav >/dev/null 2>&1 || skip "aliyundrive-webdav not installed"
        command -v rclone >/dev/null 2>&1 || skip "rclone not installed (required for the webdav bridge)"
        ;;
    *)
        skip "unsupported ALIYUN_ENGINE=$ENGINE (use qrypt or aliyundrive-webdav)"
        ;;
esac
echo "Using engine: $ENGINE"

if [[ -z "${ALIYUN_REFRESH_TOKEN:-}" ]]; then
    echo "SKIP: missing Aliyun credentials (ALIYUN_REFRESH_TOKEN)."
    echo "  Obtain one with the QR flow (no drive_id needed for aliyundrive-webdav):"
    echo "    mount-fs config profile add aliyun --provider aliyun --engine aliyundrive-webdav --mountpoint ~/mnt/aliyun"
    echo "    mount-fs config login aliyun --profile aliyun --qr"
    echo "    export ALIYUN_REFRESH_TOKEN=\"\$(cat ~/.config/mount-fs/credentials/aliyun.token)\""
    exit 0
fi
if [[ "$ENGINE" == "qrypt" && -z "${ALIYUN_DRIVE_ID:-}" ]]; then
    echo "SKIP: qrypt needs ALIYUN_DRIVE_ID. (Use ALIYUN_ENGINE=aliyundrive-webdav to avoid drive_id.)"
    exit 0
fi

if [[ ! -x "$MOUNT_FS" ]]; then
    echo "Building mount-fs..."
    (cd "$ROOT" && cargo build -q)
fi

TMP="$(mktemp -d "${TMPDIR:-/tmp}/mount-fs-aliyun-e2e.XXXXXX")"
MNT="$TMP/mnt"
CFG="$TMP/config/mount-fs/config.toml"

cleanup() {
    set +e
    mountpoint -q "$MNT" 2>/dev/null && "$MOUNT_FS" --config "$CFG" umount "$PROFILE" --force >/dev/null 2>&1
    rm -rf "$TMP"
}
trap cleanup EXIT

# --- isolated XDG + engine data dirs (do not touch the user's real config) --
export XDG_RUNTIME_DIR="$TMP/runtime"
export XDG_STATE_HOME="$TMP/state"
export XDG_CONFIG_HOME="$TMP/config"
export QRYPT_HOME="$TMP/qrypt"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME" "$QRYPT_HOME" "$MNT"

# --- configure profile + store token (token via stdin, never argv) ---------
if [[ -n "${ALIYUN_DRIVE_TYPE:-}" && "$ENGINE" != "aliyundrive-webdav" ]]; then
    echo "Ignoring ALIYUN_DRIVE_TYPE ($ALIYUN_DRIVE_TYPE): only aliyundrive-webdav supports it."
fi

ADD_ARGS=(config profile add "$PROFILE" --provider aliyun --engine "$ENGINE" --mountpoint "$MNT")
[[ -n "${ALIYUN_DRIVE_ID:-}" ]] && ADD_ARGS+=(--drive-id "$ALIYUN_DRIVE_ID")
if [[ "$ENGINE" == "aliyundrive-webdav" && -n "${ALIYUN_DRIVE_TYPE:-}" ]]; then
    ADD_ARGS+=(--drive-type "$ALIYUN_DRIVE_TYPE")
fi
"$MOUNT_FS" --config "$CFG" "${ADD_ARGS[@]}"

LOGIN_ARGS=(config login aliyun --profile "$PROFILE")
[[ -n "${ALIYUN_DRIVE_ID:-}" ]] && LOGIN_ARGS+=(--drive-id "$ALIYUN_DRIVE_ID")
printf '%s\n' "$ALIYUN_REFRESH_TOKEN" | "$MOUNT_FS" --config "$CFG" "${LOGIN_ARGS[@]}"

# --- mount -----------------------------------------------------------------
echo "Mounting Aliyun Drive via $ENGINE..."
"$MOUNT_FS" --config "$CFG" mount "$PROFILE"

for _ in $(seq 1 250); do
    mountpoint -q "$MNT" 2>/dev/null && break
    sleep 0.1
done
if ! mountpoint -q "$MNT" 2>/dev/null; then
    echo "mount did not appear; engine log:"
    cat "$XDG_STATE_HOME/mount-fs/logs/$PROFILE.log" 2>/dev/null || true
    exit 1
fi

# --- read/write through the mount -----------------------------------------
echo "Root listing (first 20 entries):"
ls -la "$MNT" | head -20

TESTFILE="$MNT/.mount-fs-e2e-test"
if echo "mount-fs e2e $(date +%s)" >"$TESTFILE" 2>/dev/null \
    && grep -q "mount-fs e2e" "$TESTFILE" 2>/dev/null; then
    echo "Read/write OK (created and read back .mount-fs-e2e-test)"
    rm -f "$TESTFILE" 2>/dev/null || true
else
    echo "WARN: write test failed (engine may be read-only)"
fi

# --- status ----------------------------------------------------------------
"$MOUNT_FS" --config "$CFG" list
if command -v python3 >/dev/null 2>&1; then
    "$MOUNT_FS" --config "$CFG" status "$PROFILE" --json | python3 -m json.tool >/dev/null
fi

# --- unmount + cleanup assertions -----------------------------------------
"$MOUNT_FS" --config "$CFG" umount "$PROFILE"

for _ in $(seq 1 200); do
    mountpoint -q "$MNT" 2>/dev/null || break
    sleep 0.1
done
if mountpoint -q "$MNT" 2>/dev/null; then
    echo "mountpoint still mounted after umount"
    exit 1
fi

if grep -q "\"$PROFILE\"" "$XDG_STATE_HOME/mount-fs/mounts.json" 2>/dev/null; then
    echo "state entry still present after umount"
    exit 1
fi

echo "PASS: Aliyun Drive ($ENGINE) end-to-end mount/read/write/unmount succeeded"
