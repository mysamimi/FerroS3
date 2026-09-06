#!/bin/bash
set -euo pipefail

IMAGE="ferros3-freebsd11-builder"

# Pick Debian mirrors to build against.
#
# deb.debian.org resolves to a CDN that is unusably slow from some networks
# (a few kB/s measured here), which stalls the apt layer for 10+ minutes and
# leaves it prone to being OOM-killed mid-download. So we probe a list of
# candidates and use the fastest one that is both reachable and up to date.
#
# The main archive and the security archive are chosen independently: local
# mirrors that carry the ~90 MB main archive frequently do not mirror
# debian-security at all, and the security indexes are small enough (~300 kB)
# that fetching them from upstream costs little.
#
# Set DEBIAN_MIRROR / DEBIAN_SECURITY_MIRROR to skip the corresponding probe.
MAIN_MIRRORS=(
    "https://mirror.arvancloud.ir/debian"
    "https://repo.iut.ac.ir/repo/debian"
    "https://mirror.aminidc.com/debian"
    "https://deb.debian.org/debian"
)
SECURITY_MIRRORS=(
    "https://security.debian.org/debian-security"
    "https://deb.debian.org/debian-security"
)

# Debian architecture the image will be built for. The mirror probe has to know
# it: several regional mirrors carry amd64 only, so on an arm64 host (Apple
# silicon) one of them can pass every reachability and freshness check and then
# fail the build with a 404 on binary-arm64/Packages.
build_arch() {
    local plat="${DOCKER_DEFAULT_PLATFORM:-}"
    case "${plat##*/}" in
        amd64|x86_64)  echo amd64; return ;;
        arm64|aarch64) echo arm64; return ;;
    esac
    case "$(uname -m)" in
        arm64|aarch64) echo arm64 ;;
        *)             echo amd64 ;;
    esac
}
DEB_ARCH=$(build_arch)

# Whether a mirror actually carries package indices for DEB_ARCH. A partial
# mirror still serves a verbatim InRelease that advertises every architecture,
# so the arch's own Packages index is the only thing worth trusting here.
has_arch() {
    local url="$1" suite="$2" ext code path
    for ext in xz gz; do
        path="$url/dists/$suite/main/binary-$DEB_ARCH/Packages.$ext"
        code=$(curl -sIL -m 15 -o /dev/null -w '%{http_code}' "$path" 2>/dev/null) || continue
        case "$code" in
            200) return 0 ;;
            405|501)
                # Server refuses HEAD; ask for a single byte instead.
                code=$(curl -sL -m 15 -o /dev/null -w '%{http_code}' -r 0-0 "$path" 2>/dev/null) || continue
                case "$code" in 200|206) return 0 ;; esac
                ;;
        esac
    done
    return 1
}

# Seconds since the epoch for an RFC 1123 date, or empty. Tries GNU date first,
# then BSD/macOS date, so the probe works on both a dev laptop and CI.
to_epoch() {
    date -u -d "$1" +%s 2>/dev/null \
        || date -j -u -f "%a, %d %b %Y %H:%M:%S %Z" "$1" +%s 2>/dev/null \
        || true
}

# Fetch a suite's InRelease. Echoes "<bytes/sec> <Valid-Until or empty>" if the
# suite is served and not expired; echoes nothing if it is missing or stale.
# A suite with no Valid-Until (Debian stable's own suite has none) is accepted.
probe_suite() {
    local url="$1" body rc code speed valid until now
    body=$(mktemp) || return 0
    rc=$(curl -sL -m 20 -o "$body" -w '%{http_code}:%{speed_download}' "$url" 2>/dev/null) || {
        rm -f "$body"; return 0; }
    code=${rc%%:*}; speed=${rc##*:}; speed=${speed%.*}
    if [ "$code" != "200" ]; then rm -f "$body"; return 0; fi
    valid=$(grep -m1 '^Valid-Until:' "$body" | sed 's/^Valid-Until: *//')
    rm -f "$body"
    if [ -n "$valid" ]; then
        until=$(to_epoch "$valid"); now=$(date -u +%s)
        # Unparseable dates are not treated as expiry — do not reject on them.
        if [ -n "$until" ] && [ "$until" -lt "$now" ]; then return 0; fi
    fi
    printf '%s %s\n' "${speed:-0}" "$valid"
}

# Rank candidates by download speed, keeping only fresh, reachable ones.
# $1 = suite used for the speed measurement, $2 = suite used for the freshness
# check (Debian's stable suite carries no Valid-Until of its own), rest = URLs.
pick_mirror() {
    local speed_suite="$1" fresh_suite="$2"; shift 2
    local url r s
    for url in "$@"; do
        r=$(probe_suite "$url/dists/$speed_suite/InRelease") || continue
        [ -n "$r" ] || continue
        has_arch "$url" "$speed_suite" || continue
        if [ "$fresh_suite" != "$speed_suite" ]; then
            s=$(probe_suite "$url/dists/$fresh_suite/InRelease") || continue
            [ -n "$s" ] || continue
            has_arch "$url" "$fresh_suite" || continue
        fi
        printf '%s %s\n' "${r%% *}" "$url"
    done | sort -rn | head -1
}

if [ -z "${DEBIAN_MIRROR:-}" ]; then
    echo "Probing Debian main mirrors (arch: $DEB_ARCH)..."
    best=$(pick_mirror trixie trixie-updates "${MAIN_MIRRORS[@]}")
    if [ -n "$best" ]; then
        read -r speed DEBIAN_MIRROR <<<"$best"
        echo "  main:     $DEBIAN_MIRROR (~$((speed / 1024)) kB/s)"
    else
        echo "WARNING: no usable main mirror; falling back to deb.debian.org" >&2
        DEBIAN_MIRROR="http://deb.debian.org/debian"
    fi
fi

if [ -z "${DEBIAN_SECURITY_MIRROR:-}" ]; then
    echo "Probing Debian security mirrors..."
    best=$(pick_mirror trixie-security trixie-security "${SECURITY_MIRRORS[@]}")
    if [ -n "$best" ]; then
        read -r speed DEBIAN_SECURITY_MIRROR <<<"$best"
        echo "  security: $DEBIAN_SECURITY_MIRROR (~$((speed / 1024)) kB/s)"
    else
        echo "WARNING: no usable security mirror; falling back to deb.debian.org" >&2
        DEBIAN_SECURITY_MIRROR="http://deb.debian.org/debian-security"
    fi
fi

echo "Building Docker image for FreeBSD 11.2 cross-compilation..."
docker build -t "$IMAGE" -f Dockerfile.freebsd11 \
    --build-arg "DEBIAN_MIRROR=$DEBIAN_MIRROR" \
    --build-arg "DEBIAN_SECURITY_MIRROR=$DEBIAN_SECURITY_MIRROR" \
    .

echo "Compiling the project inside Docker..."
docker run --rm -v "$(pwd):/app" "$IMAGE" \
    bash -c "rm -f /app/Cargo.lock && cargo build --release --target x86_64-unknown-freebsd -Z build-std"

BINARY="target/x86_64-unknown-freebsd/release/ferros3"
if [ ! -f "$BINARY" ]; then
    echo "ERROR: expected binary not found at $BINARY" >&2
    exit 1
fi

echo "Build successful! The binary is located at:"
ls -lh "$BINARY"
