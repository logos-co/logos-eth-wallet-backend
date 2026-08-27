//! Offline transaction construction + ABI encode/decode (the folded-in
//! tx-builder).
//!
//! Builds unsigned native-ETH and ERC20-transfer transactions (as JSON the
//! keystore will sign), ABI-encodes ERC20 reads (`balanceOf`/`decimals`/
//! `symbol`), and encodes/decodes **Multicall3 `aggregate3`** so the coordinator
//! can fetch every balance on a chain in one `eth_call`. No network, no keys.
//! Pure Rust, unit-tested with `cargo test`.

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use serde_json::{json, Value};

sol! {
    #[allow(missing_docs)]
    interface IERC20 {
        function balanceOf(address owner) external view returns (uint256);
        function decimals() external view returns (uint8);
        function symbol() external view returns (string);
        function transfer(address to, uint256 amount) external returns (bool);
    }

    #[allow(missing_docs)]
    struct Call3 { address target; bool allowFailure; bytes callData; }
    #[allow(missing_docs)]
    struct Result3 { bool success; bytes returnData; }

    #[allow(missing_docs)]
    interface IMulticall3 {
        function aggregate3(Call3[] calls) external payable returns (Result3[] returnData);
        function getEthBalance(address addr) external view returns (uint256);
    }
}

/// The canonical Multicall3 deployment address (same on most EVM chains).
pub const MULTICALL3: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";

/// The canonical Multicall3 address, parsed.
pub fn multicall3_address() -> Address {
    MULTICALL3.parse().expect("valid Multicall3 address")
}

