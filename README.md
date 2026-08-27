# eth_wallet_backend

An Ethereum-only wallet coordinator: balances, Send with full fee control, and local
transaction history.

Exactly one network is active at a time, chosen from **mainnet (1)**, **sepolia (11155111)**
and **hoodi (560048)** — the same set the light-client verified proxy supports, so the
verified-proxy toggle is meaningful on every one of them. There is no chain list to fan out
over and no L2s.

## What this module is not allowed to do

It never sees key material. Signatures are *requested* from `keystore_module` and authorised
by a human in `signer_ui`; account creation, import and export belong to `keystore_ui` and
are refused to everyone else. This module can read which accounts exist, and nothing more.

## Dependencies

| Module | For |
|---|---|
| `eth_rpc_module` | JSON-RPC, and the per-chain endpoint the user configures |
| `fee_module` | EIP-1559 tiers — this module does not do fee maths |
| `keystore_module` | reading the account list; later, requesting signatures |

## Tokens

A fixed table: native ETH plus WETH where its address has been **verified** — currently
mainnet only. Sepolia and Hoodi offer ETH alone until their WETH addresses are confirmed
against an authoritative source and on-chain. A guessed token address spends into the wrong
contract, so the table carries a hole rather than a guess.

## Building and testing

```bash
cargo test --no-default-features --manifest-path rust-lib/Cargo.toml   # pure cores, no runtime
nix build .#default                                                    # the module
nix build .#lidl                                                       # the derived contract
```
