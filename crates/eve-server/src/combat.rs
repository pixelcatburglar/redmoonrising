//! The numbers of a fight: hit points and how damage goes through them, the
//! shield and capacitor recharge curve, turret and missile hit quality, and how
//! long a lock takes. Everything here is pure; `heartbeat` drives it.
//!
//! Damage comes in dogma's four types, in attribute order: EM (114), explosive
//! (116), kinetic (117), thermal (118). It hits the shield first, then armour,
//! then structure, each layer scaling it by its resonances; what a layer cannot
//! absorb passes to the next one unscaled.

use std::collections::{BTreeMap, BTreeSet};

use crate::ballpark::Vec3;

/// EM, explosive, kinetic, thermal.
pub type Damage = [f64; 4];

pub mod attr {
    pub const DAMAGE: i32 = 3;
    pub const HP: i32 = 9;
    pub const CHARGE: i32 = 18;
    pub const SPEED_FACTOR: i32 = 20;
    pub const MAX_VELOCITY: i32 = 37;
    pub const SPEED: i32 = 51;
    pub const MAX_RANGE: i32 = 54;
    pub const RECHARGE_RATE: i32 = 55;
    pub const CHARGE_RATE: i32 = 56;
    pub const DAMAGE_MULTIPLIER: i32 = 64;
    pub const DURATION: i32 = 73;
    pub const CAPACITOR_BONUS: i32 = 67;
    pub const SHIELD_BONUS: i32 = 68;
    pub const AGILITY: i32 = 70;
    pub const MAX_TARGET_RANGE: i32 = 76;
    pub const MINING_AMOUNT: i32 = 77;
    pub const STRUCTURE_DAMAGE_AMOUNT: i32 = 83;
    pub const ARMOR_DAMAGE_AMOUNT: i32 = 84;
    pub const KINETIC_RESONANCE: i32 = 109;
    pub const THERMAL_RESONANCE: i32 = 110;
    pub const EXPLOSIVE_RESONANCE: i32 = 111;
    pub const EM_RESONANCE: i32 = 113;
    pub const EM_DAMAGE: i32 = 114;
    pub const EXPLOSIVE_DAMAGE: i32 = 116;
    pub const KINETIC_DAMAGE: i32 = 117;
    pub const THERMAL_DAMAGE: i32 = 118;
    pub const AMMO_LOADED: i32 = 127;
    pub const FALLOFF: i32 = 158;
    pub const TRACKING_SPEED: i32 = 160;
    pub const MAX_LOCKED_TARGETS: i32 = 192;
    pub const ENTITY_ATTACK_RANGE: i32 = 247;
    pub const SHIELD_CAPACITY: i32 = 263;
    pub const SHIELD_CHARGE: i32 = 264;
    pub const ARMOR_HP: i32 = 265;
    pub const ARMOR_DAMAGE: i32 = 266;
    pub const ARMOR_RESONANCE: [i32; 4] = [267, 268, 269, 270];
    pub const SHIELD_RESONANCE: [i32; 4] = [271, 272, 273, 274];
    pub const EXPLOSION_DELAY: i32 = 281;
    pub const ENTITY_FLY_RANGE: i32 = 416;
    pub const SHIELD_RECHARGE_RATE: i32 = 479;
    pub const ENTITY_KILL_BOUNTY: i32 = 481;
    pub const CAPACITOR_CAPACITY: i32 = 482;
    pub const MISSILE_LAUNCH_DURATION: i32 = 506;
    pub const ENTITY_MISSILE_TYPE_ID: i32 = 507;
    pub const ENTITY_CRUISE_SPEED: i32 = 508;
    pub const SIGNATURE_RADIUS: i32 = 552;
    pub const SCAN_RESOLUTION: i32 = 564;
    pub const MASS_ADDITION: i32 = 796;
    pub const SPEED_BOOST_FACTOR: i32 = 567;
    pub const OPTIMAL_SIG_RADIUS: i32 = 620;
    pub const ENTITY_ARMOR_REPAIR_DURATION: i32 = 630;
    pub const ENTITY_ARMOR_REPAIR_AMOUNT: i32 = 631;
    pub const ENTITY_SHIELD_BOOST_DURATION: i32 = 636;
    pub const ENTITY_SHIELD_BOOST_AMOUNT: i32 = 637;
    pub const AOE_VELOCITY: i32 = 653;
    pub const AOE_CLOUD_SIZE: i32 = 654;
    pub const MISSILE_DAMAGE_MULTIPLIER: i32 = 212;
    pub const SIGNATURE_RADIUS_BONUS: i32 = 554;

