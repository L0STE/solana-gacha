// Host builds (unit tests, clippy) compile the handlers without an entrypoint
// that calls them; silence the resulting dead-code noise there only.
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

pub mod admin;
pub use admin::*;

pub mod player;
pub use player::*;

pub mod operator;
pub use operator::*;

pub mod assets;
pub mod events;
pub mod helpers;
pub mod inventory;
pub mod state;

pub use gacha_core::{constants, errors, ID};

fn process_instruction(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    match instruction_data.split_first() {
        // Operator Instructions - Discriminators from 20. Settle first: it runs once per pull.
        Some((Settle::DISCRIMINATOR, data)) => Settle::try_from((data, accounts))?.process(),
        Some((Deliver::DISCRIMINATOR, data)) => Deliver::try_from((data, accounts))?.process(),

        // Player Instructions - Discriminators from 10
        Some((Buy::DISCRIMINATOR, data)) => Buy::try_from((data, accounts))?.process(),
        Some((Refund::DISCRIMINATOR, _)) => Refund::try_from(accounts)?.process(),
        Some((Buyback::DISCRIMINATOR, data)) => Buyback::try_from((data, accounts))?.process(),

        // Admin Instructions - Discriminators from 0
        Some((CreatePool::DISCRIMINATOR, data)) => {
            CreatePool::try_from((data, accounts))?.process()
        }
        Some((DepositItem::DISCRIMINATOR, data)) => {
            DepositItem::try_from((data, accounts))?.process()
        }
        Some((Withdraw::DISCRIMINATOR, data)) => Withdraw::try_from((data, accounts))?.process(),
        Some((SetStatus::DISCRIMINATOR, data)) => SetStatus::try_from((data, accounts))?.process(),
        Some((Reclaim::DISCRIMINATOR, _)) => Reclaim::try_from(accounts)?.process(),

        // Self-CPI EmitEvent - Discriminator 255
        Some((&constants::EVENT_DISCRIMINATOR, _)) => events::emit_event(accounts),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}
