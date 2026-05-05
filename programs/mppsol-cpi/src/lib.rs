// MPP.sol CPI Primitive
//
// Exposes MPP semantics as a Cross-Program Invocation target. Other Solana
// programs CPI into this program to atomically pay for and consume
// MPP-priced off-chain resources. Implements the spec defined in
// mppsol/spec/cpi.md.
//
// PROGRAM ID: PLACEHOLDER. Run `anchor keys sync` to generate a real
// keypair before deployment, and update both Anchor.toml and the
// declare_id!() below.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::{
    get_return_data, set_return_data,
};
use anchor_spl::token_interface::{
    transfer_checked, Mint, TokenAccount, TokenInterface, TransferChecked,
};
use mppsol_session::verify_ed25519_precompile_batch;

declare_id!("624xoctSeGzq1TAVwZU1xbM9RozAd3xZmjPeFXrAY14j");

// ============================================================================
// Constants
// ============================================================================

// Domain separator bound into Ed25519-signed result attestations from MPP
// servers. Distinct from DEBIT_DOMAIN_SEP to prevent cross-context reuse.
pub const RESULT_DOMAIN_SEP: [u8; 16] = *b"MPP.SOL/RESULT01";

// CPI return-data discriminators. See spec/cpi.md §4.1, §4.2.
pub const PAY_RETURN_DISCRIMINATOR: [u8; 4] = *b"PAY1";
pub const SESSION_RETURN_DISCRIMINATOR: [u8; 4] = *b"SES1";

// Receipt PDA seed. See spec/cpi.md §6 (v0.2 receipt-account variant,
// shipped in v0.1.1).
pub const RECEIPT_SEED: &[u8] = b"receipt";

// Total bytes of the PayReturn / SessionSettleReturn structure when
// serialized to return data. See spec/cpi.md §4.1.
// Layout: discriminator(4) + nonce(32) + request_hash(32) + amount(8) +
//         recipient(32) + mint(32) + slot(8) = 148.
pub const RETURN_DATA_BYTE_LENGTH: usize = 148;

// ============================================================================
// Errors
// ============================================================================

#[error_code]
pub enum CpiError {
    #[msg("payment expiry has passed")]
    DeadlinePassed,
    #[msg("amount must be greater than zero")]
    ZeroAmount,
    #[msg("Ed25519 precompile companion instruction missing or malformed")]
    MissingPrecompile,
    #[msg("server signature on result hash did not verify")]
    InvalidResultSignature,
    #[msg("return data not found or did not match a prior Pay/SettleViaSession in this transaction")]
    ReceiptNotFound,
    #[msg("receipt fields do not match VerifyPaidResult arguments")]
    ReceiptMismatch,
    #[msg("receipt has already been claimed")]
    ReceiptAlreadyClaimed,
    #[msg("receipt belongs to a different payer")]
    ReceiptPayerMismatch,
}

// ============================================================================
// State accounts
// ============================================================================

// On-chain receipt for a Pay or SettleViaSession. v0.2 variant of the
// return-data design — persists across CPIs and tx boundaries, so a
// later VerifyPaidResult (in the same or a different tx) can confirm
// payment-binding atomically. Closed via claim_receipt to recover rent.
//
// PDA seeds: [RECEIPT_SEED, payer.as_ref(), nonce.as_ref()]
#[account]
#[derive(InitSpace)]
pub struct Receipt {
    pub discriminator: [u8; 4],
    pub nonce: [u8; 32],
    pub request_hash: [u8; 32],
    pub amount: u64,
    pub recipient: Pubkey,
    pub mint: Pubkey,
    pub slot: u64,
    pub payer: Pubkey,
    pub created_at: i64,
    pub claimed: bool,
    pub bump: u8,
}

// ============================================================================
// Program entrypoints
// ============================================================================

#[program]
pub mod mppsol_cpi {
    use super::*;

    // ----- Pay ----------------------------------------------------------
    //
    // One-shot payment. Transfers `amount` of `mint` from payer to
    // recipient, emits a structured log, and writes a PayReturn struct
    // via set_return_data so subsequent instructions in the same tx can
    // verify it.
    pub fn pay(ctx: Context<Pay>, args: PayArgs) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(args.expiry >= now, CpiError::DeadlinePassed);
        require!(args.amount > 0, CpiError::ZeroAmount);

        transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.payer_token_account.to_account_info(),
                    to: ctx.accounts.recipient_token_account.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    authority: ctx.accounts.payer_authority.to_account_info(),
                },
            ),
            args.amount,
            ctx.accounts.mint.decimals,
        )?;

        // Structured log for off-chain indexers.
        msg!(
            "mppsol/pay nonce={} request_hash={} amount={}",
            bs58::encode(args.nonce).into_string(),
            bs58::encode(args.request_hash).into_string(),
            args.amount,
        );

        // Return data: discriminator || nonce || request_hash || amount ||
        //              recipient || mint || slot
        let slot = Clock::get()?.slot;
        let mut buf = Vec::with_capacity(RETURN_DATA_BYTE_LENGTH);
        buf.extend_from_slice(&PAY_RETURN_DISCRIMINATOR);
        buf.extend_from_slice(&args.nonce);
        buf.extend_from_slice(&args.request_hash);
        buf.extend_from_slice(&args.amount.to_le_bytes());
        buf.extend_from_slice(&ctx.accounts.recipient_token_account.key().to_bytes());
        buf.extend_from_slice(&ctx.accounts.mint.key().to_bytes());
        buf.extend_from_slice(&slot.to_le_bytes());
        set_return_data(&buf);

        Ok(())
    }

    // ----- SettleViaSession --------------------------------------------
    //
    // CPI-callable wrapper around mppsol_session::settle for a single
    // debit. Invokes session settlement (which itself verifies the
    // Ed25519 precompile companion ix), then emits a SES1-discriminated
    // return data block so a subsequent verify_paid_result can consume it.
    pub fn settle_via_session(
        ctx: Context<SettleViaSession>,
        args: SettleViaSessionArgs,
    ) -> Result<()> {
        // CPI into mppsol_session::settle with a 1-element batch.
        let cpi_program = ctx.accounts.mppsol_session_program.to_account_info();
        let cpi_accounts = mppsol_session::cpi::accounts::Settle {
            server: ctx.accounts.server.to_account_info(),
            session: ctx.accounts.session.to_account_info(),
            mint: ctx.accounts.mint.to_account_info(),
            escrow: ctx.accounts.escrow.to_account_info(),
            server_token_account: ctx.accounts.recipient_token_account.to_account_info(),
            token_program: ctx.accounts.token_program.to_account_info(),
            instructions_sysvar: ctx.accounts.instructions_sysvar.to_account_info(),
        };
        let cpi_args = mppsol_session::SettleArgs {
            debits: vec![args.debit.clone()],
            signatures: vec![args.signature],
        };
        mppsol_session::cpi::settle(
            CpiContext::new(cpi_program, cpi_accounts),
            cpi_args,
        )?;

        // Emit SES1-discriminated return data so a subsequent
        // verify_paid_result CPI can read the receipt.
        let slot = Clock::get()?.slot;
        let mut buf = Vec::with_capacity(RETURN_DATA_BYTE_LENGTH);
        buf.extend_from_slice(&SESSION_RETURN_DISCRIMINATOR);
        buf.extend_from_slice(&args.debit.nonce);
        buf.extend_from_slice(&args.request_hash);
        buf.extend_from_slice(&args.debit.amount.to_le_bytes());
        buf.extend_from_slice(&ctx.accounts.recipient_token_account.key().to_bytes());
        buf.extend_from_slice(&ctx.accounts.mint.key().to_bytes());
        buf.extend_from_slice(&slot.to_le_bytes());
        set_return_data(&buf);

        Ok(())
    }

    // ----- VerifyPaidResult --------------------------------------------
    //
    // Verifies that an Ed25519 precompile companion ix in the same tx
    // attests `server_pubkey` signed the canonical message
    // (nonce || request_hash || result_hash || RESULT_DOMAIN_SEP).
    //
    // v0.1 NOTE on payment-binding: this ix does NOT verify a prior Pay
    // happened in the same tx. Solana clears return data at the start of
    // every program invocation (including CPIs), so the original spec's
    // return-data lookup doesn't work across CPI boundaries. The
    // payment-binding guarantee in v0.1 comes from the nonce model:
    // servers only sign result hashes for nonces they issued challenges
    // for, so possession of a valid (nonce, signed_result) pair implies
    // payment was made off-chain.
    //
    // For stronger atomic on-chain payment-binding, the v0.2 spec will
    // add a Receipt account variant to Pay (rent-bearing, persistent
    // across CPIs). See spec/cpi.md §6 for the design.
    pub fn verify_paid_result(
        ctx: Context<VerifyPaidResult>,
        args: VerifyPaidResultArgs,
    ) -> Result<()> {
        let mut message = Vec::with_capacity(112);
        message.extend_from_slice(&args.nonce);
        message.extend_from_slice(&args.request_hash);
        message.extend_from_slice(&args.result_hash);
        message.extend_from_slice(&RESULT_DOMAIN_SEP);

        verify_ed25519_precompile_batch(
            &ctx.accounts.instructions_sysvar,
            &args.server_pubkey.to_bytes(),
            &[message],
            &[args.server_signature],
        )
        .map_err(|_| error!(CpiError::InvalidResultSignature))?;

        Ok(())
    }

    // ----- pay_with_receipt -------------------------------------------
    //
    // v0.1.1: Same as pay() but ALSO writes a Receipt PDA. The Receipt
    // persists across CPIs and tx boundaries, so a later
    // verify_paid_result_with_receipt can confirm payment-binding
    // atomically without depending on Solana's per-invocation return-data
    // clearing. Costs ~0.001 SOL of rent per receipt; reclaim via
    // claim_receipt.
    pub fn pay_with_receipt(
        ctx: Context<PayWithReceipt>,
        args: PayArgs,
    ) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(args.expiry >= now, CpiError::DeadlinePassed);
        require!(args.amount > 0, CpiError::ZeroAmount);

        // Same SPL transfer as Pay.
        transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.payer_token_account.to_account_info(),
                    to: ctx.accounts.recipient_token_account.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    authority: ctx.accounts.payer_authority.to_account_info(),
                },
            ),
            args.amount,
            ctx.accounts.mint.decimals,
        )?;

        // Populate the Receipt PDA.
        let slot = Clock::get()?.slot;
        let receipt = &mut ctx.accounts.receipt;
        receipt.discriminator = PAY_RETURN_DISCRIMINATOR;
        receipt.nonce = args.nonce;
        receipt.request_hash = args.request_hash;
        receipt.amount = args.amount;
        receipt.recipient = ctx.accounts.recipient_token_account.key();
        receipt.mint = ctx.accounts.mint.key();
        receipt.slot = slot;
        receipt.payer = ctx.accounts.payer_authority.key();
        receipt.created_at = now;
        receipt.claimed = false;
        receipt.bump = ctx.bumps.receipt;

        msg!(
            "mppsol/pay+receipt nonce={} request_hash={} amount={}",
            bs58::encode(args.nonce).into_string(),
            bs58::encode(args.request_hash).into_string(),
            args.amount,
        );

        Ok(())
    }

    // ----- verify_paid_result_with_receipt ----------------------------
    //
    // v0.1.1: Atomic on-chain payment-binding. Validates BOTH the
    // Ed25519 server signature AND that a Receipt PDA exists for the
    // (payer, nonce) pair with matching request_hash. Reverts on any
    // mismatch.
    //
    // The Receipt is rent-bearing and persistent — payable in one tx,
    // verifiable in another, by anyone with read access to the chain.
    // This is the v0.2 design (per spec/cpi.md §6) shipped early.
    pub fn verify_paid_result_with_receipt(
        ctx: Context<VerifyPaidResultWithReceipt>,
        args: VerifyPaidResultArgs,
    ) -> Result<()> {
        // 1. Validate the Receipt PDA matches the supplied nonce/request_hash.
        let receipt = &ctx.accounts.receipt;
        require!(receipt.nonce == args.nonce, CpiError::ReceiptMismatch);
        require!(
            receipt.request_hash == args.request_hash,
            CpiError::ReceiptMismatch,
        );
        require!(!receipt.claimed, CpiError::ReceiptAlreadyClaimed);

        // 2. Verify Ed25519 server signature on canonical result message.
        let mut message = Vec::with_capacity(112);
        message.extend_from_slice(&args.nonce);
        message.extend_from_slice(&args.request_hash);
        message.extend_from_slice(&args.result_hash);
        message.extend_from_slice(&RESULT_DOMAIN_SEP);

        verify_ed25519_precompile_batch(
            &ctx.accounts.instructions_sysvar,
            &args.server_pubkey.to_bytes(),
            &[message],
            &[args.server_signature],
        )
        .map_err(|_| error!(CpiError::InvalidResultSignature))?;

        Ok(())
    }

    // ----- claim_receipt ----------------------------------------------
    //
    // v0.1.1: Payer marks their Receipt as claimed and closes the PDA,
    // recovering the rent. Required after a Receipt is no longer needed
    // — receipts otherwise sit on-chain holding rent.
    //
    // Claiming is one-way: a claimed Receipt cannot be used by
    // verify_paid_result_with_receipt (it asserts !claimed). This is
    // deliberate — once you've consumed the verification, the receipt
    // is spent.
    pub fn claim_receipt(ctx: Context<ClaimReceipt>) -> Result<()> {
        // The Anchor `close = payer` constraint on the receipt account
        // does the actual close + rent return. We just gate it on the
        // payer being the original payer recorded in the receipt
        // (enforced by the seeds + has_one constraint).
        // Mark claimed = true first as a defensive belt-and-braces
        // (close happens at end of ix, the field is informational).
        ctx.accounts.receipt.claimed = true;
        Ok(())
    }

    // ----- GetReceipt --------------------------------------------------
    //
    // Asserts a return-data receipt for the given nonce exists in this tx
    // and re-emits it via set_return_data. Useful when CPI calls have
    // overwritten earlier return data.
    pub fn get_receipt(_ctx: Context<GetReceipt>, nonce: [u8; 32]) -> Result<()> {
        let (return_program_id, return_data) =
            get_return_data().ok_or(error!(CpiError::ReceiptNotFound))?;
        require!(return_program_id == crate::ID, CpiError::ReceiptNotFound);
        require!(
            return_data.len() == RETURN_DATA_BYTE_LENGTH,
            CpiError::ReceiptNotFound,
        );
        let receipt_nonce = &return_data[4..36];
        require!(receipt_nonce == nonce, CpiError::ReceiptMismatch);

        // Re-emit (no-op if already set this ix; explicit for callers
        // multiple CPIs deep).
        set_return_data(&return_data);
        Ok(())
    }
}

