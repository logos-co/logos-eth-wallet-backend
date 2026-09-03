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
| `eth_rpc_module` | JSON-RPC; it also **owns** the per-chain endpoint and verified mode |
| `fee_module` | EIP-1559 tiers — this module does not do fee maths |
| `keystore_module` | reading the account list and account names; requesting signatures |
| `token_list_module` | token metadata; the catalogue the picker shows and the snapshot an enabled token stores |

## Events, and the one this module relays

Every method that changes persisted or observable state announces it; a pure reader announces
nothing. Both halves are enforced as source-shape checks in
`tests/a_state_change_is_announced.rs`, along with a third: an event declared in the trait and
never emitted is a consumer subscribing to silence, and is rejected.

| Event | Says | Emitted by |
|---|---|---|
| `active_chain_changed(chainId)` | the active network moved | `set_active_chain` |
| `tokens_changed(chainId)` | the set OFFERED on that chain moved | `set_token_enabled` |
| `token_sort_changed(order)` | the balance-row order moved, device-wide | `set_token_sort` |
| `balances_updated(address)` | the amounts moved for one address | a confirmation, found by the sweep or by `refresh_tx_status` |
| `tx_status_changed(hash)` | one recorded transaction moved | the broadcast landing its hash, and the receipt poll |
| `history_changed(address)` | a recorded row appeared or changed that no hash can name | the intent write, and a broadcast that returned no hash |
| `accounts_changed(count)` | relayed from `keystore_module` | see below |
| `tokens_changed(chainId)` | also relayed from `token_list_module.tokens_updated` | see below |
| `networks_changed(chainId)` | relayed from `eth_rpc_module` — one chain's endpoint, transport or verified-proxy mode moved | see below |

Three rules hold across all of them:

* **On a change, not on a call.** Every `settings.json` mutator reports whether the write
  actually moved anything — a whole-value diff taken inside the one read-modify-write
  section — and announces only then. Re-selecting the network already active, or re-enabling a
  token whose snapshot is already stored, is a successful no-op that says nothing. A view that
  re-reads on every no-op write drives itself round in a loop.
* **After the write.** The announcement is in the success arm, so nothing is announced that
  did not reach disk.
* **A read never announces itself**, for the same loop reason. `get_history` and
  `refresh_pending` are the seeming exceptions and are not: both drive the receipt sweep, which
  WRITES, and the sweep announces its own confirmations — once, from the writer, so a row
  confirming under an open history view is not silent just because the caller was a read.

`tokens_changed` and `balances_updated` are deliberately separate. The set of tokens offered
changing is a different fact from an amount moving: nothing was spent, there is no address to
name, and every address on that chain now has a row more or a row fewer. `token_sort_changed`
is separate from both — nothing was offered or withdrawn, and it is chainless.

`history_changed` exists because a send is written to history BEFORE it is broadcast, and
until the node answers the row has **no hash** for `tx_status_changed` to name. On the arm
where the broadcast returns no hash at all, it never gets one. A view following a particular
send has `send_status_changed`; a plain history view has this.

`accounts_changed` is not this module's own: it is `keystore_module`'s, subscribed here and
re-emitted verbatim.

The relay exists because the keystore is mutated from a **different app** — `keystore_ui` —
and the wallet view has to learn about it. The view could subscribe to the keystore directly,
but only by naming it a dependency, which hands a renderer a token for the keystore's whole
surface including `request_approval`. Relaying keeps the view's dependency set at exactly this
module.

Three things about it:

* **Armed once.** `on_context_ready` can run again on a re-init, and the subscription is a
  blocking iterator on a thread of its own — a second one would park forever on a channel
  nothing closes.
* **Retried from the account reads.** Startup is the only chance a module gets; a client that
  could not be built there would leave the view deaf for the life of the process, so
  `list_accounts` and `get_account_labels` re-attempt the arming.
* **It announces once itself, after arming.** Arming is not retroactive and nothing buffers,
  so a change made before the subscription stood would otherwise be lost.

`count` is relayed as it arrives and is **advisory**: a rename does not move it. A consumer
that diffs counts sees nothing when an account is renamed, which is the defect this event
grew to cover — re-read both `list_accounts` and `get_account_labels` instead.

