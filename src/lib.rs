//! hl-agent — guardrailed Hyperliquid kit for AI agents — read-only tools, dry-run simulator, optional signed orders.
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
pub mod live;
pub mod mcp;
pub mod signer;
pub mod sim;

pub use guardrail::{Ctx, Decision, Guardrail, GuardrailConfig, OrderIntent};
pub use exchange::{ExchangeClient, ExchangeResponse, OrderWire, validate_builder_fee};
pub use hl::{HlClient, MAINNET, TESTNET};
pub use live::{explain, receipt_from, receipt_from_order_response, HlVenue, LiveConfig, LiveOrderRequest, LiveOutcome, LiveTool, LiveTrader, Venue};
pub use sim::{AlwaysFillStub, DryRunSimulator, FillCheck, FillCheckStub, FillStatus, SimFill, SimResult};
pub use signer::{AgentSigner, HlBuilder, HlOrder, HlOrderAction, HlOrderType, SigEnvelope, approve_builder_fee_action_msgpack, validate_builder_address};
