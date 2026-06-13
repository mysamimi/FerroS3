BINARY_NAME=ferros3

.PHONY: all build build-freebsd run test clean docker-build

all: build

build:
	cargo build --release

# Requires 'cross' tool: cargo install cross --git https://github.com/cross-rs/cross.git
build-freebsd11:
	./build-freebsd11.sh

build-freebsd:
	cross build --release --target x86_64-unknown-freebsd

build-linux:
	cross build --release --target x86_64-unknown-linux-musl

run:
	cargo run

test:
	cargo test

clean:
	cargo clean

docker-build:
	docker build -t $(BINARY_NAME) .
