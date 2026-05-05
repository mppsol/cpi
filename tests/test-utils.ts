import * as anchor from "@coral-xyz/anchor";
import { Program, web3 } from "@coral-xyz/anchor";
import {
  Connection,
  Ed25519Program,
  Keypair,
  PublicKey,
  SystemProgram,
  Transaction,
  TransactionInstruction,
  sendAndConfirmTransaction,
} from "@solana/web3.js";
import {
  createMint,
  getOrCreateAssociatedTokenAccount,
  mintTo,
  TOKEN_PROGRAM_ID,
  ASSOCIATED_TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import { ed25519 } from "@noble/curves/ed25519";

// Domain separators — must match @mppsol/core / on-chain constants exactly.
export const DEBIT_DOMAIN_SEP = Buffer.from("MPP.SOL/DEBIT001", "utf8");
export const RESULT_DOMAIN_SEP = Buffer.from("MPP.SOL/RESULT01", "utf8");

export const SESSION_SEED = Buffer.from("session", "utf8");

// PDA derivation for a session.
export function deriveSessionPda(
  programId: PublicKey,
  owner: PublicKey,
  server: PublicKey,
  sessionId: Buffer,
): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [SESSION_SEED, owner.toBuffer(), server.toBuffer(), sessionId],
    programId,
  );
}

// Encode a debit struct canonically (104 bytes).
export interface Debit {
  session: PublicKey;
  nonce: Buffer;     // 32 bytes
  amount: bigint;    // u64
  expiry: bigint;    // i64
  sequence: bigint;  // u64
}

export function encodeDebit(d: Debit): Buffer {
  const buf = Buffer.alloc(104);
  let off = 0;
  d.session.toBuffer().copy(buf, off); off += 32;
  d.nonce.copy(buf, off); off += 32;
  buf.writeBigUInt64LE(d.amount, off); off += 8;
  buf.writeBigInt64LE(d.expiry, off); off += 8;
  buf.writeBigUInt64LE(d.sequence, off); off += 8;
  DEBIT_DOMAIN_SEP.copy(buf, off);
  return buf;
}

// Sign a debit message with the authorized_signer's Ed25519 key.
export function signDebit(privateKey: Uint8Array, debitBytes: Buffer): Buffer {
  return Buffer.from(ed25519.sign(debitBytes, privateKey));
}

// Build the Ed25519 precompile instruction that verifies a single signed
// debit. This is the companion ix that goes BEFORE settle in the same tx.
export function buildEd25519PrecompileIx(
  publicKey: Uint8Array,    // 32 bytes
  message: Uint8Array,
  signature: Uint8Array,    // 64 bytes
): TransactionInstruction {
  return Ed25519Program.createInstructionWithPublicKey({
    publicKey,
    message,
    signature,
  });
}

// Build a multi-sig precompile in a single ix. Useful for batch settle.
export function buildEd25519PrecompileBatch(
  entries: Array<{
    publicKey: Uint8Array;
    message: Uint8Array;
    signature: Uint8Array;
  }>,
): TransactionInstruction {
  // @solana/web3.js doesn't directly expose a batch builder, so construct
  // the precompile data manually.
  const HEADER = 2;
  const ENTRY_SIZE = 14;
  const headerLen = HEADER + entries.length * ENTRY_SIZE;

  // Compute layout: header + entry table + raw bytes (pubkey, message, signature
  // for each entry).
  let offset = headerLen;
  const layouts = entries.map(e => {
    const sigOffset = offset;
    offset += 64;
    const pkOffset = offset;
    offset += 32;
    const msgOffset = offset;
    offset += e.message.length;
    return {
      sigOffset, pkOffset, msgOffset, msgSize: e.message.length, ...e,
    };
  });

  const data = Buffer.alloc(offset);
  data[0] = entries.length;
  data[1] = 0; // padding

  for (let i = 0; i < entries.length; i++) {
    const l = layouts[i];
    const entry = data.subarray(HEADER + i * ENTRY_SIZE, HEADER + (i + 1) * ENTRY_SIZE);
    entry.writeUInt16LE(l.sigOffset, 0);
    entry.writeUInt16LE(0xFFFF, 2);   // sig_instruction_index = same ix
    entry.writeUInt16LE(l.pkOffset, 4);
    entry.writeUInt16LE(0xFFFF, 6);
    entry.writeUInt16LE(l.msgOffset, 8);
    entry.writeUInt16LE(l.msgSize, 10);
    entry.writeUInt16LE(0xFFFF, 12);
  }

  for (const l of layouts) {
    Buffer.from(l.signature).copy(data, l.sigOffset);
    Buffer.from(l.publicKey).copy(data, l.pkOffset);
    Buffer.from(l.message).copy(data, l.msgOffset);
  }

  return new TransactionInstruction({
    programId: new PublicKey("Ed25519SigVerify111111111111111111111111111"),
    keys: [],
    data,
  });
}

// Convenience: airdrop SOL to a keypair.
export async function airdrop(
  connection: Connection,
  to: PublicKey,
  amountSol = 2,
): Promise<void> {
  const sig = await connection.requestAirdrop(to, amountSol * web3.LAMPORTS_PER_SOL);
  await connection.confirmTransaction(sig);
}

// Set up a fresh SPL mint and mint some tokens to a recipient.
export async function setupMintAndMint(
  connection: Connection,
  payer: Keypair,
  recipient: PublicKey,
  amount: bigint,
  decimals = 6,
): Promise<{ mint: PublicKey; recipientAta: PublicKey }> {
  const mint = await createMint(
    connection,
    payer,
    payer.publicKey,
    null,
    decimals,
  );
  const ata = await getOrCreateAssociatedTokenAccount(
    connection, payer, mint, recipient,
  );
  await mintTo(connection, payer, mint, ata.address, payer, amount);
  return { mint, recipientAta: ata.address };
}

// Random 32-byte buffer (for nonces, request hashes, etc.).
export function randomBytes(n: number): Buffer {
  const b = Buffer.alloc(n);
  for (let i = 0; i < n; i++) b[i] = Math.floor(Math.random() * 256);
  return b;
}

// Generate a fresh Ed25519 keypair (returns 32-byte private key + base58 pubkey).
export function generateEd25519(): { privateKey: Uint8Array; publicKey: PublicKey } {
  const privateKey = ed25519.utils.randomPrivateKey();
  const pubBytes = ed25519.getPublicKey(privateKey);
  return {
    privateKey,
    publicKey: new PublicKey(pubBytes),
  };
}
