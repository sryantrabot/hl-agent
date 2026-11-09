# "User or API Wallet 0x… does not exist" — what it means and how to clear it

This error comes from the Hyperliquid venue, not from your MCP server or client library. It means
the address the venue recovered from your signature is not an agent approved for the account you
are trading: either the signer really is not approved for that master account, or the venue hashed
a different action than the one you signed (case 2).

Two causes are documented in public reports. The fastest split is the address in the message: the
same address on every attempt points at case 1, a different address on every attempt at case 2.
Section 3 is a look-alike to rule out, and section 4 covers what does not help.

## 1. The agent was approved in the web dashboard, but the tool signs with something else

Approving an agent in the Hyperliquid web dashboard registers its public address on the master
account (visible via `extraAgents`), but the dashboard keeps the private key in the browser session
and never shows it. A local tool that needs the key therefore signs with a different key, and the
venue reports the address it recovered as unknown.[^condor]

Fix: create the API wallet at `app.hyperliquid.xyz/API`, which displays the private key once, and
give that key to the tool (in hl-agent: the `HL_AGENT_KEY` environment variable, never a flag or a
file; the key is only read when you start with `--live` — the dry-run simulator is the default).[^readme]

Then check the pair before trading: `info {"type":"userRole","user":<signer>}` returns
`role: "agent"` only when the signer is approved, and `extraAgents` on the master address lists the
approved agents.

## 2. A mixed-case builder address silently changes the action hash

The error names a *different* address on every request when this happens. The signed action carries
the builder address, so if the address in the payload is not lowercase, the hash the venue verifies
differs from the hash that was signed and signature recovery yields a garbage address. ccxt traced a
run of exactly this to `safe_string_lower` no longer lowercasing a default builder address.[^ccxt]

Fix: upgrade to ccxt 4.5.82 or later, which lowercases the default builder address; on older
versions, pass the builder in lowercase yourself:
`options: {"builder": "0x6530512a6c89c7cfcebc3ba7fcd9ada5f30827a6"}`. If the error address changes
between attempts, look at the builder field first — not at your key.

## 3. Look-alike: the agent address is not the master address

The agent is the signer; the account holding the balance is the master. Querying balances or
positions for the agent address returns an empty result, which reads like "this wallet does not
exist" — but that is an empty `/info` response, not the venue's order-submit error, so it is a
diagnostic detour rather than a cause.

Fix: keep two values apart — the master address you trade for (`--user` in hl-agent) and the agent
key, which is only used when you opt in with `--live` (`HL_AGENT_KEY`).

## 4. There is no retry that fixes it

Re-sending the same order does not fix either case. A blind replay is not safe: the venue accepts
only nonces inside its window, so "send it again" means signing a new action — read the order's
status on the venue before you resend.

A client should surface the venue's text and stop; retrying is what turns this into a silent loop.

## Related errors, so you do not chase the wrong one

- `Order must have minimum value of $10.` — a size problem, not a permissions problem.
- `Too many cumulative requests` — a rate limit; wait, do not rotate keys.
- `Ref already set` — the venue's reply to a redundant `set_ref` call made during order submit, not
  the reason an order failed. ccxt tracks this in #30794 (closed without a code fix); you can skip
  the call with `options: {"refSet": True}` at exchange creation.[^ccxt2]

[^condor]: hummingbot/condor#164, "Confusing UX: Hyperliquid agent (API) wallet approved in web dashboard can't be used by condor — orders fail with cryptic 'User or API Wallet does not exist'" (public issue, still open, opened 2026-07-23): https://github.com/hummingbot/condor/issues/164
[^ccxt]: ccxt/ccxt#30599, "[hyperliquid] Orders fail with random 'User or API Wallet … does not exist' when builder is attached (mixed-case builder address)" (closed 2026-09-23; fixed in 4.5.82): https://github.com/ccxt/ccxt/issues/30599
[^ccxt2]: ccxt/ccxt#30794, "[hyperliquid] options should be True by default: refSet, approvedBuilderFee": https://github.com/ccxt/ccxt/issues/30794
[^readme]: sryantrabot/hl-agent README, "Placing real orders (`--live`)": https://github.com/sryantrabot/hl-agent#placing-real-orders---live
