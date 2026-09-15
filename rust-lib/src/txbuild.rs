//! Offline ABI encode/decode.
//!
//! ABI-encodes the ERC20 `transfer` this wallet sends and the reads it makes
//! (`balanceOf`/`decimals`/`symbol`), and encodes/decodes **Multicall3 `aggregate3`**
//! so the coordinator can fetch every balance on a chain in one `eth_call`. The unsigned
//! transaction itself is `tx_sender_module`'s to build. No network, no keys. Pure Rust,
//! unit-tested with `cargo test`.

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;

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

/// Parse an EVM quantity written either as `0x`-hex or as decimal. Nodes answer hex,
/// this wallet stores decimal, and both reach the same fields.
///
/// No digits is `None`, never zero: an empty balance means "we could not read it", and
/// answering 0 would turn that into a number the user reads as a fact.
pub fn parse_u256_any(s: &str) -> Option<U256> {
    let t = s.trim();
    let (digits, radix) = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => (h, 16),
        None => (t, 10),
    };
    if digits.is_empty() {
        return None;
    }
    U256::from_str_radix(digits, radix).ok()
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    const ALICE: Address = address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");
    const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

    #[test]
    fn a_quantity_parses_from_either_notation_and_nothing_parses_as_zero() {
        assert_eq!(parse_u256_any("0x5208"), Some(U256::from(21_000u64)));
        assert_eq!(parse_u256_any(" 21000 "), Some(U256::from(21_000u64)));
        assert_eq!(parse_u256_any("0x0"), Some(U256::ZERO));
        // An unread balance is empty. Answering 0 would report "you have none" as a fact.
        assert_eq!(parse_u256_any(""), None);
        assert_eq!(parse_u256_any("0x"), None);
        assert_eq!(parse_u256_any("twenty"), None);
    }

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
    }}
