#![cfg_attr(not(target_os = "solana"), allow(dead_code, unused_imports))]

use pinocchio::{
    account_info::AccountInfo, default_panic_handler, no_allocator, program_entrypoint,
    program_error::ProgramError, pubkey::Pubkey, ProgramResult,
};

// The program never allocates; `no_allocator!` turns an accidental heap use
// into a hard failure.
program_entrypoint!(process_instruction);
no_allocator!();
default_panic_handler!();

mod admin;
use admin::*;

mod player;
use player::*;

mod operator;
use operator::*;

use gacha_core::{constants, errors};
mod assets;
mod helpers;
mod inventory;
mod state;

pub use gacha_core::ID;

fn process_instruction(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    let (discriminator, data) = instruction_data
        .split_first()
        .ok_or(ProgramError::InvalidInstructionData)?;

    // Settle first: it is the instruction that runs once per pull.
    match *discriminator {
        Settle::DISCRIMINATOR => Settle::try_from((data, accounts))?.process(),
        Deliver::DISCRIMINATOR => Deliver::try_from((data, accounts))?.process(),
        Buy::DISCRIMINATOR => Buy::try_from((data, accounts))?.process(),
        Refund::DISCRIMINATOR => Refund::try_from(accounts)?.process(),
        Buyback::DISCRIMINATOR => Buyback::try_from((data, accounts))?.process(),
        CreatePool::DISCRIMINATOR => CreatePool::try_from((data, accounts))?.process(),
        DepositItem::DISCRIMINATOR => DepositItem::try_from((data, accounts))?.process(),
        Withdraw::DISCRIMINATOR => Withdraw::try_from((data, accounts))?.process(),
        SetStatus::DISCRIMINATOR => SetStatus::try_from((data, accounts))?.process(),
        Reclaim::DISCRIMINATOR => Reclaim::try_from((data, accounts))?.process(),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}
