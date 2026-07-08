#!/bin/bash
set -euo pipefail

IMAGE="ferros3-freebsd11-builder"

echo "Building Docker image for FreeBSD 11.2 cross-compilation..."
docker build -t "$IMAGE" -f Dockerfile.freebsd11 .

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
