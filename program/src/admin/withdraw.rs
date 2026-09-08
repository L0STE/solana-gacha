use crate::errors::GachaError;
use crate::state::Pool;
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, ProgramResult};

/// # Withdraw
///
/// Take unreserved funds out of the vault. Every pending draw retains its
/// full payment plus timeout penalty. With no pending draws, all funds are free.
///
/// Accounts:
///
/// 1. authority:      [signer]
/// 2. pool:
/// 3. vault:          [mut]
/// 4. destination:    [mut]
/// 5. token_program:  [executable]
///
/// Parameters:
/// 1. amount: u64,
///
/// Account Checks:
/// - Authority: signer, equals pool.authority
/// - Pool: Pool::check
/// - Vault: equals pool.vault
/// - Destination: the transfer fails on a wrong mint
///
/// Instruction Checks:
/// - amount ≤ free balance
pub(crate) struct Withdraw<'a> {
    pool: &'a AccountInfo,
    vault: &'a AccountInfo,
    destination: &'a AccountInfo,
    amount: u64,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for Withdraw<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        pinocchio::log::sol_log("Withdraw");
        if data.len() != 8 {
            return Err(ProgramError::InvalidInstructionData);
        }
        let [authority, pool, vault, destination, _token_program] = accounts else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };
        if !authority.is_signer() {
            return Err(GachaError::NotSigner.into());
        }
        crate::state::check_pool(pool)?;
        let header = unsafe { Pool::from_bytes_unchecked(pool.borrow_data_unchecked()) };
        if header.authority().ne(authority.key()) {
            return Err(GachaError::InvalidAuthority.into());
        }
        if header.vault().ne(vault.key()) {
            return Err(GachaError::InvalidTokenAddress.into());
        }
        Ok(Self {
            pool,
            vault,
            destination,
            amount: u64::from_le_bytes(data.try_into().unwrap()),
        })
    }
}

impl Withdraw<'_> {
    pub(crate) const DISCRIMINATOR: u8 = 2;

    pub(crate) fn process(self) -> ProgramResult {
        crate::helpers::pay_surplus(self.pool, self.vault, self.destination, self.amount)
    }
}