    pub const DAMAGE_TYPES: [i32; 4] = [EM_DAMAGE, EXPLOSIVE_DAMAGE, KINETIC_DAMAGE, THERMAL_DAMAGE];
    pub const HULL_RESONANCE: [i32; 4] = [EM_RESONANCE, EXPLOSIVE_RESONANCE, KINETIC_RESONANCE, THERMAL_RESONANCE];
    /// Gravimetric, ladar, magnetometric, radar.
    pub const SENSOR_STRENGTHS: [i32; 4] = [211, 209, 210, 208];
    pub const JAMMER_STRENGTHS: [i32; 4] = [238, 239, 240, 241];
    pub const POWER_TRANSFER_AMOUNT: i32 = 90;
    pub const ENERGY_DESTABILIZATION_AMOUNT: i32 = 97;
    pub const WARP_SCRAMBLE_RANGE: i32 = 103;
    pub const WARP_SCRAMBLE_STRENGTH: i32 = 105;
    pub const MAX_TARGET_RANGE_MULTIPLIER: i32 = 237;
    pub const MAX_RANGE_MULTIPLIER: i32 = 243;
    pub const TRACKING_SPEED_MULTIPLIER: i32 = 244;
    pub const ENTITY_SECURITY_STATUS_KILL_BONUS: i32 = 252;
    pub const ENTITY_WARP_SCRAMBLE_CHANCE: i32 = 504;
    pub const MODIFY_TARGET_SPEED_CHANCE: i32 = 512;
    pub const MODIFY_TARGET_SPEED_DURATION: i32 = 513;
    pub const MODIFY_TARGET_SPEED_RANGE: i32 = 514;
    pub const ENTITY_SECURITY_MAX_GAIN: i32 = 563;
    pub const SCAN_RESOLUTION_MULTIPLIER: i32 = 565;
    pub const ENTITY_LOOT_COUNT_MIN: i32 = 250;
    pub const ENTITY_LOOT_COUNT_MAX: i32 = 251;
    pub const ORBIT_RANGE: i32 = 157;
    pub const MAX_ACTIVE_DRONES: i32 = 352;
    pub const SKILL_POINTS_SAVED: i32 = 419;
}

/// Milliseconds since the Unix epoch: the heartbeat's clock.
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// A FILETIME for a heartbeat time, for the client.
pub fn filetime(ms: i64) -> i64 {
    const EPOCH_DIFF_MS: i64 = 11_644_473_600_000;
    (ms + EPOCH_DIFF_MS) * 10_000
}

/// A pool that refills along dogma's curve: the shield and the capacitor.
/// With tau = rechargeRate / 5, the fill level follows
/// `(1 + (sqrt(v0 / max) - 1) * exp(-t / tau))^2 * max` (godma.py GetChargeValue),
/// which is what the client draws between updates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Charged {
    value: f64,
    at_ms: i64,
    pub max: f64,
    pub recharge_ms: f64,
}

impl Charged {
    pub fn full(max: f64, recharge_ms: f64, now: i64) -> Self {
        Charged { value: max.max(0.0), at_ms: now, max: max.max(0.0), recharge_ms }
    }

    pub fn at(&self, now: i64) -> f64 {
        if self.max <= 0.0 {
            return 0.0;
        }
        if self.recharge_ms <= 0.0 {
            return self.value;
        }
        let tau = self.recharge_ms / 5.0;
        let sq = (self.value / self.max).clamp(0.0, 1.0).sqrt();
        let decay = (-((now - self.at_ms).max(0) as f64) / tau).exp();
        (1.0 + (sq - 1.0) * decay).powi(2) * self.max
    }

