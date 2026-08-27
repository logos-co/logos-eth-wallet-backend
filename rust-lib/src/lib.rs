//! eth_wallet_backend — an Ethereum-only wallet coordinator.
//!
//! Exactly one network is active at a time, chosen from mainnet, sepolia and hoodi. The
//! token table is fixed (native ETH plus WETH where its address has been verified) and
//! there are no user-added tokens. Fees come from `fee_module`, JSON-RPC from
//! `eth_rpc_module`, and signatures are *requested* from `keystore_module` and authorised
//! by a human in `signer_ui` — no key material reaches this module.
//!
//! Everything below is plain Rust with no Logos runtime and is unit-tested with
//! `cargo test --no-default-features`; the glue lives behind the default `logos_module`
//! feature.

pub mod history;
pub mod networks;
pub mod settings;
pub mod tokens;
pub mod txbuild;

pub use history::{History, TxRecord};
pub use networks::Network;
pub use settings::{NetworkSettings, Settings, SettingsError, SettingsStore, VerifiedProxyMode};
pub use tokens::Token;

#[cfg(feature = "logos_module")]
mod glue;
