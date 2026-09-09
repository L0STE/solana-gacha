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
    let mut message = core::mem::MaybeUninit::<[u8; 129]>::uninit();
    let out = message.as_mut_ptr() as *mut u8;
    // SAFETY: the seven writes below cover bytes 0..129 exactly, with no gaps.
    unsafe {
        core::ptr::copy_nonoverlapping(b"gacha:buyback:v1".as_ptr(), out, 16);
        core::ptr::copy_nonoverlapping(ID.as_ptr(), out.add(16), 32);
        core::ptr::copy_nonoverlapping(pool.as_ptr(), out.add(48), 32);
        core::ptr::copy_nonoverlapping(asset.as_ptr(), out.add(80), 32);
        core::ptr::copy_nonoverlapping(price.to_le_bytes().as_ptr(), out.add(112), 8);
        core::ptr::copy_nonoverlapping(expires_at.to_le_bytes().as_ptr(), out.add(120), 8);
        out.add(128).write(tier);
        message.assume_init()
    }
}
