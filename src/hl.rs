//! Hyperliquid public market data client — READ-ONLY, keyless.
//! Only POST {base}/info with public body types (meta / allMids / candleSnapshot).
//! No signing, no exchange endpoint, no keys: the week-1 kit never trades for real.
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAINNET: &str = "https://api.hyperliquid.xyz";
pub const TESTNET: &str = "https://api.hyperliquid-testnet.xyz";

#[derive(Debug, Clone)]
pub struct HlClient {
    http: reqwest::blocking::Client,
    base: String,
}

/// GET /meta: perp universe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetMeta {
    pub name: String,
    #[serde(default)]
    pub sz_decimals: u32,
    #[serde(default)]
    pub max_leverage: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_delisted: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MetaOut {
    pub universe: Vec<AssetMeta>,
}

/// GET /allMids: coin -> mid price (strings). Flattened perp mids for output.
#[derive(Debug, Clone, Serialize)]
pub struct AllMidsOut {
    pub mids: Vec<MidRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MidRow {
    pub coin: String,
    /// Parsed mid; None when the feed sends a non-numeric string (e.g. index keys) — never silently dropped.
    pub mid: Option<f64>,
    /// Raw string exactly as the venue sent it.
    pub raw: String,
}

/// GET /candleSnapshot: one OHLCV bar (venue strings preserved verbatim in `raw`).
#[derive(Debug, Clone, Serialize)]
pub struct Candle {
    /// Bar open time, ms since epoch.
    pub t: i64,
    /// Bar close time, ms since epoch.
    pub t_close: i64,
    pub coin: String,
    pub interval: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub trades: u64,
}

impl HlClient {
    pub fn new(base: &str) -> Self {
        Self {
            http: reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("reqwest client"),
            base: base.trim_end_matches('/').to_string(),
        }
    }

    pub fn mainnet() -> Self {
        Self::new(MAINNET)
    }

    fn info(&self, body: &Value) -> Result<Value> {
        let url = format!("{}/info", self.base);
        let resp = self.http.post(&url).json(body).send()?;
        let status = resp.status();
        let text = resp.text()?;
        if !status.is_success() {
            anyhow::bail!("hyperliquid info {} -> HTTP {}: {}", url, status, truncate(&text, 200));
        }
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("hyperliquid info decode: {e}; body: {}", truncate(&text, 200)))
    }

    /// {type: meta} -> perp universe.
    pub fn meta(&self) -> Result<MetaOut> {
        let v = self.info(&serde_json::json!({ "type": "meta" }))?;
        let universe = v
            .get("universe")
            .and_then(|u| u.as_array())
            .ok_or_else(|| anyhow::anyhow!("meta: missing universe array"))?
            .iter()
            .filter_map(|a| {
                let name = a.get("name")?.as_str()?.to_string();
                Some(AssetMeta {
                    sz_decimals: a.get("szDecimals").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                    max_leverage: a.get("maxLeverage").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                    is_delisted: a.get("isDelisted").and_then(|x| x.as_bool()),
                    name,
                })
            })
            .collect();
        Ok(MetaOut { universe })
    }

    /// {type: allMids} -> string map; numeric keys like "@123" (perp index) are passed
    /// through with mid=None rather than dropped, so the output is honest about gaps.
    pub fn all_mids(&self) -> Result<AllMidsOut> {
        let v = self.info(&serde_json::json!({ "type": "allMids" }))?;
        let obj = v.as_object().ok_or_else(|| anyhow::anyhow!("allMids: expected object"))?;
        let mut mids: Vec<MidRow> = obj
            .iter()
            .map(|(k, val)| {
                let raw = val.as_str().unwrap_or("").to_string();
                let mid = raw.parse::<f64>().ok().filter(|f| f.is_finite());
                MidRow { coin: k.clone(), mid, raw }
            })
            .collect();
        mids.sort_by(|a, b| a.coin.cmp(&b.coin));
        Ok(AllMidsOut { mids })
    }

    /// {type: candleSnapshot} -> OHLCV bars for [start_ms, end_ms).
    pub fn candles(&self, coin: &str, interval: &str, start_ms: i64, end_ms: i64) -> Result<Vec<Candle>> {
        let v = self.info(&serde_json::json!({
            "type": "candleSnapshot",
            "req": { "coin": coin, "interval": interval, "startTime": start_ms, "endTime": end_ms }
        }))?;
        let arr = v.as_array().ok_or_else(|| anyhow::anyhow!("candleSnapshot: expected array"))?;
        let mut out: Vec<Candle> = arr
            .iter()
            .filter_map(|c| {
                let f = |k: &str| c.get(k).and_then(|x| x.as_str()).and_then(|s| s.parse::<f64>().ok());
                Some(Candle {
                    t: c.get("t")?.as_i64()?,
                    t_close: c.get("T")?.as_i64()?,
                    coin: c.get("s")?.as_str()?.to_string(),
                    interval: c.get("i")?.as_str()?.to_string(),
                    open: f("o")?,
                    high: f("h")?,
                    low: f("l")?,
                    close: f("c")?,
                    volume: f("v")?,
                    trades: c.get("n").and_then(|x| x.as_u64()).unwrap_or(0),
                })
            })
            .collect();
        out.sort_by_key(|c| c.t);
        Ok(out)
    }

