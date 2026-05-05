// Open an MPP.sol session on Solana devnet.
//
// Outputs:
//   - The session PDA pubkey (use as MPPSOL_SESSION env var when paying)
//   - authorized-signer.json (the Ed25519 keypair the agent will use to
//     sign debit messages; protect like any other private key)
//
// Run: bun run examples/open-session.ts
//
// Prerequisites:
//   - Owner wallet (e.g. ~/.config/solana/id.json) funded with devnet SOL
//     and devnet USDC. Get USDC from a devnet faucet.
//   - The wallet must have a USDC token account (auto-derived ATA).

import * as anchor from '@coral-xyz/anchor';
import { Program, BN } from '@coral-xyz/anchor';
import {
  Connection,
  Keypair,
  PublicKey,
  SystemProgram,
  SYSVAR_RENT_PUBKEY,
} from '@solana/web3.js';
import {
  TOKEN_PROGRAM_ID,
  ASSOCIATED_TOKEN_PROGRAM_ID,
  getAssociatedTokenAddressSync,
  createAssociatedTokenAccountIdempotent,
} from '@solana/spl-token';
import { ed25519 } from '@noble/curves/ed25519';
import { readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';

// --- Config ----------------------------------------------------------------
const RPC = process.env.SOLANA_RPC ?? 'https://api.devnet.solana.com';
const SESSION_PROGRAM_ID = new PublicKey('B7joeuXqPJSCTfUfMacHaWL6eseoDinV7Jxt52gVdfbi');
const DEVNET_USDC = new PublicKey('4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU');

// Server pubkey: the wallet that owns the recipient token account on the
// MPP server you intend to pay. For local testing use any pubkey; you can
// reuse your own.
const SERVER_PUBKEY = new PublicKey(
  process.env.MPP_SERVER_PUBKEY ?? 'YOUR_SERVER_PUBKEY',
);

// Cap and expiry tunables.
const TOTAL_CAP = BigInt(process.env.TOTAL_CAP_USDC_ATOMIC ?? '1_000_000'); // 1 USDC
const EXPIRY_SECS = Number(process.env.EXPIRY_SECS ?? 3600);                // 1 hour

// --- Wallet ----------------------------------------------------------------
const walletPath = process.env.WALLET ?? `${homedir()}/.config/solana/id.json`;
const ownerKeypair = Keypair.fromSecretKey(
  Uint8Array.from(JSON.parse(readFileSync(walletPath, 'utf8'))),
);

// --- Generate authorized_signer keypair (off-chain Ed25519) ---------------
const SIGNER_PATH = process.env.SIGNER_OUT ?? './authorized-signer.json';
const signerPriv = ed25519.utils.randomPrivateKey();
const signerPub = new PublicKey(ed25519.getPublicKey(signerPriv));
writeFileSync(SIGNER_PATH, JSON.stringify(Array.from(signerPriv)));

// --- Anchor setup ----------------------------------------------------------
const connection = new Connection(RPC, 'confirmed');
const wallet = new anchor.Wallet(ownerKeypair);
const provider = new anchor.AnchorProvider(connection, wallet, {
  commitment: 'confirmed',
});
anchor.setProvider(provider);

// Fetch the IDL on-chain (uploaded at deploy time).
const idl = await Program.fetchIdl(SESSION_PROGRAM_ID, provider);
if (!idl) throw new Error('Could not fetch session program IDL from chain');
const program = new Program(idl as anchor.Idl, provider);

// --- Build Open args ------------------------------------------------------
const sessionId = (() => {
  const b = new Uint8Array(16);
  crypto.getRandomValues(b);
  return b;
})();
const expiry = new BN(Math.floor(Date.now() / 1000) + EXPIRY_SECS);

// Derive the session PDA.
const [session] = PublicKey.findProgramAddressSync(
  [
    Buffer.from('session'),
    ownerKeypair.publicKey.toBuffer(),
    SERVER_PUBKEY.toBuffer(),
    Buffer.from(sessionId),
  ],
  SESSION_PROGRAM_ID,
);

// Owner's USDC ATA (source of escrow funds).
const ownerSource = getAssociatedTokenAddressSync(DEVNET_USDC, ownerKeypair.publicKey);
const escrow = getAssociatedTokenAddressSync(DEVNET_USDC, session, true);

// Cluster genesis hash for cross-cluster anti-replay.
const genesisHash = await connection.getGenesisHash();
const genesisHashBytes = (await import('bs58')).default.decode(genesisHash);

// --- Open the session -----------------------------------------------------
console.log(`Opening session...`);
console.log(`  owner: ${ownerKeypair.publicKey.toBase58()}`);
console.log(`  server: ${SERVER_PUBKEY.toBase58()}`);
console.log(`  authorized_signer: ${signerPub.toBase58()}`);
console.log(`  session PDA: ${session.toBase58()}`);
console.log(`  cap: ${TOTAL_CAP} atomic units (${Number(TOTAL_CAP) / 1e6} USDC)`);
console.log(`  expiry: ${new Date(Number(expiry) * 1000).toISOString()}`);

const sig = await program.methods
  .open({
    authorizedSigner: signerPub,
    server: SERVER_PUBKEY,
    totalCap: new BN(TOTAL_CAP.toString()),
    expiry,
    sessionId: Array.from(sessionId),
    clusterGenesisHash: Array.from(genesisHashBytes.slice(0, 32)),
  })
  .accounts({
    owner: ownerKeypair.publicKey,
    session,
    mint: DEVNET_USDC,
    escrow,
    ownerSource,
    tokenProgram: TOKEN_PROGRAM_ID,
    associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
    systemProgram: SystemProgram.programId,
    rent: SYSVAR_RENT_PUBKEY,
  })
  .rpc();

console.log();
console.log('✓ Session opened.');
console.log(`  tx: https://explorer.solana.com/tx/${sig}?cluster=devnet`);
console.log(`  authorized_signer key written to: ${SIGNER_PATH}`);
console.log();
console.log('Use these env vars in pay-session.ts:');
console.log(`  export MPPSOL_SESSION=${session.toBase58()}`);
console.log(`  export SIGNER=${SIGNER_PATH}`);
