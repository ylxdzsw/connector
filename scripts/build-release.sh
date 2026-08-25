#!/bin/sh
set -eu

root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
cd "$root"

client_target="${CONNECTOR_CLIENT_TARGET:-x86_64-unknown-linux-musl}"
cargo build --release --locked --bin connector-gateway
cargo build --release --locked --target "$client_target" --bin connector-client

client="target/$client_target/release/connector-client"
if readelf -l "$client" | grep -q 'Requesting program interpreter' ||
   readelf -d "$client" 2>/dev/null | grep -q '(NEEDED)'; then
    echo "error: $client is dynamically linked" >&2
    exit 1
fi

printf 'gateway: %s\nclient: %s (static)\n' \
    target/release/connector-gateway "$client"