    /// {type: clearinghouseState, user} -> the account's perp state. Read-only.
    pub fn clearinghouse_state(&self, user: &str) -> Result<serde_json::Value> {
        self.info(&serde_json::json!({ "type": "clearinghouseState", "user": user.trim().to_ascii_lowercase() }))
    }

    /// Current |position| notional (USD) for `coin` on `user`'s account, at `mark`.
    /// The live order path uses this to enforce the POSITION cap: the guardrail
    /// engine cannot see the account, so the caller must hand it the real number.
    /// A missing field is an ERROR, never a zero: assuming "flat" would let an
    /// agent stack positions past its own cap.
    pub fn position_notional(&self, user: &str, coin: &str, mark: f64) -> Result<f64> {
        let st = self.clearinghouse_state(user)?;
        let arr = st
            .get("assetPositions")
            .and_then(|a| a.as_array())
            .ok_or_else(|| anyhow::anyhow!("clearinghouseState: missing assetPositions array"))?;
        for p in arr {
            let pos = p.get("position").unwrap_or(p);
            let c = pos.get("coin").and_then(|v| v.as_str()).unwrap_or("");
            if c.eq_ignore_ascii_case(coin) {
                let szi = pos
                    .get("szi")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .ok_or_else(|| anyhow::anyhow!("clearinghouseState: {coin} position has no readable szi"))?;
                if !szi.is_finite() {
                    return Err(anyhow::anyhow!("clearinghouseState: {coin} szi is not finite"));
                }
                return Ok((szi.abs() * mark).abs());
            }
        }
        Ok(0.0)
    }

