//! Minimal stdio MCP (Model Context Protocol) server — JSON-RPC 2.0 over stdin/stdout.
//! Implements the methods an agent host needs: `initialize`, `notifications/initialized`,
//! `tools/list`, `tools/call`, `ping`. No external SDK: the protocol surface used here is
//! three request shapes and one notification (spec: modelcontextprotocol.io).
//! ALL stdout writes are protocol frames; logs go to stderr only.
use crate::guardrail::GuardrailConfig;
use crate::sim::{DryRunSimulator, FillStatus};
use crate::HlClient;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "hl-agent";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct Server {
    hl: HlClient,
    sim: Option<DryRunSimulator>,
}

fn tool_spec(name: &str, description: &str, props: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": props,
            "required": required
        }
    })
}

impl Server {
    pub fn new(hl: HlClient, sim: Option<DryRunSimulator>) -> Self {
        Self { hl, sim }
    }

    fn tools_list(&self) -> Value {
        let tools = vec![
            tool_spec(
                "hl_meta",
                "Hyperliquid perp universe (READ-ONLY, public REST): coin names, size decimals, max leverage, delisted flag.",
                json!({}),
                &[],
            ),
            tool_spec(
                "hl_allmids",
                "Hyperliquid mid prices for all markets (READ-ONLY, public REST). Optional filter by exact coin symbols.",
                json!({
                    "coins": {"type": "array", "items": {"type": "string"}, "description": "Optional: return only these coins (case-insensitive), e.g. [\"BTC\",\"ETH\"]"}
                }),
                &[],
            ),
            tool_spec(
                "hl_candles",
                "Hyperliquid OHLCV candles (READ-ONLY, public REST). Returns bars newest-last for [start_ms, end_ms).",
                json!({
                    "coin": {"type": "string", "description": "e.g. BTC"},
                    "interval": {"type": "string", "description": "1m 5m 15m 1h 4h 1d ..."},
                    "start_ms": {"type": "integer", "description": "bar-open time range start, ms since epoch"},
                    "end_ms": {"type": "integer", "description": "bar-open time range end, ms since epoch"}
                }),
                &["coin", "interval", "start_ms", "end_ms"],
            ),
            tool_spec(
                "simulate_order",
                "DRY-RUN order simulator behind guardrail caps (position/notional/leverage, daily loss, loop tripwire, rate limit). NO order is sent to Hyperliquid; NO key is used. Market orders fill at the live mid unless you pass mark_price; limit orders fill at limit_price. Guardrail state (daily PnL, rate window, tripwire) persists across calls in this session.",
                json!({
                    "coin": {"type": "string", "description": "e.g. BTC"},
                    "side": {"type": "string", "enum": ["buy", "sell"]},
                    "size": {"type": "number", "description": "base-asset size (coin units), > 0"},
                    "limit_price": {"type": "number", "description": "optional: limit order price; omit for market"},
                    "mark_price": {"type": "number", "description": "optional: override execution/mark price (else live mid is fetched)"}
                }),
                &["coin", "side", "size"],
            ),
        ];
        json!({ "tools": tools })
    }

    fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value, String> {
        match name {
            "hl_meta" => {
                let meta = self.hl.meta().map_err(|e| e.to_string())?;
                Ok(json!({ "universe": meta.universe, "count": meta.universe.len() }))
            }
            "hl_allmids" => {
                let mids = self.hl.all_mids().map_err(|e| e.to_string())?;
                let filter: Option<Vec<String>> = args.get("coins").and_then(|c| c.as_array()).map(|a| {
                    a.iter().filter_map(|x| x.as_str().map(|s| s.to_uppercase())).collect()
                });
                let rows: Vec<&crate::hl::MidRow> = mids
                    .mids
                    .iter()
                    .filter(|r| filter.as_ref().map_or(true, |f| f.iter().any(|c| c == &r.coin.to_uppercase())))
                    .collect();
                Ok(json!({ "mids": rows, "count": rows.len() }))
            }
            "hl_candles" => {
                let coin = args.get("coin").and_then(|c| c.as_str()).ok_or("missing coin")?;
                let interval = args.get("interval").and_then(|c| c.as_str()).ok_or("missing interval")?;
                let start = args.get("start_ms").and_then(|c| c.as_i64()).ok_or("missing start_ms")?;
                let end = args.get("end_ms").and_then(|c| c.as_i64()).ok_or("missing end_ms")?;
                if end <= start {
                    return Err("end_ms must be > start_ms".into());
                }
                let bars = self.hl.candles(coin, interval, start, end).map_err(|e| e.to_string())?;
                Ok(json!({ "coin": coin, "interval": interval, "bars": bars, "count": bars.len() }))
            }
            "simulate_order" => {
                let sim = self.sim.as_mut().ok_or("simulate_order unavailable: no guardrail config loaded (pass --config)")?;
                let coin = args.get("coin").and_then(|c| c.as_str()).ok_or("missing coin")?.to_string();
                let side = args.get("side").and_then(|c| c.as_str()).ok_or("missing side")?;
                let size = args.get("size").and_then(|c| c.as_f64()).ok_or("missing size")?;
                let limit_price = args.get("limit_price").and_then(|c| c.as_f64());
                let mark_override = args.get("mark_price").and_then(|c| c.as_f64());
                if side != "buy" && side != "sell" {
                    return Err("side must be \"buy\" or \"sell\"".into());
                }
                let intent = crate::guardrail::OrderIntent {
                    coin,
                    is_buy: side == "buy",
                    size,
                    limit_price,
                    reduce_only: false,
                    intent_id: None,
                };
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                let mark = match (mark_override, limit_price) {
                    (Some(m), _) => Some(m),
                    (None, Some(_)) => None, // limit: no mark needed for the fill, gate still price-checks via limit
                    (None, None) => Some(self.hl.mid_of(&intent.coin).map_err(|e| e.to_string())?),
                };
                let res = sim.execute(&intent, mark, now_ms);
                let status_str = match res.fill_status {
                    Some(FillStatus::Filled) => "FILLED",
                    Some(FillStatus::Partial) => "PARTIAL",
                    Some(FillStatus::Failed) => "FAILED",
                    Some(FillStatus::Unknown) => "UNKNOWN",
                    None => "NOT_RUN",
                };
                Ok(json!({
                    "dry_run": true,
                    "guardrail_decision": res.decision,
                    "fill_status": status_str,
                    "fill": res.fill,
                    "position_after": res.position_after,
                    "realized_pnl_today_usd": res.realized_pnl_today_usd,
                }))
            }
            other => Err(format!("unknown tool: {other}")),
        }
    }

    /// Serve one JSON-RPC request line; returns Some(response) for requests, None for notifications.
    pub fn handle_line(&mut self, line: &str) -> Option<Value> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return Some(json!({
                    "jsonrpc": "2.0", "id": Value::Null,
                    "error": {"code": -32700, "message": format!("parse error: {e}")}
                }))
            }
        };
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let is_request = msg.get("id").is_some() && !msg.get("id").unwrap().is_null();
        let params = msg.get("params").cloned().unwrap_or(json!({}));

        let result: Result<Option<Value>, (i64, String)> = match method.as_str() {
            "initialize" => Ok(Some(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION }
            }))),
            "notifications/initialized" => Ok(None), // notification: no response
            "ping" => Ok(Some(json!({}))),
            "tools/list" => Ok(Some(self.tools_list())),
            "tools/call" => {
                let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                match self.call_tool(&name, &args) {
                    Ok(content) => Ok(Some(json!({
                        "content": [{"type": "text", "text": serde_json::to_string(&content).unwrap_or_default()}],
                        "structuredContent": content,
                        "isError": false
                    }))),
                    Err(e) => Ok(Some(json!({
                        "content": [{"type": "text", "text": e}],
                        "isError": true
                    }))),
                }
            }
            other if other.starts_with("notifications/") => Ok(None),
            other => Err((-32601, format!("method not found: {other}"))),
        };

        if !is_request {
            return None; // never respond to notifications, even on unknown notification names
        }
        Some(match result {
            Ok(Some(v)) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
            Ok(None) => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message} }),
        })
    }

    /// Main stdio loop: one JSON-RPC message per line (stdin), responses on stdout.
    pub fn serve(&mut self) -> anyhow::Result<()> {
        let stdin = std::io::stdin();
        let mut out = std::io::stdout();
        for line in stdin.lock().lines() {
            let line = line?;
            if let Some(resp) = self.handle_line(&line) {
                out.write_all(resp.to_string().as_bytes())?;
                out.write_all(b"\n")?;
                out.flush()?;
            }
        }
        Ok(())
    }
}