The same argument carries two more relays, on the same terms — armed once, retried from the
read that needs them, listened to off-thread:

* `token_list_module.tokens_updated(chainId)` becomes this module's own `tokens_changed`. The
  rows this wallet offers on a chain are that catalogue filtered by local settings, so a token
  another app imported moves them. `token_list_module.config_changed` is deliberately **not**
  relayed: a proxy or interval edit moves no row, and the one field that does move rows comes
  back as `tokens_updated` per chain that actually changed.
* `eth_rpc_module.chain_config_changed(chainId)` and `verified_proxy_mode_changed(chainId,
  mode)` both become `networks_changed`. `list_networks` serves eth_rpc's record — `rpcUrl`,
  `verifiedProxyMode` and the verdict — and that record is edited from `eth_rpc_ui`, a
  different app again.

## The verified-proxy gate, and the one thing it remembers

Every gated method used to ask `eth_rpc_module.verified_proxy_status` again. That is a
cross-process hop on the path of every balance, fee and send, and it has frozen this wallet
before. `verified_proxy_mode_changed` makes the answer rememberable — but a gate cache that
goes stale in the open direction is a wallet showing chain data while verification is actually
blocking, which is the failure the gate exists to prevent. So the cache (`src/gate.rs`) holds
exactly one fact, and errs in exactly one direction:

* **Only `mode: off` is ever stored.** eth_rpc computes `blocking = mode_required && !usable`,
  so an `off` chain is not blocking for any proxy health, ever. Skipping the hop for it skips
  a probe that could not have changed the answer.
* **A `required` chain is never cached at all.** Its verdict turns on live proxy health, which
  no event covers — that read stays live on every call, exactly as before.
* **A miss is a live read, not a refusal.** Refusing on a cold cache would fail every wallet at
  startup. The live read refuses on its own when it cannot be answered:
  `verified::unknown_verdict` is `blocking: true`.
* **Every window where nobody would tell us ends in a live read.** Nothing is trusted before
  the subscription stands; going live, losing the feed, and every event stamp a generation, so
  a read already in flight cannot land its answer over newer news; and a mode that is not
  exactly `off` — `required`, `unknown`, a value a future eth_rpc invents — removes the entry.
* **One edge opens it, and recovery goes back through that edge.** `SubStatus::Armed` from the
  runtime's per-module subscription status channel, reaching a watcher that already has a mode
  subscription behind it (`glue.rs::arm_gate`). `Abandoned` is terminal, so a dead feed comes
  back as a NEW subscription — unarmed at creation, so "the re-subscribe worked" opens nothing.
  A runtime with no status channel latches the cache cold and pays a live probe per check.

`tests/the_gate_cache_can_only_fail_closed.rs` holds `glue.rs` to that use, each check shipping
the mutant it must kill; `gate.rs`'s own tests prove the cache's rules in isolation.

## Configuration this module cannot change

The endpoint and the verified-proxy mode live in `eth_rpc_module`'s own `chains.json`, which
is **device-wide** and shared by every Logos wallet on the machine. This module reads both and
writes neither: `list_networks` reports `rpcUrl` straight out of `get_chain_config`, and there
is no setter for either field. A user changes them in the separate `eth_rpc_ui` app.

That is a deliberate removal, not an omission. A device-wide store edited from inside one
wallet's Settings sheet is a category error, and while two writers existed they could
disagree. The deprecated `rpcUrl` in `settings.json` is still *read* at startup to seed a
chain eth_rpc has no record of, so an endpoint set while eth_rpc was down is not lost.

## Starting up: ask, then initialize

`eth_rpc_ui` and a token-list app are **not** prerequisites. On context-ready this module
asks each dependency what it already holds and, only where the answer is "nothing", tells the
dependency to apply **its own** built-in defaults:

1. **`eth_rpc_module`** — the migrated `rpcUrl` values are seeded first, so a url the user
   set while eth_rpc was down claims an absent slot ahead of the built-in default; then
   `init_defaults()` unconditionally. Both go through `ensure_chain_config`, which fills only
   absent fields, and eth_rpc is keyed per chain so re-asking is idempotent per key.