    pub fn set(&mut self, now: i64, value: f64) {
        self.value = value.clamp(0.0, self.max);
        self.at_ms = now;
    }

    pub fn add(&mut self, now: i64, delta: f64) {
        let v = self.at(now) + delta;
        self.set(now, v);
    }

    /// Takes `amount` if there is that much; false (and nothing taken) if not.
    pub fn spend(&mut self, now: i64, amount: f64) -> bool {
        let have = self.at(now);
        if have + 1e-6 < amount {
            return false;
        }
        self.set(now, have - amount);
        true
    }

    /// Keeps the fill fraction when the maximum changes (a module goes on).
    pub fn rescale(&mut self, now: i64, max: f64, recharge_ms: f64) {
        let fraction = if self.max > 0.0 { self.at(now) / self.max } else { 1.0 };
        self.max = max.max(0.0);
        self.recharge_ms = recharge_ms;
        self.set(now, fraction * self.max);
    }

    pub fn fraction(&self, now: i64) -> f64 {
        if self.max <= 0.0 { 0.0 } else { self.at(now) / self.max }
    }
}

/// Hit points and resistances.
#[derive(Debug, Clone, PartialEq)]
pub struct Vitals {
    pub shield: Charged,
    pub armor_max: f64,
    pub armor_damage: f64,
    pub hull_max: f64,
    pub hull_damage: f64,
    pub capacitor: Charged,
    /// Shield, armour, structure; each EM, explosive, kinetic, thermal.
    pub resonance: [[f64; 4]; 3],
}

/// What one volley did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Applied {
    pub total: f64,
    pub destroyed: bool,
}

impl Vitals {
    /// Puts `damage` through shield, armour and structure.
    pub fn apply(&mut self, now: i64, damage: Damage) -> Applied {
        let mut raw = damage;
        let mut total = 0.0;
        for layer in 0..3 {
            let res = self.resonance[layer];
            let effective: f64 = (0..4).map(|i| raw[i] * res[i]).sum();
            if effective <= 0.0 {
                break;
            }
            let available = match layer {
                0 => self.shield.at(now),
                1 => self.armor_max - self.armor_damage,
                _ => self.hull_max - self.hull_damage,
            }
            .max(0.0);
            let taken = effective.min(available);
            match layer {
                0 => self.shield.set(now, available - taken),
                1 => self.armor_damage += taken,
                _ => self.hull_damage += taken,
            }
            total += taken;
            if taken >= effective {
                break;
            }
            let left = 1.0 - taken / effective;
            raw = raw.map(|d| d * left);
        }
        Applied { total, destroyed: self.hull_damage >= self.hull_max - 1e-6 }
    }

    pub fn repair_armor(&mut self, amount: f64) {
        self.armor_damage = (self.armor_damage - amount).max(0.0);
    }

    pub fn repair_hull(&mut self, amount: f64) {
        self.hull_damage = (self.hull_damage - amount).max(0.0);
    }

    /// The damage state the client shows on brackets and the target window:
    /// [(shield fraction, shield tau ms), armour fraction, structure fraction].
    pub fn damage_state(&self, now: i64) -> (f64, f64, f64, f64) {
        let fraction = |left: f64, max: f64| if max > 0.0 { (left / max).clamp(0.0, 1.0) } else { 0.0 };
        (
            self.shield.fraction(now),
            (self.shield.recharge_ms / 5.0).max(1.0),
            fraction(self.armor_max - self.armor_damage, self.armor_max),
            fraction(self.hull_max - self.hull_damage, self.hull_max),
        )
    }
}

/// How well a shot landed, as the client's AttackHit1-6 and AttackMiss1-3 say.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shot {
    Miss(u8),
    Hit { quality: u8, multiplier: f64 },
}

impl Shot {
    pub fn multiplier(self) -> f64 {
        match self {
            Shot::Miss(_) => 0.0,
            Shot::Hit { multiplier, .. } => multiplier,
        }
    }
}

/// What a turret needs to know about its own aim.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gun {
    pub optimal: f64,
    pub falloff: f64,
    /// rad/s.
    pub tracking: f64,
    pub optimal_sig: f64,
}

