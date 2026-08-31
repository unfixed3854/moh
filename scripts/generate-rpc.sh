#!/usr/bin/env bash
set -euo pipefail

command -v capnp >/dev/null || {
  echo "capnp compiler is required to regenerate RPC bindings" >&2
  exit 1
}
command -v capnpc-rust >/dev/null || {
  echo "install capnpc-rust with: cargo install capnpc --version 0.27.0 --locked" >&2
  exit 1
}

capnp compile --src-prefix=schema -orust:src/rpc schema/moh.capnp
rustfmt --edition 2024 src/rpc/moh_capnp.rs
