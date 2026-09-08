//! Account lifecycle, SPL token inspection, hashing and the settlement event.

use pinocchio::{
    account_info::AccountInfo,
    instruction::{Seed, Signer},
    program_error::ProgramError,
    pubkey::Pubkey,
    sysvars::{rent::Rent, Sysvar},
    ProgramResult,
};

/// Create a program-owned PDA, rent paid by `payer`. `bump` is the last seed.
#[inline(always)]
pub(crate) fn create_pda(
    payer: &AccountInfo,
    account: &AccountInfo,
    space: usize,
    seeds: &[Seed],
) -> ProgramResult {
    if !account.is_owned_by(&pinocchio_system::ID) || account.data_len() != 0 {
        return Err(ProgramError::AccountAlreadyInitialized);
    }
    let lamports = Rent::get()?.minimum_balance(space);
    if account.lamports() == 0 {
        return pinocchio_system::instructions::CreateAccount {
            from: payer,
            to: account,
            lamports,
            space: space as u64,
            owner: &crate::ID,
        }
        .invoke_signed(&[Signer::from(seeds)]);
    }
    // Anyone can send lamports to a PDA before initialization.
    let missing = lamports.saturating_sub(account.lamports());
    if missing > 0 {
        pinocchio_system::instructions::Transfer {
            from: payer,
            to: account,
            lamports: missing,
        }
        .invoke()?;
    }
    pinocchio_system::instructions::Allocate {
        account,
        space: space as u64,
    }
    .invoke_signed(&[Signer::from(seeds)])?;
    pinocchio_system::instructions::Assign {
        account,
        owner: &crate::ID,
    }
    .invoke_signed(&[Signer::from(seeds)])
}

/// Move every lamport out and close a program-owned account.
#[inline(always)]
pub(crate) fn close(account: &AccountInfo, to: &AccountInfo) -> ProgramResult {
    unsafe {
        *to.borrow_mut_lamports_unchecked() = to
            .lamports()
            .checked_add(account.lamports())
            .ok_or(ProgramError::ArithmeticOverflow)?;
    }
    account.close()
}

#[cfg(target_os = "solana")]
#[inline(always)]
pub(crate) fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut out = core::mem::MaybeUninit::<[u8; 32]>::uninit();
    unsafe {
        pinocchio::syscalls::sol_sha256(
            parts as *const _ as *const u8,
            parts.len() as u64,
            out.as_mut_ptr() as *mut u8,
        );
        out.assume_init()
    }
}

/// Host builds (unit tests, clippy) have no syscall to link against.
#[cfg(not(target_os = "solana"))]
pub(crate) fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// The ATA of `owner` for `mint`. Used only at pool creation and deposit;
/// settlement and delivery validate stored identities without deriving ATAs.
#[inline(always)]
pub(crate) fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    pinocchio::pubkey::find_program_address(
        &[owner.as_ref(), pinocchio_token::ID.as_ref(), mint.as_ref()],
        &pinocchio_associated_token_account::ID,
    )
    .0
}

/// Read `(owner, mint, amount)` after validating the legacy SPL account layout.
#[inline(always)]
pub(crate) fn token_account(
    account: &AccountInfo,
) -> Result<(&Pubkey, &Pubkey, u64), ProgramError> {
    if !account.is_owned_by(&pinocchio_token::ID) || account.data_len() != 165 {
        return Err(crate::errors::GachaError::InvalidTokenAddress.into());
    }
    let data = unsafe { account.borrow_data_unchecked() };
    let (owner, mint) = crate::state::token_account_owner_and_mint(data);
    Ok((owner, mint, crate::state::token_account_amount(data)))
}

/// Proof and awarded mints. Auditing selection also requires the pool's
/// inventory history, since deposits remain open while pulls are pending.
const SETTLE_DISCRIMINATOR: u8 = 0;

#[inline(always)]
pub(crate) fn log_settle_event(
    pull: &Pubkey,
    alpha: &[u8; 32],
    proof: &[u8; 80],
    beta: &[u8; 64],
    outcomes: &[u8],
) {
    let head = [SETTLE_DISCRIMINATOR];
    let data: [&[u8]; 6] = [&head, pull, alpha, proof, beta, outcomes];
    pinocchio::log::sol_log_data(&data);
}
