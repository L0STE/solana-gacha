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

mod constants;
mod errors;
mod helpers;
mod inventory;
mod state;

#[cfg(not(target_os = "solana"))]
pub mod client;

// 4X8u1YspRi6Z9TkZNb8qxNdwPLs5vDi7VRC2DhTheeKp
pub const ID: Pubkey = [
    0x34, 0x4b, 0x69, 0xf5, 0x0e, 0xcb, 0x2c, 0x1c, 0x62, 0x67, 0x39, 0x5f, 0xb3, 0x3c, 0x90, 0x83,
    0x0f, 0xfe, 0x58, 0x4b, 0x95, 0x08, 0xf2, 0x75, 0xbf, 0x2c, 0x17, 0x8b, 0x7c, 0x68, 0xa5, 0xff,
];

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
        CreatePool::DISCRIMINATOR => CreatePool::try_from((data, accounts))?.process(),
        DepositItem::DISCRIMINATOR => DepositItem::try_from((data, accounts))?.process(),
        Withdraw::DISCRIMINATOR => Withdraw::try_from((data, accounts))?.process(),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

#[cfg(feature = "test-utils")]
pub mod test_utils {
    pub use crate::constants::*;
    pub use crate::errors::GachaError;
    pub use crate::state::{Item, Pool, Pull, Tier};
}
