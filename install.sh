#!/usr/bin/env bash
# Build and install beamhost from this checkout.
set -euo pipefail
cd "$(dirname "$0")"
if ! command -v cargo >/dev/null 2>&1; then
  echo "beamhost needs Rust: https://rustup.rs" >&2
  exit 1
fi
cargo install --path . --locked --force
bin="${CARGO_HOME:-$HOME/.cargo}/bin"
echo
echo "installed: $bin/beamhost"
case ":$PATH:" in
  *":$bin:"*) ;;
  *) echo "add this to your shell rc:  export PATH=\"$bin:\$PATH\"" ;;
esac
if [ "$(uname)" = "Darwin" ]; then
  echo "macOS: BeamMP servers run in Docker here — install Docker Desktop, OrbStack or colima."
fi
echo "next: get an auth key at https://keymaster.beammp.com, then run: beamhost"
