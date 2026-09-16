#!/usr/bin/env bash
# Clone the solana-sbpf fork the benchmark swaps between. Agave's runtime crates
# are fetched by cargo from a revision pinned in runner/Cargo.toml.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SBPF_REPO="${SBPF_REPO:-https://github.com/FarinM/sbpf}"

if [ ! -d "$here/sbpf/.git" ]; then
    echo "cloning solana-sbpf fork"
    git clone "$SBPF_REPO" "$here/sbpf"
fi
git -C "$here/sbpf" fetch --quiet origin
echo "solana-sbpf fork at $(git -C "$here/sbpf" remote get-url origin)"
