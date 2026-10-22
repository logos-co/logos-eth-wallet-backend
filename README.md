# Ethereum wallet backend

`eth_wallet_backend` is a wallet composer. It presents one cohesive API to wallet UIs while
leaving reusable EVM facts and actions in the modules that own them:

| Concern | Owning module |
| --- | --- |
| Chain registry, endpoint, scope and verified-proxy policy | `eth_rpc_module` |
| Token catalogue and persisted enabled-token snapshots | `token_list_module` |
| Native/token identity, balances, amount conversion and unsigned transfers | `evm_assets_module` |
| Software and offered device accounts | `signer_manager_module` |
| Software account-to-wallet provenance | `keystore_module` |
| Fee suggestions | `fee_module` |
| Approval, signing hand-off, broadcast and local history | `tx_sender_module` |
| Contacts and token sort preference | `eth_wallet_backend` |

The composer has no active-chain setting. Chain selection belongs to each UI operation;
device scope is the `mainnets`, `testnets`, or `both` setting owned by `eth_rpc_module`.

On start, and in front of every registry read until one call lands, the composer asks
`eth_rpc_module.init_defaults` for the default chains. It does not ask `config_status` first:
eth_rpc fills only what is absent and seeds a default chain at most once per device.
`token_list_module.init_defaults` is asked the same way, on start and in front of the token
reads: token_list writes its defaults only when nothing is configured.

## Read contract

- `list_networks()` returns the enabled in-scope `networks`, plus all
  `configuredNetworks`, and the current `scope`. Chain records include authoritative native
  symbol/decimals and their verified-proxy verdict.
- `verified_proxy_state()` reports a verdict for every enabled in-scope chain.
- `list_tokens(chain_id)` hands the rows `token_list_module` offers to `evm_assets_module`,
  which answers them as asset rows behind the chain's native one.
  `list_available_tokens(chain_id, query, offset, limit)` pages `token_list_module`'s
  catalogue, with that native row first on the first page.
- `list_accounts()` reads the signer manager's inventory and presents its EVM addresses.
  The manager owns aggregation and signer precedence for software and devices (including
  Keycard); this wallet does not maintain a second inventory. Bitcoin accounts are excluded. Disconnected device accounts remain visible for balance/history reads.
- `get_account_labels()` includes device labels; existing software labels take precedence.
  `get_account_wallets()` retains the keystore's derivation provenance. These reads cannot
  create, import, export or sign. A failed inventory read returns an error, allowing the UI
  to keep its previous list instead of silently dropping accounts.
- `account_roster()` relays the signer manager's `roster()`, EVM entries only and as they
  are: each account once, with the wallet it comes from (master fingerprint and name) and
  every signer holding it, ranked. It is what the account picker shows; `list_accounts()`
  stays the list of addresses this wallet reads.
- Changes from both the keystore and signer manager emit `accounts_changed`, so offering or
  removing a device account refreshes an already-open wallet. No PIN is needed to list accounts.
- `get_balances(address)` reads every enabled in-scope chain concurrently: its offered tokens,
  then one `evm_assets_module` balance read. Its top-level answer remains usable when one
  chain fails: each item in `chains` carries its own `ok`, `chainId`, balances/route or error.
  Each chain gets a bounded fifteen-second allowance so proof-backed multi-token reads are not
  mistaken for a dead proxy.
- `get_history(address)` asks the sender for all locally recorded chains, filters to the
  current device scope, then asks `evm_assets_module` to decorate every row by its own
  `(chainId, contract)` against that chain's offered tokens, plus token_list's catalogue
  rows for any other token a transfer moved, such as a swap's other side. If decorating
  fails, the rows come back undecorated with `decorationErrors` saying why, not as an
  error. This is local sender history, not a chain indexer.
- `suggest_fees(chain_id)`, `refresh_pending(address)`, `refresh_tx_status(address, hash)` and
  `get_tx_details(address, hash)` preserve the owning module's structured reply.

