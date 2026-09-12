//! eth_wallet_backend — an Ethereum-only wallet coordinator.
//!
//! Exactly one network is active at a time, chosen from mainnet, sepolia and hoodi. The
//! token table is the built-in rows plus what the user turned on from `token_list_module`.
//! Fees come from `fee_module`, JSON-RPC from `eth_rpc_module`, and every transaction LEAVES
//! through `tx_sender_module` — the one sender on the device, which reserves the nonce,
//! requests the signature from `keystore_module` for a human to authorise in `evm_signer_ui`,
//! broadcasts, and records it. No key material reaches this module.
//!
//! Everything below is plain Rust with no Logos runtime and is unit-tested with
//! `cargo test --no-default-features`; the glue lives behind the default `logos_module`
//! feature.

pub mod budget;
pub mod contacts;
pub mod depinit;
pub mod gate;
pub mod networks;
pub mod rows;
pub mod send;
pub mod settings;
pub mod store;
pub mod tokens;
pub mod txbuild;
pub mod units;
pub mod verified;

pub use networks::Network;
pub use settings::{NetworkSettings, Settings, SettingsError, SettingsStore};
pub use tokens::Token;

#[cfg(feature = "logos_module")]
mod glue;
