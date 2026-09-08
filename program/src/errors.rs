use pinocchio::program_error::ProgramError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum GachaError {
    /// Account expected to be mutable
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
}

impl From<GachaError> for ProgramError {
    fn from(e: GachaError) -> Self {
        ProgramError::Custom(e as u32)
    }
}
