//! End-to-end check of the builder approval flow on Hyperliquid TESTNET.
//!
//! NEEDS a funded builder address (Hyperliquid requires a builder to hold at least
//! 100 USDC perp account value before it can collect fees) and your own testnet
//! API-wallet key. Run it ONLY after that, manually: an agent must not run it
//! unattended.
//!
//! It signs one IOC limit order (0.005 HYPE @ $20, far from any resting order),
//! attaches OUR builder code {b: <BUILDER_ADDRESS>, f: 20} (2 bp), POSTs to the
//! testnet /exchange endpoint, and prints the venue response verbatim.
//!
//! Usage:
//!   cargo run -p hl-agent --example testnet_e2e -- <api-wallet-hex-key> \
//!       <builder-address> [fee_tenths_bp]
//!
//! The key is read from argv (never .env/disk), used in memory only, and never
//! logged. Testnet only: this binary refuses to run with HL_ALLOW_MAINNET set.

use anyhow::bail;
use hl_agent::{AgentSigner, ExchangeClient, HlClient, OrderWire, TESTNET};

/// Our builder address — pass it or set HL_BUILDER_ADDRESS when the need is funded.
/// Placeholder is intentionally invalid so an unfunded run fails fast and loud.
const BUILDER_ADDRESS_FALLBACK: &str = "0x0000000000000000000000000000000000000000";

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let key = args.next().unwrap_or_default();
    if key.is_empty() {
        bail!("usage: testnet_e2e <api-wallet-hex-key> <builder-address> [fee_tenths_bp]");
    }
    let builder_addr = args.next().unwrap_or_else(|| {
        std::env::var("HL_BUILDER_ADDRESS").unwrap_or_else(|_| BUILDER_ADDRESS_FALLBACK.to_string())
    });
    let fee_tenths_bp: u32 = args.next().map(|s| s.parse()).unwrap_or(Ok(20))?;

    if std::env::var("HL_ALLOW_MAINNET").is_ok() {
        bail!("HL_ALLOW_MAINNET set — this example is TESTNET-ONLY by design; unset it");
    }

    // 1. Read-only sanity: the testnet API must answer before anything is signed.
    let hype_mid = HlClient::new(TESTNET).mid_of("HYPE")?;
    println!("[1/3] testnet API live, HYPE mid = {hype_mid}");

    // 2. Sign: one IOC limit order, far from the mid so it does not fill immediately.
    let signer = AgentSigner::from_hex_key(&key)?;
    println!("[2/3] signer ready, agent address = {}", signer.address());

    // 3. Place the order with our builder code attached. Asset id: read HYPE's id
    //    from the testnet meta response at run time and substitute it before the
    //    funded run (do not trust a hard-coded id; see hl.rs meta()).
    let order = OrderWire::limit(1071, true, "20.0", "0.005", false, "Ioc");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as u64;
    let client = ExchangeClient::testnet(signer);
    let resp = client.place_order_with_builder(&order, &builder_addr, fee_tenths_bp, nonce, false)?;
    println!("[3/3] exchange response: {}", resp.raw);
    if resp.is_ok() {
        println!(
            "PASS: order accepted with builder code attached (fee {fee_tenths_bp} tenths-bp = {:.1} bp)",
            f64::from(fee_tenths_bp) / 10.0
        );
    } else {
        println!("FAIL: {}", resp.error_text());
        std::process::exit(1);
    }
    Ok(())
}