// ============================================================================
// Instruction args
// ============================================================================

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct PayArgs {
    pub amount: u64,
    pub nonce: [u8; 32],
    pub request_hash: [u8; 32],
    pub expiry: i64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct SettleViaSessionArgs {
    pub debit: mppsol_session::Debit,
    pub signature: [u8; 64],
    pub request_hash: [u8; 32],
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct VerifyPaidResultArgs {
    pub nonce: [u8; 32],
    pub request_hash: [u8; 32],
    pub result_hash: [u8; 32],
    pub server_pubkey: Pubkey,
    pub server_signature: [u8; 64],
}

// ============================================================================
// Account contexts
// ============================================================================

#[derive(Accounts)]
pub struct Pay<'info> {
    /// Authority over the payer's token account.
    /// Note: typed as Signer for v0.1 simplicity. PDA-callable variant
    /// (for invocation via CPI from caller programs) deferred to v0.2 —
    /// will need a separate `pay_via_cpi` ix that uses CpiContext::new_with_signer.
    pub payer_authority: Signer<'info>,

    #[account(mut)]
    pub payer_token_account: InterfaceAccount<'info, TokenAccount>,

    #[account(mut)]
    pub recipient_token_account: InterfaceAccount<'info, TokenAccount>,

    pub mint: InterfaceAccount<'info, Mint>,

    pub token_program: Interface<'info, TokenInterface>,

    /// Sysvar: Instructions, for use by VerifyPaidResult in the same tx.
    /// CHECK: well-known sysvar address.
    #[account(address = anchor_lang::solana_program::sysvar::instructions::ID)]
    pub instructions_sysvar: AccountInfo<'info>,
}

#[derive(Accounts)]
#[instruction(args: PayArgs)]
pub struct PayWithReceipt<'info> {
    #[account(mut)]
    pub payer_authority: Signer<'info>,

    #[account(mut)]
    pub payer_token_account: InterfaceAccount<'info, TokenAccount>,

    #[account(mut)]
    pub recipient_token_account: InterfaceAccount<'info, TokenAccount>,

    pub mint: InterfaceAccount<'info, Mint>,

    /// On-chain receipt PDA. PDA seeds: [RECEIPT_SEED, payer, nonce].
    #[account(
        init,
        payer = payer_authority,
        space = 8 + Receipt::INIT_SPACE,
        seeds = [RECEIPT_SEED, payer_authority.key().as_ref(), args.nonce.as_ref()],
        bump,
    )]
    pub receipt: Account<'info, Receipt>,

    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,

    /// CHECK: well-known sysvar address.
    #[account(address = anchor_lang::solana_program::sysvar::instructions::ID)]
    pub instructions_sysvar: AccountInfo<'info>,
}

