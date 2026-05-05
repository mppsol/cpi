// MPP.sol Session Program
//
// Manages on-chain session PDAs that escrow tokens for off-chain MPP debit
// settlement. Implements the spec defined in mppsol/spec/session.md.
//
// PROGRAM ID: PLACEHOLDER. Run `anchor keys sync` to generate a real
// keypair before deployment, and update both Anchor.toml and the
// declare_id!() below.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::sysvar::instructions::load_instruction_at_checked;
use anchor_spl::associated_token::AssociatedToken;

// Ed25519 native precompile program ID. Hardcoded because `ed25519_program`
// isn't re-exported in all solana-program versions.
pub const ED25519_PROGRAM_ID: Pubkey =
    pubkey!("Ed25519SigVerify111111111111111111111111111");
use anchor_spl::token_interface::{
    close_account, transfer_checked, CloseAccount, Mint, TokenAccount, TokenInterface,
    TransferChecked,
};

declare_id!("B7joeuXqPJSCTfUfMacHaWL6eseoDinV7Jxt52gVdfbi");

// ============================================================================
// Constants
// ============================================================================

pub const SESSION_SEED: &[u8] = b"session";
pub const ESCROW_SEED: &[u8] = b"escrow";

// "MPP.SOL/DEBIT001" — must exactly match @mppsol/core's DEBIT_DOMAIN_SEP.
pub const DEBIT_DOMAIN_SEP: [u8; 16] = *b"MPP.SOL/DEBIT001";

// Canonical serialized debit message length.
pub const DEBIT_BYTE_LENGTH: usize = 104;

// Max debits in a single batched Settle. Bounded by Solana CU budget.
pub const MAX_BATCH_SIZE: usize = 32;

// Grace window after `Revoke` before `Close` is permitted, so pending
// debits can still settle.
pub const REVOKE_GRACE_SECS: i64 = 24 * 60 * 60; // 24h

// Ed25519 precompile data layout. See spec/session.md §6.
pub const ED25519_HEADER_SIZE: usize = 2;       // num_signatures + padding
pub const ED25519_SIG_OFFSETS_SIZE: usize = 14; // 7 u16 fields per sig

// ============================================================================
// Account state
// ============================================================================

#[account]
#[derive(InitSpace)]
pub struct Session {
    pub owner: Pubkey,
    pub authorized_signer: Pubkey,
    pub server: Pubkey,
    pub mint: Pubkey,
    pub escrow: Pubkey,
    pub total_cap: u64,
    pub remaining_cap: u64,
    pub last_seen_sequence: u64,
    pub expiry: i64,
    pub state: u8, // 0 = Active, 1 = Revoked, 2 = Closed
    pub cluster_genesis_hash: [u8; 32],
    pub session_id: [u8; 16],
    pub bump: u8,
}

#[repr(u8)]
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Active = 0,
    Revoked = 1,
    Closed = 2,
}

// ============================================================================
// Errors
// ============================================================================

#[error_code]
pub enum SessionError {
    #[msg("expiry must be in the future")]
    ExpiryInPast,
    #[msg("total cap must be greater than zero")]
    ZeroCap,
    #[msg("session is not Active")]
    NotActive,
    #[msg("session has expired")]
    Expired,
    #[msg("debit batch is empty or too large")]
    BadBatchSize,
    #[msg("debit byte length is not 104")]
    BadDebitLength,
    #[msg("debit domain separator is invalid")]
    BadDomainSeparator,
    #[msg("debit references the wrong session pubkey")]
    SessionMismatch,
    #[msg("debit sequence is not strictly greater than last_seen_sequence")]
    SequenceReused,
    #[msg("debit expiry has passed")]
    DebitExpired,
    #[msg("cumulative debit amount exceeds remaining cap")]
    CapExceeded,
    #[msg("Ed25519 signature verification failed")]
    InvalidSignature,
    #[msg("Ed25519 precompile companion instruction missing or malformed")]
    MissingPrecompile,
    #[msg("close not yet permitted; revoke grace period still open or session not expired")]
    CloseNotPermitted,
}

