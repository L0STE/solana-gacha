//! One event per instruction, emitted through a CPI to this program signed by
//! the event authority PDA. The `sink` instruction accepts only that signer,
//! so events live in inner instructions and no other program can forge them.
//! The CPI needs the program's own account, so every instruction carries
//! `event_authority` and `program` as its last two accounts; the CPI itself
//! rejects wrong ones, so handlers do not check them.
//!
//! Wire layout: `[EVENT_DISCRIMINATOR, instruction discriminator, fields in order]`.

use crate::constants::{
    EVENT_AUTHORITY, EVENT_AUTHORITY_BUMP, EVENT_AUTHORITY_SEED, EVENT_DISCRIMINATOR,
};
use crate::errors::GachaError;
use core::mem::MaybeUninit;
use pinocchio::{
    account_info::AccountInfo,
    cpi::invoke_signed,
    instruction::{AccountMeta, Instruction, Seed, Signer},
    program_error::ProgramError,
    pubkey::Pubkey,
    ProgramResult,
};

/// The instruction every event CPI targets. It does nothing; the inner
/// instruction's data is the event. Only the event authority PDA can sign it,
/// and only this program can sign for that PDA.
pub fn sink(accounts: &[AccountInfo]) -> ProgramResult {
    let [event_authority, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    if !event_authority.is_signer() || event_authority.key().ne(&EVENT_AUTHORITY) {
        return Err(GachaError::InvalidEventAuthority.into());
    }
    Ok(())
}

pub struct CreatePoolEvent<'a> {
    pub pool: &'a Pubkey,
}

pub struct DepositItemEvent<'a> {
    pub pool: &'a Pubkey,
    pub asset: &'a Pubkey,
    pub tier: u8,
    pub position: u32,
}

pub struct WithdrawEvent<'a> {
    pub pool: &'a Pubkey,
    pub amount: u64,
}

pub struct SetStatusEvent<'a> {
    pub pool: &'a Pubkey,
    pub status: u8,
}

pub struct ReclaimEvent<'a> {
    pub pool: &'a Pubkey,
    pub asset: &'a Pubkey,
    pub position: u32,
}

pub struct BuyEvent<'a> {
    pub pool: &'a Pubkey,
    pub pull: &'a Pubkey,
    pub buyer: &'a Pubkey,
    pub index: u64,
    pub count: u8,
}

pub struct RefundEvent<'a> {
    pub pool: &'a Pubkey,
    pub pull: &'a Pubkey,
    pub amount: u64,
}

pub struct BuybackEvent<'a> {
    pub pool: &'a Pubkey,
    pub asset: &'a Pubkey,
    pub seller: &'a Pubkey,
    pub price: u64,
    pub position: u32,
}

pub struct SettleEvent<'a> {
    pub pool: &'a Pubkey,
    pub pull: &'a Pubkey,
    pub alpha: &'a [u8; 32],
    pub proof: &'a [u8; 80],
    pub beta: &'a [u8; 64],
    /// `count × (tier: u8, asset: Pubkey)`, packed as stored in the Pull.
    pub outcomes: &'a [u8],
}

pub struct DeliverEvent<'a> {
    pub pull: &'a Pubkey,
    pub asset: &'a Pubkey,
    pub outcome: u8,
}

/// `emit` for one event struct: the instruction discriminator, the buffer
/// size, and the fields in wire order.
macro_rules! event {
    ($event:ident, $discriminator:expr, $size:expr, |$this:ident| [$($field:expr),* $(,)?]) => {
        impl $event<'_> {
            pub fn emit(&self, event_authority: &AccountInfo, program: &AccountInfo) -> ProgramResult {
                let $this = self;
                let mut data = EventData::<$size>::new($discriminator);
                $(data.push($field);)*
                data.emit(event_authority, program)
            }
        }
    };
}

event!(CreatePoolEvent, crate::CreatePool::DISCRIMINATOR, 34, |e| [
    e.pool
]);
event!(
    DepositItemEvent,
    crate::DepositItem::DISCRIMINATOR,
    71,
    |e| [e.pool, e.asset, &[e.tier], &e.position.to_le_bytes(),]
);
event!(WithdrawEvent, crate::Withdraw::DISCRIMINATOR, 42, |e| [
    e.pool,
    &e.amount.to_le_bytes()
]);
event!(SetStatusEvent, crate::SetStatus::DISCRIMINATOR, 35, |e| [
    e.pool,
    &[e.status]
]);
event!(ReclaimEvent, crate::Reclaim::DISCRIMINATOR, 70, |e| [
    e.pool,
    e.asset,
    &e.position.to_le_bytes(),
]);
event!(BuyEvent, crate::Buy::DISCRIMINATOR, 107, |e| [
    e.pool,
    e.pull,
    e.buyer,
    &e.index.to_le_bytes(),
    &[e.count],
]);
event!(RefundEvent, crate::Refund::DISCRIMINATOR, 74, |e| [
    e.pool,
    e.pull,
    &e.amount.to_le_bytes(),
]);
event!(BuybackEvent, crate::Buyback::DISCRIMINATOR, 110, |e| [
    e.pool,
    e.asset,
    e.seller,
    &e.price.to_le_bytes(),
    &e.position.to_le_bytes(),
]);
event!(SettleEvent, crate::Settle::DISCRIMINATOR, 572, |e| [
    e.pool, e.pull, e.alpha, e.proof, e.beta, e.outcomes,
]);
event!(DeliverEvent, crate::Deliver::DISCRIMINATOR, 67, |e| [
    e.pull,
    e.asset,
    &[e.outcome]
]);

/// Fixed-size, uninitialized wire buffer; only bytes below `len` are ever read.
struct EventData<const N: usize> {
    data: [MaybeUninit<u8>; N],
    len: usize,
}

impl<const N: usize> EventData<N> {
    #[inline(always)]
    fn new(instruction: &u8) -> Self {
        let mut data = [MaybeUninit::uninit(); N];
        data[0].write(EVENT_DISCRIMINATOR);
        data[1].write(*instruction);
        Self { data, len: 2 }
    }

    #[inline(always)]
    fn push(&mut self, bytes: &[u8]) {
        let end = self.len + bytes.len();
        // SAFETY: `end <= N` is checked by the slice below, and the source and
        // destination cannot overlap. One memcpy: a byte loop here measured
        // 230 CU per draw on the settle event.
        let slot = &mut self.data[self.len..end];
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                slot.as_mut_ptr() as *mut u8,
                bytes.len(),
            )
        };
        self.len = end;
    }

    #[inline(always)]
    fn emit(&self, event_authority: &AccountInfo, program: &AccountInfo) -> ProgramResult {
        let bump = [EVENT_AUTHORITY_BUMP];
        let seeds = [Seed::from(EVENT_AUTHORITY_SEED), Seed::from(&bump)];
        invoke_signed(
            &Instruction {
                program_id: &crate::ID,
                accounts: &[
                    AccountMeta::readonly_signer(&EVENT_AUTHORITY),
                    AccountMeta::readonly(&crate::ID),
                ],
                // SAFETY: every byte below `len` was written by `new` or `push`.
                data: unsafe {
                    core::slice::from_raw_parts(self.data.as_ptr() as *const u8, self.len)
                },
            },
            &[event_authority, program],
            &[Signer::from(&seeds)],
        )
    }
}
