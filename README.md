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
| `place_order_live` | place a REAL order, signed by your own agent key, only after the guardrails pass (only listed when you start with `--live`) | signed POST to Hyperliquid |

`simulate_order` needs `--config`. Example call your agent makes:

```json
{"name":"simulate_order","arguments":{"coin":"BTC","side":"buy","size":0.01}}
```

Response carries `guardrail_decision` (`allow`/`reject` + reason), `fill_status`,
`fill` (simulated, at the live mid or your `limit_price`), `position_after`, and
`realized_pnl_today_usd` — the same numbers a real session would check.

## Guardrails (the point of the kit)

All caps live in one TOML file (see `examples/guardrail.example.toml`):

- **position cap** — max |position| notional per coin (USD), checked against your account's
  real position read from the venue on every live order
- **order cap** — max single-order notional (USD)
- **leverage cap** — order notional ≤ `max_leverage` × `account_equity_usd`
- **daily loss cap** — stops for the UTC day once realized PnL ≤ −cap; on live orders the
  PnL comes from the venue's own fills since UTC midnight, not from a counter in this process
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

Orders you approve will carry our builder code: `builder: {b: <address>, f: <fee in tenths
of bp>}` — a 2–5 bp per-trade fee paid to the kit maintainer, capped at approval time and
revocable by you. Mechanics (verified from Hyperliquid docs, 2026-11-04):

- You approve **once** from your main wallet via `ApproveBuilderFee` with a max-fee cap;
  revocable anytime.
- The builder must hold ≥100 USDC perp account value (a Hyperliquid builder requirement).
- Agent (API) wallets sign orders only — no withdrawals, no approvals.
- Fee evidence is public: daily per-builder fill CSVs on Hyperliquid's stats host.

Week 2 adds the local signer + builder-code attachment; week 3 ships example configs for
common agent hosts. The signed-order section is below.

## License

MIT.

## Wire your agent key (optional signed path)

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
  byte-exactly — 30/30 `cargo test -p hl-agent --lib` (includes the approveBuilderFee shape + end-to-end sign-and-recover tests), see the signed-order section of this README).
  Mainnet use still requires explicit operator opt-in.

## Placing real orders (`--live`)

The dry-run simulator stays the default. Real orders need three things and nothing else:

1. **Your own Hyperliquid API wallet key** in the environment as `HL_AGENT_KEY`. API wallets are
   sign-only: they can sign orders, never withdraw, never approve. The key is read from that
   variable only — never a flag, never a file, never `.env` — so it stays out of your shell
   history and the process list.
2. **`--user`**, your MAIN wallet address: the account the orders trade for. The venue compares it
   with the key, so a wrong pair fails on the first order instead of trading the wrong account.
3. **A guardrail config** (`--config`). The caps run *before* the signature exists.

```sh
# testnet first, tiny size, throwaway key
export HL_AGENT_KEY=0x...            # your API wallet key (sign-only)
cargo run -p hl-agent -- --live --testnet --user 0xYourMainWallet \
    --config examples/guardrail.example.toml

# mainnet, only after the testnet run behaved: same command plus --mainnet
# and HL_ALLOW_MAINNET=1 in your environment.
```

Start with the smallest size your venue allows (Hyperliquid's minimum order value is $10). The
first order is the real test: place one, read the receipt, and only then let your agent run.

What `place_order_live` does, in order:

1. Validates the arguments (limit orders only; `tif` is `Gtc`, `Ioc` or `Alo`).
2. Runs every guardrail cap on the intent. A rejection returns `outcome: rejected` with the exact
   reason and **nothing is signed, nothing is sent**.
3. Asks the venue (`maxBuilderFee`) whether your main wallet approved our builder fee. Only when
   the approval is at least our fee does the order carry `builder: {b, f}`. Otherwise it goes out
   with **no builder field at all** and the response says why.
4. Signs once and POSTs once. There is no retry: a nonce error is returned, never replayed.
5. Returns the venue's own receipt (`FILLED 0.01 @ 2000` / `RESTING` / `NOT FILLED: …`), the
   signed body for your audit trail, and one plain sentence for the errors users actually hit
   (minimum order value, unknown agent wallet, nonce).

Your agent key never leaves your machine and this server never asks for your main key.

## Approving the builder fee (your one signature)


An order carries our builder address only after YOUR main wallet approves it on the
venue, and only for the fee you approved — without that approval the order goes out
with no builder field at all. With it, a small fee (2 bp by default, hard-capped at
10 bp in the kit) lands with the builder when your order fills. Before any of that works, your **main** wallet approves the
fee once — an API/agent wallet cannot (the venue rejects it by design), and you can
revoke it at any time from your account.

```sh
# TESTNET (default; safe to run with a throwaway key). The key is read from
# HL_MAIN_KEY, never from the command line.
HL_MAIN_KEY=0x... cargo run -p hl-agent --example approve_builder_fee -- \
    0x5a229a20D9636a07583D6868FABf4B2903271Ab7 20

# MAINNET: same command plus --mainnet, and HL_ALLOW_MAINNET set in your environment.
```

The example signs the one `approveBuilderFee` action, then checks it landed by
querying `maxBuilderFee(your_address, builder)` — it prints the approved cap when
the check passes. No other signature from you is ever needed, and placing orders
without the approval simply fails on the venue side.

## Errors, explained plainly

The venue's error text is the real answer, so the kit returns it instead of retrying. Two of them
confuse nearly everyone, and both have a written explanation with the fix:

- `User or API Wallet 0x… does not exist` — a dashboard-approved agent whose private key you never
  had, or a mixed-case builder address that changed the signed hash:
  [docs/errors/user-or-api-wallet-does-not-exist.md](docs/errors/user-or-api-wallet-does-not-exist.md)