2. **`token_list_module`** — `config_status()` first, and `init_defaults()` **only** on
   `state: "unconfigured"`. It is unkeyed, so the gate is mandatory.

eth_rpc goes first: every balance, fee and send needs it, while token_list only decorates.

Three rules this module will not bend. **A failed call never licenses a write** — an `Err` or
a `state: "unready"` means ask again, not "it has no config". **No whole-record write ever
reaches a sibling's store**: this module calls `ensure_chain_config` and never
`set_chain_config`, so another wallet's tuning cannot be revoked by our silence. And a
dependency that is not up yet is **retried lazily** — one bounded attempt per `list_networks`,
`list_tokens` or `get_balances` — because a module that crashed and restarted comes back after
our startup, and a wallet that gave up at startup would stay broken until the app restarts.

Every one of these calls is bounded — 1.5 s to read a config, 5 s to write one, against a
20 s protocol default — and so is their **sum**. A per-call bound says nothing about a method
that makes ten of them: `list_networks` reached ~29 s with every call individually capped, and
~120 s without. Each entry point now spends one shared allowance — 4 s for a consumer-facing
read, 6 s for the load hook, 12 s for a send's quote and approval request, 10 s for a receipt
sweep — and a call that no longer fits is not made. `list_networks`
reads the active network first, so a short allowance costs `verifiedProxyMode: "unknown"` and
an empty `rpcUrl` on the other two rather than a stall. What the load hook could not finish is
retried on the first read.

No outbound call runs under a lock, and that is checked rather than remembered:
`state()` hands back an owned handle, so a guard cannot escape it, and
`tests/glue_never_calls_under_a_lock.rs` fails if any lock in `glue.rs` shares a scope with a
call, if a new call arrives without a deadline, or if the closure-under-guard helper comes
back. A lock held across another module's call turns one slow dependency into a stall for
every reader, and `concurrency: "multi"` means there are always other readers.

What the lock never provided is mutual exclusion between requests — a shared read guard
excludes nobody. That belongs to `History`, which does every read-modify-write under its own
gate and matches on the hash, so an apply is a compare-and-set; and to `SendLedger`, which
holds the jobs, the reserved nonces and the network switch under ONE lock, because "reserve
this nonce and record the send" and "refuse a switch while a send could still move" are each
one decision and cannot span two. A send reserves and claims under that lock, asks the
keystore for approval holding nothing, and gives the nonce back on every path that does not
reach a job — a `Drop` guard, so a new early return cannot forget. `send_status` claims the
broadcast immediately before the broadcast RPC and nowhere earlier: a claim held across a
call that does not move money wedges the send if that call fails.

The claim hands back a ticket, and past it nothing else may settle that job — the rule the
cancel door always had, extended to every door. A concurrent dispatch would otherwise read a
job mid-broadcast, find no signature because the first had already acked it, and call a
transaction on its way to a node `failed`, handing its nonce to the next send. From the claim
onward the nonce is never released either: once the raw transaction has left this process
nothing here can prove it did not reach a chain.

`send_raw_transaction` is deliberately unbounded, so a broadcast can simply never return. The
deadline is on the CLAIM, not the call — a deadline on the call would not stop the transaction,
only stop us learning its hash. After it the send reports `stuck` and stops refusing network
switches; it never becomes terminal, never gives its nonce back and is never re-sent, and if
the broadcast answers hours later its hash still lands.

The durable record goes down BEFORE the bytes leave. It used to be written after the broadcast
returned, and on a failed broadcast not at all — so a crash inside that unbounded call, or an
early return, left a number that had already left with nothing on disk, and the next process
handed it straight to another send. `record_intent` writes an `unknown` row carrying (chain,
from, nonce) first and the outcome only completes it; `broadcast` takes the `Recorded` that
write hands back, so broadcasting before recording is not something the glue can express. A
row that reached no disk at all refuses the send rather than reporting itself written.