    /// Sum of the venue's own closed PnL (USD) since `start_ms`. The live order path
    /// seeds the daily-loss cap from this: without it the cap can never trip.
    pub fn realized_pnl_since(&self, user: &str, start_ms: i64) -> Result<f64> {
        let v = self.info(&serde_json::json!({
            "type": "userFillsByTime",
            "user": user.trim().to_ascii_lowercase(),
            "startTime": start_ms
        }))?;
        let arr = v
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("userFillsByTime: expected an array"))?;
        let mut sum = 0.0;
        for f in arr {
            if let Some(p) = f.get("closedPnl").and_then(|x| x.as_str()).and_then(|s| s.parse::<f64>().ok()) {
                if p.is_finite() {
                    sum += p;
                }
            }
        }
        Ok(sum)
    }

    /// The SAME position, but SIGNED (long > 0, short < 0), in USD at `mark`.
    /// The position cap needs the side: from |position| alone a sell against a short
    /// looks like a reduction and the cap switches off for the order that doubles it,
    /// while a buy that CLOSES a short looks like it opens one (critic round 4).
    /// A flat or missing coin is a real 0.0; a missing `szi` is still an error.
    pub fn position_signed_notional(&self, user: &str, coin: &str, mark: f64) -> Result<f64> {
        let st = self.clearinghouse_state(user)?;
        let arr = st
            .get("assetPositions")
            .and_then(|a| a.as_array())
            .ok_or_else(|| anyhow::anyhow!("clearinghouseState: missing assetPositions array"))?;
        for p in arr {
            let pos = p.get("position").unwrap_or(p);
            let c = pos.get("coin").and_then(|v| v.as_str()).unwrap_or("");
            if c.eq_ignore_ascii_case(coin) {
                let szi = pos
                    .get("szi")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .ok_or_else(|| anyhow::anyhow!("clearinghouseState: {coin} position has no readable szi"))?;
                if !szi.is_finite() {
                    return Err(anyhow::anyhow!("clearinghouseState: {coin} szi is not finite"));
                }
                return Ok(szi * mark);
            }
        }
        Ok(0.0)
    }

    /// The account's perp account value (USD) from the venue: `marginSummary.accountValue`.
    /// The live path seeds the guardrail's equity with this so the leverage cap is a
    /// statement about the real account, not about a number typed into the TOML.
    /// Missing or non-positive value is an ERROR: a zero equity would block every order
    /// for the wrong reason, and guessing would let an under-funded account over-lever.
    pub fn account_equity(&self, user: &str) -> Result<f64> {
        let st = self.clearinghouse_state(user)?;
        let raw = st
            .get("marginSummary")
            .and_then(|m| m.get("accountValue"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("clearinghouseState: missing marginSummary.accountValue"))?;
        let v = raw
            .parse::<f64>()
            .map_err(|e| anyhow::anyhow!("clearinghouseState: accountValue {raw:?} is not a number: {e}"))?;
        if !v.is_finite() || v <= 0.0 {
            return Err(anyhow::anyhow!("clearinghouseState: account value {v} is not a usable equity"));
        }
        Ok(v)
    }

    /// The agent addresses approved to trade for `user` (the venue's `extraAgents`).
    /// The live path uses this at startup so "the caps read `--user`" cannot silently
    /// mean "the caps read an account this key cannot trade for": the signed action
    /// carries no user field, so a wrong `--user` would leave the position and
    /// daily-loss caps reading an empty account while the order still executes.
    pub fn extra_agents(&self, user: &str) -> Result<Vec<String>> {
        let v = self.info(&serde_json::json!({
            "type": "extraAgents",
            "user": user.trim().to_ascii_lowercase()
        }))?;
        let arr = v
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("extraAgents: expected an array, got {}", truncate(&v.to_string(), 120)))?;
        Ok(arr
            .iter()
            .filter_map(|a| a.get("address").and_then(|x| x.as_str()).map(|s| s.to_ascii_lowercase()))
            .collect())
    }

    /// Latest mid for a coin from allMids (used by the simulator as the mark).

    pub fn mid_of(&self, coin: &str) -> Result<f64> {
        let mids = self.all_mids()?;
        mids
            .mids
            .iter()
            .find(|m| m.coin.eq_ignore_ascii_case(coin))
            .and_then(|m| m.mid)
            .ok_or_else(|| anyhow::anyhow!("no mid for coin {coin} (or mid not numeric)"))
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() > n {
        format!("{}…[{}B total]", &s[..n], s.len())
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Offline parse tests on REAL captured payloads (2026-11-04T23:3xZ, api.hyperliquid.xyz
    /// via the host browser MCP — values stored verbatim; no network in unit tests).

    #[test]
    fn meta_universe_parses_captured_payload() {
        // captured: universe head = BTC/ETH/ATOM/MATIC (+delisted flag on MATIC), len 235
        let payload = serde_json::json!({
            "universe": [
                {"szDecimals":5,"name":"BTC","maxLeverage":40,"marginTableId":56},
                {"szDecimals":4,"name":"ETH","maxLeverage":25,"marginTableId":55},
                {"szDecimals":2,"name":"ATOM","maxLeverage":5,"marginTableId":5},
                {"szDecimals":1,"name":"MATIC","maxLeverage":20,"marginTableId":20,"isDelisted":true}
            ]
        });
        let universe = payload
            .get("universe")
            .and_then(|u| u.as_array())
            .unwrap()
            .iter()
            .filter_map(|a| {
                let name = a.get("name")?.as_str()?.to_string();
                Some(AssetMeta {
                    sz_decimals: a.get("szDecimals").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                    max_leverage: a.get("maxLeverage").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                    is_delisted: a.get("isDelisted").and_then(|x| x.as_bool()),
                    name,
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(universe.len(), 4);
        assert_eq!(universe[0].name, "BTC");
        assert_eq!(universe[0].sz_decimals, 5);
        assert_eq!(universe[3].is_delisted, Some(true));
    }

    #[test]
    fn all_mids_map_parses_and_flags_non_numeric_keys() {
        let payload = serde_json::json!({"BTC":"80536.5","@142":"80494.5","HYPE":"83.7885","PURR/USDC":"0.117915"});
        let obj = payload.as_object().unwrap();
        let rows: Vec<MidRow> = obj
            .iter()
            .map(|(k, val)| {
                let raw = val.as_str().unwrap_or("").to_string();
                MidRow { coin: k.clone(), mid: raw.parse::<f64>().ok(), raw }
            })
            .collect();
        let btc = rows.iter().find(|r| r.coin == "BTC").unwrap();
        assert_eq!(btc.mid, Some(80_536.5));
        let idx = rows.iter().find(|r| r.coin == "@142").unwrap();
        assert_eq!(idx.mid, Some(80_494.5)); // numeric strings still parse
        assert!(rows.iter().all(|r| !r.raw.is_empty()));
    }

    #[test]
    fn candle_array_parses_captured_bars() {
        // captured: BTC 1h bars, o/c/h/l/v strings, n = trade count
        let payload = serde_json::json!([
            {"t":1793811600000i64,"T":1793815199999i64,"s":"BTC","i":"1h","o":"80475.0","c":"80382.0","h":"80500.0","l":"80188.0","v":"1337.39062","n":14775},
            {"t":1793815200000i64,"T":1793818799999i64,"s":"BTC","i":"1h","o":"80383.0","c":"80306.0","h":"80685.0","l":"80255.0","v":"2740.91212","n":16864}
        ]);
        let f = |c: &Value, k: &str| c.get(k).and_then(|x| x.as_str()).and_then(|s| s.parse::<f64>().ok());
        let bars: Vec<Candle> = payload
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| {
                Some(Candle {
                    t: c.get("t")?.as_i64()?,
                    t_close: c.get("T")?.as_i64()?,
                    coin: c.get("s")?.as_str()?.to_string(),
                    interval: c.get("i")?.as_str()?.to_string(),
                    open: f(c, "o")?,
                    high: f(c, "h")?,
                    low: f(c, "l")?,
                    close: f(c, "c")?,
                    volume: f(c, "v")?,
                    trades: c.get("n").and_then(|x| x.as_u64()).unwrap_or(0),
                })
            })
            .collect();
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].close, 80_382.0);
        assert_eq!(bars[1].trades, 16_864);
        assert_eq!(bars[0].coin, "BTC");
    }
}
