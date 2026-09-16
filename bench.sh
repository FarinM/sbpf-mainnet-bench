#!/usr/bin/env bash
# Build the runner against two solana-sbpf refs and run every fixture with both,
# round by round, so the comparison is interleaved and noise-resistant.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
command -v cargo > /dev/null || export PATH="$HOME/.cargo/bin:$PATH"
SBPF_BASE="${SBPF_BASE:-main}"
SBPF_PATCH="${SBPF_PATCH:-shared-address-translation}"
ROUNDS="${ROUNDS:-3}"
ITERATIONS="${ITERATIONS:-10000}"
WARMUP="${WARMUP:-10}"

if [ ! -d "$here/sbpf/.git" ]; then
    echo "run ./setup.sh first" >&2
    exit 1
fi

if [ "$(uname -m)" != "x86_64" ]; then
    echo "WARNING: the JIT only exists on x86_64. This machine will run the"
    echo "         interpreter, so base and patched timings will be identical."
    echo
fi

build() { # $1 = solana-sbpf ref, $2 = target dir, $3 = output binary name
    git -C "$here/sbpf" fetch --quiet origin
    git -C "$here/sbpf" checkout -q "$1"
    (cd "$here/runner" && cargo build --release --target-dir "$here/$2")
    mkdir -p "$here/bin"
    cp "$here/$2/release/sbpf-mainnet-bench-runner" "$here/bin/$3"
    echo "built $3 from solana-sbpf $(git -C "$here/sbpf" rev-parse --short HEAD) ($1)"
}

build "$SBPF_BASE" target-base runner-base
build "$SBPF_PATCH" target-patch runner-patch

mkdir -p "$here/results"
for round in $(seq 1 "$ROUNDS"); do
    "$here/bin/runner-base" --fixtures "$here/fixtures/data" \
        --warmup "$WARMUP" --iterations "$ITERATIONS" \
        --json "$here/results/base-$round.jsonl" > /dev/null
    "$here/bin/runner-patch" --fixtures "$here/fixtures/data" \
        --warmup "$WARMUP" --iterations "$ITERATIONS" \
        --json "$here/results/patch-$round.jsonl" > /dev/null
    echo "round $round done"
done

echo
python3 "$here/compare.py" "$here/results"
