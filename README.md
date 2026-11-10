# hl-agent — guardrailed Hyperliquid kit for AI agents (v0.2.0)

A Rust **MCP server** that gives your AI agent Hyperliquid market tools, a **dry-run order
simulator** behind hard guardrail caps, and an OPTIONAL **live-order path**: your own
sign-only agent key signs a real order, but only after every cap passes. **The default is
dry-run: no key, no order.** Real orders need `--live`, your key in `HL_AGENT_KEY`, and a
guardrail file you wrote.

These are leveraged perps. You can lose money, and testnet behaviour does not predict
mainnet. The kit is MIT-licensed with no warranty: the caps you write and the agent you point
at it are yours to own. Rate-limit and tripwire state live in the process and reset when it
restarts.

## Install

**Prebuilt binary (Linux ARM64, glibc 2.34+):** no Rust toolchain needed.

```sh
curl -L -O https://github.com/sryantrabot/hl-agent/releases/download/v0.2.0/hl-agent-aarch64-unknown-linux-gnu.tar.gz
curl -L -O https://github.com/sryantrabot/hl-agent/releases/download/v0.2.0/hl-agent-aarch64-unknown-linux-gnu.tar.gz.sha256
sha256sum -c hl-agent-aarch64-unknown-linux-gnu.tar.gz.sha256
tar xzf hl-agent-aarch64-unknown-linux-gnu.tar.gz
./hl-agent < /dev/null            # starts clean, answers MCP, Ctrl-D to exit
```

The archive is the v0.2.0 tag build (`7d81b65`), which predates `main` in two ways: its
`--check-config` prints a caps line over built-in defaults even when you passed no file, and it
has no `--approve-builder-fee` or `--print-example-config`. Both are in `main` now: to get
them today build from source at the branch head with the `--branch main` line below (the
`--tag v0.2.0` line does NOT have them); a new archive with them is next.

**Any other platform (x86_64 Linux, macOS, older ARM), about two minutes:**

```sh
# branch head: has --approve-builder-fee and --print-example-config
cargo install --locked --git https://github.com/sryantrabot/hl-agent --branch main
# the pinned release instead (v0.2.0): same kit, but NOT the two flags above
# cargo install --locked --git https://github.com/sryantrabot/hl-agent --tag v0.2.0
```

`--locked` matters: without it cargo can resolve newer crates than the committed `Cargo.lock`.
A C compiler must be installed, because the TLS library builds native code. There is **no
x86_64 prebuilt archive**; do not use the file named `hl-agent-x86_64-unknown-linux-gnu.tar.gz`
on the v0.2.0 release page, it holds an ARM64 binary (see the release notes).

**From a checkout:**

```sh
cargo run -p hl-agent                      # read-only market tools only
cargo run -p hl-agent -- --config examples/guardrail.example.toml   # + dry-run simulator
cargo run -p hl-agent -- --testnet         # Hyperliquid testnet API
cargo run -p hl-agent -- --check-config --config my.toml   # validate caps and exit
```

## Flags

`--config <file>` guardrail caps · `--check-config` validate and exit · `--testnet` use the
testnet API · `--live` enable the real-order tool · `--user <addr>` main wallet the orders
trade for · `--builder <addr>` builder address · `--fee-tenths-bp <n>` fee, tenths of a bp · `--base <url>` override the API URL.
`--approve-builder-fee` sign the one-off builder-fee approval and exit · `--print-example-config`
print the guardrail TOML to stdout. Both of these are on `main` and in any build from it; the
v0.2.0 archive below predates them. There is no `--help`; this list is it.

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
 | ./hl-agent --config guardrail.toml
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

Without `--live` the gate runs against the live market with no orders and no keys: your agent
rehearses, and gets rejected by the same caps that guard its real orders.

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

All caps live in one TOML file. Get a starting point from `examples/guardrail.example.toml` in
the repo, or with no checkout at all: `./hl-agent --print-example-config > guardrail.toml`.

- **position cap** — max |position| notional per coin (USD), judged on the position AFTER the
  order using the venue's own signed position on every live order: a sell against a short is
  not treated as a "reduction", and an order that shrinks your position is never blocked
- **order cap** — max single-order notional (USD), priced at the worse of the mark and the
  limit price you sign (a limit far above the mid cannot sneak past the cap)
- **leverage cap** — order notional ≤ `max_leverage` × your account's real equity, read from
  the venue before every live order (the `account_equity_usd` in the TOML is the simulator's
  assumption only)
- **daily loss cap** — stops for the UTC day once realized PnL ≤ −cap; on live orders the
  PnL comes from the venue's own fills since UTC midnight, not from a counter in this process
- **loop tripwire** — rejects N near-identical orders inside a sliding window (stuck-loop
  protection for agents that retry)
- **rate limit** — max order intents per rolling hour

Every rejection returns the exact reason (`"order notional 1234.00 USD > cap 500.00"`)
so the agent can correct itself instead of retrying blind.

## Safety model

- **The default is dry-run: no key, no order, no signature code.** Without `--live` the
  process never reads a key and every network call is a public `/info` read.
- The simulator mutates only its own in-memory book. Nothing is persisted, nothing is sent.
- **Your agent key never leaves your machine.** It is a Hyperliquid API wallet: sign-only,
  it cannot withdraw or approve. It is read from `HL_AGENT_KEY` only — never a flag, never a
  file, never `.env` — and its Debug output is redacted to the public address.
