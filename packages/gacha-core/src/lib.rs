//! Shared account layouts, error codes and draw rules for the program and Rust SDK.
#![no_std]

pub mod asset;
pub mod constants;
pub mod errors;
pub mod state;

// 4X8u1YspRi6Z9TkZNb8qxNdwPLs5vDi7VRC2DhTheeKp
pub const ID: [u8; 32] = [
    0x34, 0x4b, 0x69, 0xf5, 0x0e, 0xcb, 0x2c, 0x1c, 0x62, 0x67, 0x39, 0x5f, 0xb3, 0x3c, 0x90, 0x83,
    0x0f, 0xfe, 0x58, 0x4b, 0x95, 0x08, 0xf2, 0x75, 0xbf, 0x2c, 0x17, 0x8b, 0x7c, 0x68, 0xa5, 0xff,
];

/// Canonical reusable buyback quote, signed by the pool authority.
/// The payment mint is fixed by the pool; expiry is a Unix timestamp in seconds.
pub fn buyback_message(
    pool: &[u8; 32],
    asset: &[u8; 32],
    price: u64,
    expires_at: i64,
    tier: u8,
) -> [u8; 129] {
    let mut message = [0; 129];
    message[..16].copy_from_slice(b"gacha:buyback:v1");
    message[16..48].copy_from_slice(&ID);
    message[48..80].copy_from_slice(pool);
    message[80..112].copy_from_slice(asset);
    message[112..120].copy_from_slice(&price.to_le_bytes());
    message[120..128].copy_from_slice(&expires_at.to_le_bytes());
    message[128] = tier;
    message
}
