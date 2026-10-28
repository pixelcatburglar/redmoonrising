//! The attribute engine: what a character's ship, modules, charges, skills and
//! implants add up to.
//!
//! A [`Loadout`] holds the items and their base attributes (type values, then
//! the item's own stored values); [`Loadout::value`] applies every modifier
//! aimed at an attribute, computing the modifying values the same way, so
//! chains resolve themselves (a skill's level scales its bonus, which then
//! modifies the ship). Modifiers come from dgmTypeModifiers, plus the few that
//! 2.10 hard-coded: an online module loads the ship's CPU and power grid, and a
//! fitted turret or launcher takes a hardpoint.
//!
//! Operations apply in dogma's order (assign, multiply, divide, add, subtract,
//! post-multiply, post-divide, post-percent, post-assign). Multiplications
//! from modules and charges on an attribute that does not stack are stacking
//! penalised: the n-th strongest counts for exp(-(n/2.67)^2) of itself.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use tokio_postgres::GenericClient;

pub mod attr {
    pub const IS_ONLINE: i32 = 2;
    pub const POWER_OUTPUT: i32 = 11;
    pub const LOW_SLOTS: i32 = 12;
    pub const MED_SLOTS: i32 = 13;
    pub const HI_SLOTS: i32 = 14;
    pub const POWER_LOAD: i32 = 15;
    pub const POWER: i32 = 30;
    pub const CPU_OUTPUT: i32 = 48;
    pub const CPU_LOAD: i32 = 49;
    pub const CPU: i32 = 50;
    pub const LAUNCHER_SLOTS_LEFT: i32 = 101;
    pub const TURRET_SLOTS_LEFT: i32 = 102;
    pub const RIG_SLOTS: i32 = 1137;
    pub const REQUIRED_SKILLS: [i32; 3] = [182, 183, 184];
}

pub mod effect {
    pub const LAUNCHER_FITTED: i32 = 40;
    pub const TURRET_FITTED: i32 = 42;
    pub const HI_POWER: i32 = 12;
    pub const MED_POWER: i32 = 13;
    pub const LO_POWER: i32 = 11;
    pub const RIG_SLOT: i32 = 2663;
}

pub mod category {
    pub const SHIP: i64 = 6;
    pub const MODULE: i64 = 7;
    pub const CHARGE: i64 = 8;
    pub const SKILL: i64 = 16;
    pub const IMPLANT: i64 = 20;
    pub const SUBSYSTEM: i64 = 32;
}

const FLAG_SKILL: i32 = 7;
const FLAG_SKILL_IN_TRAINING: i32 = 61;
const FLAG_BOOSTER: i32 = 88;
const FLAG_IMPLANT: i32 = 89;
pub const FLAG_SLOT_FIRST: i64 = 11;
pub const FLAG_SLOT_LAST: i64 = 35;
pub const FLAG_RIG_FIRST: i64 = 92;
pub const FLAG_RIG_LAST: i64 = 99;

/// Effect categories, as dgmTypeModifiers stores them.
const PASSIVE: i16 = 0;
const ACTIVE: i16 = 1;
const ONLINE: i16 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Offline,
    Online,
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Character,
    Ship,
    /// Fitted in a slot of the ship.
    Module,
    /// Loaded in a module; `module` is the module's index.
    Charge { module: usize },
    Skill,
    Implant,
}

#[derive(Debug, Clone)]
pub struct Piece {
    pub item_id: i64,
    pub type_id: i64,
    pub group_id: i64,
    pub category_id: i64,
    pub flag: i64,
    pub role: Role,
    pub state: State,
    base: HashMap<i32, f64>,
    effects: HashSet<i32>,
}

impl Piece {
    pub fn has_effect(&self, effect_id: i32) -> bool {
        self.effects.contains(&effect_id)
    }

    fn required_skills(&self) -> impl Iterator<Item = i64> + '_ {
        attr::REQUIRED_SKILLS.iter().filter_map(|a| self.base.get(a)).map(|v| *v as i64).filter(|v| *v > 0)
    }

    /// Every attribute this piece has a value for before modification.
    fn base_attributes(&self) -> impl Iterator<Item = i32> + '_ {
        self.base.keys().copied()
    }
}

