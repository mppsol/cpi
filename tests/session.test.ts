import * as anchor from "@coral-xyz/anchor";
import { Program, BN } from "@coral-xyz/anchor";
import {
  Keypair,
  PublicKey,
  SystemProgram,
  SYSVAR_RENT_PUBKEY,
  SYSVAR_INSTRUCTIONS_PUBKEY,
  Transaction,
  sendAndConfirmTransaction,
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  ASSOCIATED_TOKEN_PROGRAM_ID,
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
  setupMintAndMint,
  signDebit,
} from "./test-utils";

describe("mppsol_session", () => {
  // Anchor wires up provider + workspace from Anchor.toml.
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.MppsolSession as Program<any>;
  const connection = provider.connection;

  let payer: Keypair;
  let mint: PublicKey;
  let owner: Keypair;
  let server: Keypair;
  let ownerAta: PublicKey;
  let serverAta: PublicKey;

  const DECIMALS = 6;
  const TOTAL_CAP = 100_000_000n; // 100 USDC

  before(async () => {
    payer = (provider.wallet as anchor.Wallet).payer;
    owner = Keypair.generate();
    server = Keypair.generate();

    await airdrop(connection, owner.publicKey, 2);
    await airdrop(connection, server.publicKey, 2);

    // Mint and fund owner with USDC-like tokens.
    const setup = await setupMintAndMint(connection, payer, owner.publicKey, TOTAL_CAP * 10n, DECIMALS);
    mint = setup.mint;
    ownerAta = setup.recipientAta;

    // Server's ATA for receiving settlement transfers.
    serverAta = getAssociatedTokenAddressSync(mint, server.publicKey);
  });

  describe("open + topup + revoke", () => {
    let session: PublicKey;
    let escrow: PublicKey;
    const sessionId = randomBytes(16);
    const authorizedSigner = generateEd25519();
    const expiry = new BN(Math.floor(Date.now() / 1000) + 3600);

    it("opens a session and escrows tokens", async () => {
      const [sessionPda] = deriveSessionPda(
        program.programId,
        owner.publicKey,
        server.publicKey,
        sessionId,
      );
      session = sessionPda;
      escrow = getAssociatedTokenAddressSync(mint, session, true);

      await program.methods
        .open({
          authorizedSigner: authorizedSigner.publicKey,
          server: server.publicKey,
          totalCap: new BN(TOTAL_CAP.toString()),
          expiry,
          sessionId: Array.from(sessionId),
          clusterGenesisHash: Array.from(randomBytes(32)),
        })
        .accounts({
          owner: owner.publicKey,
          session,
          mint,
          escrow,
          ownerSource: ownerAta,
          tokenProgram: TOKEN_PROGRAM_ID,
          associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
          rent: SYSVAR_RENT_PUBKEY,
        })
        .signers([owner])
        .rpc();

      const sessionAcct = await program.account.session.fetch(session);
      expect(sessionAcct.owner.toBase58()).to.equal(owner.publicKey.toBase58());
      expect(sessionAcct.server.toBase58()).to.equal(server.publicKey.toBase58());
      expect(sessionAcct.totalCap.toString()).to.equal(TOTAL_CAP.toString());
      expect(sessionAcct.remainingCap.toString()).to.equal(TOTAL_CAP.toString());
      expect(sessionAcct.lastSeenSequence.toString()).to.equal("0");
      expect(sessionAcct.state).to.equal(0); // Active
    });

    it("tops up the cap", async () => {
      const topupAmount = 50_000_000n;
      await program.methods
        .topup(new BN(topupAmount.toString()))
        .accounts({
          owner: owner.publicKey,
          session,
          mint,
          escrow,
          ownerSource: ownerAta,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([owner])
        .rpc();

      const sessionAcct = await program.account.session.fetch(session);
      expect(sessionAcct.totalCap.toString()).to.equal((TOTAL_CAP + topupAmount).toString());
      expect(sessionAcct.remainingCap.toString()).to.equal((TOTAL_CAP + topupAmount).toString());
    });

    it("can be revoked by the owner", async () => {
      await program.methods
        .revoke()
        .accounts({
          signer: owner.publicKey,
          session,
        })
        .signers([owner])
        .rpc();

      const sessionAcct = await program.account.session.fetch(session);
      expect(sessionAcct.state).to.equal(1); // Revoked
    });
  });

  describe("revoke by server", () => {
    it("allows the server to revoke (server != owner)", async () => {
      const sessionId = randomBytes(16);
      const authorizedSigner = generateEd25519();
      const [session] = deriveSessionPda(
        program.programId,
        owner.publicKey,
        server.publicKey,
        sessionId,
      );
      const escrow = getAssociatedTokenAddressSync(mint, session, true);

      await program.methods
        .open({
          authorizedSigner: authorizedSigner.publicKey,
          server: server.publicKey,
          totalCap: new BN(TOTAL_CAP.toString()),
          expiry: new BN(Math.floor(Date.now() / 1000) + 3600),
          sessionId: Array.from(sessionId),
          clusterGenesisHash: Array.from(randomBytes(32)),
        })
        .accounts({
          owner: owner.publicKey,
          session,
          mint,
          escrow,
          ownerSource: ownerAta,
          tokenProgram: TOKEN_PROGRAM_ID,
          associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
          rent: SYSVAR_RENT_PUBKEY,
        })
        .signers([owner])
        .rpc();

      // Server (not owner) revokes.
      await program.methods
        .revoke()
        .accounts({ signer: server.publicKey, session })
        .signers([server])
        .rpc();

      const sessionAcct = await program.account.session.fetch(session);
      expect(sessionAcct.state).to.equal(1);
    });
  });

  describe("settle (multi-debit batch)", () => {
    it("settles a 2-debit batch atomically", async () => {
      // Ensure server's ATA exists on-chain (outer before only derives the address).
      const { createAssociatedTokenAccountIdempotent } = await import("@solana/spl-token");
      await createAssociatedTokenAccountIdempotent(connection, payer, mint, server.publicKey);

      const sessionId = randomBytes(16);
      const authorizedSigner = generateEd25519();
      const [session] = deriveSessionPda(
        program.programId,
        owner.publicKey,
        server.publicKey,
        sessionId,
      );
      const escrow = getAssociatedTokenAddressSync(mint, session, true);

      await program.methods
        .open({
          authorizedSigner: authorizedSigner.publicKey,
          server: server.publicKey,
          totalCap: new BN(TOTAL_CAP.toString()),
          expiry: new BN(Math.floor(Date.now() / 1000) + 3600),
          sessionId: Array.from(sessionId),
          clusterGenesisHash: Array.from(randomBytes(32)),
        })
        .accounts({
          owner: owner.publicKey,
          session,
          mint,
          escrow,
          ownerSource: ownerAta,
          tokenProgram: TOKEN_PROGRAM_ID,
          associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
          rent: SYSVAR_RENT_PUBKEY,
        })
        .signers([owner])
        .rpc();

      const debitExpiry = BigInt(Math.floor(Date.now() / 1000) + 60);
      // 3+ debits exceed the legacy tx size limit (1232 bytes); use a
      // versioned tx with ALTs for larger batches in production.
      const debits = [1n, 2n].map((seq) => {
        const nonce = randomBytes(32);
        const amount = seq * 100_000n;
        const bytes = encodeDebit({ session, nonce, amount, expiry: debitExpiry, sequence: seq });
        const sig = signDebit(authorizedSigner.privateKey, bytes);
        return { seq, nonce, amount, bytes, sig };
      });

      const precompileIx = buildEd25519PrecompileBatch(
        debits.map((d) => ({
          publicKey: authorizedSigner.publicKey.toBytes(),
          message: d.bytes,
          signature: d.sig,
        })),
      );

      const settleIx = await program.methods
        .settle({
          debits: debits.map((d) => ({
            session: Array.from(session.toBuffer()),
            nonce: Array.from(d.nonce),
            amount: new BN(d.amount.toString()),
            expiry: new BN(debitExpiry.toString()),
            sequence: new BN(d.seq.toString()),
            domainSep: Array.from(Buffer.from("MPP.SOL/DEBIT001")),
          })),
          signatures: debits.map((d) => Array.from(d.sig)),
        })
        .accounts({
          server: server.publicKey,
          session,
          mint,
          escrow,
          serverTokenAccount: serverAta,
          tokenProgram: TOKEN_PROGRAM_ID,
          instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
        })
        .instruction();

      const balBefore = (await import("@solana/spl-token")).getAccount;
      const before = await balBefore(connection, serverAta);

      const tx = new Transaction().add(precompileIx, settleIx);
      await sendAndConfirmTransaction(connection, tx, [server]);

      const after = await (await import("@solana/spl-token")).getAccount(connection, serverAta);
      const cumulative = debits.reduce((acc, d) => acc + d.amount, 0n);
      expect((after.amount - before.amount).toString()).to.equal(cumulative.toString());

      const sessionAcct = await program.account.session.fetch(session);
      expect(sessionAcct.lastSeenSequence.toString()).to.equal("2");
    });
  });

  describe("close", () => {
    it("drains escrow + closes the session PDA after expiry", async () => {
      const sessionId = randomBytes(16);
      const authorizedSigner = generateEd25519();
      const [session] = deriveSessionPda(
        program.programId,
        owner.publicKey,
        server.publicKey,
        sessionId,
      );
      const escrow = getAssociatedTokenAddressSync(mint, session, true);

      // Open with a near-future expiry so we can wait it out.
      const expirySecs = 3;
      await program.methods
        .open({
          authorizedSigner: authorizedSigner.publicKey,
          server: server.publicKey,
          totalCap: new BN(TOTAL_CAP.toString()),
          expiry: new BN(Math.floor(Date.now() / 1000) + expirySecs),
          sessionId: Array.from(sessionId),
          clusterGenesisHash: Array.from(randomBytes(32)),
        })
        .accounts({
          owner: owner.publicKey,
          session,
          mint,
          escrow,
          ownerSource: ownerAta,
          tokenProgram: TOKEN_PROGRAM_ID,
          associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
          rent: SYSVAR_RENT_PUBKEY,
        })
        .signers([owner])
        .rpc();

      const ownerBalBefore = (await (await import("@solana/spl-token")).getAccount(connection, ownerAta)).amount;

      // Wait for expiry to pass.
      await new Promise((r) => setTimeout(r, (expirySecs + 1) * 1000));

      await program.methods
        .close()
        .accounts({
          owner: owner.publicKey,
          session,
          mint,
          escrow,
          ownerDestination: ownerAta,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([owner])
        .rpc();

      // Session PDA should be closed.
      const sessionAcctRaw = await connection.getAccountInfo(session);
      expect(sessionAcctRaw).to.be.null;

      // Escrow contents (TOTAL_CAP) should have returned to owner.
      const ownerBalAfter = (await (await import("@solana/spl-token")).getAccount(connection, ownerAta)).amount;
      expect((ownerBalAfter - ownerBalBefore).toString()).to.equal(TOTAL_CAP.toString());
    });
  });

  describe("settle (Ed25519 precompile + transfer)", () => {
    let session: PublicKey;
    let escrow: PublicKey;
    const sessionId = randomBytes(16);
    const authorizedSigner = generateEd25519();
    const expiry = new BN(Math.floor(Date.now() / 1000) + 3600);

    before(async () => {
      const [sessionPda] = deriveSessionPda(
        program.programId,
        owner.publicKey,
        server.publicKey,
        sessionId,
      );
      session = sessionPda;
      escrow = getAssociatedTokenAddressSync(mint, session, true);

      await program.methods
        .open({
          authorizedSigner: authorizedSigner.publicKey,
          server: server.publicKey,
          totalCap: new BN(TOTAL_CAP.toString()),
          expiry,
          sessionId: Array.from(sessionId),
          clusterGenesisHash: Array.from(randomBytes(32)),
        })
        .accounts({
          owner: owner.publicKey,
          session,
          mint,
          escrow,
          ownerSource: ownerAta,
          tokenProgram: TOKEN_PROGRAM_ID,
          associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
          rent: SYSVAR_RENT_PUBKEY,
        })
        .signers([owner])
        .rpc();

      // Create server's ATA so we can receive transfers.
      const ix = anchor.web3.SystemProgram.transfer({
        fromPubkey: payer.publicKey,
        toPubkey: server.publicKey,
        lamports: 1,
      });
      // Actually let's create the ATA properly:
      const { createAssociatedTokenAccountIdempotent } = await import("@solana/spl-token");
      await createAssociatedTokenAccountIdempotent(connection, payer, mint, server.publicKey);
    });

    it("settles a single signed debit and transfers from escrow", async () => {
      const debitAmount = 1_000_000n; // 1 USDC
      const sequence = 1n;
      const debitExpiry = BigInt(Math.floor(Date.now() / 1000) + 60);
      const nonce = randomBytes(32);

      const debitBytes = encodeDebit({
        session,
        nonce,
        amount: debitAmount,
        expiry: debitExpiry,
        sequence,
      });
      const signature = signDebit(authorizedSigner.privateKey, debitBytes);

      // Build precompile companion ix.
      const precompileIx = buildEd25519PrecompileBatch([
        {
          publicKey: authorizedSigner.publicKey.toBytes(),
          message: debitBytes,
          signature,
        },
      ]);

      const settleIx = await program.methods
        .settle({
          debits: [
            {
              session: Array.from(session.toBuffer()),
              nonce: Array.from(nonce),
              amount: new BN(debitAmount.toString()),
              expiry: new BN(debitExpiry.toString()),
              sequence: new BN(sequence.toString()),
              domainSep: Array.from(Buffer.from("MPP.SOL/DEBIT001")),
            },
          ],
          signatures: [Array.from(signature)],
        })
        .accounts({
          server: server.publicKey,
          session,
          mint,
          escrow,
          serverTokenAccount: serverAta,
          tokenProgram: TOKEN_PROGRAM_ID,
          instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
        })
        .instruction();

      const { getAccount } = await import("@solana/spl-token");
      const serverBefore = await getAccount(connection, serverAta);

      const tx = new Transaction().add(precompileIx, settleIx);
      await sendAndConfirmTransaction(connection, tx, [server], {
        skipPreflight: false,
      });

      const sessionAcct = await program.account.session.fetch(session);
      expect(sessionAcct.lastSeenSequence.toString()).to.equal(sequence.toString());
      expect(sessionAcct.remainingCap.toString()).to.equal(
        (TOTAL_CAP - debitAmount).toString(),
      );

      const serverAfter = await getAccount(connection, serverAta);
      expect((serverAfter.amount - serverBefore.amount).toString()).to.equal(
        debitAmount.toString(),
      );
    });
  });
});