Settings are written by rename for the same reason `History` is. `std::fs::write` truncates
before it writes, and a read landing in that window used to parse nothing and report
**mainnet** — so the wallet could briefly gate, price and label against a network the user was
not on. A config that cannot be read now says so instead of answering chain 1.

`settings.json` holds the active chain, `enabledTokens` per network (full snapshotted records,
see **Tokens**) and the device-wide `tokenSort`. Every mutator goes through one
read-modify-write under one lock: a second such shape is how one change silently drops
another's. Unknown keys load and are dropped on the next write, and an enabled row that could
never be spent — no address, or claiming to be the native currency — is dropped at the door,
because that file can be hand-edited.

## Tokens

Two sets with two different provenances, and keeping them apart is the whole design.

**Built-in** is this wallet asserting a network-wide fact on the user's behalf: native ETH,
plus WETH where its address has been **verified** against an authoritative source *and*
confirmed on-chain — currently mainnet only. Sepolia and Hoodi ship ETH alone. A guessed token
address spends into the wrong contract, so the table carries a hole rather than a guess, and
that rule is unchanged. Built-in rows cannot be turned off: the native currency pays every fee,
and WETH is the wallet's claim, not something the user opted into.

**Enabled** is the user's own set, one whole record per row snapshotted from
`token_list_module` at the moment it was turned on. This wallet asserts nothing about those
addresses. It relays the bucket the row came from and marks every published row
`builtin: false`, so a screen can say who vouched for the address. The user who wants one of
sepolia's six WETH deployments names it themselves — which is the honest answer the built-in
hole leaves open.

What it will not do is invent a field. `set_token_enabled` reads the record from
`token_list_module` and **refuses an address it does not hold on that chain**, because
`decimals` scales every amount this wallet renders or signs and a wrong one is a balance off by
a power of ten. Records are snapshotted rather than referenced by address so `tokens::for_chain`
stays synchronous and a token keeps meaning what it meant when the user chose it, even if the
list is later re-fetched or gone.

`tokens::for_chain` is the **one** choke point where the two sets meet, and the balance list,
the send validator (`tokens::resolve`), the history decoder and the picker all read it — so none
of them can disagree about what the wallet offers. A built-in wins every field, so an enabled
row naming a built-in address is dropped rather than shown twice; so is a repeat inside the
enabled set, and so is a row with no address or one claiming to be native. An enabled token
calling itself `ETH` cannot shadow the currency a plain send is denominated in.

This module ships **no token metadata of its own** and pushes nothing into
`token_list_module`. A list is that module's to own, and a wallet injecting rows into a
device-wide store is the same category error as a wallet writing endpoints. What this module
does instead is ask token_list to initialize itself (above); its default config is offline and
uses its embedded list, so the wallet still does no network beyond JSON-RPC.

`list_tokens` fetches metadata **by address** — the offered rows and nothing else. A mainnet
`get_tokens` is ~86 KB of list to decorate two rows, and that reply crossed the wire on every
call. The picker (`list_available_tokens`) does read the whole catalogue, because showing it is
the point.

Each row reports two independent things, and neither can be derived from the other:

- `builtin` — whose assertion the address is. A built-in row and an enabled one both read
  `embedded` when the same list decorates them, so `metadataSource` cannot say it.
- `metadataSource` (`source` on a picker row) — who described the row: `native` | `allowlist`
  (ours, undecorated) | `custom` | `downloaded` | `embedded` | `unknown` | `enabled`. The three
  bucket names are token_list's own, **relayed, never inferred** — this module cannot tell a
  user-typed row from a downloaded one, so it does not guess; a match from a token_list too old
  to label its buckets reads `unknown`. `enabled` is a snapshot the list no longer holds.

`inTokenList` is the older flag and is not the one to show: it is false for the native currency
on every chain forever (there is no contract to match against), which made it useless as a
yes/no.

### The picker, and testnets

`list_available_tokens(chain_id, query, limit)` returns everything offered plus everything
token_list holds for the chain, native first, then what is enabled, then the rest
alphabetically. `total` counts the matches **before** the cut and `shown` after, so a view can
say what it is hiding instead of presenting a truncated list as the whole answer. A `limit` of
zero or less is no limit.

