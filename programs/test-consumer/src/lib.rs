// Test-only companion program for mppsol_cpi.
//
// Exists solely to exercise mppsol_cpi::verify_paid_result via CPI in
// the same call stack as mppsol_cpi::pay. Solana clears return data
// between top-level instructions, so verify_paid_result must run AS A
// CPI from within a parent instruction that also called pay (or
// settle_via_session) earlier in the same call stack. This program is
// the minimal such parent for testing.
//
// NOT intended for deployment beyond test environments.

use anchor_lang::prelude::*;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};
use mppsol_cpi::cpi::accounts::{GetReceipt, Pay, VerifyPaidResult};
use mppsol_cpi::program::MppsolCpi;
use mppsol_cpi::{PayArgs, VerifyPaidResultArgs};

declare_id!("65ndFCiYYM3tznTg5Te1x8ALfVP7SxFEwvvUeANYy3Ex");

#[program]
pub mod test_consumer {
    use super::*;

    // Performs Pay then VerifyPaidResult in one call stack so return
    // data set by Pay is still readable by VerifyPaidResult.
    pub fn pay_and_verify(
        ctx: Context<PayAndVerify>,
        args: PayAndVerifyArgs,
    ) -> Result<()> {
        // 1. CPI to mppsol_cpi::pay
        let pay_accounts = Pay {
            payer_authority: ctx.accounts.payer_authority.to_account_info(),
            payer_token_account: ctx.accounts.payer_token_account.to_account_info(),
            recipient_token_account: ctx.accounts.recipient_token_account.to_account_info(),
            mint: ctx.accounts.mint.to_account_info(),
            token_program: ctx.accounts.token_program.to_account_info(),
            instructions_sysvar: ctx.accounts.instructions_sysvar.to_account_info(),
        };
        let pay_args = PayArgs {
            amount: args.amount,
            nonce: args.nonce,
            request_hash: args.request_hash,
            expiry: args.expiry,
        };
        mppsol_cpi::cpi::pay(
            CpiContext::new(ctx.accounts.mppsol_cpi_program.to_account_info(), pay_accounts),
            pay_args,
        )?;

        // 2. CPI to mppsol_cpi::verify_paid_result
        let verify_accounts = VerifyPaidResult {
            caller: ctx.accounts.payer_authority.to_account_info(),
            instructions_sysvar: ctx.accounts.instructions_sysvar.to_account_info(),
        };
        let verify_args = VerifyPaidResultArgs {
            nonce: args.nonce,
            request_hash: args.request_hash,
            result_hash: args.result_hash,
            server_pubkey: args.server_pubkey,
            server_signature: args.server_signature,
        };
        mppsol_cpi::cpi::verify_paid_result(
            CpiContext::new(ctx.accounts.mppsol_cpi_program.to_account_info(), verify_accounts),
            verify_args,
        )?;

        Ok(())
    }

    // Performs Pay then GetReceipt in one call stack so the receipt set
    // by Pay is still readable. Used by anchor tests for get_receipt.
    pub fn pay_and_get_receipt(
        ctx: Context<PayAndGetReceipt>,
        args: PayArgs,
    ) -> Result<()> {
        let pay_accounts = Pay {
            payer_authority: ctx.accounts.payer_authority.to_account_info(),
            payer_token_account: ctx.accounts.payer_token_account.to_account_info(),
            recipient_token_account: ctx.accounts.recipient_token_account.to_account_info(),
            mint: ctx.accounts.mint.to_account_info(),
            token_program: ctx.accounts.token_program.to_account_info(),
            instructions_sysvar: ctx.accounts.instructions_sysvar.to_account_info(),
        };
        mppsol_cpi::cpi::pay(
            CpiContext::new(ctx.accounts.mppsol_cpi_program.to_account_info(), pay_accounts),
            args.clone(),
        )?;

        let get_receipt_accounts = GetReceipt {
            caller: ctx.accounts.payer_authority.to_account_info(),
        };
        mppsol_cpi::cpi::get_receipt(
            CpiContext::new(
                ctx.accounts.mppsol_cpi_program.to_account_info(),
                get_receipt_accounts,
            ),
            args.nonce,
        )?;
        Ok(())
    }
}

#[derive(Accounts)]
pub struct PayAndGetReceipt<'info> {
    pub payer_authority: Signer<'info>,
    #[account(mut)]
    pub payer_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(mut)]
    pub recipient_token_account: InterfaceAccount<'info, TokenAccount>,
    pub mint: InterfaceAccount<'info, Mint>,
    pub token_program: Interface<'info, TokenInterface>,
    /// CHECK: well-known sysvar address.
    #[account(address = anchor_lang::solana_program::sysvar::instructions::ID)]
    pub instructions_sysvar: AccountInfo<'info>,
    pub mppsol_cpi_program: Program<'info, MppsolCpi>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct PayAndVerifyArgs {
    pub amount: u64,
    pub nonce: [u8; 32],
    pub request_hash: [u8; 32],
    pub expiry: i64,
    pub result_hash: [u8; 32],
    pub server_pubkey: Pubkey,
    pub server_signature: [u8; 64],
}

#[derive(Accounts)]
pub struct PayAndVerify<'info> {
    pub payer_authority: Signer<'info>,

    #[account(mut)]
    pub payer_token_account: InterfaceAccount<'info, TokenAccount>,

    #[account(mut)]
    pub recipient_token_account: InterfaceAccount<'info, TokenAccount>,

    pub mint: InterfaceAccount<'info, Mint>,

    pub token_program: Interface<'info, TokenInterface>,

    /// CHECK: well-known sysvar address.
    #[account(address = anchor_lang::solana_program::sysvar::instructions::ID)]
    pub instructions_sysvar: AccountInfo<'info>,

    pub mppsol_cpi_program: Program<'info, MppsolCpi>,
}
