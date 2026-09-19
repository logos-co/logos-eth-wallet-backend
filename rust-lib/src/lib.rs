//! eth_wallet_backend — one composition of reusable EVM modules.
//!
//! Chains and scope come from `eth_rpc_module`, offered tokens from `token_list_module`, asset
//! rows and balances from `evm_assets_module`, accounts from `keystore_module`, and fees from
//! `fee_module`. Every transaction leaves through `tx_sender_module`, which reserves the nonce,
//! requests human approval, broadcasts, and records it. No key material reaches this module.
//!
//! Everything below is plain Rust with no Logos runtime and is unit-tested with
//! `cargo test --no-default-features`; the glue lives behind the default `logos_module`
//! feature.

pub mod budget;
pub mod catalogue;
pub mod contacts;
pub mod depinit;
pub mod history;
pub mod send_status;
pub mod settings;
pub mod store;
pub mod verified;

pub use settings::{Settings, SettingsError, SettingsStore, TokenSort};

#[cfg(feature = "logos_module")]
mod glue;