#[derive(Accounts)]
#[instruction(args: VerifyPaidResultArgs)]
pub struct VerifyPaidResultWithReceipt<'info> {
    /// CHECK: caller (any program or signer requesting verification).
    pub caller: AccountInfo<'info>,

    /// The original payer of the receipt (used to derive the PDA).
    /// CHECK: only used to derive the seeds.
    pub payer: AccountInfo<'info>,

    /// Receipt PDA. Seeds: [RECEIPT_SEED, payer, nonce].
    #[account(
        seeds = [RECEIPT_SEED, payer.key().as_ref(), args.nonce.as_ref()],
        bump = receipt.bump,
        has_one = payer @ CpiError::ReceiptPayerMismatch,
    )]
    pub receipt: Account<'info, Receipt>,

    /// CHECK: well-known sysvar address.
    #[account(address = anchor_lang::solana_program::sysvar::instructions::ID)]
    pub instructions_sysvar: AccountInfo<'info>,
}

#[derive(Accounts)]
pub struct ClaimReceipt<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    #[account(
        mut,
        close = payer,
        seeds = [RECEIPT_SEED, payer.key().as_ref(), receipt.nonce.as_ref()],
        bump = receipt.bump,
        has_one = payer @ CpiError::ReceiptPayerMismatch,
    )]
    pub receipt: Account<'info, Receipt>,
}