- **Before arming, the kit proves the key may trade for the `--user` account** (your address,
  or an entry in that account's `extraAgents`). A wrong `--user` would otherwise leave the
  position and daily-loss caps reading an empty account.

## Builder fee

Orders you approve will carry our builder code: `builder: {b: <address>, f: <fee in tenths
of bp>}` — a 2–5 bp per-trade fee paid to the kit maintainer, capped at approval time and
revocable by you. Mechanics (verified from Hyperliquid docs, 2026-11-04):

- You approve **once** from your main wallet via `ApproveBuilderFee` with a max-fee cap;
  revocable anytime.
- The builder must hold ≥100 USDC perp account value (a Hyperliquid builder requirement).
- Agent (API) wallets sign orders only — no withdrawals, no approvals.
- Fee evidence is public: daily per-builder fill CSVs on Hyperliquid's stats host.

The local signer and builder-code attachment ship today (see "Placing real orders" below);
example configs for common agent hosts are the next thing we add.

## License

MIT.

## Wire your agent key (optional signed path)

The kit now has an OPTIONAL signed path (`hl_agent::signer::AgentSigner` +
`hl_agent::exchange::ExchangeClient`). The dry-run `DryRunSimulator` remains the
DEFAULT — nothing signs or places an order unless you start the server with `--live`,
and then only after every cap in your TOML passes.

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
  byte-exactly — 60/60 `cargo test -p hl-agent --lib` (includes the approveBuilderFee shape + end-to-end sign-and-recover tests), see the signed-order section above).
  Mainnet use still requires explicit operator opt-in.

## Placing real orders (`--live`)

The dry-run simulator stays the default. Real orders need four things you control (plus one venue opt-in for mainnet):

0. **`--live`** on the command line. Without it the process never reads a key and lists no
   live tool at all.
1. **Your own Hyperliquid API wallet key** in the environment as `HL_AGENT_KEY`. API wallets are
   sign-only: they can sign orders, never withdraw, never approve. The key is read from that
   variable only — never a flag, never a file, never `.env` — so it stays out of your shell
   history and the process list.
2. **`--user`**, your MAIN wallet address: the account the orders trade for. The signed action
   carries no user field, so the venue cannot catch a wrong `--user`; the kit itself proves the
   key may trade for that account (its address is `--user`, or an entry in that account's
   `extraAgents`) and refuses to arm otherwise.
3. **A guardrail config** (`--config`). The caps run *before* the signature exists.

Mainnet on top of those four: `HL_ALLOW_MAINNET=1`. The default base URL IS mainnet, so without
that variable the kit refuses to sign and exits. `--testnet` needs nothing extra.

```sh
# testnet first, tiny size, throwaway key
export HL_AGENT_KEY=0x...            # your API wallet key (sign-only)
./hl-agent --live --testnet --user 0xYourMainWallet \
    --config my-guardrail.toml
# mainnet, only after the testnet run behaved: DROP --testnet and set HL_ALLOW_MAINNET=1.
# There is no --mainnet flag on the server: mainnet is the default base URL, gated by that
# env var so it cannot happen by accident.
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
# The main wallet key controls ALL funds in that account and can withdraw. It is not the
# sign-only agent key, and it must not be typed on a command line (argv lands in your shell
# history and in the process list). Read it into the environment without echoing it:
read -rs HL_MAIN_KEY && export HL_MAIN_KEY

# Use the account that actually trades: the venue refuses this action for an address it has
# never seen (see the limitation below), so a brand-new throwaway wallet will not work.
# This is the binary itself: no cargo, no checkout. It needs a build that has the flag
# (--branch main above; the v0.2.0 archive does not).
./hl-agent --approve-builder-fee --fee-tenths-bp 20

# MAINNET is the default base URL, so the line above IS mainnet unless you add --testnet;
# HL_ALLOW_MAINNET=1 is required there (only 1, true, yes or on count). It signs ONE
# approveBuilderFee action, then re-reads maxBuilderFee from the venue and prints the
# approved cap when the approval is active.
# From a checkout, `cargo run -p hl-agent -- --approve-builder-fee` is the same thing.
```

The venue is the judge, not this kit: it re-reads your approval on every order before a builder
field is attached, so an approval you later revoke stops the fee immediately.
Known limitation: on 2026-11-10 this refused a never-used throwaway wallet with "Multi-sig
required". If your own funded account returns that too, the venue is not taking a single-key
approval from you and this command cannot help; use a route the venue accepts. If you would rather not hand this binary your main key, the official Python SDK ships
the same action (hyperliquid-python-sdk `examples/basic_builder_fee.py`), and anything that lands
an `ApproveBuilderFee` for 0x5a229a20D9636a07583D6868FABf4B2903271Ab7 works — the kit reads the
approval from the venue, never from a file.

`--approve-builder-fee` signs the one `approveBuilderFee` action, then checks it landed by
querying `maxBuilderFee(your_address, builder)` — it prints the approved cap when
the check passes, and exits non-zero with the venue's own words when it does not. No other
signature from you is ever needed. Without the approval nothing
breaks: your orders still go, they just carry no builder field, so you pay the venue alone.
