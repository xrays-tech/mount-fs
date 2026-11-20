#!/usr/bin/env bash
#
# End-to-end S3 smoke test for mount-fs.
#
# Starts a local S3 server with `rclone serve s3`, mounts it through s3fs-fuse
# using the built mount-fs binary, performs a small read/write, checks
# list/status, unmounts, and verifies cleanup. No Docker is required.
#
# Requirements: rclone, s3fs, fusermount3, /dev/fuse, and an environment where
# FUSE mounts are permitted (NoNewPrivs=0). Otherwise the script prints a SKIP
# line and exits 0. It is NOT run by `cargo test`.
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MOUNT_FS="${MOUNT_FS:-$ROOT/target/debug/mount-fs}"

skip() {
    echo "SKIP: cannot mount FUSE in this environment ($1)"
    exit 0
}

# --- environment preflight -------------------------------------------------
[[ -e /dev/fuse ]] || skip "/dev/fuse is missing"
grep -q '^NoNewPrivs:[[:space:]]*0' /proc/self/status \
    || skip "NoNewPrivs is set"
command -v fusermount3 >/dev/null 2>&1 || skip "fusermount3 not found"
command -v s3fs >/dev/null 2>&1 || skip "s3fs not found"
command -v rclone >/dev/null 2>&1 || skip "rclone not found"

if [[ ! -x "$MOUNT_FS" ]]; then
    echo "Building mount-fs..."
    (cd "$ROOT" && cargo build -q)
fi

TMP="$(mktemp -d "${TMPDIR:-/tmp}/mount-fs-e2e.XXXXXX")"
MNT="$TMP/mnt"
DATA="$TMP/data"
CFG="$TMP/config/mount-fs/config.toml"
RCLONE_PID=""

is_mounted() {
    grep -qF " $MNT " /proc/self/mountinfo 2>/dev/null
}

cleanup() {
    set +e
    if is_mounted; then
        "$MOUNT_FS" --config "$CFG" umount e2e >/dev/null 2>&1 \
            || fusermount3 -uz "$MNT" >/dev/null 2>&1
    fi
    if [[ -n "$RCLONE_PID" ]]; then
        kill "$RCLONE_PID" >/dev/null 2>&1
        wait "$RCLONE_PID" 2>/dev/null
    fi
    rm -rf "$TMP"
}
trap cleanup EXIT

# --- isolated XDG dirs -----------------------------------------------------
export XDG_RUNTIME_DIR="$TMP/runtime"
export XDG_STATE_HOME="$TMP/state"
export XDG_CONFIG_HOME="$TMP/config"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_STATE_HOME" "$XDG_CONFIG_HOME" \
    "$MNT" "$DATA/bucket"

# --- local S3 server -------------------------------------------------------
if command -v python3 >/dev/null 2>&1; then
    PORT="$(python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
)"
else
    PORT=$((20000 + RANDOM % 20000))
fi
AK="AKIAE2EACCESS"
SK="e2e-secret-key"

# `:local:` serves the data directory; the top-level `bucket/` dir is the S3
# bucket mounted below.
rclone serve s3 ":local:$DATA" \
    --addr "127.0.0.1:$PORT" \
    --auth-key "$AK,$SK" \
    >"$TMP/rclone.log" 2>&1 &
RCLONE_PID=$!

for _ in $(seq 1 100); do
    if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then
        break
    fi
    if ! kill -0 "$RCLONE_PID" >/dev/null 2>&1; then
        echo "rclone failed to start:"
        cat "$TMP/rclone.log"
        exit 1
    fi
    sleep 0.1
done

# --- configure + mount -----------------------------------------------------
"$MOUNT_FS" --config "$CFG" config profile add e2e \
    --provider s3 \
    --engine s3fs-fuse \
    --mountpoint "$MNT" \
    --bucket bucket \
    --endpoint "http://127.0.0.1:$PORT" \
    --region us-east-1 \
    --path-style

# Credentials come from stdin (never argv).
printf '%s\n%s\n' "$AK" "$SK" \
    | "$MOUNT_FS" --config "$CFG" config login s3 --profile e2e

"$MOUNT_FS" --config "$CFG" mount e2e

# --- read/write through the mount -----------------------------------------
for _ in $(seq 1 100); do
    is_mounted && break
    sleep 0.1
done
if ! is_mounted; then
    echo "mount did not appear; engine logs:"
    cat "$XDG_STATE_HOME"/mount-fs/logs/e2e.log 2>/dev/null || true
    exit 1
fi

echo "hello from mount-fs e2e" >"$MNT/hello.txt"
grep -qx "hello from mount-fs e2e" "$MNT/hello.txt"

"$MOUNT_FS" --config "$CFG" list
if command -v python3 >/dev/null 2>&1; then
    "$MOUNT_FS" --config "$CFG" status e2e --json | python3 -m json.tool >/dev/null
else
    "$MOUNT_FS" --config "$CFG" status e2e --json >/dev/null
fi

# --- unmount + cleanup assertions -----------------------------------------
"$MOUNT_FS" --config "$CFG" umount e2e

for _ in $(seq 1 100); do
    is_mounted || break
    sleep 0.1
done
if is_mounted; then
    echo "mountpoint still mounted after umount"
    exit 1
fi

if grep -q '"e2e"' "$XDG_STATE_HOME/mount-fs/mounts.json" 2>/dev/null; then
    echo "state entry still present after umount"
    exit 1
fi
if compgen -G "$XDG_RUNTIME_DIR/mount-fs/creds-*" >/dev/null; then
    echo "temporary credential leftovers found"
    exit 1
fi

echo "PASS: S3 end-to-end mount/read/write/unmount succeeded"