/// The chance a turret hits: 0.5 ^ ((angular * sigRes / (tracking * sig))^2 +
/// (max(0, distance - optimal) / falloff)^2).
pub fn hit_chance(gun: &Gun, distance: f64, angular: f64, target_sig: f64) -> f64 {
    let tracking_term = if gun.tracking > 0.0 && target_sig > 0.0 {
        // A small turret's signature resolution when the type has none.
        let sig_res = if gun.optimal_sig > 0.0 { gun.optimal_sig } else { 40.0 };
        (angular * sig_res / (gun.tracking * target_sig)).powi(2)
    } else {
        0.0
    };
    let range_term = if gun.falloff > 0.0 {
        ((distance - gun.optimal).max(0.0) / gun.falloff).powi(2)
    } else if distance > gun.optimal {
        f64::INFINITY
    } else {
        0.0
    };
    0.5f64.powf(tracking_term + range_term)
}

/// A turret shot from two uniform rolls in [0, 1): `roll` decides the hit and,
/// as in the original game, its quality (a lower roll is a better hit; under 1%
/// wrecks for triple damage).
pub fn turret_shot(chance: f64, roll: f64, style: f64) -> Shot {
    if roll >= chance {
        // A near miss when it was close.
        let quality = if roll < chance + 0.1 { 2 } else if style < 0.5 { 1 } else { 3 };
        return Shot::Miss(quality);
    }
    if roll < 0.01 {
        return Shot::Hit { quality: 6, multiplier: 3.0 };
    }
    let multiplier = 0.49 + (1.0 - roll);
    let quality = match multiplier {
        m if m > 1.3 => 5,
        m if m > 1.1 => 4,
        m if m > 0.9 => 3,
        m if m > 0.7 => 2,
        _ => 1,
    };
    Shot::Hit { quality, multiplier }
}

/// What a missile does to a target of `sig` moving at `speed`: full damage
/// against anything bigger than its explosion and slower than its spread.
pub fn missile_factor(cloud: f64, aoe_velocity: f64, sig: f64, speed: f64) -> f64 {
    if cloud <= 0.0 {
        return 1.0;
    }
    let size = sig / cloud;
    let motion = if speed > aoe_velocity && speed > 0.0 { size * aoe_velocity / speed } else { 1.0 };
    size.min(motion).min(1.0).max(0.0)
}

/// Seconds a lock takes: 40000 / (scanResolution * asinh(signatureRadius)^2).
pub fn lock_seconds(scan_resolution: f64, signature: f64) -> f64 {
    if scan_resolution <= 0.0 || signature <= 0.0 {
        return 5.0;
    }
    (40_000.0 / (scan_resolution * signature.asinh().powi(2))).clamp(0.5, 180.0)
}

/// Angular velocity of `b` as seen from `a`, rad/s.
pub fn angular_velocity(a_pos: Vec3, a_vel: Vec3, b_pos: Vec3, b_vel: Vec3) -> f64 {
    let offset = b_pos - a_pos;
    let distance = offset.len();
    if distance < 1.0 {
        return 0.0;
    }
    let rel = b_vel - a_vel;
    let radial = offset.unit() * rel.dot(offset.unit());
    (rel - radial).len() / distance
}

/// A small, fast PRNG (xorshift64*): nothing here needs better.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn seeded(seed: u64) -> Self {
        Rng(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }

    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next_u64() % n as u64) as usize }
    }

    /// A point in a ball of `radius` around the origin.
    pub fn in_sphere(&mut self, radius: f64) -> Vec3 {
        loop {
            let p = Vec3::new(self.range(-1.0, 1.0), self.range(-1.0, 1.0), self.range(-1.0, 1.0));
            if p.len() <= 1.0 {
                return p * radius;
            }
        }
    }
}

// ---- who is fighting -------------------------------------------------------------

/// One locked (or locking) target.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Lock {
    /// Locked once the clock reaches this.
    Pending(i64),
    Locked,
}

