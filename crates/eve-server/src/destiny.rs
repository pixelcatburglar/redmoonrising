//! The destiny ball stream that SetState and AddBalls carry: a little-endian
//! binary format read by destiny.dll's ReadFullStateFromStream.
//!
//! Record layout (sizes and order from the disassembly of ReadBallFromStream):
//!
//! ```text
//! i32 ballID  u8 mode  f64 radius  f64 x,y,z  u8 flags
//! if mode != RIGID:  f64 mass  u8 cloak  i64 harmonic  i32 corporationID  i32 allianceID
//! if flags & FREE:   f64 maxVelocity  f64 vx,vy,vz  f64 agility  f64 speedFraction
//! u8 formationID     (0xFF = none)
//! <per-mode payload> (empty for STOP, FIELD and RIGID, the only modes written here)
//! u8 nameLength  u16[nameLength] name (UTF-16LE)
//! ```
//!
//! The field *names* in the two conditional blocks follow the later, documented
//! form of this record; the reader's defaults agree (mass 1e34 = immovable,
//! agility 1.0, speedFraction 0.0, harmonic -1).
//!
//! Moving ships are written in STOP mode with their current velocity; the
//! command they are executing is sent as an ordinary opcode right after, which
//! avoids the per-mode payloads.

#[allow(dead_code)]
pub mod mode {
    pub const GOTO: u8 = 0;
    pub const FOLLOW: u8 = 1;
    pub const STOP: u8 = 2;
    pub const WARP: u8 = 3;
    pub const ORBIT: u8 = 4;
    /// A force field: solid to every ball without its harmonic, corporation
    /// or alliance (Partition.cpp's harmonics test; destiny 2.10 0x10028d40).
    pub const FIELD: u8 = 10;
    pub const RIGID: u8 = 11;
}

#[allow(dead_code)]
pub mod flags {
    pub const IS_FREE: u8 = 0x01;
    pub const IS_GLOBAL: u8 = 0x02;
    pub const IS_MASSIVE: u8 = 0x04;
    pub const IS_INTERACTIVE: u8 = 0x08;
    pub const IS_CLOAKED: u8 = 0x10;
    pub const HAS_MINIBALLS: u8 = 0x40;

    pub const SHIP: u8 = IS_FREE | IS_GLOBAL | IS_MASSIVE | IS_INTERACTIVE;
    /// A ship in warp: destiny drops IS_MASSIVE for the warp (0x100186b0).
    pub const SHIP_IN_WARP: u8 = SHIP & !IS_MASSIVE;
    pub const CELESTIAL: u8 = IS_GLOBAL | IS_MASSIVE | IS_INTERACTIVE;
    /// Fixed, but ships pass through it: stations, cans.
    pub const CELESTIAL_PASSABLE: u8 = CELESTIAL & !IS_MASSIVE;
}

const NO_FORMATION: u8 = 0xFF;
pub const NO_HARMONIC: i64 = -1;

#[derive(Debug, Clone)]
pub struct Ball {
    pub id: i64,
    pub mode: u8,
    pub radius: f64,
    pub pos: (f64, f64, f64),
    pub flags: u8,
    /// Only for non-rigid balls.
    pub mass: f64,
    pub corp_id: i32,
    pub alliance_id: i32,
    /// Only for non-rigid balls: -1 for none, -2 passes every field.
    pub harmonic: i64,
    /// Only for free balls.
    pub max_velocity: f64,
    pub velocity: (f64, f64, f64),
    pub agility: f64,
    pub speed_fraction: f64,
    pub name: String,
}

impl Ball {
    /// A ball that never moves; `massive` if ships bounce off it.
    pub fn celestial(id: i64, radius: f64, pos: (f64, f64, f64), name: &str, massive: bool) -> Self {
        Ball {
            id,
            mode: mode::RIGID,
            radius,
            pos,
            flags: if massive { flags::CELESTIAL } else { flags::CELESTIAL_PASSABLE },
            mass: 0.0,
            corp_id: -1,
            alliance_id: -1,
            harmonic: NO_HARMONIC,
            max_velocity: 0.0,
            velocity: (0.0, 0.0, 0.0),
            agility: 1.0,
            speed_fraction: 0.0,
            name: name.to_string(),
        }
    }
}

pub fn encode(state_stamp: u32, balls: &[Ball]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + balls.len() * 128);
    out.push(0u8); // packet type
    out.extend_from_slice(&state_stamp.to_le_bytes());
    for ball in balls {
        write_ball(&mut out, ball);
    }
    out
}

