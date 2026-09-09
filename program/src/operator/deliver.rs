use crate::assets::Transfer;
use crate::constants::{DELIVERED, STATUS_SETTLED};
use crate::errors::GachaError;
use crate::events::DeliverEvent;
use crate::helpers::close;
use crate::state::{Load, Pool, Pull};
use core::mem::size_of;
use pinocchio::instruction::Signer;
use pinocchio::log::sol_log;
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, ProgramResult};

/// # Deliver
///
/// Transfer one recorded award from the pool PDA to its buyer. Anyone may pay;
/// ownership goes only to the recorded buyer. The final delivery closes the
/// Pull and returns its rent to the buyer.
///
/// Accounts:
///
/// 1. payer:               [signer, mut]   relayer paying the Core transfer
/// 2. pool:
/// 3. pull:                [mut]
/// 4. buyer:               [mut]           receives the asset and, last, the Pull rent
/// 5. asset:               [mut]
/// 6. collection:                          the asset's collection, or the Core program
/// 7. core_program:        [executable]
/// 8. system_program:      [executable]
/// 9. event_authority:
/// 10. program:            [executable]    this program, for the event CPI
///
/// Parameters:
/// 1. outcome: u8,                 // index into the Pull's recorded awards
///
/// Account Checks:
/// - Payer: signer
/// - Pool: deserialized
/// - Pull: writable, deserialized, belongs to the pool, settled
/// - Buyer: writable and equal to pull.buyer
/// - Asset: must be the recorded award, which needs the outcome index, so it runs in process
/// - Collection, CoreProgram, SystemProgram: no need to check since Core validates them on transfer
/// - EventAuthority, Program: no need to check since the event CPI fails otherwise
///
/// Instruction Checks:
/// - Outcome: below pull.count and not yet delivered, checked in process
///
/// Event Data:
/// - discriminator: u8, (255u8, 21u8)
/// - pull: Pubkey,
/// - asset: Pubkey,
/// - outcome: u8,
pub struct DeliverAccounts<'a> {
    pub payer: &'a AccountInfo,
    pub pool: &'a AccountInfo,
    pub pull: &'a AccountInfo,
    pub buyer: &'a AccountInfo,
    pub asset: &'a AccountInfo,
    pub collection: &'a AccountInfo,
    pub core_program: &'a AccountInfo,
    pub system_program: &'a AccountInfo,
    pub event_authority: &'a AccountInfo,
    pub program: &'a AccountInfo,
}

impl<'a> TryFrom<&'a [AccountInfo]> for DeliverAccounts<'a> {
    type Error = ProgramError;

    fn try_from(accounts: &'a [AccountInfo]) -> Result<Self, Self::Error> {
        let [payer, pool, pull, buyer, asset, collection, core_program, system_program, event_authority, program] =
            accounts
        else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };

        // Account Checks
        if !payer.is_signer() {
            return Err(GachaError::NotSigner.into());
        }
        if !pull.is_writable() || !buyer.is_writable() {
            return Err(GachaError::NotMutable.into());
        }

        Pool::load(pool)?;
        let pull_data = Pull::load(pull)?;
        if pull_data.pool().ne(pool.key()) {
            return Err(GachaError::PoolMismatch.into());
        }
        if pull_data.status().ne(&STATUS_SETTLED) {
            return Err(GachaError::InvalidPullStatus.into());
        }
        if pull_data.buyer().ne(buyer.key()) {
            return Err(GachaError::InvalidBuyer.into());
        }

        // Return the accounts
        Ok(Self {
            payer,
            pool,
            pull,
            buyer,
            asset,
            collection,
            core_program,
            system_program,
            event_authority,
            program,
        })
    }
}

pub struct DeliverInstructionData {
    pub outcome: u8,
}

impl<'a> TryFrom<&'a [u8]> for DeliverInstructionData {
    type Error = ProgramError;

    fn try_from(data: &'a [u8]) -> Result<Self, Self::Error> {
        if data.len().ne(&size_of::<u8>()) {
            return Err(ProgramError::InvalidInstructionData);
        }

        let outcome = data[0];

        Ok(Self { outcome })
    }
}

pub struct Deliver<'a> {
    pub accounts: DeliverAccounts<'a>,
    pub instruction_data: DeliverInstructionData,
}

impl<'a> TryFrom<(&'a [u8], &'a [AccountInfo])> for Deliver<'a> {
    type Error = ProgramError;

    fn try_from((data, accounts): (&'a [u8], &'a [AccountInfo])) -> Result<Self, Self::Error> {
        sol_log("Deliver");

        let accounts = DeliverAccounts::try_from(accounts)?;
        let instruction_data = DeliverInstructionData::try_from(data)?;

        // Return the initialized struct
        Ok(Self {
            accounts,
            instruction_data,
        })
    }
}

impl<'a> Deliver<'a> {
    pub const DISCRIMINATOR: &'a u8 = &21;

    pub fn process(&mut self) -> ProgramResult {
        // The outcome must be a recorded, undelivered award for this asset
        let pull = Pull::load_mut(self.accounts.pull)?;
        let outcome = self.instruction_data.outcome as usize;
        if outcome >= pull.count() as usize {
            return Err(GachaError::InvalidOutcome.into());
        }
        let (tier, asset) = pull.outcome(outcome);
        if tier.eq(&DELIVERED) {
            return Err(GachaError::InvalidOutcome.into());
        }
        if asset.ne(self.accounts.asset.key()) {
            return Err(GachaError::InvalidAsset.into());
        }

        // Transfer the award to the buyer
        let pool = Pool::load(self.accounts.pool)?;
        let seeds = pool.signer_seeds();
        let seeds = seeds.as_seeds();
        Transfer {
            payer: self.accounts.payer,
            authority: self.accounts.pool,
            new_owner: self.accounts.buyer,
            asset: self.accounts.asset,
            collection: self.accounts.collection,
            core_program: self.accounts.core_program,
            system_program: self.accounts.system_program,
        }
        .invoke_signed(&[Signer::from(&seeds)])?;

        // Mark it delivered; the last delivery closes the Pull to the buyer
        pull.mark_delivered(outcome);
        if pull.all_delivered() {
            close(self.accounts.pull, self.accounts.buyer)?;
        }

        // Log the Deliver Event
        DeliverEvent {
            pull: self.accounts.pull.key(),
            asset: self.accounts.asset.key(),
            outcome: self.instruction_data.outcome,
        }
        .emit(self.accounts.event_authority, self.accounts.program)
    }
}