// ============================================================================
// Program entrypoints
// ============================================================================

#[program]
pub mod mppsol_session {
    use super::*;

    // ----- Open ---------------------------------------------------------
    pub fn open(ctx: Context<Open>, args: OpenArgs) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        require!(args.expiry > now, SessionError::ExpiryInPast);
        require!(args.total_cap > 0, SessionError::ZeroCap);

        let session = &mut ctx.accounts.session;
        session.owner = ctx.accounts.owner.key();
        session.authorized_signer = args.authorized_signer;
        session.server = args.server;
        session.mint = ctx.accounts.mint.key();
        session.escrow = ctx.accounts.escrow.key();
        session.total_cap = args.total_cap;
        session.remaining_cap = args.total_cap;
        session.last_seen_sequence = 0;
        session.expiry = args.expiry;
        session.state = SessionState::Active as u8;
        session.cluster_genesis_hash = args.cluster_genesis_hash;
        session.session_id = args.session_id;
        session.bump = ctx.bumps.session;

        // Fund escrow with total_cap from owner's source token account.
        transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.owner_source.to_account_info(),
                    to: ctx.accounts.escrow.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                },
            ),
            args.total_cap,
            ctx.accounts.mint.decimals,
        )?;

        Ok(())
    }

    // ----- Settle -------------------------------------------------------
    //
    // Settles a batch of debit messages. The companion instruction in this
    // tx MUST be the Ed25519 precompile (program id
    // Ed25519SigVerify111111111111111111111111111) verifying every debit's
    // signature against the session's authorized_signer. See
    // spec/session.md §6.
    pub fn settle(ctx: Context<Settle>, args: SettleArgs) -> Result<()> {
        let session_state = ctx.accounts.session.state;
        require!(
            session_state == SessionState::Active as u8
                || session_state == SessionState::Revoked as u8,
            SessionError::NotActive,
        );
        require!(
            !args.debits.is_empty() && args.debits.len() <= MAX_BATCH_SIZE,
            SessionError::BadBatchSize,
        );
        require!(
            args.signatures.len() == args.debits.len(),
            SessionError::BadBatchSize,
        );

        // Encode each debit canonically (104 bytes). These are the messages
        // we expect the Ed25519 precompile to have verified.
        let messages: Vec<Vec<u8>> = args
            .debits
            .iter()
            .map(|d| {
                let mut buf = Vec::with_capacity(104);
                buf.extend_from_slice(&d.session);
                buf.extend_from_slice(&d.nonce);
                buf.extend_from_slice(&d.amount.to_le_bytes());
                buf.extend_from_slice(&d.expiry.to_le_bytes());
                buf.extend_from_slice(&d.sequence.to_le_bytes());
                buf.extend_from_slice(&d.domain_sep);
                buf
            })
            .collect();

        let authorized_signer_bytes = ctx.accounts.session.authorized_signer.to_bytes();
        verify_ed25519_precompile_batch(
            &ctx.accounts.instructions_sysvar,
            &authorized_signer_bytes,
            &messages,
            &args.signatures,
        )?;

        // Verify debit fields after Ed25519 binding succeeds.
        let session_key = ctx.accounts.session.key();
        let session = &mut ctx.accounts.session;
        let mut cumulative: u64 = 0;
        let mut max_seq = session.last_seen_sequence;
        let now = Clock::get()?.unix_timestamp;

        for debit in &args.debits {
            require!(
                debit.session == session_key.to_bytes(),
                SessionError::SessionMismatch,
            );
            require!(
                debit.domain_sep == DEBIT_DOMAIN_SEP,
                SessionError::BadDomainSeparator,
            );
            require!(debit.sequence > max_seq, SessionError::SequenceReused);
            require!(debit.expiry >= now, SessionError::DebitExpired);

            cumulative = cumulative
                .checked_add(debit.amount)
                .ok_or(error!(SessionError::CapExceeded))?;
            require!(
                cumulative <= session.remaining_cap,
                SessionError::CapExceeded,
            );

            if debit.sequence > max_seq {
                max_seq = debit.sequence;
            }
        }

        // Transfer cumulative amount from escrow → server token account.
        let owner_key = session.owner;
        let server_key = session.server;
        let session_id_bytes = session.session_id;
        let bump = session.bump;
        let signer_seeds: &[&[&[u8]]] = &[&[
            SESSION_SEED,
            owner_key.as_ref(),
            server_key.as_ref(),
            session_id_bytes.as_ref(),
            &[bump],
        ]];

        transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.escrow.to_account_info(),
                    to: ctx.accounts.server_token_account.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    authority: session.to_account_info(),
                },
                signer_seeds,
            ),
            cumulative,
            ctx.accounts.mint.decimals,
        )?;

        session.remaining_cap = session
            .remaining_cap
            .checked_sub(cumulative)
            .ok_or(error!(SessionError::CapExceeded))?;
        session.last_seen_sequence = max_seq;

        Ok(())
    }

    // ----- Topup --------------------------------------------------------
    pub fn topup(ctx: Context<Topup>, amount: u64) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let session = &mut ctx.accounts.session;
        require!(
            session.state == SessionState::Active as u8,
            SessionError::NotActive,
        );
        require!(now < session.expiry, SessionError::Expired);
        require!(amount > 0, SessionError::ZeroCap);

        transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.owner_source.to_account_info(),
                    to: ctx.accounts.escrow.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    authority: ctx.accounts.owner.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.mint.decimals,
        )?;

        session.total_cap = session
            .total_cap
            .checked_add(amount)
            .ok_or(error!(SessionError::CapExceeded))?;
        session.remaining_cap = session
            .remaining_cap
            .checked_add(amount)
            .ok_or(error!(SessionError::CapExceeded))?;

        Ok(())
    }

    // ----- Revoke -------------------------------------------------------
    //
    // Either the owner or the server may revoke. Pending debits with
    // expiry > now MAY still settle until expired.
    pub fn revoke(ctx: Context<Revoke>) -> Result<()> {
        let session = &mut ctx.accounts.session;
        require!(
            session.state == SessionState::Active as u8,
            SessionError::NotActive,
        );
        let signer_key = ctx.accounts.signer.key();
        require!(
            signer_key == session.owner || signer_key == session.server,
            SessionError::NotActive,
        );
        session.state = SessionState::Revoked as u8;
        Ok(())
    }

    // ----- Close --------------------------------------------------------
    //
    // Closes a Revoked session past the grace period, or an Active session
    // past expiry. Drains residual escrow + escrow ATA rent + session PDA
    // rent back to owner. Anchor's `close = owner` constraint on session
    // handles the PDA close + rent refund.
    pub fn close(ctx: Context<Close>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let session = &ctx.accounts.session;

        let permitted = match session.state {
            s if s == SessionState::Revoked as u8 => {
                now >= session.expiry + REVOKE_GRACE_SECS
            }
            s if s == SessionState::Active as u8 => now >= session.expiry,
            _ => false,
        };
        require!(permitted, SessionError::CloseNotPermitted);

        let owner_key = session.owner;
        let server_key = session.server;
        let session_id_bytes = session.session_id;
        let bump = session.bump;
        let signer_seeds: &[&[&[u8]]] = &[&[
            SESSION_SEED,
            owner_key.as_ref(),
            server_key.as_ref(),
            session_id_bytes.as_ref(),
            &[bump],
        ]];

        // Drain remaining escrow → owner_destination.
        let escrow_balance = ctx.accounts.escrow.amount;
        if escrow_balance > 0 {
            transfer_checked(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    TransferChecked {
                        from: ctx.accounts.escrow.to_account_info(),
                        to: ctx.accounts.owner_destination.to_account_info(),
                        mint: ctx.accounts.mint.to_account_info(),
                        authority: ctx.accounts.session.to_account_info(),
                    },
                    signer_seeds,
                ),
                escrow_balance,
                ctx.accounts.mint.decimals,
            )?;
        }

        // Close the escrow token account, returning rent to owner.
        close_account(CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            CloseAccount {
                account: ctx.accounts.escrow.to_account_info(),
                destination: ctx.accounts.owner.to_account_info(),
                authority: ctx.accounts.session.to_account_info(),
            },
            signer_seeds,
        ))?;

        // Session PDA itself is closed by the `close = owner` constraint
        // on the session account in the Close context struct.
        Ok(())
    }
}

