//! Guardrail gate: the real engine lives in the bundled `guardrail` crate\n//! (pure, no network, no keys, no clock reads). This module is a re-export shim\n//! so every existing `hl_agent::guardrail::*` path keeps working. Its behavior\n//! (same caps, same rejection order, same reasons) is pinned by the engine's\n//! 36-test suite plus this crate's own tests.
pub use ::guardrail::*;
