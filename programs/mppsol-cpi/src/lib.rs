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
use anchor_lang::solana_program::program::{get_return_data, set_return_data};
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
    // CPI-callable wrapper around mppsol_session::Settle for a single
    // debit. Verifies the Ed25519 precompile companion ix, invokes
    // session settlement via CPI, then emits a SettleReturn struct.
    //
    // STATUS: skeleton. The session::Settle CPI invocation is deferred
    // until session's Settle is implemented (v0.1.1).
    pub fn settle_via_session(
        _ctx: Context<SettleViaSession>,
        _args: SettleViaSessionArgs,
    ) -> Result<()> {
        // TODO(v0.1.1):
        //   1. Verify Ed25519 precompile companion ix matches the supplied
        //      debit and the session's authorized_signer.
        //   2. Invoke mppsol_session::settle via CPI with a 1-element
        //      debits batch.
        //   3. Emit SES1-discriminated return data with the same shape
        //      as PayReturn.
        return err!(CpiError::MissingPrecompile);
    }

    // ----- VerifyPaidResult --------------------------------------------
    //
    // Read-only. Confirms that:
    //   1. A prior Pay or SettleViaSession in this tx wrote return data
    //      whose nonce + request_hash match the args.
    //   2. The Ed25519 precompile companion ix verified `server_pubkey`'s
    //      signature over the canonical message
    //      (nonce || request_hash || result_hash || RESULT_DOMAIN_SEP).
    //
    // On failure the caller's tx reverts. This is the killer instruction
    // for atomic pay-and-consume composition.
    pub fn verify_paid_result(
        ctx: Context<VerifyPaidResult>,
        args: VerifyPaidResultArgs,
    ) -> Result<()> {
        // 1. Read return data set by a prior Pay/SettleViaSession in this tx.
        let (return_program_id, return_data) =
            get_return_data().ok_or(error!(CpiError::ReceiptNotFound))?;
        require!(return_program_id == crate::ID, CpiError::ReceiptNotFound);
        require!(
            return_data.len() == RETURN_DATA_BYTE_LENGTH,
            CpiError::ReceiptNotFound,
        );

        // 2. Discriminator must be PAY1 or SES1.
        let discriminator = &return_data[0..4];
        require!(
            discriminator == PAY_RETURN_DISCRIMINATOR
                || discriminator == SESSION_RETURN_DISCRIMINATOR,
            CpiError::ReceiptNotFound,
        );

        // 3. Nonce + request_hash must match.
        let receipt_nonce = &return_data[4..36];
        let receipt_request_hash = &return_data[36..68];
        require!(receipt_nonce == args.nonce, CpiError::ReceiptMismatch);
        require!(
            receipt_request_hash == args.request_hash,
            CpiError::ReceiptMismatch,
        );

        // 4. Build canonical result message (112 bytes) and verify Ed25519.
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
pub struct SettleViaSession<'info> {
    /// CPI caller (often a program PDA).
    /// CHECK: not authoritative for fund movement; recipient is fixed by
    /// the session record.
    pub caller: AccountInfo<'info>,

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