fn hex0x(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

fn u256_hex(v: U256) -> String {
    format!("0x{:x}", v)
}

fn u64_hex(v: u64) -> String {
    format!("0x{v:x}")
}

// ── ERC20 calldata ───────────────────────────────────────────────────────────

pub fn erc20_transfer_calldata(to: Address, amount: U256) -> Vec<u8> {
    IERC20::transferCall { to, amount }.abi_encode()
}

pub fn erc20_balance_of_calldata(owner: Address) -> Vec<u8> {
    IERC20::balanceOfCall { owner }.abi_encode()
}

pub fn erc20_decimals_calldata() -> Vec<u8> {
    IERC20::decimalsCall {}.abi_encode()
}

pub fn erc20_symbol_calldata() -> Vec<u8> {
    IERC20::symbolCall {}.abi_encode()
}

/// Decode a 32-byte ABI `uint256` return (balanceOf).
pub fn decode_uint256(data: &[u8]) -> Option<U256> {
    if data.len() < 32 {
        return None;
    }
    Some(U256::from_be_slice(&data[..32]))
}

/// Decode an ABI `uint8` return (decimals) — the value sits in the last byte.
pub fn decode_u8(data: &[u8]) -> Option<u8> {
    if data.len() < 32 {
        return None;
    }
    Some(data[31])
}

/// Decode an ABI dynamic `string` return (symbol).
pub fn decode_string(data: &[u8]) -> Option<String> {
    IERC20::symbolCall::abi_decode_returns(data).ok()
}

// ── Multicall3 ───────────────────────────────────────────────────────────────

/// Encode `aggregate3` over `(target, callData)` pairs, all with
/// `allowFailure = true` (a single reverting call won't sink the batch).
pub fn multicall3_aggregate3_calldata(calls: &[(Address, Vec<u8>)]) -> Vec<u8> {
    let calls3: Vec<Call3> = calls
        .iter()
        .map(|(t, d)| Call3 { target: *t, allowFailure: true, callData: Bytes::from(d.clone()) })
        .collect();
    IMulticall3::aggregate3Call { calls: calls3 }.abi_encode()
}

/// Multicall3's own `getEthBalance(address)` (native balance inside a batch).
pub fn multicall3_get_eth_balance_calldata(addr: Address) -> Vec<u8> {
    IMulticall3::getEthBalanceCall { addr }.abi_encode()
}

/// Decode an `aggregate3` return into per-call `returnData` (None where the call
/// failed).
pub fn decode_aggregate3_returns(data: &[u8]) -> Option<Vec<Option<Vec<u8>>>> {
    let decoded = IMulticall3::aggregate3Call::abi_decode_returns(data).ok()?;
    Some(
        decoded
            .into_iter()
            .map(|r| if r.success { Some(r.returnData.to_vec()) } else { None })
            .collect(),
    )
}

// ── Unsigned transactions (keystore-signable JSON) ───────────────────────────

/// Fee policy for an unsigned transaction.
#[derive(Clone, Debug)]
pub enum Fee {
    Eip1559 { max_fee_per_gas: U256, max_priority_fee_per_gas: U256 },
    Legacy { gas_price: U256 },
}

fn apply_fee(o: &mut serde_json::Map<String, Value>, fee: &Fee) {
    match fee {
        Fee::Eip1559 { max_fee_per_gas, max_priority_fee_per_gas } => {
            o.insert("fee_mode".into(), json!("eip1559"));
            o.insert("max_fee_per_gas".into(), json!(u256_hex(*max_fee_per_gas)));
            o.insert("max_priority_fee_per_gas".into(), json!(u256_hex(*max_priority_fee_per_gas)));
        }
        Fee::Legacy { gas_price } => {
            o.insert("fee_mode".into(), json!("legacy"));
            o.insert("gas_price".into(), json!(u256_hex(*gas_price)));
        }
    }
}

/// Build an unsigned native-ETH transfer as the JSON the keystore signs.
pub fn unsigned_native_tx(to: Address, value: U256, nonce: u64, gas_limit: u64, fee: &Fee) -> Value {
    let mut o = serde_json::Map::new();
    o.insert("to".into(), json!(to.to_string()));
    o.insert("value".into(), json!(u256_hex(value)));
    o.insert("nonce".into(), json!(u64_hex(nonce)));
    o.insert("gas_limit".into(), json!(u64_hex(gas_limit)));
    o.insert("data".into(), json!("0x"));
    apply_fee(&mut o, fee);
    Value::Object(o)
}

/// Build an unsigned ERC20 `transfer` (to = token, value = 0, data = calldata).
pub fn unsigned_erc20_tx(
    token: Address,
    to: Address,
    amount: U256,
    nonce: u64,
    gas_limit: u64,
    fee: &Fee,
) -> Value {
    let data = erc20_transfer_calldata(to, amount);
    let mut o = serde_json::Map::new();
    o.insert("to".into(), json!(token.to_string()));
    o.insert("value".into(), json!("0x0"));
    o.insert("nonce".into(), json!(u64_hex(nonce)));
    o.insert("gas_limit".into(), json!(u64_hex(gas_limit)));
    o.insert("data".into(), json!(hex0x(&data)));
    apply_fee(&mut o, fee);
    Value::Object(o)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    const ALICE: Address = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");
    const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

    #[test]
    fn erc20_transfer_selector_and_args() {
        let data = erc20_transfer_calldata(ALICE, U256::from(1_000_000u64));
        // transfer(address,uint256) selector
        assert_eq!(&data[0..4], &[0xa9, 0x05, 0x9c, 0xbb]);
        // address right-aligned in the first 32-byte word
        assert_eq!(&data[4 + 12..4 + 32], ALICE.as_slice());
        // amount in the second word
        assert_eq!(decode_uint256(&data[36..68]).unwrap(), U256::from(1_000_000u64));
    }

    #[test]
    fn erc20_balance_of_selector() {
        let data = erc20_balance_of_calldata(ALICE);
        assert_eq!(&data[0..4], &[0x70, 0xa0, 0x82, 0x31]);
    }

    #[test]
    fn multicall3_aggregate3_roundtrips() {
        let calls = vec![
            (multicall3_address(), multicall3_get_eth_balance_calldata(ALICE)),
            (USDC, erc20_balance_of_calldata(ALICE)),
        ];
        let encoded = multicall3_aggregate3_calldata(&calls);
        // aggregate3(Call3[]) selector 0x82ad56cb
        assert_eq!(&encoded[0..4], &[0x82, 0xad, 0x56, 0xcb]);

        // Build a synthetic return: native = 5 wei (success), token = failed.
        let native = U256::from(5u64).to_be_bytes::<32>().to_vec();
        let results = vec![
            Result3 { success: true, returnData: Bytes::from(native) },
            Result3 { success: false, returnData: Bytes::new() },
        ];
        let ret = IMulticall3::aggregate3Call::abi_encode_returns(&results);
        let decoded = decode_aggregate3_returns(&ret).unwrap();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decode_uint256(decoded[0].as_ref().unwrap()).unwrap(), U256::from(5u64));
        assert!(decoded[1].is_none());
    }

    #[test]
    fn unsigned_native_has_keystore_fields() {
        let fee = Fee::Eip1559 { max_fee_per_gas: U256::from(2_000_000_000u64), max_priority_fee_per_gas: U256::from(1_000_000_000u64) };
        let tx = unsigned_native_tx(ALICE, U256::from(1_000u64), 7, 21_000, &fee);
        assert_eq!(tx["nonce"], "0x7");
        assert_eq!(tx["value"], "0x3e8");
        assert_eq!(tx["gas_limit"], "0x5208");
        assert_eq!(tx["fee_mode"], "eip1559");
        assert_eq!(tx["data"], "0x");
    }

    #[test]
    fn unsigned_erc20_targets_token_with_calldata() {
        let fee = Fee::Legacy { gas_price: U256::from(1_000_000_000u64) };
        let tx = unsigned_erc20_tx(USDC, ALICE, U256::from(42u64), 0, 60_000, &fee);
        assert_eq!(tx["to"], USDC.to_string());
        assert_eq!(tx["value"], "0x0");
        assert_eq!(tx["fee_mode"], "legacy");
        let data = tx["data"].as_str().unwrap();
        assert!(data.starts_with("0xa9059cbb")); // transfer selector
    }
}