Every raw amount is a decimal string. The modules provide exact and display-safe strings;
consumers must not scale through JavaScript numbers.

## Write contract

`set_chain_enabled(chain_id, enabled)` and `set_network_scope(scope)` are explicit relays to
the chain registry. `set_token_enabled(chain_id, address, enabled)` is an explicit relay to
the token list. `set_token_sort(order)` and the contact methods write only composer-owned
preferences.

### Contacts and settings are sealed

`contacts.json` and `settings.json` are each one box (`logos-signer-interface`'s
`wallets::seal`, 0600, the file's name as associated data) under this module's profile keys:
`signer_manager_module.data_key({ seed: "profile", scope: "contacts" | "settings" })`, which
answers while any wallet is unlocked in the Signer app. A key worker asks for them on its own
thread, at start and on every `wallets_changed`; `wallets_locked` drops them and asks again.
A box sealed under an older generation is opened with that generation's key and sealed again
under the current one. Plain files from before sealing are ignored.

While no profile key is at hand, nothing is read or written: `list_contacts`, `save_contact`,
`forget_contact` and `set_token_sort` answer `{ ok: false, code: "locked", error }`, and the
reads that order rows use the default `alpha` sort. `token_sort_changed` is emitted when the
keys come or go, so a view re-reads both.

Every send request must name `chainId`:

```json
{
  "chainId": 1,
  "from": "0x...",
  "to": "0x...",
  "amountUnits": "0.1",
  "token": "ETH",
  "tier": "normal"
}
```

`prepare_send` and `send` reject a chain outside the current scope. The composer delegates
asset resolution and construction to `evm_assets_module`, with the chain's offered tokens as
the candidates, then passes the resulting unsigned call to `tx_sender_module`. It never signs
or broadcasts itself. `send` returns a pending request; `send_status` advances the
approval/broadcast state machine.

Poll `send_status` until `final` is true, and on nothing else. `final` is the sender's own:
false while the send awaits approval or is broadcasting, and false on a refusal that may yet
pass — a refusal is final only when the sender no longer holds the request. A sender that
predates `final` is read off `status` (only `awaitingApproval` and `broadcasting` still move,
and none of its refusals is final), and a sender that did not answer is never final.

For ERC-20s, `tokenAddress` is the identity and wins over `token`. An ambiguous symbol is
refused. For native sends the sender remains responsible for native affordability and fees.

## Verification and partial failure

All RPC traffic flows through `eth_rpc_module`. A `verified_blocked` reply is a structured
safety outcome and is preserved through the asset, fee, Uniswap, sender and composer layers.
Balances surface failures per chain, and history decoration reports `decorationErrors` per
chain without discarding activity from healthy chains.

Outbound work is explicitly bounded. The balance fan-out runs concurrently and joins in
registry order, so adding a configured chain does not add another serial network allowance.
No dependency call is made while composer state is locked.

## Events

The composer relays or emits:

- `networks_changed(chainId)`
- `tokens_changed(chainId)`, from `token_list_module` and from the chain record that names the
  native asset
- `accounts_changed(count)`
- `balances_updated(address)`
- `history_changed(address)` and `tx_status_changed(hash)`
- `send_status_changed(requestId)`
- `token_sort_changed(order)`, also when the profile keys come or go

Views re-read the relevant owner after an event; event payloads are hints, not snapshots.

## Build and test

```bash
cargo test --manifest-path rust-lib/Cargo.toml --no-default-features --locked
nix build .#default .#lgx
```

The Rust suite covers exact amounts, contacts, settings, dependency startup, verified-proxy
refusals, the `send_status` relay and source-shape guards. `doctests/headless-send.test.yaml`
stages the full module graph, including `evm_assets_module`, and exercises the public
composer contract.

Account inventory comes from `signer_manager_module`, including disconnected hardware
accounts. The per-address `wallets` metadata also carries `signer`, `available`, and
the provider's optional public `origin`, so selectors can show device, recovery source,
and account address without changing account identity or signing selection.