fn write_ball(out: &mut Vec<u8>, b: &Ball) {
    debug_assert!(
        matches!(b.mode, mode::STOP | mode::FIELD | mode::RIGID),
        "only STOP, FIELD and RIGID balls carry no per-mode payload (ReadBallFromStream)"
    );
    let f64s = |out: &mut Vec<u8>, vals: &[f64]| {
        for v in vals {
            out.extend_from_slice(&v.to_le_bytes());
        }
    };
    out.extend_from_slice(&(b.id as i32).to_le_bytes());
    out.push(b.mode);
    f64s(out, &[b.radius, b.pos.0, b.pos.1, b.pos.2]);
    out.push(b.flags);

    if b.mode != mode::RIGID {
        f64s(out, &[b.mass]);
        out.push(0); // not cloaked
        out.extend_from_slice(&b.harmonic.to_le_bytes());
        out.extend_from_slice(&b.corp_id.to_le_bytes());
        out.extend_from_slice(&b.alliance_id.to_le_bytes());
    }
    if b.flags & flags::IS_FREE != 0 {
        f64s(out, &[b.max_velocity, b.velocity.0, b.velocity.1, b.velocity.2, b.agility, b.speed_fraction]);
    }
    out.push(NO_FORMATION);

    let utf16: Vec<u16> = b.name.encode_utf16().take(255).collect();
    out.push(utf16.len() as u8);
    for unit in utf16 {
        out.extend_from_slice(&unit.to_le_bytes());
    }
}

/// Destiny ticks are whole seconds; the clock is FILETIME seconds.
pub fn tick_now() -> i64 {
    crate::bulkdata::filetime_now() / 10_000_000
}

/// A tick as the client's 32-bit stamp.
pub fn stamp(tick: i64) -> u32 {
    tick as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ship() -> Ball {
        Ball {
            id: 140_000_016,
            mode: mode::STOP,
            radius: 40.0,
            pos: (1.0, 2.0, 3.0),
            flags: flags::SHIP,
            mass: 1_650_000.0,
            corp_id: 1_000_002,
            alliance_id: -1,
            harmonic: NO_HARMONIC,
            max_velocity: 260.0,
            velocity: (10.0, 0.0, 0.0),
            agility: 3.1,
            speed_fraction: 1.0,
            name: "x".into(),
        }
    }

    #[test]
    fn packet_header_is_type_then_stamp() {
        let blob = encode(0x0102_0304, &[]);
        assert_eq!(blob, vec![0x00, 0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn a_rigid_ball_skips_both_conditional_blocks() {
        let b = Ball::celestial(60_014_689, 27_072.0, (1.0, 2.0, 3.0), "AB", true);
        let blob = encode(0, &[b]);
        // header 4+1+32+1, formation 1, name 1+4
        assert_eq!(blob.len(), 5 + 38 + 1 + 5);
        assert_eq!(&blob[5..9], &60_014_689i32.to_le_bytes());
        assert_eq!(blob[9], mode::RIGID);
        assert_eq!(blob[9 + 1 + 32], flags::CELESTIAL);
        assert_eq!(blob[9 + 1 + 32 + 1], 0xFF, "no formation");
    }

    #[test]
    fn a_free_ball_carries_mass_then_motion_in_reader_order() {
        let blob = encode(0, &[ship()]);
        // header 38, mass block 8+1+8+4+4, free block 6*8, formation 1, name 1+2
        assert_eq!(blob.len(), 5 + 38 + 25 + 48 + 1 + 3);
        let at = 5 + 38;
        let f = |o: usize| f64::from_le_bytes(blob[o..o + 8].try_into().unwrap());
        let i = |o: usize| i32::from_le_bytes(blob[o..o + 4].try_into().unwrap());
        assert_eq!(f(at), 1_650_000.0, "mass first");
        assert_eq!(blob[at + 8], 0, "not cloaked");
        assert_eq!(i64::from_le_bytes(blob[at + 9..at + 17].try_into().unwrap()), -1, "no harmonic");
        assert_eq!(i(at + 17), 1_000_002, "corporationID");
        assert_eq!(i(at + 21), -1, "allianceID");
        let free = at + 25;
        assert_eq!(f(free), 260.0, "maxVelocity");
        assert_eq!(f(free + 8), 10.0, "vx");
        assert_eq!(f(free + 32), 3.1, "agility");
        assert_eq!(f(free + 40), 1.0, "speedFraction");
        assert_eq!(blob[free + 48], 0xFF, "no formation");
    }

    #[test]
    fn the_name_is_utf16_counted_in_characters() {
        let b = Ball::celestial(1, 0.0, (0.0, 0.0, 0.0), "Ab", true);
        let blob = encode(0, &[b]);
        let tail = &blob[blob.len() - 5..];
        assert_eq!(tail, &[2, b'A', 0, b'b', 0]);
    }

    #[test]
    fn an_overlong_name_is_truncated_not_wrapped() {
        let name = "z".repeat(400);
        let b = Ball::celestial(1, 0.0, (0.0, 0.0, 0.0), &name, true);
        let blob = encode(0, &[b]);
        assert_eq!(blob[blob.len() - 1 - 255 * 2], 255);
    }
}