/// What a running module does each cycle.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Turret { damage: Damage, gun: Gun },
    Missile { damage: Damage, flight_ms: i64, range: f64, cloud: f64, aoe_velocity: f64 },
    ShieldBoost(f64),
    ArmorRepair(f64),
    HullRepair(f64),
    CapBoost(f64),
    /// m3 per cycle, delivered at the end of it.
    Mining { amount: f64, range: f64 },
    /// Changes attributes while it runs (hardeners, afterburners); dogma does
    /// the work, the cycle only costs capacitor.
    Passive,
    /// Each cycle: take this much of the target's capacitor.
    Neutralize(f64),
    /// Each cycle: move up to this much of the target's capacitor to ours.
    Leech(f64),
    /// Each cycle: give the target capacitor, shield or armour.
    GiveCapacitor(f64),
    GiveShield(f64),
    GiveArmor(f64),
    /// While it holds: the target's top speed times this.
    Web(f64),
    /// While it holds: this many points of warp scrambling.
    Scramble(f64),
    /// Each cycle, a chance to jam: strength against gravimetric, ladar,
    /// magnetometric and radar sensors.
    Jam([f64; 4]),
    /// While it holds: the target's lock range and scan resolution times these.
    Dampen { range: f64, resolution: f64 },
    /// While it holds: the target's turret optimal and tracking times these.
    Disrupt { range: f64, tracking: f64 },
    /// While it holds: the target's signature times this.
    Paint(f64),
}

impl Action {
    /// Whether it does its work on another ship over a range (and a falloff).
    pub fn projected(&self) -> bool {
        !matches!(
            self,
            Action::Turret { .. }
                | Action::Missile { .. }
                | Action::ShieldBoost(_)
                | Action::ArmorRepair(_)
                | Action::HullRepair(_)
                | Action::CapBoost(_)
                | Action::Mining { .. }
                | Action::Passive
        )
    }
}

/// What other ships' modules are doing to a ship right now. All factors are
/// 1 and all flags clear when nothing is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Afflictions {
    pub speed: f64,
    pub signature: f64,
    pub scan_resolution: f64,
    pub lock_range: f64,
    pub optimal: f64,
    pub tracking: f64,
    pub scrambled: f64,
    pub jammed: bool,
}

impl Default for Afflictions {
    fn default() -> Self {
        Afflictions {
            speed: 1.0,
            signature: 1.0,
            scan_resolution: 1.0,
            lock_range: 1.0,
            optimal: 1.0,
            tracking: 1.0,
            scrambled: 0.0,
            jammed: false,
        }
    }
}

/// The chance a projected effect takes hold at `distance`:
/// 2^-((distance - optimal) / falloff)^2 past optimal (godma.py
/// CheckApplyModifiers), none at all past optimal without a falloff.
pub fn falloff_chance(distance: f64, optimal: f64, falloff: f64) -> f64 {
    if distance <= optimal {
        1.0
    } else if falloff <= 0.0 {
        0.0
    } else {
        2f64.powf(-((distance - optimal) / falloff).powi(2))
    }
}

/// The chance a jammer of `strength` (per sensor type) jams sensors of
/// `sensors`: the ship's strongest sensor type decides.
pub fn jam_chance(strength: [f64; 4], sensors: [f64; 4]) -> f64 {
    let (kind, &defence) = sensors.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap_or((0, &0.0));
    if defence <= 0.0 {
        return 1.0;
    }
    (strength[kind] / defence).clamp(0.0, 1.0)
}

/// The charge a module burns.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ammo {
    pub item_id: i64,
    pub type_id: i64,
    pub quantity: i64,
    pub per_cycle: i64,
}

/// A module (or an NPC's weapon) that is running.
#[derive(Debug, Clone, PartialEq)]
pub struct Active {
    pub module_id: i64,
    pub type_id: i64,
    pub effect_id: i64,
    pub guid: String,
    pub offensive: bool,
    pub action: Action,
    pub target: Option<i64>,
    pub ammo: Option<Ammo>,
    pub duration_ms: i64,
    pub cap_need: f64,
    pub started_ms: i64,
    /// When the next cycle starts.
    pub next_ms: i64,
    pub cycles: i64,
    /// Cycles left after this one; at 1000 or more, until told to stop.
    pub repeat: i64,
    pub stopping: bool,
    /// The weapon's name, for damage messages.
    pub name: String,
    /// For a projected effect: its optimal range and falloff.
    pub reach: Option<(f64, f64)>,
    /// Whether this cycle's projected effect took hold.
    pub landed: bool,
}