The embedded Uniswap list is overwhelmingly mainnet, so on sepolia and hoodi `listed` is
legitimately **0** and the reply carries the built-in rows alone. That is an answer, not a
failure: `ok` stays `true`, and `listError` — present only when the token_list call itself
failed — is what tells an empty catalogue from an unread one.

### The balance order

`get_balances` returns a row for **every** offered token, a zero balance included: a token the
user turned on and then cannot find in the list reads as the wallet having lost it. The array
arrives **already sorted** by the persisted `tokenSort`, because comparing 18-decimal amounts is
exact `U256` work and belongs where it is testable rather than in QML, where it would mean
`parseFloat`.

- `alpha` — by symbol, case-insensitive.
- `balance` — non-zero first, then rows whose sub-call failed, then zero; descending by amount
  within each band, alphabetical on a tie. An unread row sits between the two because "we could
  not read it" is not "you have none", and burying it under the zeros hides that.

The native currency stays **first in both orders**. It is the only row that pays for a fee, so
it is the one figure a user must always be able to find without hunting — and it is not
comparable with the rest anyway.

That last point is the constraint worth stating plainly: this wallet has **no fiat prices and
will not fetch any**, because a price feed discloses the user's IP. So `balance` orders by each
token's own amount, which across two different tokens is **not** a value order. Nothing
published here calls it one.

## Amounts

Every amount crosses the wire as base units — a decimal string, because 1 wei needs 256 bits
and a JS number carries 53 — and is rendered **here**, in `units.rs`, which contains no
floating point in any form. Callers render the string verbatim and do no arithmetic.

- `<field>Exact` carries every digit. It is what an error message, a signer intent and a
  detail screen use: an error about money must not round, and a human approving a signature
  must see the whole number.
- `<field>Display` is bounded to 5 places and **truncated, not rounded**. Only an exactly-zero
  amount renders `"0"`; anything below the resolution renders `"<0.00001"`, so 1 wei can never
  look like an empty account.
- Both are **absent** — not `""`, not `"0"` — when the amount or its decimals could not be
  read, so a view shows an em-dash. "We could not read it" is not "you have none".

A send may name its amount as `amount` (base units) or `amountUnits` (token units, as typed),
never both: they mean different things, and silently reinterpreting one as the other is how a
wallet signs a number off by 10^18.

A send names its token as `tokenAddress` — the contract, exactly — or as `token`, a symbol or
an address. A symbol is **not** an identity: the shipped Uniswap list holds two mainnet
contracts both calling themselves `LIT`, both 18 decimals, and a user may hold both. So
`tokens::resolve` accepts a symbol only while it names ONE contract on that chain and
**refuses an ambiguous one**, naming every candidate, rather than sending whichever row came
first. A built-in symbol is never ambiguous: the native currency and WETH are this wallet's
own assertion. `prepare_send` reports the resolved `tokenAddress` beside `amountSymbol`, so a
confirmation step can say which contract the send will call — a symbol cannot.

There is no `explorer` field on a network and no link to one anywhere. Nothing ever fetched or
opened one, so this closes no live leak — it removes the loaded gun a live explorer URL leaves
for whoever next adds a button that would disclose the user's IP to a third party.

## Verified proxy

`eth_rpc_module` owns the mode and the verdict outright — this wallet stores no copy, so
there is nothing to disagree with. Every chain read is gated on that verdict: when
verification is required and the proxy is not usable, the call refuses and carries the
verdict back, rather than showing stale numbers, zeros, or a clear-net read.

The gate opens on an explicit `blocking: false` and on nothing else. A verdict missing
`state`, missing `blocking`, or carrying either in a shape we do not recognise is a verdict
we could not read, and an unread verdict blocks — the whole point of the unknown case is to
fail closed on a shape it does not understand, so it may not have a hole in it.

**Every gate is inside its method's own allowance.** The probe is a cross-process hop and the
protocol answers an unbounded one with its 20s default, so a gate in FRONT of a budget bounds
nothing a user can feel: `get_balances` could hold a view for its read budget, then twenty
seconds of gate, then an untimed Multicall3 on top. There is now one allowance per entry
point, taken before the gate and spent by everything behind it, and no unbounded probe left
to reach for — a budget the probe outruns still refuses, because its verdict is `blocking`
and a timeout is not permission.

