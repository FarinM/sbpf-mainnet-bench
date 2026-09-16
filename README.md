# sbpf-mainnet-bench

**AI-generated, unmaintained, demonstration-only.** This repository measures the
execution time of real mainnet program invocations through Agave's SVM with a
patched `solana-sbpf`, so a change to the SBPF JIT can be compared against the
baseline on actual mainnet traffic. It is not production code.

Only `InvokeContext::process_message` is timed: no bank, no fees, no signature
verification, no account loading, no block production. A replay either succeeds
or it does not, which is the correctness check. The runner's feature set is
`SVMFeatureSet::all_enabled()` with `virtual_address_space_adjustments` forced
off, matching mainnet.

## Running

```bash
./setup.sh                 # once: clone the solana-sbpf fork
./bench.sh                 # builds main and shared-address-translation, compares
```

Environment knobs: `SBPF_BASE` (default `main`), `SBPF_PATCH` (default
`shared-address-translation`), `ROUNDS` (default 3), `ITERATIONS` (default
10000).

**The JIT only exists on x86_64.** On other architectures both builds run the
interpreter and the timings will be identical. `runner/Cargo.toml` pulls Agave
from a pinned upstream revision (no submodule, nothing to clone) and patches
`solana-sbpf` to `../sbpf`, which `bench.sh` checks out at each ref.

## Fixtures

| fixture | what it exercises |
|---|---|
| `memo_v2` | Memo program, system transfers |
| `spl_token_transfer_checked` | SPL Token transferChecked + ATA program |
| `token2022_transfer_checked` | Token-2022 |
| `orca_whirlpool_swap` | Whirlpool SwapV2 with its CPI tree (Whirlpool, Token, Token-2022, event program); trailing post-swap instructions removed |
| `jupiter_swap` | Jupiter SharedAccountsRoute with CPIs; balances funded in the fixture |
| `jupiter_route_v2` | Jupiter RouteV2 into the goon AMM (USDC→SOL) with an idempotent ATA creation |
| `humidifi_oracle_update` | HumidiFi oracle update against a 1.7 KB state account |

Fixtures were captured from mainnet once and frozen; each JSON records its
signature, slot, message, accounts and program ELFs.

## Results

`./bench.sh` on a 32-core x86_64 host, 10,000 iterations per fixture per
interleaved round, medians of the per-round medians

| fixture | base | patched | patched/base |
|---|---|---|---|
| `memo_v2` | 21.28 µs | 10.80 µs | 0.508 |
| `spl_token_transfer_checked` | 13.75 µs | 11.70 µs | 0.851 |
| `token2022_transfer_checked` | 5.21 µs | 4.00 µs | 0.767 |
| `humidifi_oracle_update` | 4.18 µs | 4.08 µs | 0.976 |
| `orca_whirlpool_swap` | 104.02 µs | 69.06 µs | 0.664 |
| `jupiter_swap` | 181.55 µs | 135.86 µs | 0.748 |
| `jupiter_route_v2` | 461.19 µs | 296.72 µs | 0.643 |

`base` is `solana-sbpf` `main`, `patched` is `shared-address-translation`.
These times are pure `process_message`, so they exclude the bank/account
overhead that dilutes end-to-end replay deltas.