// ============================================================================
// Ed25519 precompile verification helper (used by Settle and re-exported
// for use by mppsol_cpi). See spec/session.md §6.
// ============================================================================

pub fn verify_ed25519_precompile_batch(
    instructions_sysvar: &AccountInfo,
    expected_pubkey: &[u8; 32],
    messages: &[Vec<u8>],
    signatures: &[[u8; 64]],
) -> Result<()> {
    let n = messages.len();
    require!(n > 0, SessionError::BadBatchSize);
    require!(signatures.len() == n, SessionError::BadBatchSize);

    // Find the Ed25519 precompile instruction in this transaction.
    let mut idx: u16 = 0;
    let precompile_ix = loop {
        if idx > 64 {
            return err!(SessionError::MissingPrecompile);
        }
        match load_instruction_at_checked(idx as usize, instructions_sysvar) {
            Ok(ix) => {
                if ix.program_id == ED25519_PROGRAM_ID {
                    break ix;
                }
                idx += 1;
            }
            Err(_) => return err!(SessionError::MissingPrecompile),
        }
    };

    let data = &precompile_ix.data;
    require!(
        data.len() >= ED25519_HEADER_SIZE,
        SessionError::MissingPrecompile,
    );
    require!(data[0] as usize == n, SessionError::MissingPrecompile);
    require!(
        data.len() >= ED25519_HEADER_SIZE + n * ED25519_SIG_OFFSETS_SIZE,
        SessionError::MissingPrecompile,
    );

    for i in 0..n {
        let entry_start = ED25519_HEADER_SIZE + i * ED25519_SIG_OFFSETS_SIZE;
        let entry = &data[entry_start..entry_start + ED25519_SIG_OFFSETS_SIZE];

        let sig_offset = u16::from_le_bytes([entry[0], entry[1]]) as usize;
        let pk_offset = u16::from_le_bytes([entry[4], entry[5]]) as usize;
        let msg_offset = u16::from_le_bytes([entry[8], entry[9]]) as usize;
        let msg_size = u16::from_le_bytes([entry[10], entry[11]]) as usize;

        require!(
            pk_offset.checked_add(32).map_or(false, |e| e <= data.len()),
            SessionError::MissingPrecompile,
        );
        require!(
            sig_offset.checked_add(64).map_or(false, |e| e <= data.len()),
            SessionError::MissingPrecompile,
        );
        require!(
            msg_offset.checked_add(msg_size).map_or(false, |e| e <= data.len()),
            SessionError::MissingPrecompile,
        );

        require!(
            &data[pk_offset..pk_offset + 32] == expected_pubkey,
            SessionError::InvalidSignature,
        );
        require!(
            &data[sig_offset..sig_offset + 64] == signatures[i].as_slice(),
            SessionError::InvalidSignature,
        );
        require!(
            &data[msg_offset..msg_offset + msg_size] == messages[i].as_slice(),
            SessionError::InvalidSignature,
        );
    }

    Ok(())
}