**The broadcast is gated in its own right.** `send` passing the gate proves nothing by the
time the signature comes back: a human spends seconds or minutes in the signer, and the proxy
can go unusable or the mode flip to `required` in that window. `send_status` checks again
immediately before it claims the broadcast — as late as a check can be and still be in front
of the money.

A refusal there is **not** a failed send: the transaction never left. The job is left exactly
as it stood — `awaitingApproval`, nothing claimed, nothing recorded, its nonce still reserved
— and the reply carries `blocked: true`, a `reason` and the whole `verifiedProxy` verdict with
`ok` still true and the status unchanged, which is what keeps a poller coming back. The next
poll sends it once the proxy is usable; `cancel_send` is still open in the meantime, and is
what stops a proxy that never returns from wedging the account. Marking it `failed` instead
would hand its number to the next send while a transaction signed at that number is still
waiting to leave — `tests/a_closed_gate_holds_a_send_it_does_not_lose_it.rs` drives the
ledger through exactly that.

## What was actually proved

Success replies carry `eth_rpc`'s own `route` label: **`verified`** (proof-backed against a
header's stateRoot), **`proxied`** (through the proxy, but forwarded to its execution
provider on trust), **`direct`** (never touched the proxy) or **`unknown`** (unlabelled).
Only `eth_getBalance`, `eth_getTransactionCount`, `eth_getCode`, `eth_getStorageAt` and
`eth_call` are ever `verified`; a receipt, a fee and a broadcast never are. A view must badge
on `route` and not on the network's mode, or it puts one "Verified" chip over numbers a light
client cannot prove. Where a reply mixes reads, `route` is the weakest of them, so it can
never over-claim; `prepare_send` additionally reports `feeRoute`, which is always `unknown`
because `fee_module` emits no label of its own.

## Transaction status

A row is written `unknown` before its transaction leaves, becomes `pending` when the node
answers with a hash, and is moved on from there by a receipt poll on **its own** chain.
`get_history` sweeps before it answers, so a transaction that confirmed while the view was
closed already reads `confirmed` the moment anything asks; `refresh_pending` is the same
sweep on demand. Both gate per record, never on the active chain, so a blocking proxy on
one network cannot suppress a due row on another. There is no background thread — the
schedule in `history.rs` backs off (3 s → 15 s → 60 s → stop) and an unanswered poll still
stamps its attempt.

Both replies carry `stillDue`: false once no row can move again. That is the signal a
caller's poll timer stops on — without it, one transaction that was dropped or replaced
keeps the wallet polling the endpoint for as long as the view is open. In `get_history` it
is derived from the rows in that same reply, so a due row on a chain the caller is not being
shown cannot keep a timer running over a screen with nothing pending; `stillDueAnyChain`
reports the all-chain answer for anyone who wants it. A row we polled for the whole hour and
never saw a receipt for reads `stalled: true` alongside its unchanged `pending` status: it is
not confirmed, not failed and not still coming, and inventing a fourth chain status to say so
would be a status the chain never gave us.

A row whose broadcast never answered stays `unknown` for good: we cannot say the transaction
reached a chain and we cannot say it did not, so we say neither. It carries no hash, so nothing
polls it and no timer waits on it — but it keeps its nonce, in this process and in every one
after. `get_history` answers `unresolved: [{ nonce, requestId, timestamp, detail, message }]`
for exactly those rows, because a number that never mines blocks every later send from that
account and the one way out is a replacement send with that nonce pinned — which a user cannot
do without being told the number. `strandedNonces` says the same thing for a number left
reserved by a job another send's duplicate request id overwrote.

### What else the receipt carries

The same receipt also records `txTo` — the transaction's **own** `to` — and `transfers`, the
ERC-20 `Transfer` logs decoded from it. Both are free: the whole receipt object is already in
hand when the row settles.

