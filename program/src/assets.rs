use gacha_core::asset::CORE_ID;
use pinocchio::{
    account_info::AccountInfo,
    cpi::invoke_signed,
    instruction::{AccountMeta, Instruction, Signer},
    ProgramResult,
};

/// Core TransferV1 with no compression proof. Core validates the asset, its
/// owner's signature, the collection account and plugin rules itself, so the
/// program passes the accounts through unchecked.
pub struct Transfer<'a> {
    pub payer: &'a AccountInfo,
    pub authority: &'a AccountInfo,
    pub new_owner: &'a AccountInfo,
    pub asset: &'a AccountInfo,
    pub collection: &'a AccountInfo,
    pub core_program: &'a AccountInfo,
    pub system_program: &'a AccountInfo,
}

impl Transfer<'_> {
    pub fn invoke_signed(self, signers: &[Signer]) -> ProgramResult {
        let accounts = [
            AccountMeta::writable(self.asset.key()),
            AccountMeta::readonly(self.collection.key()),
            AccountMeta::writable_signer(self.payer.key()),
            AccountMeta::readonly_signer(self.authority.key()),
            AccountMeta::readonly(self.new_owner.key()),
            AccountMeta::readonly(self.system_program.key()),
            AccountMeta::readonly(self.core_program.key()), // Core sentinel: no log wrapper.
        ];
        invoke_signed(
            &Instruction {
                program_id: &CORE_ID,
                accounts: &accounts,
                data: &[14, 0],
            },
            &[
                self.asset,
                self.collection,
                self.payer,
                self.authority,
                self.new_owner,
                self.system_program,
                self.core_program,
            ],
            signers,
        )
    }
}
