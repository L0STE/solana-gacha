use crate::{
    assets::Transfer,
    constants::*,
    errors::GachaError,
    helpers::close,
    state::{Pool, Pull},
};
use pinocchio::{
    account_info::AccountInfo,
    instruction::{Seed, Signer},
    program_error::ProgramError,
    ProgramResult,
};

/// Deliver one settled Core asset. Anyone may pay; ownership goes only to the
/// recorded buyer. Final delivery closes the pull and returns its rent to them.
/// Accounts: payer, pool, pull, buyer, asset, collection (or Core sentinel),
/// Core program, system program. Payer, pull, buyer and asset are writable.
pub(crate) struct Deliver<'a> {
    payer: &'a AccountInfo,
    pool: &'a AccountInfo,
    pull: &'a AccountInfo,
    buyer: &'a AccountInfo,
    asset: &'a AccountInfo,
    collection: &'a AccountInfo,
    core_program: &'a AccountInfo,
    system_program: &'a AccountInfo,
    outcome: usize,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for Deliver<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        let [payer, pool, pull, buyer, asset, collection, core_program, system_program] = accounts
        else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };
        let [outcome] = data else {
            return Err(ProgramError::InvalidInstructionData);
        };
        if !payer.is_signer() {
            return Err(GachaError::NotSigner.into());
        }
        if !payer.is_writable()
            || !pull.is_writable()
            || !asset.is_writable()
            || !buyer.is_writable()
        {
            return Err(GachaError::NotMutable.into());
        }
        crate::state::check_pool(pool)?;
        crate::state::check_pull(pull)?;
        let header = unsafe { Pull::from_bytes_unchecked(pull.borrow_data_unchecked()) };
        if header.pool() != pool.key() || header.status() != STATUS_SETTLED {
            return Err(GachaError::InvalidPullStatus.into());
        }
        if header.buyer() != buyer.key() {
            return Err(GachaError::InvalidBuyer.into());
        }
        let outcome = *outcome as usize;
        if outcome >= header.count() as usize {
            return Err(GachaError::InvalidOutcome.into());
        }
        let (tier, expected_asset) = header.outcome(outcome);
        if tier == DELIVERED {
            return Err(GachaError::InvalidOutcome.into());
        }
        if expected_asset != asset.key() {
            return Err(GachaError::InvalidAsset.into());
        }
        Ok(Self {
            payer,
            pool,
            pull,
            buyer,
            asset,
            collection,
            core_program,
            system_program,
            outcome,
        })
    }
}

impl Deliver<'_> {
    pub(crate) const DISCRIMINATOR: u8 = 21;

    pub(crate) fn process(self) -> ProgramResult {
        let pool = unsafe { Pool::from_bytes_unchecked(self.pool.borrow_data_unchecked()) };
        let id = pool.id().to_le_bytes();
        let bump = [pool.bump()];
        let seeds = [
            Seed::from(POOL_SEED),
            Seed::from(pool.authority()),
            Seed::from(&id),
            Seed::from(&bump),
        ];
        Transfer {
            payer: self.payer,
            authority: self.pool,
            new_owner: self.buyer,
            asset: self.asset,
            collection: self.collection,
            core_program: self.core_program,
            system_program: self.system_program,
        }
        .invoke_signed(&[Signer::from(&seeds)])?;
        let pull = unsafe { Pull::from_bytes_unchecked_mut(self.pull.borrow_mut_data_unchecked()) };
        pull.mark_delivered(self.outcome);
        if pull.all_delivered() {
            close(self.pull, self.buyer)?;
        }
        Ok(())
    }
}
