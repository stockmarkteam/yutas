//! Key codes hook.rs works with. keys.rs translates the hardware key codes of
//! the event tap into these. Letters are their ASCII capitals (0x41..=0x5A)
//! and digits their ASCII digits (0x30..=0x39).

pub const BACK: u16 = 0x08;
pub const RETURN: u16 = 0x0D;
pub const ESCAPE: u16 = 0x1B;
pub const SPACE: u16 = 0x20;
pub const RIGHT: u16 = 0x27;
pub const LCOMMAND: u16 = 0x5B;
pub const RCOMMAND: u16 = 0x5C;
pub const LSHIFT: u16 = 0xA0;
pub const RSHIFT: u16 = 0xA1;
pub const LCONTROL: u16 = 0xA2;
pub const RCONTROL: u16 = 0xA3;
pub const LOPTION: u16 = 0xA4;
pub const ROPTION: u16 = 0xA5;
/// Any symbol key (the tracker only counts it).
pub const SYMBOL: u16 = 0xBA;
/// A key the tracker does not model (arrows, Tab, function keys...).
pub const OTHER: u16 = 0xFF;

pub fn is_modifier(vk: u16) -> bool {
    matches!(vk, LSHIFT | RSHIFT | LCONTROL | RCONTROL | LOPTION | ROPTION | LCOMMAND | RCOMMAND)
}
