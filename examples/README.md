# @mppsol/cpi examples

Helpers that exercise the on-chain programs deployed on devnet.

| File | What it does |
| --- | --- |
| `open-session.ts` | Opens a new session PDA on devnet, escrows USDC, generates an off-chain Ed25519 keypair for the agent to sign debits with. |

## open-session.ts

### Setup

1. Get a devnet wallet funded with SOL:
   ```sh
   solana airdrop 2 --url devnet
   ```
2. Get devnet USDC. Faucets:
   - https://faucet.circle.com (USDC, supports Solana devnet)
3. Decide who the server is. For local testing, set
   `MPP_SERVER_PUBKEY` to your own wallet (your wallet plays both
   roles). For real testing, set it to the server operator's wallet
   pubkey.

### Run

```sh
export MPP_SERVER_PUBKEY=<server-wallet-pubkey>
bun run examples/open-session.ts
# or
npx tsx examples/open-session.ts
```

### Output

```
Opening session...
  owner: <your wallet>
  server: <server pubkey>
  authorized_signer: <ed25519 pubkey>
  session PDA: <session pda>
  cap: 1000000 atomic units (1 USDC)
  expiry: 2026-05-05T...

✓ Session opened.
  tx: https://explorer.solana.com/tx/.../?cluster=devnet
  authorized_signer key written to: ./authorized-signer.json

Use these env vars in pay-session.ts:
  export MPPSOL_SESSION=...
  export SIGNER=./authorized-signer.json
```

The generated `authorized-signer.json` is a 32-byte raw Ed25519 private
key as a JSON array. Move it to a secure location; `pay-session.ts`
reads it to sign debit messages.

### What you can do next

- Run `pay-session.ts` from `mppsol-agent/examples/` to settle off-chain
  debits against this session.
- Inspect the session on-chain at the printed PDA address via
  `https://explorer.solana.com/address/<session-pda>?cluster=devnet`.
- Top up the session: see `mppsol_session::topup` in the program IDL.
- Revoke (owner can): `mppsol_session::revoke`.

### Tunables (env vars)

| Var | Default | Notes |
| --- | --- | --- |
| `WALLET` | `~/.config/solana/id.json` | Owner keypair path |
| `SOLANA_RPC` | `https://api.devnet.solana.com` | RPC URL |
| `MPP_SERVER_PUBKEY` | (required) | The server's wallet pubkey |
| `TOTAL_CAP_USDC_ATOMIC` | `1000000` (1 USDC) | Cap in atomic units (6 decimals) |
| `EXPIRY_SECS` | `3600` (1 hour) | Session expiry from now |
| `SIGNER_OUT` | `./authorized-signer.json` | Where to write the generated private key |