`txTo` is not `to`. For an ERC-20 send `to` is the recipient the user typed and `txTo` is the
token contract, and an explorer shows both under different labels. `to` is never touched by a
receipt; `row_json` answers `interactedWithDiffers` so the view can label them apart without
comparing addresses itself, and a native send — where they are the same — gets no second row.

A log is a `Transfer` when `topics[0]` is `keccak256("Transfer(address,address,uint256)")`,
there are exactly **three** topics, and `data` is exactly one 32-byte word. The topic is the
full 32-byte hash, so recognising it is a fact rather than a lookup — unlike a 4-byte function
selector, which collides and whose public registries are deliberately poisoned. The topic
COUNT is what separates ERC-20 from ERC-721, whose `Transfer` carries the same topic0 with a
fourth topic because `tokenId` is indexed; without it an NFT id renders as an amount.

Amounts stay unscaled on disk. The symbol, the decimals and every rendering are applied at read
time from the token table, so a token added to it later decorates rows already stored — and a
token the table does not hold gets `known: false` and **no rendered amount at all**, because
`units::decorate` writes no key without decimals. The view then shows the raw integer, labelled
as one. Nothing is ever scaled by an assumed 18. At most `TRANSFERS_MAX` are kept, sorted so
this account's own is never the one the cap drops, with `transfersMore` counting the rest.

`get_tx_details(address, hash)` reads what a receipt genuinely does not carry: the block's
timestamp (`eth_getBlockByNumber` through `raw_rpc`) and the transaction's own `gas` limit,
`maxPriorityFeePerGas` and `input` (`eth_getTransactionByHash`). The second call is **skipped**
when the row already stores all three, so a send recorded by this build costs one call — and a
row written before its calldata was recorded still makes it, which is how that row learns it. The two legs
are independent — `ok` is true when either landed, and the failed one's own words come back as
`blockError` / `transactionError` beside the fields it could not fill. Every reply names its
`hash`, refusals included, because a view renders it beside one transaction's own rows. Nothing
it fetches is ever `verified`: both methods are proxied, not proof-backed.

Gas prices leave in **gwei**, decorated with `gasPriceUnit`. The bounded rendering keeps five
fraction digits, and every gas price there has ever been is under that in ether — so an ether
figure would read `<0.00001` for every transaction on the screen: true, and useless.

A row skipped by its own chain's proxy verdict is disclosed, not silently frozen. Both
replies carry `blockedChains: [{ chainId, network, count, hashes, message, action,
verifiedProxy }]`, and `get_history` marks each affected row `verificationBlocked`. No
unverified data leaks either way — but a row stuck at `pending` on a network the view's
banner never mentions is otherwise unexplainable, and the user cannot act on what they
cannot see.

When a receipt settles a row it also records `blockNumber`, `gasUsed`, `effectiveGasPrice`,
`feeWei` and — native **and confirmed** sends only — `totalWei`; a reverted transaction moved
the fee and nothing else, so it has no total rather than one that counts the amount it did
not send. Every address a receipt or a log topic supplies is checksummed to EIP-55 on the way
in and again on the way out, so a row written by an earlier build renders in one casing too; the broadcast records `nonce`, `gasLimit`,
`maxFeePerGas`, `maxPriorityFeePerGas`, `feeCeilingWei`, `txInput` — the transaction's own
calldata, `"0x"` for a plain transfer and the `transfer` call for an ERC-20 one — and the
token's symbol and decimals.
All optional, so a history file written by an earlier build still loads, and absent means "the
node did not say" — the view renders an em-dash, never a zero. Every read-modify-write of the file runs
under one lock and lands by rename, because `concurrency: "multi"` really does run these
concurrently.

One entry that does not parse costs that entry and nothing else: entries are read one at a
time and an unreadable one is carried through every write untouched, where parsing the array
all-or-nothing meant a single legacy row hid every transaction — and every nonce — in the file.
A file that cannot be read at all still does not swallow the write: the row goes to a sidecar
beside it, which the nonce sweep already reads and the next successful write folds back in.

## Building and testing

```bash
cargo test --no-default-features --manifest-path rust-lib/Cargo.toml   # pure cores + source-shape guards
nix build .#default                                                    # the module
nix build .#lidl                                                       # the derived contract
```
