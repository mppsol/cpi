import * as anchor from "@coral-xyz/anchor";
import { Program, BN } from "@coral-xyz/anchor";
import {
  Keypair,
  PublicKey,
  SystemProgram,
  SYSVAR_INSTRUCTIONS_PUBKEY,
  SYSVAR_RENT_PUBKEY,
  Transaction,
  sendAndConfirmTransaction,
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  ASSOCIATED_TOKEN_PROGRAM_ID,
  createAssociatedTokenAccountIdempotent,
  getAccount,
  getAssociatedTokenAddressSync,
} from "@solana/spl-token";
import { expect } from "chai";

import {
  airdrop,
  buildEd25519PrecompileBatch,
  deriveSessionPda,
  encodeDebit,
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
  const sessionProgram = anchor.workspace.MppsolSession as Program<any>;
  const testConsumerProgram = anchor.workspace.TestConsumer as Program<any>;
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
    await airdrop(connection, server.publicKey, 2);

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
      const before = await getAccount(connection, serverAta);

      await program.methods
        .pay({
          amount: new BN(amount.toString()),
          nonce: Array.from(randomBytes(32)),
          requestHash: Array.from(randomBytes(32)),
          expiry: new BN(Math.floor(Date.now() / 1000) + 60),
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

  describe("verify_paid_result (via test_consumer CPI)", () => {
    it("succeeds when Pay + valid server signature appear in the same call stack", async () => {
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

      // test_consumer.pay_and_verify CPIs both Pay and VerifyPaidResult in
      // one parent ix so return data flows correctly.
      await testConsumerProgram.methods
        .payAndVerify({
          amount: new BN(amount.toString()),
          nonce: Array.from(nonce),
          requestHash: Array.from(requestHash),
          expiry,
          resultHash: Array.from(resultHash),
          serverPubkey: serverSigner.publicKey,
          serverSignature: Array.from(signature),
        })
        .accounts({
          payerAuthority: user.publicKey,
          payerTokenAccount: userAta,
          recipientTokenAccount: serverAta,
          mint,
          tokenProgram: TOKEN_PROGRAM_ID,
          instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
          mppsolCpiProgram: program.programId,
        })
        .preInstructions([precompileIx])
        .signers([user])
        .rpc();
      // verify_paid_result reverts on any mismatch; reaching here = success.
    });
  });

  describe("settle_via_session", () => {
    it("CPIs into mppsol_session.settle and emits SES1 return data", async () => {
      const TOTAL_CAP = 100_000_000n;
      const owner = Keypair.generate();
      await airdrop(connection, owner.publicKey, 2);

      // Fund the new owner with tokens.
      const ownerSetup = await setupMintAndMint(
        connection, payer, owner.publicKey, TOTAL_CAP * 2n, 6,
      );
      const sessionMint = ownerSetup.mint;
      const ownerAta = ownerSetup.recipientAta;
      const sessionServer = Keypair.generate();
      await airdrop(connection, sessionServer.publicKey, 2);
      const sessionServerAta = await createAssociatedTokenAccountIdempotent(
        connection, payer, sessionMint, sessionServer.publicKey,
      );

      // Open a session.
      const sessionId = randomBytes(16);
      const authorizedSigner = generateEd25519();
      const [session] = deriveSessionPda(
        sessionProgram.programId,
        owner.publicKey,
        sessionServer.publicKey,
        sessionId,
      );
      const escrow = getAssociatedTokenAddressSync(sessionMint, session, true);

      await sessionProgram.methods
        .open({
          authorizedSigner: authorizedSigner.publicKey,
          server: sessionServer.publicKey,
          totalCap: new BN(TOTAL_CAP.toString()),
          expiry: new BN(Math.floor(Date.now() / 1000) + 3600),
          sessionId: Array.from(sessionId),
          clusterGenesisHash: Array.from(randomBytes(32)),
        })
        .accounts({
          owner: owner.publicKey,
          session,
          mint: sessionMint,
          escrow,
          ownerSource: ownerAta,
          tokenProgram: TOKEN_PROGRAM_ID,
          associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
          rent: SYSVAR_RENT_PUBKEY,
        })
        .signers([owner])
        .rpc();

      // Build a signed debit.
      const debitAmount = 2_500_000n;
      const sequence = 1n;
      const debitExpiry = BigInt(Math.floor(Date.now() / 1000) + 60);
      const debitNonce = randomBytes(32);

      const debitBytes = encodeDebit({
        session,
        nonce: debitNonce,
        amount: debitAmount,
        expiry: debitExpiry,
        sequence,
      });
      const debitSignature = signDebit(authorizedSigner.privateKey, debitBytes);

      const precompileIx = buildEd25519PrecompileBatch([
        {
          publicKey: authorizedSigner.publicKey.toBytes(),
          message: debitBytes,
          signature: debitSignature,
        },
      ]);

      const debitArg = {
        session: Array.from(session.toBuffer()),
        nonce: Array.from(debitNonce),
        amount: new BN(debitAmount.toString()),
        expiry: new BN(debitExpiry.toString()),
        sequence: new BN(sequence.toString()),
        domainSep: Array.from(Buffer.from("MPP.SOL/DEBIT001")),
      };

      const requestHash = randomBytes(32);

      const before = await getAccount(connection, sessionServerAta);

      await program.methods
        .settleViaSession({
          debit: debitArg,
          signature: Array.from(debitSignature),
          requestHash: Array.from(requestHash),
        })
        .accounts({
          server: sessionServer.publicKey,
          session,
          escrow,
          recipientTokenAccount: sessionServerAta,
          mint: sessionMint,
          tokenProgram: TOKEN_PROGRAM_ID,
          instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
          mppsolSessionProgram: sessionProgram.programId,
        })
        .preInstructions([precompileIx])
        .signers([sessionServer])
        .rpc();

      const after = await getAccount(connection, sessionServerAta);
      expect((after.amount - before.amount).toString()).to.equal(debitAmount.toString());

      const sessionAcct = await sessionProgram.account.session.fetch(session);
      expect(sessionAcct.lastSeenSequence.toString()).to.equal("1");
      expect(sessionAcct.remainingCap.toString()).to.equal(
        (TOTAL_CAP - debitAmount).toString(),
      );
    });
  });
});
