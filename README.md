# hl-agent — guardrailed Hyperliquid kit for AI agents (week 1)

A Rust **MCP server** that gives your AI agent read-only Hyperliquid market tools and a
**dry-run order simulator** behind hard guardrail caps. Week 1 places **no real orders and
uses no keys** — it exists so you can rehearse an agent's trading behavior safely before
wiring a real (sign-only) agent key in week 2.

## Install & run

```sh
cargo run -p hl-agent                      # read-only market tools only
cargo run -p hl-agent -- --config examples/guardrail.example.toml   # + dry-run simulator
cargo run -p hl-agent -- --testnet         # Hyperliquid testnet API
cargo run -p hl-agent -- --check-config --config my.toml   # validate caps and exit
```

## Wire it to your agent host (quickstart)

The server speaks MCP (JSON-RPC 2.0) over **stdio** — one JSON message per line. Point your
MCP-capable agent host at the binary:

- **Claude Code / Hermes / OpenClaw style config (JSON):**

```json
{
  "mcpServers": {
    "hl-agent": {
      "command": "/path/to/hl-agent",
      "args": ["--config", "/path/to/guardrail.toml"]
    }
  }
}
```

- **Manual smoke test** (what your host does under the hood):

```sh
printf '%s\n' \
 '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
 '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
 '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
 | ./hl-agent --config examples/guardrail.example.toml
```

### Why guardrails (read this before you wire your agent in)

An agent that trades fails in boring ways. It retries an order because the first response timed out, and
keeps retrying all night. It acts on more permission than you meant to give it. It reports done when the
order never landed. None of that shows up as an error in your logs; the first sign is the balance.

hl-agent puts one gate between your agent and those mistakes. Every order intent passes through it, in a
fixed order, and the first cap broken stops the order:

- position, order-size and leverage caps in USD, all in one TOML file you can read
- a daily loss cap that ends the trading day once realized PnL crosses it, resetting at UTC midnight
- a loop tripwire that refuses near-identical repeat orders inside a sliding window
- a rolling rate limit on order intents per hour
- every rejection returns the exact reason with the numbers, so the agent fixes the size instead of
  retrying blind
- after each simulated fill: fill status, position after, realized PnL today

In week 1 the gate runs with no orders and no keys. Your agent rehearses against the live market and gets
rejected by the same caps that will guard its real orders in week 2.

## Tools

| tool | what it does | network |
|---|---|---|
| `hl_meta` | perp universe: coins, size decimals, max leverage | public REST, no key |
| `hl_allmids` | mid prices for all markets (`coins: ["BTC","ETH"]` filters) | public REST, no key |
| `hl_candles` | OHLCV bars (`coin`, `interval`, `start_ms`, `end_ms`) | public REST, no key |
| `simulate_order` | dry-run an order through the guardrail gate | none (or one mid fetch) |

`simulate_order` needs `--config`. Example call your agent makes:

```json
{"name":"simulate_order","arguments":{"coin":"BTC","side":"buy","size":0.01}}
```

Response carries `guardrail_decision` (`allow`/`reject` + reason), `fill_status`,
`fill` (simulated, at the live mid or your `limit_price`), `position_after`, and
`realized_pnl_today_usd` — the same numbers a real session would check.

## Guardrails (the point of the kit)

All caps live in one TOML file (see `examples/guardrail.example.toml`):

- **position cap** — max |position| notional per coin (USD)
- **order cap** — max single-order notional (USD)
- **leverage cap** — order notional ≤ `max_leverage` × `account_equity_usd`
- **daily loss cap** — stops for the UTC day once realized PnL ≤ −cap
- **loop tripwire** — rejects N near-identical orders inside a sliding window (stuck-loop
  protection for agents that retry)
- **rate limit** — max order intents per rolling hour

Every rejection returns the exact reason (`"order notional 1234.00 USD > cap 500.00"`)
so the agent can correct itself instead of retrying blind.

## Safety model

- **Week 1 never signs, never sends orders, never holds keys.** The only network calls are
  Hyperliquid's public `/info` endpoints (read-only market data).
- The simulator mutates only its own in-memory book. Nothing is persisted, nothing is sent.
- Your future **agent key never leaves your machine**: in week 2 you run the signer locally
  (Hyperliquid API wallets are sign-only — they cannot withdraw or approve). This server
  still won't see the key; the signer runs as a separate local process.

## Builder fee (what changes in week 2)

Orders you approve carry our builder address (0x5a229a20D9636a07583D6868FABf4B2903271Ab7)
with a per-trade fee of 2–5 bp, paid to the kit maintainer, capped at approval time and
revocable by you. Mechanics (verified from Hyperliquid docs, 2026-11-04):

- You approve **once** from your main wallet via `ApproveBuilderFee` with a max-fee cap;
  revocable anytime.
- The builder must hold ≥100 USDC perp account value (a Hyperliquid builder requirement).
- Agent (API) wallets sign orders only — no withdrawals, no approvals.
- Fee evidence is public: daily per-builder fill CSVs on Hyperliquid's stats host.

This release includes the local signer and the builder-code attachment; example
configs for common agent hosts are in the quickstart above.


## License

MIT.

## Wiring your agent key (the optional signed path)

The kit now has an OPTIONAL signed path (`hl_agent::signer::AgentSigner` +
`hl_agent::exchange::ExchangeClient`). The dry-run `DryRunSimulator` remains the
DEFAULT — nothing in the MCP server or the binary signs or places an order.

- **API wallet, not your main key.** Create a Hyperliquid API wallet and approve it to
  act for your master account. API wallets are **sign-only**: they can sign orders but
  can never approve anything or withdraw. Never share your main wallet's key with this
  kit, a process, or a person. The hex key is passed in memory only; nothing is read
  from `.env` or disk.
- **One-time revocable approval, by YOUR main wallet.** `ApproveBuilderFee` must be
  signed by your main wallet (the agent key cannot do it). You approve a max fee for
  our builder address and can revoke at any time.
- **Builder code on every order.** `place_order_with_builder` attaches
  `builder: {"b": <our address>, "f": <fee>}`; `f` is tenths of a basis point — our
  range is 20–50 (2–5 bp). The kit enforces the venue caps (10 bp perps, 100 bp spot;
  spot buys cannot carry a builder fee) and refuses to sign anything over the cap.
- **Testnet first.** `ExchangeClient::testnet` is the default base; MAINNET exists as
  a const for explicit operator opt-in only. Nonce errors are returned, never retried
  (Hyperliquid's nonce window makes blind replays dangerous).
- **Signature construction is VERIFIED against the official SDKs’ pinned vectors**
  (hyperliquid-rust-sdk + hyperliquid-python-sdk, fetched live 2026-11-05; the
  two SDK exchange_client test vectors + the create_signature EIP-712 vector pass
  byte-exactly (27/27 `cargo test -p hl-agent --lib`)).
  Mainnet use still requires explicit operator opt-in.
