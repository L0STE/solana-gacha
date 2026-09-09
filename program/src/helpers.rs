//! Account lifecycle, SPL token views, hashing and vault payments.
//! Checks live in `check_*`/`load` helpers; the rest only executes.

use crate::{
    constants::*,
    errors::GachaError,
    state::{Load, Pool},
};
use core::mem::MaybeUninit;
use pinocchio::{
    account_info::AccountInfo,
    instruction::{Seed, Signer},
    program_error::ProgramError,
    pubkey::Pubkey,
    sysvars::{rent::Rent, Sysvar},
    ProgramResult,
};

/// An account about to be created must be an empty system account.
#[inline(always)]
pub fn check_uninitialized(account: &AccountInfo) -> ProgramResult {
    if !account.is_owned_by(&pinocchio_system::ID) || account.data_len() != 0 {
        return Err(GachaError::AlreadyInitialized.into());
    }
    Ok(())
}

/// Create a program-owned PDA, rent paid by `payer`. `bump` is the last seed.
#[inline(always)]
pub fn create_pda(
    payer: &AccountInfo,
    account: &AccountInfo,
    space: usize,
    seeds: &[Seed],
) -> ProgramResult {
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
pub fn close(account: &AccountInfo, to: &AccountInfo) -> ProgramResult {
    // SAFETY: the program never holds a checked borrow on lamports, and `to`
    // is distinct from `account` at every call site.
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
pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut out = MaybeUninit::<[u8; 32]>::uninit();
    // SAFETY: `sol_sha256` takes an array of slices and fills exactly 32 bytes.
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
pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// The payment ATA of `owner` for `mint`, used to validate the pool vault.
#[inline(always)]
pub fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    pinocchio::pubkey::find_program_address(
        &[owner.as_ref(), pinocchio_token::ID.as_ref(), mint.as_ref()],
        &pinocchio_associated_token_account::ID,
    )
    .0
}

/// The fields of a legacy SPL token account this program reads.
pub struct TokenAccount<'a> {
    pub owner: &'a Pubkey,
    pub mint: &'a Pubkey,
    pub amount: u64,
}

impl<'a> TokenAccount<'a> {
    /// Validate owner program and layout, then view the fields in place.
    #[inline(always)]
    pub fn load(account: &'a AccountInfo) -> Result<Self, ProgramError> {
        if !account.is_owned_by(&pinocchio_token::ID) || account.data_len() != 165 {
            return Err(GachaError::InvalidTokenAddress.into());
        }
        // SAFETY: the length check above covers every offset read; `Pubkey`
        // has alignment 1, and the program holds no mutable borrow of a token account.
        let data = unsafe { account.borrow_data_unchecked() };
        Ok(Self {
            mint: unsafe { &*(data.as_ptr() as *const Pubkey) },
            owner: unsafe { &*(data.as_ptr().add(32) as *const Pubkey) },
            amount: u64::from_le_bytes(data[64..72].try_into().unwrap()),
        })
    }
}

/// Vault balance left after reserving every pending refund and penalty.
#[inline(always)]
pub fn spendable(pool: &Pool, vault: &AccountInfo) -> Result<u64, ProgramError> {
    TokenAccount::load(vault)?
        .amount
        .checked_sub(pool.refund_amount(pool.pending_draws())?)
        .ok_or(GachaError::InsufficientBalance.into())
}

/// Pay `amount` from the vault, signed by the pool PDA. Callers check `spendable`.
#[inline(always)]
pub fn pay(
    pool_account: &AccountInfo,
    vault: &AccountInfo,
    destination: &AccountInfo,
    amount: u64,
) -> ProgramResult {
    let pool = Pool::load(pool_account)?;
    let seeds = pool.signer_seeds();
    let seeds = seeds.as_seeds();
    pinocchio_token::instructions::Transfer {
        from: vault,
        to: destination,
        authority: pool_account,
        amount,
    }
    .invoke_signed(&[Signer::from(&seeds)])
}