/// Load and validate the guardrail TOML from disk (path from --config).
pub fn load_guardrail_config(path: &str) -> Result<GuardrailConfig, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
    GuardrailConfig::from_toml_str(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guardrail::Guardrail;
    use crate::sim::DryRunSimulator;

    fn server_with_sim() -> Server {
        let cfg = crate::guardrail::GuardrailConfig {
            max_position_notional_usd: 10_000.0,
            max_order_notional_usd: 1_000.0,
            max_leverage: 2.0,
            account_equity_usd: 5_000.0,
            daily_loss_cap_usd: 50.0,
            max_similar_actions: 5,
            loop_window_seconds: 60,
            max_calls_per_hour: 100,
        };
        Server::new(HlClient::new("http://127.0.0.1:1"), Some(DryRunSimulator::new(Guardrail::new(cfg))))
    }

    #[test]
    fn initialize_handshake_shape() {
        let mut s = server_with_sim();
        let resp = s
            .handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .expect("response");
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(resp["result"]["serverInfo"]["name"], "hl-agent");
        // notification gets NO response
        assert!(s.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
    }

    #[test]
    fn tools_list_has_four_tools() {
        let mut s = server_with_sim();
        let resp = s.handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 4);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["hl_meta", "hl_allmids", "hl_candles", "simulate_order"]);
        // every tool carries a JSON-Schema inputSchema
        assert!(tools.iter().all(|t| t["inputSchema"]["type"] == "object"));
    }

    #[test]
    fn unknown_method_is_protocol_error() {
        let mut s = server_with_sim();
        let resp = s.handle_line(r#"{"jsonrpc":"2.0","id":3,"method":"bogus"}"#).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
        // malformed JSON gets a parse error frame with null id
        let resp = s.handle_line("not json").unwrap();
        assert_eq!(resp["error"]["code"], -32700);
        assert!(resp["id"].is_null());
    }

    #[test]
    fn tools_call_simulate_order_gates_and_reports() {
        let mut s = server_with_sim();
        // mark override avoids any network
        let line = r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"simulate_order","arguments":{"coin":"BTC","side":"buy","size":0.01,"mark_price":80000.0}}}"#;
        let resp = s.handle_line(line).unwrap();
        assert_eq!(resp["result"]["isError"], false);
        let sc = &resp["result"]["structuredContent"];
        assert_eq!(sc["dry_run"], true);
        assert_eq!(sc["guardrail_decision"]["status"], "allow");
        assert_eq!(sc["fill_status"], "FILLED");
        // and a breach is rejected through the same tool
        let line = r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"simulate_order","arguments":{"coin":"BTC","side":"buy","size":99.0,"mark_price":80000.0}}}"#;
        let resp = s.handle_line(line).unwrap();
        let sc = &resp["result"]["structuredContent"];
        assert_eq!(sc["guardrail_decision"]["status"], "reject");
        assert!(sc["guardrail_decision"]["reason"].as_str().unwrap().contains("cap"));
    }

    #[test]
    fn unknown_tool_is_tool_error_not_protocol_error() {
        let mut s = server_with_sim();
        let resp = s.handle_line(r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#).unwrap();
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["error"].is_null());
    }

    #[test]
    fn guardrail_config_loads_from_toml_text() {
        let tmp = std::env::temp_dir().join(format!("hl-agent-test-{}.toml", std::process::id()));
        std::fs::write(&tmp, "max_position_notional_usd = 1000.0\nmax_order_notional_usd = 100.0\nmax_leverage = 1.0\naccount_equity_usd = 500.0\ndaily_loss_cap_usd = 20.0\nmax_similar_actions = 3\nloop_window_seconds = 30\nmax_calls_per_hour = 10\n").unwrap();
        let cfg = load_guardrail_config(tmp.to_str().unwrap()).expect("config loads");
        assert_eq!(cfg.max_position_notional_usd, 1000.0);
        let _ = std::fs::remove_file(&tmp);
    }
}
