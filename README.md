# eth_wallet_backend

An Ethereum-only wallet coordinator: balances, Send with full fee control, and local transaction history.

Exactly one network is active at a time, chosen from mainnet, sepolia and hoodi. Fee estimation comes from `fee_module`; JSON-RPC from `eth_rpc_module`, optionally routed through a light-client verified proxy. Signing is never performed here — it is requested from `keystore_module` and authorised by a human in `signer_ui`.

Part of the [Logos](https://github.com/logos-co) modular application platform.
Built and tested through the `logos-workspace` `ws` CLI.

> Status: scaffolding. See the architecture plan for scope and phases.
