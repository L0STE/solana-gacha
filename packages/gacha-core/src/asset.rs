//! The fixed Core AssetV1 header needed to route transfers. Core validates the
//! complete asset, collection and plugin rules when the transfer executes.

use crate::errors::GachaError;

// CoREENxT6tW1HoK8ypY1SxRMZTcVPm7R94rH4PZNhX7d
pub const CORE_ID: [u8; 32] = [
    175, 84, 171, 16, 189, 151, 165, 66, 160, 158, 247, 179, 152, 137, 221, 12, 211, 148, 164, 204,
    233, 223, 166, 205, 201, 126, 190, 45, 35, 91, 167, 72,
];

pub struct CoreAsset<'a> {
    pub owner: &'a [u8; 32],
    pub collection: Option<&'a [u8; 32]>,
}

impl<'a> CoreAsset<'a> {
    pub fn from_account(owner: &[u8; 32], data: &'a [u8]) -> Result<Self, GachaError> {
        if owner != &CORE_ID || data.len() < 34 || data[0] != 1 {
            return Err(GachaError::InvalidAsset);
        }
        let collection = match data[33] {
            0 => None,
            kind @ (1 | 2) => {
                let key = data.get(34..66).ok_or(GachaError::InvalidAsset)?;
                (kind == 2).then(|| key.try_into().unwrap())
            }
            _ => return Err(GachaError::InvalidAsset),
        };
        Ok(Self {
            owner: data[1..33].try_into().unwrap(),
            collection,
        })
    }
}