pub const REPEAT_FOREVER: i64 = 1000;

/// What a ship brings to a fight, beyond how it moves.
#[derive(Debug, Clone, PartialEq)]
pub struct Fighter {
    pub vitals: Vitals,
    pub signature: f64,
    pub scan_resolution: f64,
    pub max_target_range: f64,
    pub max_locked: usize,
    pub locks: BTreeMap<i64, Lock>,
    pub modules: Vec<Active>,
    /// Who last hurt it: (character, ship), for bounties and kill reports.
    pub last_attacker: Option<(i64, i64)>,
    /// The running modules whose effects the ship's attributes were last
    /// computed with.
    pub applied: BTreeSet<i64>,
    /// Top speed before anyone webs it.
    pub top_speed: f64,
    /// Sensor strengths: gravimetric, ladar, magnetometric, radar.
    pub sensors: [f64; 4],
    pub afflictions: Afflictions,
    /// Damage done to it, by attacking character: (ship type, weapon type, damage).
    pub damage_log: BTreeMap<i64, (i64, i64, f64)>,
}

impl Fighter {
    pub fn new(vitals: Vitals, signature: f64, scan_resolution: f64, max_target_range: f64, max_locked: usize) -> Self {
        Fighter {
            vitals,
            signature,
            scan_resolution,
            max_target_range,
            max_locked,
            locks: BTreeMap::new(),
            modules: Vec::new(),
            last_attacker: None,
            applied: BTreeSet::new(),
            top_speed: 0.0,
            sensors: [0.0; 4],
            afflictions: Afflictions::default(),
            damage_log: BTreeMap::new(),
        }
    }

    pub fn sig(&self) -> f64 {
        self.signature * self.afflictions.signature
    }

    pub fn scan_res(&self) -> f64 {
        self.scan_resolution * self.afflictions.scan_resolution
    }

    pub fn lock_range(&self) -> f64 {
        self.max_target_range * self.afflictions.lock_range
    }