// ============================================================================
// Instruction args
// ============================================================================

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct OpenArgs {
    pub authorized_signer: Pubkey,
    pub server: Pubkey,
    pub total_cap: u64,
    pub expiry: i64,
    pub session_id: [u8; 16],
    pub cluster_genesis_hash: [u8; 32],
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct SettleArgs {
    pub debits: Vec<Debit>,
    pub signatures: Vec<[u8; 64]>,
}

// Canonical 104-byte off-chain debit message. Layout MUST match
// @mppsol/core's encodeDebit and spec/wire.md §4.2.
#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct Debit {
    pub session: [u8; 32],
    pub nonce: [u8; 32],
    pub amount: u64,
    pub expiry: i64,
    pub sequence: u64,
    pub domain_sep: [u8; 16],
}

// ============================================================================
// Account contexts
// ============================================================================

#[derive(Accounts)]
#[instruction(args: OpenArgs)]
pub struct Open<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    #[account(
        init,
        payer = owner,
        space = 8 + Session::INIT_SPACE,
        seeds = [
            SESSION_SEED,
            owner.key().as_ref(),
            args.server.as_ref(),
            args.session_id.as_ref(),
        ],
        bump,
    )]
    pub session: Account<'info, Session>,

    pub mint: InterfaceAccount<'info, Mint>,

    #[account(
        init,
        payer = owner,
        associated_token::mint = mint,
        associated_token::authority = session,
    )]
    pub escrow: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        constraint = owner_source.mint == mint.key(),
        constraint = owner_source.owner == owner.key(),
    )]
    pub owner_source: InterfaceAccount<'info, TokenAccount>,

    pub token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Settle<'info> {
    pub server: Signer<'info>,

    #[account(
        mut,
        seeds = [
            SESSION_SEED,
            session.owner.as_ref(),
            session.server.as_ref(),
            session.session_id.as_ref(),
        ],
        bump = session.bump,
        constraint = session.server == server.key(),
    )]
    pub session: Account<'info, Session>,

    pub mint: InterfaceAccount<'info, Mint>,

    #[account(
        mut,
        constraint = escrow.key() == session.escrow,
    )]
    pub escrow: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        constraint = server_token_account.mint == mint.key(),
        constraint = server_token_account.owner == server.key(),
    )]
    pub server_token_account: InterfaceAccount<'info, TokenAccount>,

    pub token_program: Interface<'info, TokenInterface>,

    /// Sysvar: Instructions — used to verify the Ed25519 precompile
    /// companion instruction.
    /// CHECK: this account is the well-known instructions sysvar.
    #[account(address = anchor_lang::solana_program::sysvar::instructions::ID)]
    pub instructions_sysvar: AccountInfo<'info>,
}

