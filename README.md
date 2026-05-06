# @mppsol/cpi

Solana on-chain programs for [MPP.sol](https://mppsol.org). This Anchor
workspace contains two programs deployed together:

| Program | Role |
| --- | --- |
| **`mppsol_session`** | Stateful escrow + off-chain debit settlement. See [`spec/session.md`](https://github.com/mppsol/spec/blob/main/spec/session.md). |
| **`mppsol_cpi`** | CPI-callable wrappers for atomic pay-and-consume composition from other Solana programs. See [`spec/cpi.md`](https://github.com/mppsol/spec/blob/main/spec/cpi.md). |

This is the differentiating piece of MPP.sol versus all other MPP
adapters: **MPP becomes an on-chain composable primitive.** No EVM-based
MPP adapter (Tempo included) can match it because Solana's atomic
multi-instruction tx model and Ed25519-precompile pattern make
off-chain-signed message verification cheap on-chain.

## Status

**v0.1.1 draft. Both programs deployed to Solana devnet.** Anchor test
suite (12 passing) validates the Ed25519 settle path end-to-end plus
all 7 cpi instructions including the v0.1.1 Receipt-PDA variants.
Audit required before mainnet.

### Deployed program IDs (devnet)

| Program | Program ID |
| --- | --- |
| `mppsol_session` | [`B7joeuXqPJSCTfUfMacHaWL6eseoDinV7Jxt52gVdfbi`](https://explorer.solana.com/address/B7joeuXqPJSCTfUfMacHaWL6eseoDinV7Jxt52gVdfbi?cluster=devnet) |
| `mppsol_cpi` | [`624xoctSeGzq1TAVwZU1xbM9RozAd3xZmjPeFXrAY14j`](https://explorer.solana.com/address/624xoctSeGzq1TAVwZU1xbM9RozAd3xZmjPeFXrAY14j?cluster=devnet) |
| `test_consumer` (test-only) | [`65ndFCiYYM3tznTg5Te1x8ALfVP7SxFEwvvUeANYy3Ex`](https://explorer.solana.com/address/65ndFCiYYM3tznTg5Te1x8ALfVP7SxFEwvvUeANYy3Ex?cluster=devnet) |

IDLs are uploaded on-chain — fetch via `Program.fetchIdl(programId, provider)`.

### Build artifacts

```
target/deploy/
├── mppsol_session.so   ~324 KB
└── mppsol_cpi.so       ~261 KB
```

What's implemented in v0.1 source:

| Instruction | Status |
| --- | --- |
| `mppsol_session::open` | ✅ Full (PDA init, escrow ATA init, token transfer) |
| `mppsol_session::topup` | ✅ Full |
| `mppsol_session::revoke` | ✅ Full (owner or server) |
| `mppsol_session::settle` | ✅ Full (Ed25519 precompile batch verify + transfer + state update) |
| `mppsol_session::close` | ✅ Full (drain escrow → owner_destination, close ATA, close PDA) |
| `mppsol_cpi::pay` | ✅ Full (transfer + log + return data) |
| `mppsol_cpi::verify_paid_result` | ✅ Full (Ed25519 result-hash verify; off-chain nonce-binding flow — for atomic on-chain binding use `verify_paid_result_with_receipt` below) |
| `mppsol_cpi::get_receipt` | ✅ Full (return-data assertion + re-emit, same call stack only) |
| `mppsol_cpi::settle_via_session` | ✅ Full (CPI to `mppsol_session::settle` + SES1 return data) |
| `mppsol_cpi::pay_with_receipt` | ✅ **v0.1.1** — Pay + writes a Receipt PDA (atomic on-chain payment-binding, persists across CPIs and tx boundaries) |
| `mppsol_cpi::verify_paid_result_with_receipt` | ✅ **v0.1.1** — Ed25519 verify + on-chain Receipt PDA lookup (replaces v0.2 design — shipped early) |
| `mppsol_cpi::claim_receipt` | ✅ **v0.1.1** — payer reclaims rent from a consumed Receipt |

All 12 instructions are implemented. Anchor test suite: 7/7 passing on
localnet. Audit required before mainnet.

#### v0.1 verify_paid_result simplification

The original `cpi.md` spec described `verify_paid_result` as also
checking that a prior Pay/SettleViaSession set return data with a
matching nonce. **This doesn't work in Solana**: the runtime clears
return data at the start of every program invocation (including CPIs),
so even a parent program calling Pay then verify_paid_result via
back-to-back CPIs sees empty return data inside verify_paid_result.

For v0.1, `verify_paid_result` only checks the Ed25519 server signature
on the canonical result message. The on-chain payment-binding guarantee
is replaced by an off-chain one: **servers only sign result hashes for
nonces they issued challenges for**, so possession of a valid `(nonce,
signed_result)` pair implies payment was made off-chain.

**v0.1.1 (shipped early — was v0.2):** `pay_with_receipt` writes a
rent-bearing Receipt PDA (keyed by `payer + nonce`) that persists
across CPIs and tx boundaries. `verify_paid_result_with_receipt` looks
it up by nonce for true on-chain payment-binding atomicity, and
`claim_receipt` lets the payer reclaim rent once the receipt is
consumed. See `spec/cpi.md` §6 for the design.

## Architecture

```
                       ┌────────────────────┐
   off-chain signer ──▶│ debit message      │──┐
                       │ (104 bytes, signed)│  │
                       └────────────────────┘  │
                                               ▼
caller program ──CPI──▶ mppsol_cpi ──CPI──▶ mppsol_session
                            │                    │
                            ├─ Pay  ─────────────┤
                            │  (writes return    │
                            │   data: PAY1...)   │
                            │                    │
                            ├─ SettleViaSession ─┤
                            │  (writes return    ├─ Settle (escrow → server)
                            │   data: SES1...)   │
                            │                    │
                            └─ VerifyPaidResult ─┘
                               (reads return data
                                + Ed25519 precompile)
```

## Build

Requires:
- Solana CLI 2.2+
- Anchor CLI 0.32.1

```sh
# Build BPF binaries
anchor build

# Run the test suite (TODO: tests)
anchor test
```

Program keypairs are committed under `target/deploy/`. Program IDs are
already embedded in source and `Anchor.toml`. To regenerate:

```sh
solana-keygen new -o target/deploy/mppsol_session-keypair.json --force
solana-keygen new -o target/deploy/mppsol_cpi-keypair.json --force
anchor keys sync
```

### Toolchain notes (resolved)

Earlier (May 2026) versions of this README claimed an upstream blocker
on Solana platform-tools v1.49+. That was a misdiagnosis — v1.49 had
shipped almost a year prior (June 2025), and Solana CLI 2.2.x simply
bundled the older v1.48. **Upgrading to Solana CLI 3.1.14+ (which
bundles platform-tools v1.52, rustc 1.89) resolves the build.** Plus
adding `bs58 = "0.5"` as a direct dep in `mppsol-cpi/Cargo.toml`.

```sh
agave-install init 3.1.14
anchor build  # ✓ succeeds, produces both .so files
```

## Domain separators

These are bound into Ed25519-signed messages on-chain to prevent
cross-context signature reuse. They MUST exactly match
[`@mppsol/core`](https://github.com/mppsol/core)'s constants:

| Constant | Bytes |
| --- | --- |
| `DEBIT_DOMAIN_SEP` | `MPP.SOL/DEBIT001` (16 bytes) |
| `RESULT_DOMAIN_SEP` | `MPP.SOL/RESULT01` (16 bytes) |

## CPI return data

`mppsol_cpi::pay` writes a 140-byte structured return data block. Other
programs in the same tx read it via `get_return_data` to verify a
payment occurred:

```
discriminator: [u8; 4]   "PAY1" or "SES1"
nonce:         [u8; 32]
request_hash:  [u8; 32]
amount:        u64       (little-endian)
recipient:     [u8; 32]
mint:          [u8; 32]
slot:          u64       (little-endian)
```

Total: 4 + 32 + 32 + 8 + 32 + 32 + 8 = **148 bytes**. Constant in source
is 140 because the version reserved 8 bytes for an optional flag — to be
finalized at v0.1.1.

## Security

- **Three-key model** (owner / authorized_signer / server) is enforced
  on-chain via `constraint =` checks on each context struct.
- **Cluster confusion** is mitigated by storing
  `cluster_genesis_hash` on each `Session` PDA at `Open` time.
- **Replay** is prevented by `last_seen_sequence` on the session and
  the server's nonce store off-chain (per
  [`spec/wire.md` §6](https://github.com/mppsol/spec/blob/main/spec/wire.md#6-server-side-verification-rules)).
- **Recipient redirection** is impossible for `SettleViaSession` because
  the recipient is fixed at session `Open` and the inner CPI to
  `mppsol_session::settle` validates it.
- Programs target `overflow-checks = true` in release.

A formal audit by a Solana-experienced firm is **required** before
mainnet deployment. See
[`spec/security.md` §12](https://github.com/mppsol/spec/blob/main/spec/security.md#12-audit-and-conformance).

## TypeScript bindings

The `package.json` here reserves the `@mppsol/cpi` npm scope for the
generated IDL bindings. Once `anchor build` succeeds and IDL JSON is
emitted, `ts/` will hold `@coral-xyz/anchor`-style instruction builders
and account decoders. Not present in v0.1.

## Examples

See [`examples/open-session.ts`](./examples/open-session.ts) for a
runnable script that opens a session on devnet and writes the
generated authorized-signer key to disk.

## License

Apache-2.0. Maintained by [psyto](https://github.com/psyto).