#[derive(Accounts)]
pub struct SettleViaSession<'info> {
    /// Server signer — passed through to mppsol_session::settle as the
    /// session's `server`. Must match session.server.
    pub server: Signer<'info>,

    /// Session PDA owned by mppsol_session.
    /// CHECK: validated by the inner CPI to mppsol_session::settle.
    #[account(mut)]
    pub session: AccountInfo<'info>,

    /// Escrow ATA owned by the session PDA.
    /// CHECK: validated by inner CPI.
    #[account(mut)]
    pub escrow: AccountInfo<'info>,

    /// Recipient (= session.server's token account).
    /// CHECK: validated by inner CPI.
    #[account(mut)]
    pub recipient_token_account: AccountInfo<'info>,

    pub mint: InterfaceAccount<'info, Mint>,

    pub token_program: Interface<'info, TokenInterface>,

    /// CHECK: well-known sysvar address.
    #[account(address = anchor_lang::solana_program::sysvar::instructions::ID)]
    pub instructions_sysvar: AccountInfo<'info>,

    /// CHECK: program account for mppsol_session.
    pub mppsol_session_program: AccountInfo<'info>,
}

#[derive(Accounts)]
pub struct VerifyPaidResult<'info> {
    /// CHECK: the program or signer requesting verification.
    pub caller: AccountInfo<'info>,

    /// CHECK: well-known sysvar address.
    #[account(address = anchor_lang::solana_program::sysvar::instructions::ID)]
    pub instructions_sysvar: AccountInfo<'info>,
}

#[derive(Accounts)]
pub struct GetReceipt<'info> {
    /// CHECK: caller.
    pub caller: AccountInfo<'info>,
}