    pub fn locked(&self) -> impl Iterator<Item = i64> + '_ {
        self.locks.iter().filter(|(_, l)| **l == Lock::Locked).map(|(id, _)| *id)
    }

    pub fn is_locked(&self, id: i64) -> bool {
        self.locks.get(&id) == Some(&Lock::Locked)
    }

    pub fn module(&self, module_id: i64) -> Option<&Active> {
        self.modules.iter().find(|m| m.module_id == module_id)
    }

    pub fn module_mut(&mut self, module_id: i64) -> Option<&mut Active> {
        self.modules.iter_mut().find(|m| m.module_id == module_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vitals() -> Vitals {
        Vitals {
            shield: Charged::full(100.0, 500_000.0, 0),
            armor_max: 100.0,
            armor_damage: 0.0,
            hull_max: 100.0,
            hull_damage: 0.0,
            capacitor: Charged::full(200.0, 150_000.0, 0),
            resonance: [[1.0, 0.5, 0.5, 1.0], [0.5; 4], [1.0; 4]],
        }
    }

    #[test]
    fn a_drained_pool_refills_along_the_dogma_curve() {
        let mut cap = Charged::full(100.0, 100_000.0, 0);
        cap.set(0, 0.0);
        // Peak recharge is at 25%; after one tau the pool is (1 - 1/e)^2 full.
        let after_tau = cap.at(20_000);
        let expected = (1.0 - (-1.0f64).exp()).powi(2) * 100.0;
        assert!((after_tau - expected).abs() < 1e-6, "{after_tau} vs {expected}");
        assert!(cap.at(1_000_000) > 99.99);
    }

    #[test]
    fn spending_more_than_the_capacitor_holds_takes_nothing() {
        let mut cap = Charged::full(10.0, 100_000.0, 0);
        assert!(!cap.spend(0, 11.0));
        assert!((cap.at(0) - 10.0).abs() < 1e-9);
        assert!(cap.spend(0, 4.0));
        assert!((cap.at(0) - 6.0).abs() < 1e-9);
    }

    #[test]
    fn damage_spills_from_shield_to_armor_unscaled() {
        let mut v = vitals();
        // 150 kinetic at a 0.5 resonance is 75: the shield absorbs all of it.
        let a = v.apply(0, [0.0, 0.0, 150.0, 0.0]);
        assert!((a.total - 75.0).abs() < 1e-9);
        assert!((v.shield.at(0) - 25.0).abs() < 1e-9);
        // 100 EM: 25 into the shield (25 raw), 75 raw left at half in armour.
        let a = v.apply(0, [100.0, 0.0, 0.0, 0.0]);
        assert!((a.total - (25.0 + 37.5)).abs() < 1e-9, "{a:?}");
        assert!((v.armor_damage - 37.5).abs() < 1e-9);
        assert!(!a.destroyed);
    }

    #[test]
    fn running_out_of_structure_destroys() {
        let mut v = vitals();
        let a = v.apply(0, [1_000.0, 0.0, 0.0, 0.0]);
        assert!(a.destroyed);
        assert!((a.total - 300.0).abs() < 1e-9, "every layer emptied: {a:?}");
    }

    #[test]
    fn a_turret_in_optimal_against_a_still_target_always_hits() {
        let gun = Gun { optimal: 1_000.0, falloff: 2_000.0, tracking: 0.5, optimal_sig: 40.0 };
        assert!((hit_chance(&gun, 800.0, 0.0, 35.0) - 1.0).abs() < 1e-12);
        // One falloff out: half.
        assert!((hit_chance(&gun, 3_000.0, 0.0, 35.0) - 0.5).abs() < 1e-12);
        // Tracking at its limit against a matching signature: half.
        assert!((hit_chance(&gun, 500.0, 0.5, 40.0) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn shot_quality_follows_the_roll() {
        assert_eq!(turret_shot(0.5, 0.005, 0.0), Shot::Hit { quality: 6, multiplier: 3.0 });
        assert!(matches!(turret_shot(0.5, 0.45, 0.0), Shot::Hit { quality: 3 | 4, .. }));
        assert!(matches!(turret_shot(0.5, 0.55, 0.0), Shot::Miss(2)));
        assert!(matches!(turret_shot(0.5, 0.95, 0.9), Shot::Miss(3)));
    }

    #[test]
    fn missiles_lose_damage_on_small_fast_targets() {
        assert_eq!(missile_factor(50.0, 1_750.0, 400.0, 100.0), 1.0);
        assert!((missile_factor(50.0, 1_750.0, 25.0, 100.0) - 0.5).abs() < 1e-12);
        let fast = missile_factor(50.0, 1_000.0, 50.0, 2_000.0);
        assert!((fast - 0.5).abs() < 1e-12, "{fast}");
    }

    #[test]
    fn a_frigate_locks_a_frigate_in_a_few_seconds() {
        let t = lock_seconds(660.0, 35.0);
        assert!(t > 2.0 && t < 5.0, "{t}");
        assert!(lock_seconds(100.0, 400.0) > lock_seconds(1_000.0, 400.0));
    }

    #[test]
    fn angular_velocity_ignores_closing_speed() {
        let a = Vec3::ZERO;
        let still = Vec3::ZERO;
        let b = Vec3::new(1_000.0, 0.0, 0.0);
        assert_eq!(angular_velocity(a, still, b, Vec3::new(-300.0, 0.0, 0.0)), 0.0);
        let w = angular_velocity(a, still, b, Vec3::new(0.0, 0.0, 300.0));
        assert!((w - 0.3).abs() < 1e-12);
    }

    #[test]
    fn the_rng_stays_in_range() {
        let mut r = Rng::seeded(42);
        for _ in 0..1_000 {
            let u = r.unit();
            assert!((0.0..1.0).contains(&u));
            assert!(r.in_sphere(10.0).len() <= 10.0);
        }
    }
}
