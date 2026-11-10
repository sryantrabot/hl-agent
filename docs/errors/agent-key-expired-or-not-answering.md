# "My agent key stopped working" — read the approval list before approving another

An agent (API) wallet can sign for an account until its approval expires, is revoked, or is
deregistered; a tool that signs with a different address than the approved one fails for a separate
reason. When orders stop going through after a period of working, the first question is not "is my key
wrong" but "is that exact address still approved for this account right now". The venue answers that
from public state.

## 1. Read the approval list instead of inferring it

```json
{"type": "extraAgents", "user": "<your master/main wallet address>"}
```

POST that to the `/info` endpoint of the same network your tool trades on — mainnet
`https://api.hyperliquid.xyz/info`, testnet `https://api.hyperliquid-testnet.xyz/info`. Each entry is
an approved agent with three fields: `address`, `name`, and `validUntil`, a timestamp in milliseconds
— `null` means that agent has **no expiry**.[^info]

Query the master account, not the agent: the venue's docs name that as the common pitfall, because the
agent is the signer and querying it returns an empty result.[^docs]

Read two things: does the address your tool signs with appear at all, and if it does, is its
`validUntil` in the future or `null`. A past `validUntil` means the approval has expired, though the
venue may prune the entry, so an expired agent can also be absent. A missing entry means the address
is not approved for the account you queried: revoked, expired and pruned, pruned because the account
no longer holds funds, or never landed there.[^docs]

## 2. Who replaces whom

A new approval deregisters an earlier agent when the addresses collide by name: a new unnamed approval
deregisters the existing unnamed one, and a named approval deregisters an existing agent with the same
name. A differently-named approval leaves the earlier agents approved.[^docs]

So if an approval looked like it failed and you sent the same approval again — unnamed both times, or
the same name — the second, if it landed, replaced the first rather than adding to it. And "I
re-approved it and it is still broken" can mean the tool signs with a different address than the one
you just approved: compare the address in the rejection message with the `address` values from step 1,
and check the tool's account and network settings too.

## 3. Set the expiry in the name

The approval action's `agentName` has a 16-character base-name limit, and the venue sets a custom
expiration when you append ` valid_until {timestamp}` after it — at most 180 days ahead. An account can
hold one unnamed approved wallet and up to three named ones, plus two named agents per
subaccount.[^approve] The `@nktkas/hyperliquid` SDK's validator ignores that suffix when counting the
16 characters.[^sdk]

## 4. After the check

- **Address present with `validUntil` in the future or `null`:** the approval is live, so the failure
  is elsewhere — the wrong account in the tool's config, a signature or action-hash mismatch, or the
  wrong network (testnet approval against the mainnet endpoint, or the reverse).
- **Address present with `validUntil` past, or address absent:** generate a fresh API wallet and
  approve that new address from the main wallet. The venue advises against reusing an agent address
  once its approval is gone, because its nonce state may be pruned and previously signed actions
  replayed. Then re-read step 1 rather than trusting the approval call's own response.[^docs]
- **Agents live that you no longer use:** each approved agent signs independently, but the slots are
  limited, so stale agents can block a new named approval. Read the list first, then revoke the ones
  you no longer use from the account's API page rather than deleting key files and hoping.[^docs]

## What this means inside hl-agent

Before arming the live path, the kit checks that the key's address is `--user` or appears in that
account's `extraAgents`, and otherwise refuses — with a pointer to `app.hyperliquid.xyz/API` when the
address is not listed.[^kit] The kit reads only `address` and never compares `validUntil` with the
clock, so an approved-but-expired agent passes that check and step 1 is still yours to run.

[^info]: `extraAgents` request and response shape, including `validUntil` as "validity period as a timestamp (in ms since epoch); `null` when the agent has no expiry": https://github.com/nktkas/hyperliquid/blob/master/src/api/info/_methods/extraAgents.ts (read 2026-11-10).
[^docs]: Hyperliquid docs, "API wallet pruning" (an unnamed approval deregisters the existing unnamed wallet; a named approval deregisters an existing wallet with a matching name; a wallet is pruned when it expires or when the account that registered it no longer has funds; do not reuse an agent address after deregistration because its nonce state may be pruned), the "common pitfall" note above it, and "Approve an API wallet" (one unnamed plus up to three named per account, two named per subaccount; ` valid_until {timestamp}` sets a custom expiration of at most 180 days): https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/nonces-and-api-wallets and https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/exchange-endpoint#approve-an-api-wallet (read 2026-11-10).
[^approve]: Same "Approve an API wallet" section as [^docs], for the slot limits and the 180-day cap.
[^sdk]: The validator's trailing-` valid_until` handling: https://github.com/nktkas/hyperliquid/blob/master/src/api/exchange/_methods/approveAgent.ts (read 2026-11-10).
[^kit]: `src/main.rs` arming check and refusal message; `src/hl.rs` `extra_agents` reads each entry's `address`: https://github.com/sryantrabot/hl-agent/blob/main/src/main.rs and https://github.com/sryantrabot/hl-agent/blob/main/src/hl.rs (read 2026-11-10).
