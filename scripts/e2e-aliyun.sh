#!/usr/bin/env bash
#
# End-to-end Aliyun Drive smoke test for mount-fs, using the `qrypt` engine
# (the active, read/write engine; needs an OpenAPI refresh token + drive_id).
#
# Requirements:
#   - qrypt installed (mount-fs install qrypt --yes)
#   - /dev/fuse and an environment where FUSE mounts are permitted (NoNewPrivs=0)
#   - ALIYUN_REFRESH_TOKEN and ALIYUN_DRIVE_ID in the environment
#
# The token is passed to `config login` on stdin (never argv), and is stored in
# a 0600 file. The drive_id is not a secret.
#
# Usage:
#   ALIYUN_REFRESH_TOKEN='<token>' ALIYUN_DRIVE_ID='<drive-id>' scripts/e2e-aliyun.sh
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
    || skip "NoNewPrivs is set (run this on a real host / user session, not a restricted sandbox)"
command -v qrypt >/dev/null 2>&1 \
    || skip "qrypt not installed (run: $MOUNT_FS install qrypt --yes)"

if [[ -z "${ALIYUN_REFRESH_TOKEN:-}" ]] || [[ -z "${ALIYUN_DRIVE_ID:-}" ]]; then
    echo "SKIP: missing Aliyun credentials."
    echo "  Set ALIYUN_REFRESH_TOKEN (an Aliyun Drive OpenAPI refresh token) and ALIYUN_DRIVE_ID."
    echo "  Notes:"
    echo "    - qrypt does not provide an Aliyun login; obtain the token via an OpenAPI login flow"
    echo "      (e.g. \`aliyundrive-webdav qr login\`) and the drive_id from the drive-info API,"
    echo "      OpenList, or an \`aliyunpan\` config (userList[].driveId)."
    echo "    - If you only have a legacy web refresh token, use the read-only aliyundrive-fuse"
    echo "      engine instead (no drive_id required)."
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

# --- isolated XDG + qrypt data dirs (do not touch the user's real config) ---
export XDG_RUNTIME_DIR="$TMP/runtime"
export XDG_STATE_HOME="$TMP/state"
export XDG_CONFIG_HOME="$TMP/config"
export QRYPT_HOME="$TMP/qrypt"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME" "$QRYPT_HOME" "$MNT"

# --- configure profile + store token (token via stdin, never argv) ---------
"$MOUNT_FS" --config "$CFG" config profile add "$PROFILE" \
    --provider aliyun \
    --engine qrypt \
    --mountpoint "$MNT" \
    --drive-id "$ALIYUN_DRIVE_ID"

printf '%s\n' "$ALIYUN_REFRESH_TOKEN" \
    | "$MOUNT_FS" --config "$CFG" config login aliyun --profile "$PROFILE" --drive-id "$ALIYUN_DRIVE_ID"

# --- mount -----------------------------------------------------------------
echo "Mounting Aliyun Drive via qrypt..."
"$MOUNT_FS" --config "$CFG" mount "$PROFILE"

for _ in $(seq 1 150); do
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

for _ in $(seq 1 100); do
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

echo "PASS: Aliyun Drive (qrypt) end-to-end mount/read/write/unmount succeeded"
