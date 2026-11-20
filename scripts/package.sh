#!/usr/bin/env bash
#
# Build a release tarball for mount-fs.
#
# Usage:
#   scripts/package.sh [--locked] [--target <triple>] [--cross] [--skip-build]
#
# Produces:
#   dist/mount-fs-<version>-<target>.tar.gz
#   dist/mount-fs-<version>-<target>.tar.gz.sha256
#
# The tarball contains:
#   mount-fs-<version>-<target>/mount-fs
#   mount-fs-<version>-<target>/README.md
#   mount-fs-<version>-<target>/LICENSE
#   mount-fs-<version>-<target>/dev-docs/...
#   mount-fs-<version>-<target>/docs/...
#
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

die() {
    echo "package.sh: $*" >&2
    exit 1
}

usage() {
    sed -n '3,17p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 0
}

LOCKED=0
CROSS=0
SKIP_BUILD=0
EXPLICIT_TARGET=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --locked)
            LOCKED=1
            shift
            ;;
        --target)
            [[ $# -ge 2 ]] || die "--target requires a value"
            EXPLICIT_TARGET="$2"
            shift 2
            ;;
        --target=*)
            EXPLICIT_TARGET="${1#--target=}"
            shift
            ;;
        --cross)
            CROSS=1
            shift
            ;;
        --skip-build)
            SKIP_BUILD=1
            shift
            ;;
        -h | --help)
            usage
            ;;
        *)
            die "unknown argument: $1 (try --help)"
            ;;
    esac
done

[[ -f Cargo.toml ]] || die "Cargo.toml not found; run from the repository root"
[[ -f Cargo.lock ]] || die "Cargo.lock not found; releases must build reproducibly"
[[ -d dev-docs ]] || die "dev-docs/ not found"

# Version from the [package] table in Cargo.toml.
VERSION="$(
    awk '
        /^\[package\]/ { in_package = 1; next }
        /^\[/ { in_package = 0 }
        in_package && /^[[:space:]]*version[[:space:]]*=/ {
            line = $0
            sub(/^[[:space:]]*version[[:space:]]*=[[:space:]]*"/, "", line)
            sub(/".*$/, "", line)
            print line
            exit
        }
    ' Cargo.toml
)"
[[ -n "$VERSION" ]] || die "could not determine the version from Cargo.toml"

# Host target triple, with a POSIX-ish fallback when rustc is unavailable.
HOST_TARGET=""
if command -v rustc >/dev/null 2>&1; then
    # Read rustc's output to EOF (no early `exit`) so `pipefail` cannot see a
    # spurious SIGPIPE from rustc when awk closes the pipe first.
    HOST_TARGET="$(rustc -vV | awk '/^host:/ { print $2 }')"
fi
if [[ -z "$HOST_TARGET" ]]; then
    case "$(uname -m)" in
        x86_64) HOST_TARGET="x86_64-unknown-linux-gnu" ;;
        aarch64 | arm64) HOST_TARGET="aarch64-unknown-linux-gnu" ;;
        armv7l) HOST_TARGET="armv7-unknown-linux-gnueabihf" ;;
        *) die "unsupported architecture: $(uname -m)" ;;
    esac
fi

# The packaging target names the archive. An explicit --target wins; otherwise
# honour Cargo's own CARGO_BUILD_TARGET, then the rustc host triple.
PACKAGE_TARGET="${EXPLICIT_TARGET:-${CARGO_BUILD_TARGET:-$HOST_TARGET}}"
[[ -n "$PACKAGE_TARGET" ]] || die "could not determine the target triple"

TARGET_ROOT="${CARGO_TARGET_DIR:-$ROOT/target}"
if [[ "$TARGET_ROOT" != /* ]]; then
    TARGET_ROOT="$ROOT/$TARGET_ROOT"
fi

if [[ "$SKIP_BUILD" -eq 0 ]]; then
    BUILD_TOOL="cargo"
    if [[ "$CROSS" -eq 1 ]]; then
        command -v cross >/dev/null 2>&1 || die '--cross requested but "cross" is not installed'
        BUILD_TOOL="cross"
    fi

    BUILD_CMD=("$BUILD_TOOL" build --release)
    if [[ "$LOCKED" -eq 1 ]]; then
        BUILD_CMD+=(--locked)
    fi
    if [[ -n "$EXPLICIT_TARGET" ]]; then
        BUILD_CMD+=(--target "$EXPLICIT_TARGET")
    fi

    echo "==> ${BUILD_CMD[*]}"
    "${BUILD_CMD[@]}"
fi

if [[ -n "$EXPLICIT_TARGET" || -n "${CARGO_BUILD_TARGET:-}" ]]; then
    BIN="$TARGET_ROOT/$PACKAGE_TARGET/release/mount-fs"
else
    BIN="$TARGET_ROOT/release/mount-fs"
fi
[[ -x "$BIN" ]] || die "release binary not found at $BIN"

DIST="$ROOT/dist"
mkdir -p "$DIST"

NAME="mount-fs-${VERSION}-${PACKAGE_TARGET}"
TARBALL="$DIST/${NAME}.tar.gz"
SHA_FILE="$TARBALL.sha256"

# Idempotent: rebuild the final artifacts from scratch.
rm -f "$TARBALL" "$SHA_FILE"
STAGE="$(mktemp -d "$DIST/.package-stage.XXXXXX")"
cleanup() {
    rm -rf "$STAGE"
}
trap cleanup EXIT

mkdir -p "$STAGE/$NAME"
install -m 0755 "$BIN" "$STAGE/$NAME/mount-fs"
cp README.md LICENSE "$STAGE/$NAME/"
mkdir -p "$STAGE/$NAME/dev-docs"
cp -a dev-docs/. "$STAGE/$NAME/dev-docs/"
mkdir -p "$STAGE/$NAME/docs"
cp -a docs/. "$STAGE/$NAME/docs/"

echo "==> creating $TARBALL"
tar -czf "$TARBALL" -C "$STAGE" "$NAME"

if command -v sha256sum >/dev/null 2>&1; then
    HASH="$(sha256sum "$TARBALL" | awk '{ print $1 }')"
elif command -v shasum >/dev/null 2>&1; then
    HASH="$(shasum -a 256 "$TARBALL" | awk '{ print $1 }')"
else
    die "neither sha256sum nor shasum is available"
fi
printf '%s  %s\n' "$HASH" "$(basename "$TARBALL")" >"$SHA_FILE"

echo "Archive:  $TARBALL"
echo "Checksum: $SHA_FILE"
echo "SHA256:   $HASH"
