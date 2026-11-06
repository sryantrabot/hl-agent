//! hl-agent — guardrailed Hyperliquid kit for AI agents.
//! Week 1: read-only market tools + dry-run order simulator. Week 2: an OPTIONAL signed
//! path (AgentSigner + ExchangeClient) so an agent can place TESTNET orders with the
//! user's own API-wallet key — but `DryRunSimulator` remains the DEFAULT path: nothing
//! in the MCP server or main binary signs or POSTs an order. The builder code
//! `{"b": <our address>, "f": 20..=50}` (2-5 bp) is attached by
//! `place_order_with_builder` only after the user's MAIN wallet approves the fee
//! (one-time, revocable; API wallets cannot approve). Public docs: README.md.
pub mod exchange;
pub mod guardrail;
pub mod hl;
pub mod mcp;
pub mod signer;
pub mod sim;

pub use guardrail::{Ctx, Decision, Guardrail, GuardrailConfig, OrderIntent};
pub use exchange::{ExchangeClient, ExchangeResponse, OrderWire, validate_builder_fee};
pub use hl::{HlClient, MAINNET, TESTNET};
pub use sim::{AlwaysFillStub, DryRunSimulator, FillCheck, FillCheckStub, FillStatus, SimFill, SimResult};
pub use signer::{AgentSigner, HlBuilder, HlOrder, HlOrderAction, HlOrderType, SigEnvelope};