#[derive(Debug, Clone)]
struct Modifier {
    type_id: i64,
    category: i16,
    func: Func,
    domain: Domain,
    modified: i32,
    modifying: i32,
    operation: i16,
    group_id: Option<i64>,
    skill_type_id: Option<i64>,
    off_while_active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Func {
    Item,
    LocationGroup,
    LocationRequiredSkill,
    OwnerRequiredSkill,
    Location,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Domain {
    Ship,
    Char,
    Itself,
    Other,
    Target,
}

/// One modifier aimed at one (piece, attribute).
#[derive(Debug, Clone, Copy)]
struct Incoming {
    source: usize,
    modifying: i32,
    operation: i16,
    /// A literal value instead of an attribute of the source.
    fixed: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
struct AttributeType {
    default: f64,
    stackable: bool,
}

/// What dogma needs from the database, loaded once and shared.
pub struct Catalog {
    attributes: HashMap<i32, AttributeType>,
}

impl Catalog {
    pub async fn load(db: &impl GenericClient) -> Result<Self, tokio_postgres::Error> {
        let rows = db
            .query(r#"SELECT "attributeID", coalesce("defaultValue", 0)::float8, coalesce("stackable", 1)::int FROM "dgmAttributeTypes""#, &[])
            .await?;
        let attributes = rows
            .iter()
            .map(|r| (r.get::<_, i32>(0), AttributeType { default: r.get(1), stackable: r.get::<_, i32>(2) != 0 }))
            .collect();
        Ok(Catalog { attributes })
    }
}

pub struct Loadout<'c> {
    catalog: &'c Catalog,
    pub pieces: Vec<Piece>,
    modifiers: Vec<Modifier>,
    incoming: HashMap<(usize, i32), Vec<Incoming>>,
    memo: RefCell<HashMap<(usize, i32), f64>>,
    busy: RefCell<HashSet<(usize, i32)>>,
}

impl<'c> Loadout<'c> {
    /// The character, its skills and implants, and (if given) its ship with
    /// whatever is fitted. A module is online when its isOnline attribute says
    /// so; `active` lists modules whose active effects are running.
    pub async fn load(
        db: &impl GenericClient,
        catalog: &'c Catalog,
        char_id: i64,
        ship_id: Option<i64>,
        active: &HashSet<i64>,
    ) -> Result<Loadout<'c>, tokio_postgres::Error> {
        let rows = db
            .query(
                r#"SELECT e."itemID", e."typeID", t."groupID", g."categoryID", e."flag", e."locationID"
                     FROM entity e
                     JOIN "invTypes" t ON t."typeID" = e."typeID"
                     JOIN "invGroups" g ON g."groupID" = t."groupID"
                    WHERE e."itemID" = $1 OR e."itemID" = $2
                       OR (e."locationID" = $1 AND e."flag" IN ($3, $4, $5, $6))
                       OR (e."locationID" = $2 AND (e."flag" BETWEEN $7 AND $8 OR e."flag" BETWEEN $9 AND $10))
                    ORDER BY e."itemID""#,
                &[
                    &(char_id as i32),
                    &(ship_id.unwrap_or(0) as i32),
                    &FLAG_SKILL,
                    &FLAG_SKILL_IN_TRAINING,
                    &FLAG_BOOSTER,
                    &FLAG_IMPLANT,
                    &(FLAG_SLOT_FIRST as i32),
                    &(FLAG_SLOT_LAST as i32),
                    &(FLAG_RIG_FIRST as i32),
                    &(FLAG_RIG_LAST as i32),
                ],
            )
            .await?;
        let mut pieces = Vec::with_capacity(rows.len());
        for r in &rows {
            let item_id = r.get::<_, i32>(0) as i64;
            let category_id = r.get::<_, i32>(3) as i64;
            let flag = r.get::<_, i32>(4) as i64;
            let role = if item_id == char_id {
                Role::Character
            } else if Some(item_id) == ship_id {
                Role::Ship
            } else if category_id == category::SKILL {
                Role::Skill
            } else if r.get::<_, i32>(5) as i64 == char_id {
                Role::Implant
            } else if category_id == category::CHARGE {
                Role::Charge { module: usize::MAX }
            } else {
                Role::Module
            };
            pieces.push(Piece {
                item_id,
                type_id: r.get::<_, i32>(1) as i64,
                group_id: r.get::<_, i32>(2) as i64,
                category_id,
                flag,
                role,
                state: State::Offline,
                base: HashMap::new(),
                effects: HashSet::new(),
            });
        }
        // A charge sits in its module's slot.
        for i in 0..pieces.len() {
            if let Role::Charge { .. } = pieces[i].role {
                let flag = pieces[i].flag;
                if let Some(m) = pieces.iter().position(|p| p.role == Role::Module && p.flag == flag) {
                    pieces[i].role = Role::Charge { module: m };
                }
            }
        }

        let type_ids: Vec<i32> = pieces.iter().map(|p| p.type_id as i32).collect();
        let item_ids: Vec<i32> = pieces.iter().map(|p| p.item_id as i32).collect();
        let index: HashMap<i64, usize> = pieces.iter().enumerate().map(|(i, p)| (p.item_id, i)).collect();

        let type_attrs = db
            .query(
                r#"SELECT "typeID", "attributeID", coalesce("valueFloat", "valueInt"::float8)
                     FROM "dgmTypeAttributes" WHERE "typeID" = ANY($1)"#,
                &[&type_ids],
            )
            .await?;
        let mut by_type: HashMap<i64, Vec<(i32, f64)>> = HashMap::new();
        for r in &type_attrs {
            if let Some(v) = r.get::<_, Option<f64>>(2) {
                by_type.entry(r.get::<_, i32>(0) as i64).or_default().push((r.get(1), v));
            }
        }
        for p in pieces.iter_mut() {
            p.base.extend(by_type.get(&p.type_id).into_iter().flatten().copied());
        }
        let own = db
            .query(
                r#"SELECT "itemID", "attributeID", coalesce("valueFloat", "valueInt"::float8)
                     FROM entity_attributes WHERE "itemID" = ANY($1)"#,
                &[&item_ids],
            )
            .await?;
        for r in &own {
            let (Some(&i), Some(v)) = (index.get(&(r.get::<_, i32>(0) as i64)), r.get::<_, Option<f64>>(2)) else {
                continue;
            };
            pieces[i].base.insert(r.get(1), v);
        }

        let effects = db
            .query(r#"SELECT "typeID", "effectID" FROM "dgmTypeEffects" WHERE "typeID" = ANY($1)"#, &[&type_ids])
            .await?;
        let mut effects_by_type: HashMap<i64, HashSet<i32>> = HashMap::new();
        for r in &effects {
            effects_by_type.entry(r.get::<_, i32>(0) as i64).or_default().insert(r.get(1));
        }
        for p in pieces.iter_mut() {
            p.effects = effects_by_type.get(&p.type_id).cloned().unwrap_or_default();
            p.state = match p.role {
                Role::Module if active.contains(&p.item_id) => State::Active,
                Role::Module if p.base.get(&attr::IS_ONLINE).copied().unwrap_or(0.0) != 0.0 => State::Online,
                Role::Module => State::Offline,
                _ => State::Online,
            };
        }

        let rows = db
            .query(
                r#"SELECT "typeID","effectCategory",func,domain,"modifiedAttributeID","modifyingAttributeID",
                          operation,"groupID","skillTypeID","offWhileActive"
                     FROM "dgmTypeModifiers" WHERE "typeID" = ANY($1)"#,
                &[&type_ids],
            )
            .await?;
        let modifiers = rows
            .iter()
            .filter_map(|r| {
                let func = match r.get::<_, &str>(2) {
                    "ItemModifier" => Func::Item,
                    "LocationGroupModifier" => Func::LocationGroup,
                    "LocationRequiredSkillModifier" => Func::LocationRequiredSkill,
                    "OwnerRequiredSkillModifier" => Func::OwnerRequiredSkill,
                    "LocationModifier" => Func::Location,
                    _ => return None,
                };
                let domain = match r.get::<_, &str>(3) {
                    "shipID" => Domain::Ship,
                    "charID" => Domain::Char,
                    "itemID" => Domain::Itself,
                    "otherID" => Domain::Other,
                    "targetID" | "target" => Domain::Target,
                    _ => return None,
                };
                Some(Modifier {
                    type_id: r.get::<_, i32>(0) as i64,
                    category: r.get(1),
                    func,
                    domain,
                    modified: r.get(4),
                    modifying: r.get(5),
                    operation: r.get(6),
                    group_id: r.get::<_, Option<i32>>(7).map(i64::from),
                    skill_type_id: r.get::<_, Option<i32>>(8).map(i64::from),
                    off_while_active: r.get(9),
                })
            })
            .collect();

        let mut loadout = Loadout {
            catalog,
            pieces,
            modifiers,
            incoming: HashMap::new(),
            memo: RefCell::new(HashMap::new()),
            busy: RefCell::new(HashSet::new()),
        };
        loadout.wire();
        Ok(loadout)
    }

    fn index_of(&self, role: Role) -> Option<usize> {
        self.pieces.iter().position(|p| p.role == role)
    }

    pub fn find(&self, item_id: i64) -> Option<usize> {
        self.pieces.iter().position(|p| p.item_id == item_id)
    }

    /// Whether a modifier of `category` from piece `i` applies right now.
    fn applies(&self, i: usize, m: &Modifier) -> bool {
        let p = &self.pieces[i];
        match p.role {
            Role::Module => match m.category {
                PASSIVE => true,
                ONLINE => match p.state {
                    State::Online => true,
                    State::Active => !m.off_while_active,
                    State::Offline => false,
                },
                ACTIVE => p.state == State::Active,
                _ => false,
            },
            // A loaded charge counts; its active effects only while its module runs.
            Role::Charge { module } if module != usize::MAX => {
                m.category != ACTIVE || self.pieces[module].state == State::Active
            }
            Role::Charge { .. } => false,
            _ => m.category == PASSIVE || m.category == ONLINE,
        }
    }

    /// Resolve every modifier to the (piece, attribute) pairs it touches.
    fn wire(&mut self) {
        let mut incoming: HashMap<(usize, i32), Vec<Incoming>> = HashMap::new();
        let ship = self.index_of(Role::Ship);
        let character = self.index_of(Role::Character);
        let on_ship: Vec<usize> = self
            .pieces
            .iter()
            .enumerate()
            .filter(|(_, p)| matches!(p.role, Role::Module | Role::Charge { .. }))
            .map(|(i, _)| i)
            .collect();

        for (i, p) in self.pieces.iter().enumerate() {
            for m in self.modifiers.iter().filter(|m| m.type_id == p.type_id) {
                if !self.applies(i, m) {
                    continue;
                }
                let domain = match m.domain {
                    Domain::Ship => ship,
                    Domain::Char => character,
                    Domain::Itself => Some(i),
                    Domain::Other => match p.role {
                        Role::Charge { module } if module != usize::MAX => Some(module),
                        Role::Module => self.pieces.iter().position(|c| c.role == Role::Charge { module: i }),
                        _ => None,
                    },
                    Domain::Target => None,
                };
                let Some(domain) = domain else { continue };
                let located: &[usize] = if Some(domain) == ship { &on_ship } else { &[] };
                let targets: Vec<usize> = match m.func {
                    Func::Item => vec![domain],
                    Func::Location => located.to_vec(),
                    Func::LocationGroup => {
                        located.iter().copied().filter(|&t| Some(self.pieces[t].group_id) == m.group_id).collect()
                    }
                    Func::LocationRequiredSkill | Func::OwnerRequiredSkill => {
                        let skill = m.skill_type_id.unwrap_or(-1);
                        let scope: &[usize] = if m.func == Func::OwnerRequiredSkill { &on_ship } else { located };
                        scope.iter().copied().filter(|&t| self.pieces[t].required_skills().any(|s| s == skill)).collect()
                    }
                };
                for t in targets {
                    incoming.entry((t, m.modified)).or_default().push(Incoming {
                        source: i,
                        modifying: m.modifying,
                        operation: m.operation,
                        fixed: None,
                    });
                }
            }
        }

        // What 2.10 hard-coded: fitting load and hardpoints.
        if let Some(ship) = ship {
            for (i, p) in self.pieces.iter().enumerate().filter(|(_, p)| p.role == Role::Module) {
                if p.state != State::Offline {
                    for (load, need) in [(attr::CPU_LOAD, attr::CPU), (attr::POWER_LOAD, attr::POWER)] {
                        incoming.entry((ship, load)).or_default().push(Incoming { source: i, modifying: need, operation: 2, fixed: None });
                    }
                }
                for (fitted, left) in [(effect::TURRET_FITTED, attr::TURRET_SLOTS_LEFT), (effect::LAUNCHER_FITTED, attr::LAUNCHER_SLOTS_LEFT)] {
                    if p.has_effect(fitted) {
                        incoming.entry((ship, left)).or_default().push(Incoming { source: i, modifying: 0, operation: 3, fixed: Some(1.0) });
                    }
                }
            }
        }
        self.incoming = incoming;
    }

    fn base(&self, i: usize, a: i32) -> f64 {
        self.pieces[i]
            .base
            .get(&a)
            .copied()
            .unwrap_or_else(|| self.catalog.attributes.get(&a).map_or(0.0, |t| t.default))
    }

    /// The value of attribute `a` on piece `i` with every modifier applied.
    pub fn value(&self, i: usize, a: i32) -> f64 {
        if let Some(v) = self.memo.borrow().get(&(i, a)) {
            return *v;
        }
        // A modifier loop (none should exist) falls back to the base value.
        if !self.busy.borrow_mut().insert((i, a)) {
            return self.base(i, a);
        }
        let v = self.compute(i, a);
        self.busy.borrow_mut().remove(&(i, a));
        self.memo.borrow_mut().insert((i, a), v);
        v
    }

    fn compute(&self, i: usize, a: i32) -> f64 {
        let mut value = self.base(i, a);
        let Some(mods) = self.incoming.get(&(i, a)) else { return value };
        let stackable = self.catalog.attributes.get(&a).map_or(true, |t| t.stackable);
        let mut by_op: HashMap<i16, Vec<(f64, bool)>> = HashMap::new();
        for m in mods {
            let v = m.fixed.unwrap_or_else(|| self.value(m.source, m.modifying));
            let exempt = matches!(
                self.pieces[m.source].category_id,
                category::SHIP | category::CHARGE | category::SKILL | category::IMPLANT | category::SUBSYSTEM
            ) || self.pieces[m.source].role == Role::Character;
            by_op.entry(m.operation).or_default().push((v, !stackable && !exempt));
        }
        for op in [-1, 0, 1, 2, 3, 4, 5, 6, 7] {
            let Some(vals) = by_op.get(&op) else { continue };
            match op {
                -1 | 7 => value = vals.last().map_or(value, |(v, _)| *v),
                2 => value += vals.iter().map(|(v, _)| v).sum::<f64>(),
                3 => value -= vals.iter().map(|(v, _)| v).sum::<f64>(),
                _ => {
                    let factor = |v: f64| match op {
                        0 | 4 => v,
                        1 | 5 => {
                            if v == 0.0 {
                                1.0
                            } else {
                                1.0 / v
                            }
                        }
                        _ => 1.0 + v / 100.0,
                    };
                    let free: f64 = vals.iter().filter(|(_, p)| !p).map(|(v, _)| factor(*v)).product();
                    let penalised: Vec<f64> = vals.iter().filter(|(_, p)| *p).map(|(v, _)| factor(*v)).collect();
                    value *= free * stacking_penalised(&penalised);
                }
            }
        }
        value
    }

    /// Every attribute piece `i` has, computed.
    pub fn attributes(&self, i: usize) -> HashMap<i32, f64> {
        let mut ids: HashSet<i32> = self.pieces[i].base_attributes().collect();
        ids.extend(self.incoming.keys().filter(|(p, _)| *p == i).map(|(_, a)| *a));
        ids.into_iter().map(|a| (a, self.value(i, a))).collect()
    }

    /// Every piece's computed attributes, by item ID.
    pub fn snapshot(&self) -> HashMap<i64, HashMap<i32, f64>> {
        (0..self.pieces.len()).map(|i| (self.pieces[i].item_id, self.attributes(i))).collect()
    }
}

/// Multiply `factors` together, the n-th furthest from 1 (counted separately
/// for bonuses and penalties) weighted by exp(-(n/2.67)^2).
fn stacking_penalised(factors: &[f64]) -> f64 {
    let mut total = 1.0;
    for side in [true, false] {
        let mut group: Vec<f64> = factors.iter().copied().filter(|f| (*f > 1.0) == side && *f != 1.0).collect();
        group.sort_by(|a, b| (b - 1.0).abs().total_cmp(&(a - 1.0).abs()));
        for (n, f) in group.iter().enumerate() {
            total *= 1.0 + (f - 1.0) * (-(n as f64 / 2.67).powi(2)).exp();
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_strongest_bonus_counts_in_full_and_the_rest_less() {
        let one = stacking_penalised(&[1.1]);
        assert!((one - 1.1).abs() < 1e-9);
        let two = stacking_penalised(&[1.1, 1.1]);
        let expected = 1.1 * (1.0 + 0.1 * (-(1.0f64 / 2.67).powi(2)).exp());
        assert!((two - expected).abs() < 1e-9, "{two} vs {expected}");
    }

    #[test]
    fn bonuses_and_penalties_are_penalised_separately() {
        let v = stacking_penalised(&[1.2, 0.8]);
        assert!((v - 1.2 * 0.8).abs() < 1e-9);
    }
}
