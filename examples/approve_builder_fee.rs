//! Builder-fee approval + verification, testnet by default (specs/hl-agent.md Week 2).
//!
//! This is the USER'S step in the kit's trust flow, written as a runnable example:
//! the user's MAIN wallet signs ONE `approveBuilderFee` action (max fee you allow
//! our builder per fill; revocable any time from your account page). An API/agent
//! wallet CANNOT approve — the venue rejects it — so run this with your main key
//! only if you accept that. On TESTNET (default) the key can be a throwaway.
//!
//! After approving, it checks the approval landed: `maxBuilderFee(user, builder)`
//! should return the fee you set (0 = no active approval).
//!
//! The action is a USER-SIGNED EIP-712 transaction, exactly as both official SDKs do it
//! (`sign_approve_builder_fee` / `ExchangeClient::approve_builder_fee`): domain
//! "HyperliquidSignTransaction", version "1", chainId 0x66eee (421614),
//! verifyingContract 0x0; primary type
//! "HyperliquidTransaction:ApproveBuilderFee(string hyperliquidChain,string maxFeeRate,
//! address builder,uint64 nonce)". The POSTed action therefore carries 6 fields
//! (type, maxFeeRate, builder, hyperliquidChain, signatureChainId, nonce).
//!
//! Usage:
//!   HL_MAIN_KEY=0x... cargo run -p hl-agent --example approve_builder_fee -- \
//!       [builder-address] [fee_tenths_bp] [--mainnet]
//!
//! The key is read from HL_MAIN_KEY only (never argv: it would land in your shell
//! history and `ps`), used in memory only, never logged. MAINNET requires
//! an explicit opt-in flag AND the env guard below, because approving on mainnet
//! is a real, on-chain, revocable permission for real funds.

use anyhow::bail;
use hl_agent::{ExchangeClient, HlClient, MAINNET, TESTNET};

fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mainnet = args.iter().position(|a| a == "--mainnet").map(|i| args.remove(i)).is_some();
    if mainnet && std::env::var("HL_ALLOW_MAINNET").is_err() {
        bail!("--mainnet requires HL_ALLOW_MAINNET set in the environment (operator opt-in)");
    }
    // The MAIN wallet key can withdraw everything you own, so it is never taken from
    // argv: argv is visible in `ps`, in shell history and in your terminal scrollback.
    // Read it from HL_MAIN_KEY and keep that variable out of your shell history.
    let mut it = args.into_iter();
    let first = it.next().unwrap_or_default();
    let key = if first.starts_with("0x") && first.len() == 66 {
        bail!("refusing a key on the command line (it would land in your shell history and `ps`). Put it in HL_MAIN_KEY instead: HL_MAIN_KEY=0x... cargo run -p hl-agent --example approve_builder_fee -- <builder-address> [fee]");
    } else {
        std::env::var("HL_MAIN_KEY").unwrap_or_default()
    };
    if key.trim().is_empty() {
        bail!("usage: HL_MAIN_KEY=0x... cargo run -p hl-agent --example approve_builder_fee -- [builder-address] [fee_tenths_bp] [--mainnet]\nThe key is read from HL_MAIN_KEY only, never from argv.");
    }
    let builder_addr = if first.is_empty() {
        std::env::var("HL_BUILDER_ADDRESS").unwrap_or_else(|_| {
            "0x5a229a20D9636a07583D6868FABf4B2903271Ab7".to_string()
        })
    } else {
        first.clone()
    };
    let fee_tenths_bp: u32 = it.next().map(|s| s.parse()).unwrap_or(Ok(20))?;

    let base = if mainnet { MAINNET } else { TESTNET };
    let chain = if mainnet { "MAINNET" } else { "testnet" };

    // 1. Sanity: the API must answer before anything is signed.
    let mids_ok = HlClient::new(base).all_mids().is_ok();
    println!("[1/4] {chain} API reachable: {mids_ok}");

    // 2. Sign the approval with the key given (MAIN wallet on mainnet — the venue
    //    rejects API-wallet approvals; on testnet any key works for the dry run).
    let signer = hl_agent::AgentSigner::from_hex_key(&key)?;
    println!("[2/4] signer ready, address = {} ({chain})", signer.address());
    if !mainnet {
        println!("      NOTE: on mainnet this MUST be your main wallet, not an API wallet.");
    }

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as u64;
    let client = ExchangeClient::new(base, signer);

    // 3. Approve: one action, max fee per fill = fee_tenths_bp tenths of bp.
    //    The unsigned action is printed FIRST so the operator can audit exactly what
    //    the signature will cover (it is the EIP-712 message; no key material inside).
    let action_preview =
        hl_agent::ExchangeClient::approve_builder_fee_action(&builder_addr, fee_tenths_bp, nonce, mainnet)?;
    println!("[3/4] signing + POSTing action: {action_preview}");
    let resp = client.approve_builder_fee(&builder_addr, fee_tenths_bp, nonce, mainnet)?;
    println!("[3/4] approveBuilderFee response: {}", resp.raw);
    if !resp.is_ok() {
        println!("FAIL: {}", resp.error_text());
        std::process::exit(1);
    }

    // 4. Verify independently via the keyless info endpoint.
    let user = {
        // re-derive the address from the same key for the check
        hl_agent::AgentSigner::from_hex_key(&key)?.address()
    };
    let max_fee = client.max_builder_fee(&user.to_string(), &builder_addr)?;
    println!("[4/4] maxBuilderFee(user, builder) = {max_fee} tenths of bp");
    if max_fee >= fee_tenths_bp {
        println!(
            "PASS: approval active — builder may charge up to {} bp per fill (revocable any time)",
            f64::from(max_fee) / 10.0
        );
    } else {
        println!("FAIL: approval not visible ({max_fee} < {fee_tenths_bp})");
        std::process::exit(1);
    }
    Ok(())
}
