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

**v0.1 draft. Not yet deployed. Not yet audited.**

What's implemented in v0.1:

| Instruction | Status |
| --- | --- |
| `mppsol_session::open` | Full implementation (PDA init, escrow ATA init, token transfer) |
| `mppsol_session::topup` | Full implementation |
| `mppsol_session::revoke` | Full implementation (owner or server) |
| `mppsol_session::settle` | Skeleton — Ed25519 precompile binding deferred to v0.1.1 |
| `mppsol_session::close` | Skeleton — escrow drain logic deferred to v0.1.1 |
| `mppsol_cpi::pay` | Full implementation (transfer + log + return data) |
| `mppsol_cpi::settle_via_session` | Skeleton — pending `mppsol_session::settle` |
| `mppsol_cpi::verify_paid_result` | Skeleton — sysvar:instructions parsing deferred |
| `mppsol_cpi::get_receipt` | Skeleton |

The `Session` and `Pay` flows are real and reviewable; the verification
paths are explicitly stubbed and return `MissingPrecompile` /
`ReceiptNotFound` errors so consumers can't accidentally rely on them
yet.

## Architecture

```
                       ┌────────────────────┐
   off-chain signer ──▶│ debit message      │──┐
                       │ (122 bytes, signed)│  │
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

### Known toolchain issue (May 2026)

As of Solana platform-tools v1.48 (rustc 1.84.1, cargo 1.84.0), the
transitive dependency `constant_time_eq 0.4.2` requires the unstable
`edition2024` cargo feature (stabilized in cargo 1.85). The build will
fail with:

```
error: feature `edition2024` is required
  Caused by: parse manifest at constant_time_eq-0.4.2/Cargo.toml
```

This affects many Solana/Anchor projects in this window and is not
specific to MPP.sol. Workarounds:

1. Wait for Solana to bump platform-tools to v1.49+ (rustc 1.85+).
2. Use Anchor 0.30.1 + Solana 1.18 (older, but stable combination).
3. Pin `constant_time_eq = "=0.4.1"` via a git patch in workspace
   `Cargo.toml` (SemVer-compatible patches require git source).

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

## License

Apache-2.0. Maintained by [psyto](https://github.com/psyto).