#[derive(Accounts)]
pub struct Topup<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    #[account(
        mut,
        seeds = [
            SESSION_SEED,
            session.owner.as_ref(),
            session.server.as_ref(),
            session.session_id.as_ref(),
        ],
        bump = session.bump,
        constraint = session.owner == owner.key(),
    )]
    pub session: Account<'info, Session>,

    pub mint: InterfaceAccount<'info, Mint>,

    #[account(
        mut,
        constraint = escrow.key() == session.escrow,
    )]
    pub escrow: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        constraint = owner_source.mint == mint.key(),
        constraint = owner_source.owner == owner.key(),
    )]
    pub owner_source: InterfaceAccount<'info, TokenAccount>,

    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct Revoke<'info> {
    pub signer: Signer<'info>,

    #[account(mut)]
    pub session: Account<'info, Session>,
}

#[derive(Accounts)]
pub struct Close<'info> {
    #[account(mut, constraint = owner.key() == session.owner)]
    pub owner: Signer<'info>,

    #[account(
        mut,
        close = owner,
        seeds = [
            SESSION_SEED,
            session.owner.as_ref(),
            session.server.as_ref(),
            session.session_id.as_ref(),
        ],
        bump = session.bump,
    )]
    pub session: Account<'info, Session>,

    pub mint: InterfaceAccount<'info, Mint>,

    #[account(
        mut,
        constraint = escrow.key() == session.escrow,
    )]
    pub escrow: InterfaceAccount<'info, TokenAccount>,

    #[account(
        mut,
        constraint = owner_destination.mint == mint.key(),
        constraint = owner_destination.owner == owner.key(),
    )]
    pub owner_destination: InterfaceAccount<'info, TokenAccount>,

    pub token_program: Interface<'info, TokenInterface>,
}
