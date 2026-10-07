#!/usr/bin/env bash
# Install beamhost.
#
#   curl -fsSL https://raw.githubusercontent.com/codingsushi79/bmps/main/install.sh | bash
#
# Run from a checkout, it builds that checkout; piped from curl, it builds
# straight from GitHub, so there is nothing to clone either way.
#
# Overrides: BEAMHOST_REPO, BEAMHOST_BRANCH, BEAMHOST_INSTALL_DIR (a cargo
# install root; the binary lands in its bin/).
set -euo pipefail

REPO="${BEAMHOST_REPO:-https://github.com/codingsushi79/bmps}"
BRANCH="${BEAMHOST_BRANCH:-main}"

say() { printf '\033[1;35m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

if ! command -v cargo >/dev/null 2>&1; then
  die "beamhost is built from source and needs Rust. Install it with:
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
then open a new shell and run this installer again."
fi

root_args=()
if [ -n "${BEAMHOST_INSTALL_DIR:-}" ]; then
  root_args=(--root "$BEAMHOST_INSTALL_DIR")
fi

# BASH_SOURCE is empty or "bash" when the script arrives on stdin.
script_dir=""
if [ -n "${BASH_SOURCE[0]:-}" ] && [ -f "${BASH_SOURCE[0]}" ]; then
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fi

if [ -n "$script_dir" ] && [ -f "$script_dir/Cargo.toml" ]; then
  say "building beamhost from $script_dir"
  cargo install --path "$script_dir" --locked --force ${root_args[@]+"${root_args[@]}"}
else
  say "building beamhost from $REPO ($BRANCH)"
  cargo install --git "$REPO" --branch "$BRANCH" --locked --force ${root_args[@]+"${root_args[@]}"}
fi

bin="${BEAMHOST_INSTALL_DIR:-${CARGO_HOME:-$HOME/.cargo}}/bin"
echo
say "installed: $bin/beamhost"
case ":$PATH:" in
  *":$bin:"*) ;;
  *) echo "    add this to your shell rc:  export PATH=\"$bin:\$PATH\"" ;;
esac
if [ "$(uname)" = "Darwin" ]; then
  echo "    macOS: BeamMP servers run in Docker here — install Docker Desktop, OrbStack or colima."
fi
echo "    next: get an auth key at https://keymaster.beammp.com, then run: beamhost"
