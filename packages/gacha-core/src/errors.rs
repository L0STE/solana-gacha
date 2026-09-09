use pinocchio::program_error::ProgramError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum GachaError {
    /// An account this program writes directly (data, or lamports in `close`)
    /// must be writable. The runtime would reject the write at the end of the
    /// instruction, after every CPI ran; checking first fails early and
    /// clearly. Accounts mutated only through a CPI are not checked: the CPI
    /// enforces it.
    NotMutable,
    /// Account expected to be a signer
    NotSigner,
    /// Account expected to be owned by our program
    InvalidAccountOwner,
    /// The account data length is not the expected one
    InvalidAccountLength,
    /// The account version is not the expected one
    InvalidVersion,

    /// The pool parameters are invalid
    InvalidPoolParams,
    /// The signer is not the pool authority
    InvalidAuthority,
    /// The account is not the pool's VRF operator
    InvalidOperator,
    /// The token account is not the expected one
    InvalidTokenAddress,

    /// The tier index is out of range or the tier is full
    InvalidTier,
    /// The item account does not match the drawn (tier, position)
    InvalidItem,
    /// The pull count must be between 1 and 10
    InvalidCount,
    /// The pull is not at the head of the settle queue
    NotNextInQueue,
    /// The pull is not in the expected status
    InvalidPullStatus,

    /// The VRF proof does not verify
    InvalidProof,
    /// Not enough unreserved inventory remains
    SoldOut,
    /// The settle deadline has not passed
    DeadlineNotReached,
    /// The outcome index is out of range or already delivered
    InvalidOutcome,
    /// The pool does not hold enough free balance
    InsufficientBalance,

    /// The account is not the recorded buyer
    InvalidBuyer,
    /// The settlement deadline has passed; only a refund is allowed
    DeadlinePassed,
    /// Restocking changed the candidate list since the buyer prepared the purchase
    InventoryChanged,
    /// The buyback terms or pool authority signature are invalid.
    InvalidQuote,
    /// The buyback quote has expired.
    QuoteExpired,
    /// The prize is not a supported Core asset or has the wrong owner/collection.
    InvalidAsset,
    /// This operation or transition is not allowed in the pool's current status.
    InvalidPoolStatus,
    /// Resolve pending purchases before reclaiming unsold inventory.
    PendingPurchases,
    /// The account is not the PDA for the supplied seeds and bump.
    InvalidSeeds,
    /// The account to create already holds data or is not system-owned.
    AlreadyInitialized,
    /// The pull belongs to a different pool.
    PoolMismatch,
    /// Settlement needs exactly one item account per draw.
    InvalidItemCount,
    /// Only the event authority PDA may invoke the event instruction.
    InvalidEventAuthority,
}

impl From<GachaError> for ProgramError {
    #[inline]
    fn from(e: GachaError) -> Self {
        ProgramError::Custom(e as u32)
    }
}
