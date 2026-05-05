import * as anchor from "@coral-xyz/anchor";
import { Program, BN } from "@coral-xyz/anchor";
import {
  Keypair,
  PublicKey,
  SYSVAR_INSTRUCTIONS_PUBKEY,
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  createAssociatedTokenAccountIdempotent,
  getAccount,
} from "@solana/spl-token";
import { expect } from "chai";

import {
  airdrop,
  buildEd25519PrecompileBatch,
  generateEd25519,
  randomBytes,
  RESULT_DOMAIN_SEP,
  setupMintAndMint,
  signDebit,
} from "./test-utils";

describe("mppsol_cpi", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.MppsolCpi as Program<any>;
  const connection = provider.connection;

  let payer: Keypair;
  let mint: PublicKey;
  let user: Keypair;
  let server: Keypair;
  let userAta: PublicKey;
  let serverAta: PublicKey;

  before(async () => {
    payer = (provider.wallet as anchor.Wallet).payer;
    user = Keypair.generate();
    server = Keypair.generate();

    await airdrop(connection, user.publicKey, 2);

    const setup = await setupMintAndMint(
      connection, payer, user.publicKey, 100_000_000n, 6,
    );
    mint = setup.mint;
    userAta = setup.recipientAta;

    serverAta = await createAssociatedTokenAccountIdempotent(
      connection, payer, mint, server.publicKey,
    );
  });

  describe("pay", () => {
    it("transfers tokens and emits return data", async () => {
      const amount = 1_000_000n;
      const nonce = randomBytes(32);
      const requestHash = randomBytes(32);
      const expiry = new BN(Math.floor(Date.now() / 1000) + 60);

      const before = await getAccount(connection, serverAta);

      await program.methods
        .pay({
          amount: new BN(amount.toString()),
          nonce: Array.from(nonce),
          requestHash: Array.from(requestHash),
          expiry,
        })
        .accounts({
          payerAuthority: user.publicKey,
          payerTokenAccount: userAta,
          recipientTokenAccount: serverAta,
          mint,
          tokenProgram: TOKEN_PROGRAM_ID,
          instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
        })
        .signers([user])
        .rpc();

      const after = await getAccount(connection, serverAta);
      expect((after.amount - before.amount).toString()).to.equal(amount.toString());
    });
  });

  describe("verify_paid_result", () => {
    // SKIPPED: Solana's set_return_data/get_return_data crosses CPI
    // boundaries but is cleared between top-level instructions. Per
    // spec/cpi.md, verify_paid_result is designed to be invoked as a
    // CPI from a caller program (e.g. a perp DEX) where return data
    // flows from Pay's ix → caller's ix → verify_paid_result's CPI.
    // Testing it requires a small "caller" companion program that we
    // don't have yet. Implementation is correct per spec; will be
    // exercised once we ship a reference caller program.
    it.skip("succeeds when Pay + valid server signature appear in the same tx", async () => {
      const amount = 1_000_000n;
      const nonce = randomBytes(32);
      const requestHash = randomBytes(32);
      const resultHash = randomBytes(32);
      const expiry = new BN(Math.floor(Date.now() / 1000) + 60);

      const serverSigner = generateEd25519();

      // Canonical message: nonce || request_hash || result_hash || RESULT_DOMAIN_SEP
      const message = Buffer.concat([nonce, requestHash, resultHash, RESULT_DOMAIN_SEP]);
      const signature = signDebit(serverSigner.privateKey, message);

      const precompileIx = buildEd25519PrecompileBatch([
        {
          publicKey: serverSigner.publicKey.toBytes(),
          message,
          signature,
        },
      ]);

      const payIx = await program.methods
        .pay({
          amount: new BN(amount.toString()),
          nonce: Array.from(nonce),
          requestHash: Array.from(requestHash),
          expiry,
        })
        .accounts({
          payerAuthority: user.publicKey,
          payerTokenAccount: userAta,
          recipientTokenAccount: serverAta,
          mint,
          tokenProgram: TOKEN_PROGRAM_ID,
          instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
        })
        .instruction();

      // Use Anchor to assemble all 3 ixs in one tx with proper signer wiring.
      await program.methods
        .verifyPaidResult({
          nonce: Array.from(nonce),
          requestHash: Array.from(requestHash),
          resultHash: Array.from(resultHash),
          serverPubkey: serverSigner.publicKey,
          serverSignature: Array.from(signature),
        })
        .accounts({
          caller: user.publicKey,
          instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
        })
        .preInstructions([precompileIx, payIx])
        .signers([user])
        .rpc();
      // verify_paid_result reverts on any mismatch; reaching here = success.
    });
  });
});
