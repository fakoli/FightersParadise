//! # fp-combat
//!
//! Combat data model and hit-detection primitive for the Fighters Paradise engine.
//!
//! This crate is a deliberately **leaf** crate: it depends only on [`fp_core`] and
//! [`fp_physics`], never on `fp-character` or `fp-vm`. This keeps the dependency graph
//! acyclic (`fp-character` depends on `fp-combat`, not the other way around).
//!
//! # Scope
//!
//! Phase 6.1 covers **data + geometry** only:
//!
//! - [`HitDef`] — a concrete, plain-data description of a single attack, mirroring
//!   MUGEN's `HitDef` state controller parameters. The HitDef state *controller* (task
//!   6.2) is responsible for evaluating expressions and populating this struct; the
//!   damage / guard *resolution* logic (task 6.3) consumes it.
//! - [`detect_hit`] — the geometric hit-detection primitive: does any attacker `Clsn1`
//!   box overlap any defender `Clsn2` box in world space?
//!
//! Damage, guard, priority and juggle *resolution* are intentionally **out of scope**
//! here.
//!
//! # MUGEN background
//!
//! In MUGEN, an attack carries an `attr` (attack attribute) of the form
//! `<state-class>, <attack-string>` — for example `S, NA` (standing normal attack). A
//! hit lands when, while a `HitDef` is active, any of the attacker's `Clsn1` (attack)
//! boxes overlaps any of the defender's `Clsn2` (hurt) boxes. See the engine
//! architecture knowledge base, §5, for the full parameter list.
//!
//! # Conventions
//!
//! Following the workspace philosophy, nothing in this crate ever panics: parsing of
//! malformed input substitutes documented safe defaults and logs via [`tracing`].

#![warn(missing_docs)]

use fp_core::Vec2;
use fp_physics::{any_overlap, place_clsn, Clsn, Facing};

// Re-export the geometry types callers need so they can build the slices for
// [`detect_hit`] without depending on `fp-physics` directly.
pub use fp_physics::{Clsn as ClsnBox, Facing as ClsnFacing};

/// MUGEN state-class of an attack: which stance the *attacker* is in.
///
/// This is the first token of a HitDef `attr` string (e.g. the `S` in `S, NA`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StateClass {
    /// Standing (`S`).
    Standing,
    /// Crouching (`C`).
    Crouching,
    /// Airborne (`A`).
    Air,
}

impl Default for StateClass {
    /// Defaults to [`StateClass::Standing`] — the most common attack stance and a safe
    /// fallback for malformed input.
    fn default() -> Self {
        StateClass::Standing
    }
}

/// The "power class" of an attack: the first character of the 2-char attack string.
///
/// `{N|S|H}` = Normal / Special / Hyper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttackPower {
    /// Normal attack (`N`).
    Normal,
    /// Special attack (`S`).
    Special,
    /// Hyper attack (`H`).
    Hyper,
}

impl Default for AttackPower {
    /// Defaults to [`AttackPower::Normal`].
    fn default() -> Self {
        AttackPower::Normal
    }
}

/// The "delivery kind" of an attack: the second character of the 2-char attack string.
///
/// `{A|T|P}` = Attack (normal strike) / Throw / Projectile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttackKind {
    /// A normal attack / strike (`A`).
    Attack,
    /// A throw (`T`).
    Throw,
    /// A projectile (`P`).
    Projectile,
}

impl Default for AttackKind {
    /// Defaults to [`AttackKind::Attack`].
    fn default() -> Self {
        AttackKind::Attack
    }
}

/// A parsed MUGEN attack attribute (`attr`), e.g. `S, NA`.
///
/// Composed of the attacker's [`StateClass`] plus a 2-character attack string
/// ([`AttackPower`] + [`AttackKind`]). Build one with [`AttackAttr::parse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct AttackAttr {
    /// State-class of the attacker (`S`/`C`/`A`).
    pub class: StateClass,
    /// Power class of the attack (`N`/`S`/`H`).
    pub power: AttackPower,
    /// Delivery kind of the attack (`A`/`T`/`P`).
    pub kind: AttackKind,
}

impl AttackAttr {
    /// Parses a MUGEN attack-attribute string of the form `"<class>, <PK>"`.
    ///
    /// Accepted forms include `"S, NA"`, `"C, HP"`, `"A, ST"`. Parsing is tolerant of
    /// surrounding whitespace and of letter case (`"s,na"` works). The comma is
    /// optional — `"S NA"` also parses.
    ///
    /// On any malformed input (unknown class letter, attack string not exactly two
    /// recognised characters, missing tokens, etc.) this returns
    /// [`AttackAttr::default`] (`S, NA`) and logs a warning via [`tracing::warn!`]
    /// rather than failing. It **never panics**.
    ///
    /// # Examples
    ///
    /// ```
    /// use fp_combat::{AttackAttr, StateClass, AttackPower, AttackKind};
    ///
    /// let a = AttackAttr::parse("S, NA");
    /// assert_eq!(a.class, StateClass::Standing);
    /// assert_eq!(a.power, AttackPower::Normal);
    /// assert_eq!(a.kind, AttackKind::Attack);
    ///
    /// // Whitespace / case tolerant.
    /// assert_eq!(AttackAttr::parse("  c ,  hp "), AttackAttr::parse("C, HP"));
    ///
    /// // Malformed input falls back to the default (S, NA) rather than panicking.
    /// assert_eq!(AttackAttr::parse("garbage"), AttackAttr::default());
    /// ```
    pub fn parse(s: &str) -> Self {
        match Self::try_parse(s) {
            Some(attr) => attr,
            None => {
                tracing::warn!(input = %s, "malformed attack attribute; using default (S, NA)");
                Self::default()
            }
        }
    }

    /// Fallible core of [`AttackAttr::parse`]. Returns `None` on malformed input.
    ///
    /// Kept private so the public surface always yields a value (never an error), in
    /// keeping with the engine's "never crash on bad content" philosophy.
    fn try_parse(s: &str) -> Option<Self> {
        // Split into class token and attack-string token. The separator may be a comma
        // or whitespace; tolerate either and any surrounding spaces.
        let mut parts = s.split(',');
        let class_tok = parts.next()?.trim();
        // Everything after the first comma is the attack string (it shouldn't itself
        // contain commas, but joining defends against odd input without panicking).
        let rest: String = parts.collect::<Vec<_>>().join(",");
        if rest.trim().is_empty() {
            // No comma: maybe whitespace-separated like "S NA".
            let mut ws = class_tok.split_whitespace();
            let class_only = ws.next()?;
            let attack = ws.next().unwrap_or("");
            return Self::from_tokens(class_only, attack);
        }

        Self::from_tokens(class_tok, rest.trim())
    }

    /// Builds an [`AttackAttr`] from already-split class and attack tokens.
    fn from_tokens(class_tok: &str, attack_tok: &str) -> Option<Self> {
        let class = match class_tok.trim().to_ascii_uppercase().as_str() {
            "S" => StateClass::Standing,
            "C" => StateClass::Crouching,
            "A" => StateClass::Air,
            _ => return None,
        };

        let attack = attack_tok.trim();
        if attack.chars().count() != 2 {
            return None;
        }
        let upper = attack.to_ascii_uppercase();
        let mut chars = upper.chars();
        // Both `next()` calls are guaranteed to succeed by the length check above.
        let p = chars.next()?;
        let k = chars.next()?;

        let power = match p {
            'N' => AttackPower::Normal,
            'S' => AttackPower::Special,
            'H' => AttackPower::Hyper,
            _ => return None,
        };
        let kind = match k {
            'A' => AttackKind::Attack,
            'T' => AttackKind::Throw,
            'P' => AttackKind::Projectile,
            _ => return None,
        };

        Some(Self { class, power, kind })
    }
}

/// A set of hit/guard target flags, a bit-set over MUGEN's `H/L/A/M/F/D` letters.
///
/// Used by both [`HitDef::guardflag`] and [`HitDef::hitflag`]:
///
/// - `H` — affects a high (standing) defender.
/// - `L` — affects a low (crouching) defender.
/// - `A` — affects an airborne defender.
/// - `M` — shorthand for both ground heights (`H` + `L`).
/// - `F` — affects a falling defender / allows juggle (hitflag only, semantically).
/// - `D` — affects a downed defender (hitflag only, semantically).
///
/// This is a tiny, dependency-free bit-set (no external `bitflags` crate) so the crate
/// stays leaf-weight. Parsing is whitespace/case tolerant and never panics.
///
/// # Examples
///
/// ```
/// use fp_combat::HitFlags;
///
/// let f = HitFlags::parse("MAF");
/// assert!(f.high() && f.low() && f.air() && f.fall());
/// assert!(!f.down());
///
/// // `M` expands to H + L.
/// assert!(HitFlags::parse("M").high());
/// assert!(HitFlags::parse("M").low());
///
/// // Empty guardflag = unblockable.
/// assert!(HitFlags::parse("").is_empty());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct HitFlags(u8);

impl HitFlags {
    /// Bit for high (standing) targets (`H`).
    pub const HIGH: u8 = 1 << 0;
    /// Bit for low (crouching) targets (`L`).
    pub const LOW: u8 = 1 << 1;
    /// Bit for airborne targets (`A`).
    pub const AIR: u8 = 1 << 2;
    /// Bit for falling targets / juggle-allowing (`F`).
    pub const FALL: u8 = 1 << 3;
    /// Bit for downed targets (`D`).
    pub const DOWN: u8 = 1 << 4;

    /// Creates an empty flag set (no targets — for a guardflag, "unblockable").
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Creates a flag set directly from a raw bit value (masked to known bits).
    pub const fn from_bits_truncate(bits: u8) -> Self {
        Self(bits & (Self::HIGH | Self::LOW | Self::AIR | Self::FALL | Self::DOWN))
    }

    /// Returns the raw bit value.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Returns `true` if no flags are set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Returns `true` if every bit in `other` is also set in `self`.
    pub const fn contains(self, other: HitFlags) -> bool {
        (self.0 & other.0) == other.0
    }

    /// `true` if the high (standing) bit is set.
    pub const fn high(self) -> bool {
        self.0 & Self::HIGH != 0
    }
    /// `true` if the low (crouching) bit is set.
    pub const fn low(self) -> bool {
        self.0 & Self::LOW != 0
    }
    /// `true` if the air bit is set.
    pub const fn air(self) -> bool {
        self.0 & Self::AIR != 0
    }
    /// `true` if the fall bit is set.
    pub const fn fall(self) -> bool {
        self.0 & Self::FALL != 0
    }
    /// `true` if the down bit is set.
    pub const fn down(self) -> bool {
        self.0 & Self::DOWN != 0
    }

    /// Parses a MUGEN flag string such as `"MAF"`, `"HLA"`, `"HL"` or `""`.
    ///
    /// Each recognised letter (`H`/`L`/`A`/`M`/`F`/`D`, case-insensitive) sets the
    /// corresponding bit; `M` sets both `H` and `L`. Whitespace and unrecognised
    /// characters are ignored (a warning is logged for unrecognised ones). An empty
    /// string yields [`HitFlags::empty`]. Never panics.
    pub fn parse(s: &str) -> Self {
        let mut bits: u8 = 0;
        for c in s.chars() {
            if c.is_whitespace() {
                continue;
            }
            match c.to_ascii_uppercase() {
                'H' => bits |= Self::HIGH,
                'L' => bits |= Self::LOW,
                'A' => bits |= Self::AIR,
                'M' => bits |= Self::HIGH | Self::LOW,
                'F' => bits |= Self::FALL,
                'D' => bits |= Self::DOWN,
                other => {
                    tracing::warn!(
                        flag_char = %other,
                        input = %s,
                        "unrecognised hit/guard flag; ignoring"
                    );
                }
            }
        }
        Self(bits)
    }
}

/// The MUGEN `ground.type` / `air.type` of a hit — how the defender reacts on the ground.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HitType {
    /// `High` — a high hit (e.g. a standing reaction).
    High,
    /// `Low` — a low hit (e.g. a crouching reaction).
    Low,
    /// `Trip` — a trip / sweep that knocks the defender down.
    Trip,
    /// `None` — no hit reaction is applied.
    None,
}

impl Default for HitType {
    /// Defaults to [`HitType::High`], matching MUGEN's default `ground.type = High`.
    fn default() -> Self {
        HitType::High
    }
}

/// The MUGEN `priority` type, used to resolve simultaneous (trading) hits.
///
/// Resolution itself is task 6.3; this enum is just the data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PriorityType {
    /// `Hit` — both attacks connect (trade).
    Hit,
    /// `Miss` — this attack is ignored when it loses a priority comparison.
    Miss,
    /// `Dodge` — neither attack connects.
    Dodge,
}

impl Default for PriorityType {
    /// Defaults to [`PriorityType::Hit`], matching MUGEN's default priority type.
    fn default() -> Self {
        PriorityType::Hit
    }
}

/// Hit/guard priority: a numeric value (MUGEN `1..=7`, default `4`) plus a [`PriorityType`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Priority {
    /// Numeric priority value (MUGEN clamps to `1..=7`; default `4`).
    pub value: i32,
    /// How a priority comparison is resolved.
    pub kind: PriorityType,
}

impl Default for Priority {
    /// Defaults to value `4`, type [`PriorityType::Hit`] (MUGEN's defaults).
    fn default() -> Self {
        Self {
            value: 4,
            kind: PriorityType::Hit,
        }
    }
}

/// A `(hit, guard)` pair of damage values: damage dealt on a clean hit vs. on block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Damage {
    /// Damage dealt when the attack lands cleanly.
    pub hit: i32,
    /// Damage dealt when the attack is guarded (blocked).
    pub guard: i32,
}

/// A `(p1, p2)` pause-time pair, in ticks.
///
/// `p1` is how long the attacker pauses; `p2` is the defender's shake time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PauseTime {
    /// Ticks the attacker (P1) is paused.
    pub p1: i32,
    /// Ticks the defender (P2) shakes.
    pub p2: i32,
}

/// Per-situation hit-time values (ticks the defender stays in hit-stun), in ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HitTimes {
    /// `ground.hittime` — hit-stun on the ground (MUGEN default `0`).
    pub ground: i32,
    /// `air.hittime` — hit-stun in the air (MUGEN default `20`).
    pub air: i32,
    /// `guard.hittime` — guard-stun when blocked (MUGEN default `0`).
    pub guard: i32,
}

impl Default for HitTimes {
    /// MUGEN defaults: `ground = 0`, `air = 20`, `guard = 0`.
    fn default() -> Self {
        Self {
            ground: 0,
            air: 20,
            guard: 0,
        }
    }
}

/// Resource id fields attached to a hit: spark, hit sound, guard sound.
///
/// Each is a raw numeric id; a value of `-1` means "unset / none" by MUGEN convention.
/// The `S`-prefix (use the character's own AIR/SND rather than the common set) is not
/// modelled here — it is resolved by the controller in task 6.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HitResources {
    /// `sparkno` — spark animation action id (`-1` = none).
    pub sparkno: i32,
    /// `hitsound` — sound id played on a clean hit (`-1` = none).
    pub hitsound: i32,
    /// `guardsound` — sound id played when guarded (`-1` = none).
    pub guardsound: i32,
}

impl Default for HitResources {
    /// All ids default to `-1` (unset), matching MUGEN's "no resource" convention.
    fn default() -> Self {
        Self {
            sparkno: -1,
            hitsound: -1,
            guardsound: -1,
        }
    }
}

/// A concrete, plain-data MUGEN `HitDef`.
///
/// This is **pure data**: it carries the resolved numeric/enum parameters of a single
/// attack. The HitDef state *controller* (task 6.2) evaluates the character's
/// expressions and builds one of these; the damage/guard *resolution* logic (task 6.3)
/// reads it. No method on `HitDef` performs game logic.
///
/// All fields have sensible MUGEN-faithful defaults via [`HitDef::default`].
///
/// # Examples
///
/// ```
/// use fp_combat::{HitDef, AttackAttr, Damage};
///
/// let hd = HitDef {
///     attr: AttackAttr::parse("S, NA"),
///     damage: Damage { hit: 20, guard: 5 },
///     ..HitDef::default()
/// };
/// assert_eq!(hd.damage.hit, 20);
/// // p2stateno defaults to "no change".
/// assert_eq!(hd.p2stateno, None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HitDef {
    /// `attr` — the attack attribute (state-class + 2-char attack string).
    pub attr: AttackAttr,
    /// `damage` — `(hit, guard)` damage values.
    pub damage: Damage,
    /// `guardflag` — which stances may block this; empty = **unblockable**.
    pub guardflag: HitFlags,
    /// `hitflag` — which defender states this hit affects (MUGEN default `"MAF"`).
    pub hitflag: HitFlags,
    /// `pausetime` — `(p1_pausetime, p2_shaketime)` in ticks.
    pub pausetime: PauseTime,
    /// Spark / sound resource ids (`sparkno`, `hitsound`, `guardsound`).
    pub resources: HitResources,
    /// `ground.type` — the ground hit reaction type.
    pub ground_type: HitType,
    /// `ground.velocity` — `(x, y)` knockback applied to a grounded defender.
    pub ground_velocity: Vec2<f32>,
    /// `air.velocity` — `(x, y)` knockback applied to an airborne defender.
    pub air_velocity: Vec2<f32>,
    /// `guard.velocity` — `x` pushback applied to a guarding defender (Y unused).
    pub guard_velocity: f32,
    /// Per-situation hit-stun times (`ground`/`air`/`guard`).
    pub hittimes: HitTimes,
    /// `fall` — whether this hit knocks the defender into a falling state.
    pub fall: bool,
    /// `fall.yvelocity` — initial upward Y velocity when entering the falling state.
    pub fall_yvelocity: f32,
    /// `p1stateno` — state to force the **attacker** into (`None` = no change).
    pub p1stateno: Option<i32>,
    /// `p2stateno` — state to force the **defender** into (`None` = no change).
    pub p2stateno: Option<i32>,
    /// `priority` — `(value, type)` used to resolve trades.
    pub priority: Priority,
    /// `id` — this hit's id (for `chainID`/`hitcount` bookkeeping; `0` = unset).
    pub id: i32,
    /// `chainid` — the id this hit may chain from (`-1` = any / unset).
    pub chainid: i32,
}

impl Default for HitDef {
    /// MUGEN-faithful defaults. Note the two sentinels that differ from a naive
    /// zero-default: `hitflag` defaults to `"MAF"` (mid + air + fall-allows-juggle)
    /// and `chainid` defaults to `-1` ("chain from any"); `0` is a distinct valid id.
    fn default() -> Self {
        Self {
            attr: AttackAttr::default(),
            damage: Damage::default(),
            guardflag: HitFlags::default(),
            hitflag: HitFlags::parse("MAF"),
            pausetime: PauseTime::default(),
            resources: HitResources::default(),
            ground_type: HitType::default(),
            ground_velocity: Vec2::default(),
            air_velocity: Vec2::default(),
            guard_velocity: 0.0,
            hittimes: HitTimes::default(),
            fall: false,
            fall_yvelocity: 0.0,
            p1stateno: None,
            p2stateno: None,
            priority: Priority::default(),
            id: 0,
            chainid: -1,
        }
    }
}

/// Where two box sets connected, returned by [`detect_hit_contact`].
///
/// Reports the world-space overlap (intersection) rectangle of the **first** colliding
/// attacker `Clsn1` / defender `Clsn2` pair, plus its center point — handy for placing a
/// hit spark. This is geometry only; it carries no [`HitDef`] / damage information.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HitContact {
    /// World-space center of the overlap region (a good spark anchor).
    pub point: Vec2<f32>,
    /// Left edge X of the overlap region.
    pub x: f32,
    /// Top edge Y of the overlap region.
    pub y: f32,
    /// Width of the overlap region (always `>= 0`).
    pub w: f32,
    /// Height of the overlap region (always `>= 0`).
    pub h: f32,
}

/// Detects whether an attack connects: does any attacker `Clsn1` box overlap any
/// defender `Clsn2` box, once both are placed into world space?
///
/// This is the MUGEN hit-detection primitive. Both box sets are character-local; they
/// are transformed into world space via [`fp_physics::place_clsn`] (applying position
/// and facing mirroring) and tested with [`fp_physics::any_overlap`].
///
/// Pure and deterministic; **never panics**. Empty slices never produce a hit.
///
/// For the connection geometry (overlap rectangle / spark point), use
/// [`detect_hit_contact`].
///
/// # Arguments
///
/// - `attacker_clsn1` — the attacker's attack (`Clsn1`) boxes, in local space.
/// - `attacker_pos` / `attacker_facing` — the attacker's axis position and facing.
/// - `defender_clsn2` — the defender's hurt (`Clsn2`) boxes, in local space.
/// - `defender_pos` / `defender_facing` — the defender's axis position and facing.
///
/// # Examples
///
/// ```
/// use fp_core::Vec2;
/// use fp_combat::{detect_hit, ClsnBox, ClsnFacing};
///
/// // Attacker at x=0 facing right; a punch reaching out to x=55.
/// let attack = [ClsnBox::new(10.0, -60.0, 55.0, -40.0)];
/// // Defender at x=60 facing left; hurt box ±18 about its axis -> world 42..78.
/// let hurt = [ClsnBox::new(-18.0, -70.0, 18.0, 0.0)];
///
/// assert!(detect_hit(
///     &attack, Vec2::new(0.0, 0.0), ClsnFacing::Right,
///     &hurt, Vec2::new(60.0, 0.0), ClsnFacing::Left,
/// ));
///
/// // No hurt boxes -> never a hit.
/// assert!(!detect_hit(
///     &attack, Vec2::new(0.0, 0.0), ClsnFacing::Right,
///     &[], Vec2::new(60.0, 0.0), ClsnFacing::Left,
/// ));
/// ```
pub fn detect_hit(
    attacker_clsn1: &[Clsn],
    attacker_pos: Vec2<f32>,
    attacker_facing: Facing,
    defender_clsn2: &[Clsn],
    defender_pos: Vec2<f32>,
    defender_facing: Facing,
) -> bool {
    // Place both sets into world space, then test any-overlap.
    let world_attack: Vec<_> = attacker_clsn1
        .iter()
        .map(|c| place_clsn(*c, attacker_pos, attacker_facing))
        .collect();
    let world_hurt: Vec<_> = defender_clsn2
        .iter()
        .map(|c| place_clsn(*c, defender_pos, defender_facing))
        .collect();
    any_overlap(&world_attack, &world_hurt)
}

/// Like [`detect_hit`], but returns *where* the attack connected.
///
/// Returns `Some(`[`HitContact`]`)` describing the world-space overlap of the **first**
/// colliding attacker/defender box pair (iteration order: attacker boxes outer,
/// defender boxes inner), or `None` if nothing overlaps.
///
/// "First" is deterministic given the input slice order. Edge-touching boxes (zero
/// shared area) do **not** count as a hit, matching [`fp_physics::any_overlap`].
///
/// Pure and deterministic; **never panics**.
///
/// # Examples
///
/// ```
/// use fp_core::Vec2;
/// use fp_combat::{detect_hit_contact, ClsnBox, ClsnFacing};
///
/// let attack = [ClsnBox::new(10.0, -60.0, 55.0, -40.0)];
/// let hurt = [ClsnBox::new(-18.0, -70.0, 18.0, 0.0)];
///
/// let contact = detect_hit_contact(
///     &attack, Vec2::new(0.0, 0.0), ClsnFacing::Right,
///     &hurt, Vec2::new(60.0, 0.0), ClsnFacing::Left,
/// );
/// assert!(contact.is_some());
/// let c = contact.expect("boxes overlap");
/// assert!(c.w > 0.0 && c.h > 0.0);
/// ```
pub fn detect_hit_contact(
    attacker_clsn1: &[Clsn],
    attacker_pos: Vec2<f32>,
    attacker_facing: Facing,
    defender_clsn2: &[Clsn],
    defender_pos: Vec2<f32>,
    defender_facing: Facing,
) -> Option<HitContact> {
    for ca in attacker_clsn1 {
        let ra = place_clsn(*ca, attacker_pos, attacker_facing);
        for cb in defender_clsn2 {
            let rb = place_clsn(*cb, defender_pos, defender_facing);
            // Strict overlap (positive shared area), matching fp_physics semantics.
            let left = ra.x.max(rb.x);
            let right = ra.right().min(rb.right());
            let top = ra.y.max(rb.y);
            let bottom = ra.bottom().min(rb.bottom());
            if left < right && top < bottom {
                let w = right - left;
                let h = bottom - top;
                return Some(HitContact {
                    point: Vec2::new(left + w / 2.0, top + h / 2.0),
                    x: left,
                    y: top,
                    w,
                    h,
                });
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hitdef_default_is_mugen_faithful() {
        let hd = HitDef::default();
        // Damage defaults to (0, 0).
        assert_eq!(hd.damage, Damage { hit: 0, guard: 0 });
        // Default attr is S, NA.
        assert_eq!(hd.attr, AttackAttr::parse("S, NA"));
        // Hit-times: ground=0, air=20, guard=0.
        assert_eq!(hd.hittimes.ground, 0);
        assert_eq!(hd.hittimes.air, 20);
        assert_eq!(hd.hittimes.guard, 0);
        // No forced states by default.
        assert_eq!(hd.p1stateno, None);
        assert_eq!(hd.p2stateno, None);
        // Priority value 4, type Hit.
        assert_eq!(hd.priority, Priority::default());
        assert_eq!(hd.priority.value, 4);
        // Resource ids default to -1 (none).
        assert_eq!(hd.resources.sparkno, -1);
        // ground.type default High.
        assert_eq!(hd.ground_type, HitType::High);
        // Not falling by default.
        assert!(!hd.fall);
    }

    #[test]
    fn attack_attr_parses_canonical_forms() {
        let a = AttackAttr::parse("S, NA");
        assert_eq!(a.class, StateClass::Standing);
        assert_eq!(a.power, AttackPower::Normal);
        assert_eq!(a.kind, AttackKind::Attack);

        let b = AttackAttr::parse("C, HP");
        assert_eq!(b.class, StateClass::Crouching);
        assert_eq!(b.power, AttackPower::Hyper);
        assert_eq!(b.kind, AttackKind::Projectile);

        let c = AttackAttr::parse("A, ST");
        assert_eq!(c.class, StateClass::Air);
        assert_eq!(c.power, AttackPower::Special);
        assert_eq!(c.kind, AttackKind::Throw);
    }

    #[test]
    fn attack_attr_is_whitespace_and_case_tolerant() {
        assert_eq!(AttackAttr::parse("  c ,  hp "), AttackAttr::parse("C, HP"));
        assert_eq!(AttackAttr::parse("s na"), AttackAttr::parse("S, NA")); // no comma
        assert_eq!(AttackAttr::parse("A,ST"), AttackAttr::parse("A, ST")); // no space
    }

    #[test]
    fn attack_attr_malformed_falls_back_to_default() {
        // Unknown class.
        assert_eq!(AttackAttr::parse("X, NA"), AttackAttr::default());
        // Attack string wrong length.
        assert_eq!(AttackAttr::parse("S, N"), AttackAttr::default());
        assert_eq!(AttackAttr::parse("S, NAB"), AttackAttr::default());
        // Unknown attack letters.
        assert_eq!(AttackAttr::parse("S, ZZ"), AttackAttr::default());
        // Completely garbage / empty.
        assert_eq!(AttackAttr::parse("garbage"), AttackAttr::default());
        assert_eq!(AttackAttr::parse(""), AttackAttr::default());
        // Default is S, NA.
        assert_eq!(AttackAttr::default().class, StateClass::Standing);
    }

    #[test]
    fn hit_flags_parse_m_expansion_and_empty() {
        let f = HitFlags::parse("MAF");
        assert!(f.high() && f.low() && f.air() && f.fall());
        assert!(!f.down());

        // M expands to H | L.
        let m = HitFlags::parse("M");
        assert!(m.high() && m.low());

        // Empty => unblockable guardflag.
        assert!(HitFlags::parse("").is_empty());
        assert!(HitFlags::empty().is_empty());

        // Case-insensitive, whitespace ignored, D recognised.
        let d = HitFlags::parse(" h l d ");
        assert!(d.high() && d.low() && d.down());
        assert!(!d.air());

        // contains() works as a subset test.
        assert!(f.contains(HitFlags::parse("HA")));
        assert!(!HitFlags::parse("H").contains(HitFlags::parse("HL")));
    }

    /// Two characters facing each other; attacker's punch box reaches the defender's
    /// hurt box. Mirroring (facing flip) must place the boxes correctly.
    #[test]
    fn detect_hit_connects_when_boxes_overlap() {
        // Attacker at x=0 facing right; punch box local x 10..55.
        let attack = [Clsn::new(10.0, -60.0, 55.0, -40.0)];
        // Defender at x=60 facing left; hurt box local ±18 -> world 42..78.
        let hurt = [Clsn::new(-18.0, -70.0, 18.0, 0.0)];

        assert!(detect_hit(
            &attack,
            Vec2::new(0.0, 0.0),
            Facing::Right,
            &hurt,
            Vec2::new(60.0, 0.0),
            Facing::Left,
        ));

        // detect_hit_contact agrees and reports a positive-area overlap.
        let c = detect_hit_contact(
            &attack,
            Vec2::new(0.0, 0.0),
            Facing::Right,
            &hurt,
            Vec2::new(60.0, 0.0),
            Facing::Left,
        )
        .expect("should connect");
        assert!(c.w > 0.0 && c.h > 0.0);
    }

    /// Same geometry, but flip the attacker to face *left*: its punch box now mirrors to
    /// the negative side (x -55..-10) and no longer reaches the defender at x=60.
    #[test]
    fn detect_hit_misses_when_facing_flips_box_away() {
        let attack = [Clsn::new(10.0, -60.0, 55.0, -40.0)];
        let hurt = [Clsn::new(-18.0, -70.0, 18.0, 0.0)];

        assert!(!detect_hit(
            &attack,
            Vec2::new(0.0, 0.0),
            Facing::Left, // punch mirrored to the left, away from defender
            &hurt,
            Vec2::new(60.0, 0.0),
            Facing::Left,
        ));

        assert!(detect_hit_contact(
            &attack,
            Vec2::new(0.0, 0.0),
            Facing::Left,
            &hurt,
            Vec2::new(60.0, 0.0),
            Facing::Left,
        )
        .is_none());
    }

    /// A far-away defender is not hit even with valid boxes.
    #[test]
    fn detect_hit_misses_when_too_far() {
        let attack = [Clsn::new(10.0, -60.0, 55.0, -40.0)];
        let hurt = [Clsn::new(-18.0, -70.0, 18.0, 0.0)];
        assert!(!detect_hit(
            &attack,
            Vec2::new(0.0, 0.0),
            Facing::Right,
            &hurt,
            Vec2::new(500.0, 0.0),
            Facing::Left,
        ));
    }

    /// Empty box sets never produce a hit (either side empty).
    #[test]
    fn detect_hit_empty_boxes_never_hit() {
        let attack = [Clsn::new(10.0, -60.0, 55.0, -40.0)];
        let hurt = [Clsn::new(-18.0, -70.0, 18.0, 0.0)];
        let pos = Vec2::new(0.0, 0.0);

        // No attack boxes.
        assert!(!detect_hit(&[], pos, Facing::Right, &hurt, pos, Facing::Left));
        // No hurt boxes.
        assert!(!detect_hit(&attack, pos, Facing::Right, &[], pos, Facing::Left));
        // Both empty.
        assert!(!detect_hit(&[], pos, Facing::Right, &[], pos, Facing::Left));
        // Contact variant agrees.
        assert!(detect_hit_contact(&[], pos, Facing::Right, &hurt, pos, Facing::Left).is_none());
    }

    // ---------------------------------------------------------------------
    // Proctor-added tests: additional edge cases, error paths, and
    // MUGEN-semantics coverage layered on top of Forge's originals.
    // These exercise each acceptance criterion (1) data model + Default,
    // (2) detect_hit / detect_hit_contact world-space overlap, (3) tolerant
    // AttackAttr::parse, (4) leaf-crate / never-panic, (5) inline tests.
    // No impl code is modified.
    // ---------------------------------------------------------------------

    // ---- (1) HitDef plain-data model + Default ----

    /// Every nested default sub-struct must itself default sanely, so a
    /// `HitDef::default()` is a coherent "no-op" attack description.
    #[test]
    fn hitdef_nested_defaults_are_coherent() {
        let hd = HitDef::default();
        // guardflag empty == unblockable by default; hitflag defaults to MUGEN "MAF".
        assert!(hd.guardflag.is_empty());
        assert_eq!(hd.hitflag, HitFlags::parse("MAF"));
        // Pausetime (0, 0).
        assert_eq!(hd.pausetime, PauseTime { p1: 0, p2: 0 });
        // All velocities zero.
        assert_eq!(hd.ground_velocity, Vec2::new(0.0, 0.0));
        assert_eq!(hd.air_velocity, Vec2::new(0.0, 0.0));
        assert_eq!(hd.guard_velocity, 0.0);
        // fall.yvelocity zero, not falling.
        assert_eq!(hd.fall_yvelocity, 0.0);
        assert!(!hd.fall);
        // ids: id 0 (unset), chainid -1 (chain-from-any sentinel).
        assert_eq!(hd.id, 0);
        assert_eq!(hd.chainid, -1);
        // All three resource ids are -1 (none).
        assert_eq!(
            hd.resources,
            HitResources { sparkno: -1, hitsound: -1, guardsound: -1 }
        );
        // priority is the MUGEN default (4, Hit).
        assert_eq!(hd.priority.kind, PriorityType::Hit);
    }

    /// `HitDef` is plain data: `Copy` + struct-update syntax works, and copies
    /// are independent values comparing equal (no interior mutability/refs).
    #[test]
    fn hitdef_is_copy_plain_data() {
        let base = HitDef {
            attr: AttackAttr::parse("C, HP"),
            damage: Damage { hit: 90, guard: 12 },
            guardflag: HitFlags::parse("MA"),
            hitflag: HitFlags::parse("MAF"),
            pausetime: PauseTime { p1: 12, p2: 12 },
            ground_type: HitType::Low,
            ground_velocity: Vec2::new(-4.0, 0.0),
            air_velocity: Vec2::new(-3.0, -6.0),
            guard_velocity: -2.0,
            hittimes: HitTimes { ground: 15, air: 18, guard: 9 },
            fall: true,
            fall_yvelocity: -4.5,
            p1stateno: Some(1000),
            p2stateno: Some(5050),
            priority: Priority { value: 5, kind: PriorityType::Miss },
            id: 7,
            chainid: -1,
            resources: HitResources { sparkno: 2, hitsound: 5, guardsound: 6 },
        };
        let copy = base; // Copy, not move.
        assert_eq!(base, copy); // original still usable -> proves Copy
        assert_eq!(copy.damage.hit, 90);
        assert_eq!(copy.p2stateno, Some(5050));
        assert_eq!(copy.ground_type, HitType::Low);
        assert_eq!(copy.priority.kind, PriorityType::Miss);
    }

    /// All the leaf enums must round-trip through their `Default` and be the
    /// MUGEN-faithful defaults.
    #[test]
    fn enum_defaults_match_mugen() {
        assert_eq!(StateClass::default(), StateClass::Standing);
        assert_eq!(AttackPower::default(), AttackPower::Normal);
        assert_eq!(AttackKind::default(), AttackKind::Attack);
        assert_eq!(HitType::default(), HitType::High);
        assert_eq!(PriorityType::default(), PriorityType::Hit);
        assert_eq!(Priority::default(), Priority { value: 4, kind: PriorityType::Hit });
        assert_eq!(AttackAttr::default(), AttackAttr::parse("S, NA"));
    }

    // ---- (3) AttackAttr::parse: tolerance + malformed safe-default ----

    /// All nine class×(power×kind) canonical combinations parse to the exact
    /// enum triple, covering every recognised letter at least once.
    #[test]
    fn attack_attr_all_letter_combinations() {
        let cases = [
            ("S, NA", StateClass::Standing, AttackPower::Normal, AttackKind::Attack),
            ("S, NT", StateClass::Standing, AttackPower::Normal, AttackKind::Throw),
            ("S, NP", StateClass::Standing, AttackPower::Normal, AttackKind::Projectile),
            ("C, SA", StateClass::Crouching, AttackPower::Special, AttackKind::Attack),
            ("C, ST", StateClass::Crouching, AttackPower::Special, AttackKind::Throw),
            ("C, SP", StateClass::Crouching, AttackPower::Special, AttackKind::Projectile),
            ("A, HA", StateClass::Air, AttackPower::Hyper, AttackKind::Attack),
            ("A, HT", StateClass::Air, AttackPower::Hyper, AttackKind::Throw),
            ("A, HP", StateClass::Air, AttackPower::Hyper, AttackKind::Projectile),
        ];
        for (s, class, power, kind) in cases {
            let a = AttackAttr::parse(s);
            assert_eq!(a, AttackAttr { class, power, kind }, "parsing {s:?}");
        }
    }

    /// Whitespace forms: tabs, leading/trailing spaces, no-space-after-comma,
    /// and whitespace-separated (no comma) all normalize to the same value.
    #[test]
    fn attack_attr_whitespace_variants_all_equal() {
        let canon = AttackAttr::parse("C, HP");
        assert_eq!(AttackAttr::parse("C,HP"), canon);
        assert_eq!(AttackAttr::parse("  C , HP  "), canon);
        assert_eq!(AttackAttr::parse("\tC,\tHP\t"), canon);
        assert_eq!(AttackAttr::parse("C HP"), canon); // whitespace separator, no comma
        assert_eq!(AttackAttr::parse("  c   hp  "), canon); // lowercase + extra spaces
    }

    /// Lowercase and mixed-case for both tokens parse identically to uppercase.
    #[test]
    fn attack_attr_case_insensitive_full() {
        assert_eq!(AttackAttr::parse("a, hp"), AttackAttr::parse("A, HP"));
        assert_eq!(AttackAttr::parse("c, Sp"), AttackAttr::parse("C, SP"));
        assert_eq!(AttackAttr::parse("s, nA"), AttackAttr::parse("S, NA"));
    }

    /// Every malformed shape documented to fall back to the default actually
    /// does — and never panics. Covers: empty, whitespace-only, unknown class,
    /// short/long attack token, unknown power, unknown kind, swapped order,
    /// trailing comma, multiple commas, and a numeric token.
    #[test]
    fn attack_attr_malformed_exhaustive_safe_default() {
        let bad = [
            "",
            "   ",
            ",",
            "S",          // class only, no attack token
            "S,",         // trailing comma -> empty attack
            "S, ",        // comma + whitespace -> empty attack
            "X, NA",      // unknown class
            "S, N",       // attack too short
            "S, NAB",     // attack too long
            "S, ZA",      // unknown power letter
            "S, NZ",      // unknown kind letter
            "S, ZZ",      // both unknown
            "NA, S",      // tokens swapped
            "S, N, A",    // multiple commas
            "1, 23",      // numeric garbage
            "SS, NA",     // 2-char class
            "garbage",
        ];
        for s in bad {
            assert_eq!(
                AttackAttr::parse(s),
                AttackAttr::default(),
                "expected default for malformed input {s:?}"
            );
        }
    }

    /// A 2-char attack token where exactly one half is wrong still fails fully
    /// (no partial accept that keeps the good half).
    #[test]
    fn attack_attr_partial_attack_token_rejected() {
        // power good, kind bad -> default; kind good, power bad -> default.
        assert_eq!(AttackAttr::parse("S, NZ"), AttackAttr::default());
        assert_eq!(AttackAttr::parse("S, ZA"), AttackAttr::default());
        // But a fully-valid token with an unusual but valid class still parses.
        assert_eq!(AttackAttr::parse("A, NP").class, StateClass::Air);
    }

    // ---- HitFlags bit-set semantics (data model helper) ----

    /// Bit constants are distinct powers of two and `from_bits_truncate` masks
    /// off unknown high bits.
    #[test]
    fn hit_flags_bits_and_truncate() {
        // All five bits set via "MAFD" (M=H|L) plus contains/round-trip.
        let all = HitFlags::parse("MAFD");
        assert!(all.high() && all.low() && all.air() && all.fall() && all.down());
        assert_eq!(
            all.bits(),
            HitFlags::HIGH | HitFlags::LOW | HitFlags::AIR | HitFlags::FALL | HitFlags::DOWN
        );
        // Unknown high bits are truncated away.
        let truncated = HitFlags::from_bits_truncate(0b1110_0000 | HitFlags::HIGH);
        assert_eq!(truncated, HitFlags::from_bits_truncate(HitFlags::HIGH));
        assert!(truncated.high() && !truncated.low());
    }

    /// `contains` is a subset test, reflexive, and empty is contained by all.
    #[test]
    fn hit_flags_contains_subset_semantics() {
        let hla = HitFlags::parse("HLA");
        assert!(hla.contains(hla)); // reflexive
        assert!(hla.contains(HitFlags::parse("HL")));
        assert!(hla.contains(HitFlags::empty())); // everything contains empty
        assert!(!HitFlags::parse("H").contains(hla)); // smaller doesn't contain bigger
        // Empty contains only empty.
        assert!(HitFlags::empty().contains(HitFlags::empty()));
        assert!(!HitFlags::empty().contains(HitFlags::parse("H")));
    }

    /// Unrecognised flag characters are ignored (not an error / no panic),
    /// recognised ones around them still apply, and duplicates are idempotent.
    #[test]
    fn hit_flags_unknown_chars_ignored_and_idempotent() {
        let f = HitFlags::parse("H?L!A"); // '?' and '!' ignored
        assert!(f.high() && f.low() && f.air());
        assert!(!f.fall() && !f.down());
        // Duplicate letters don't double-set anything.
        assert_eq!(HitFlags::parse("HH"), HitFlags::parse("H"));
        assert_eq!(HitFlags::parse("MM"), HitFlags::parse("M"));
        // 'M' plus explicit 'H'/'L' is still just H|L.
        assert_eq!(HitFlags::parse("MHL"), HitFlags::parse("M"));
    }

    // ---- (2) detect_hit / detect_hit_contact geometry ----

    /// detect_hit and detect_hit_contact must always agree on hit/miss for the
    /// strict-overlap cases (the common path), across a battery of positions.
    #[test]
    fn detect_hit_and_contact_agree_on_strict_overlap() {
        let attack = [Clsn::new(10.0, -60.0, 55.0, -40.0)];
        let hurt = [Clsn::new(-18.0, -70.0, 18.0, 0.0)];
        let a_pos = Vec2::new(0.0, 0.0);
        for dx in [40.0_f32, 50.0, 60.0, 70.0, 73.0, 80.0, 200.0, -200.0] {
            let d_pos = Vec2::new(dx, 0.0);
            let hit = detect_hit(&attack, a_pos, Facing::Right, &hurt, d_pos, Facing::Left);
            let contact =
                detect_hit_contact(&attack, a_pos, Facing::Right, &hurt, d_pos, Facing::Left);
            assert_eq!(
                hit,
                contact.is_some(),
                "detect_hit and detect_hit_contact disagree at dx={dx}"
            );
            if let Some(c) = contact {
                assert!(c.w > 0.0 && c.h > 0.0, "contact must have positive area at dx={dx}");
            }
        }
    }

    /// The HitContact rectangle equals the true world-space intersection of the
    /// colliding pair, and `point` is its center.
    #[test]
    fn detect_hit_contact_reports_correct_intersection() {
        // Attacker box world x 10..55, y -60..-40 (facing right, axis 0).
        // Defender hurt local -18..18 -> facing left at axis 60 -> world 42..78,
        // y -70..0. Intersection: x 42..55, y -60..-40 -> (42, -60, 13, 20).
        let attack = [Clsn::new(10.0, -60.0, 55.0, -40.0)];
        let hurt = [Clsn::new(-18.0, -70.0, 18.0, 0.0)];
        let c = detect_hit_contact(
            &attack,
            Vec2::new(0.0, 0.0),
            Facing::Right,
            &hurt,
            Vec2::new(60.0, 0.0),
            Facing::Left,
        )
        .expect("should connect");
        assert!((c.x - 42.0).abs() < 1e-4, "x={}", c.x);
        assert!((c.y - (-60.0)).abs() < 1e-4, "y={}", c.y);
        assert!((c.w - 13.0).abs() < 1e-4, "w={}", c.w);
        assert!((c.h - 20.0).abs() < 1e-4, "h={}", c.h);
        // Center point is mid-rect.
        assert!((c.point.x - (42.0 + 13.0 / 2.0)).abs() < 1e-4);
        assert!((c.point.y - (-60.0 + 20.0 / 2.0)).abs() < 1e-4);
    }

    /// Edge-touch (zero shared area) is a MISS for both detectors, matching
    /// fp-physics strict-inequality semantics.
    #[test]
    fn detect_hit_edge_touch_is_miss() {
        // Attacker world x 0..10; defender placed so its hurt box starts exactly
        // at x=10 (shares only the x=10 edge).
        let attack = [Clsn::new(0.0, -10.0, 10.0, 0.0)];
        // Hurt local 0..10, facing right at axis 10 -> world 10..20.
        let hurt = [Clsn::new(0.0, -10.0, 10.0, 0.0)];
        let a_pos = Vec2::new(0.0, 0.0);
        let d_pos = Vec2::new(10.0, 0.0);
        assert!(!detect_hit(&attack, a_pos, Facing::Right, &hurt, d_pos, Facing::Right));
        assert!(
            detect_hit_contact(&attack, a_pos, Facing::Right, &hurt, d_pos, Facing::Right).is_none()
        );
    }

    /// Y-axis separation alone blocks a hit even when X overlaps fully (guards
    /// against an OR instead of AND in the overlap test).
    #[test]
    fn detect_hit_y_separation_blocks_hit() {
        // Both boxes share x 0..20, but attacker y -100..-80 (high) vs hurt y
        // -20..0 (low): disjoint in Y.
        let attack = [Clsn::new(0.0, -100.0, 20.0, -80.0)];
        let hurt = [Clsn::new(0.0, -20.0, 20.0, 0.0)];
        let pos = Vec2::new(0.0, 0.0);
        assert!(!detect_hit(&attack, pos, Facing::Right, &hurt, pos, Facing::Right));
        assert!(detect_hit_contact(&attack, pos, Facing::Right, &hurt, pos, Facing::Right).is_none());
    }

    /// Both characters facing right (e.g. a cross-up): facing applies to BOTH
    /// box sets independently. Mirrors fp-physics's both-facing-right case.
    #[test]
    fn detect_hit_both_facing_right() {
        // Attacker axis 0, box 10..30. Defender axis 15 facing right, hurt -5..25
        // -> world 10..40. Overlap in x 10..30.
        let attack = [Clsn::new(10.0, -20.0, 30.0, 0.0)];
        let hurt = [Clsn::new(-5.0, -20.0, 25.0, 0.0)];
        assert!(detect_hit(
            &attack,
            Vec2::new(0.0, 0.0),
            Facing::Right,
            &hurt,
            Vec2::new(15.0, 0.0),
            Facing::Right,
        ));
    }

    /// The defender's facing also mirrors its hurt boxes: an asymmetric hurt box
    /// that connects when the defender faces one way must miss when it faces the
    /// other (with everything else fixed).
    #[test]
    fn detect_hit_defender_facing_mirrors_hurt_box() {
        // Attacker world x 0..10 (facing right, axis 0).
        let attack = [Clsn::new(0.0, -10.0, 10.0, 0.0)];
        // Asymmetric hurt box that extends only to the LEFT of its axis: local
        // x -30..-5. Defender axis at x=12.
        let hurt = [Clsn::new(-30.0, -10.0, -5.0, 0.0)];
        let a_pos = Vec2::new(0.0, 0.0);
        let d_pos = Vec2::new(12.0, 0.0);
        // Facing right: hurt world x 12-30..12-5 = -18..7 -> overlaps attack 0..10.
        assert!(detect_hit(&attack, a_pos, Facing::Right, &hurt, d_pos, Facing::Right));
        // Facing left: hurt mirrors to world x 12+5..12+30 = 17..42 -> no overlap.
        assert!(!detect_hit(&attack, a_pos, Facing::Right, &hurt, d_pos, Facing::Left));
    }

    /// Multiple attacker and defender boxes: a hit registers if ANY pair
    /// overlaps, and detect_hit_contact returns the FIRST overlapping pair in
    /// attacker-outer / defender-inner iteration order.
    #[test]
    fn detect_hit_multiple_boxes_any_pair_and_first_contact() {
        // Two attacker boxes: [0] far away (whiffs), [1] connects.
        let attack = [
            Clsn::new(500.0, -10.0, 510.0, 0.0), // far, never hits
            Clsn::new(0.0, -10.0, 20.0, 0.0),    // connects
        ];
        // Two defender hurt boxes both reachable by attack[1]; defender facing
        // right at axis 5: box A world 5..15, box B world 8..28.
        let hurt = [
            Clsn::new(0.0, -10.0, 10.0, 0.0), // -> world 5..15
            Clsn::new(3.0, -10.0, 23.0, 0.0), // -> world 8..28
        ];
        let a_pos = Vec2::new(0.0, 0.0);
        let d_pos = Vec2::new(5.0, 0.0);
        assert!(detect_hit(&attack, a_pos, Facing::Right, &hurt, d_pos, Facing::Right));
        // First contact: attacker[0] whiffs both, attacker[1] vs defender[0] is
        // the first overlapping pair -> intersection x 5..15 (overlap of 0..20 &
        // 5..15) = (5, w=10).
        let c = detect_hit_contact(&attack, a_pos, Facing::Right, &hurt, d_pos, Facing::Right)
            .expect("a pair overlaps");
        assert!((c.x - 5.0).abs() < 1e-4, "first contact x={}", c.x);
        assert!((c.w - 10.0).abs() < 1e-4, "first contact w={}", c.w);
    }

    /// Reversed corner ordering on input boxes is normalized — a box authored as
    /// (x2,y2,x1,y1) detects identically to (x1,y1,x2,y2).
    #[test]
    fn detect_hit_reversed_corner_order_equivalent() {
        let normal = [Clsn::new(10.0, -60.0, 55.0, -40.0)];
        let reversed = [Clsn::new(55.0, -40.0, 10.0, -60.0)];
        let hurt = [Clsn::new(-18.0, -70.0, 18.0, 0.0)];
        let a_pos = Vec2::new(0.0, 0.0);
        let d_pos = Vec2::new(60.0, 0.0);
        assert_eq!(
            detect_hit(&normal, a_pos, Facing::Right, &hurt, d_pos, Facing::Left),
            detect_hit(&reversed, a_pos, Facing::Right, &hurt, d_pos, Facing::Left),
        );
        assert!(detect_hit(&reversed, a_pos, Facing::Right, &hurt, d_pos, Facing::Left));
    }

    /// Self-overlapping at the same axis: a character "hitting itself" geometry
    /// (same pos, both facing right) registers when boxes share area. Confirms
    /// no special-casing of identical positions.
    #[test]
    fn detect_hit_same_position_overlapping_boxes() {
        let attack = [Clsn::new(-10.0, -10.0, 10.0, 10.0)];
        let hurt = [Clsn::new(-5.0, -5.0, 5.0, 5.0)];
        let pos = Vec2::new(100.0, 50.0);
        assert!(detect_hit(&attack, pos, Facing::Right, &hurt, pos, Facing::Right));
    }

    // ---- (4) never-panics / purity ----

    /// Acceptance criterion 4: never panics. Feed NaN / infinities through both
    /// detection entry points; output is unspecified but must not panic.
    #[test]
    fn detect_hit_never_panics_on_non_finite() {
        let nan = f32::NAN;
        let inf = f32::INFINITY;
        let weird = [Clsn::new(nan, -inf, inf, nan)];
        let normal = [Clsn::new(0.0, 0.0, 1.0, 1.0)];
        let _ = detect_hit(
            &weird,
            Vec2::new(inf, nan),
            Facing::Left,
            &normal,
            Vec2::new(0.0, 0.0),
            Facing::Right,
        );
        let _ = detect_hit_contact(
            &weird,
            Vec2::new(inf, nan),
            Facing::Left,
            &normal,
            Vec2::new(0.0, 0.0),
            Facing::Right,
        );
        // Also feed non-finite through parse helpers (strings, so trivially safe)
        // and flag parsing of odd unicode.
        let _ = AttackAttr::parse("💥, NA");
        let _ = HitFlags::parse("h\u{0}l");
    }

    /// Acceptance criterion: pure & deterministic — identical inputs always give
    /// identical results across many calls, for both detectors.
    #[test]
    fn detect_hit_is_deterministic() {
        let attack = [Clsn::new(10.0, -60.0, 55.0, -40.0)];
        let hurt = [Clsn::new(-18.0, -70.0, 18.0, 0.0)];
        let a_pos = Vec2::new(0.0, 0.0);
        let d_pos = Vec2::new(60.0, 0.0);
        let first = detect_hit(&attack, a_pos, Facing::Right, &hurt, d_pos, Facing::Left);
        let first_c =
            detect_hit_contact(&attack, a_pos, Facing::Right, &hurt, d_pos, Facing::Left);
        for _ in 0..32 {
            assert_eq!(
                detect_hit(&attack, a_pos, Facing::Right, &hurt, d_pos, Facing::Left),
                first
            );
            assert_eq!(
                detect_hit_contact(&attack, a_pos, Facing::Right, &hurt, d_pos, Facing::Left),
                first_c
            );
        }
        assert!(first);
        assert!(first_c.is_some());
    }

    // ---- Real-fixture test (gated: skips cleanly when test-assets/ absent) ----

    /// Minimal test-only parser for `ClsnN[i] = x1,y1,x2,y2` lines in a `.air`
    /// file. Returns all boxes matching the given prefix. Lives in tests only;
    /// impl code is untouched.
    fn parse_clsn_lines(text: &str, prefix: &str) -> Vec<Clsn> {
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if !line.starts_with(prefix) {
                continue;
            }
            let Some((_, rhs)) = line.split_once('=') else {
                continue;
            };
            let nums: Vec<f32> = rhs
                .split(',')
                .filter_map(|t| t.trim().parse::<f32>().ok())
                .collect();
            if let [x1, y1, x2, y2] = nums[..] {
                out.push(Clsn::new(x1, y1, x2, y2));
            }
        }
        out
    }

    /// Drives `detect_hit` / `detect_hit_contact` with real Clsn boxes parsed
    /// from the Kung Fu Man `.air` fixture. Skips cleanly when test-assets/ is
    /// absent so the suite stays green in minimal checkouts.
    #[test]
    fn fixture_kfm_air_detect_hit() {
        let candidates = [
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-assets/kfm/kfm.air"),
            concat!(env!("CARGO_MANIFEST_DIR"), "/test-assets/kfm/kfm.air"),
        ];
        let path = candidates.iter().find(|p| std::path::Path::new(p).exists());
        let Some(path) = path else {
            eprintln!("skipping fixture_kfm_air_detect_hit: test-assets/ not present");
            return;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            eprintln!("skipping fixture_kfm_air_detect_hit: could not read {path}");
            return;
        };

        let clsn1 = parse_clsn_lines(&text, "Clsn1[");
        let clsn2 = parse_clsn_lines(&text, "Clsn2[");
        assert!(!clsn1.is_empty(), "expected Clsn1 attack boxes in kfm.air");
        assert!(!clsn2.is_empty(), "expected Clsn2 hurt boxes in kfm.air");

        // Every real box must survive the full detection pipeline without panic
        // and place to a finite, non-negative-size world rect.
        let a_pos = Vec2::new(80.0, 0.0);
        let d_pos = Vec2::new(120.0, 0.0);
        for &atk in &clsn1 {
            for &hrt in &clsn2 {
                // Just exercising it must never panic.
                let _ = detect_hit(&[atk], a_pos, Facing::Right, &[hrt], d_pos, Facing::Left);
                let _ =
                    detect_hit_contact(&[atk], a_pos, Facing::Right, &[hrt], d_pos, Facing::Left);
            }
        }

        // Concrete real-data hit: KFM's first jab `Clsn1[0] = 16,-80, 61,-71`
        // against the standing hurt box `Clsn2[0] = -13,0,16,-79`.
        let jab = Clsn::new(16.0, -80.0, 61.0, -71.0);
        let stand_hurt = Clsn::new(-13.0, 0.0, 16.0, -79.0);
        assert!(clsn1.contains(&jab), "expected KFM jab Clsn1 box in fixture");
        assert!(
            clsn2.contains(&stand_hurt),
            "expected KFM standing hurt box in fixture"
        );

        // Attacker axis 80 facing right -> jab world x 96..141, y -80..-71.
        // Defender axis 120 facing left -> hurt world x 104..133, y -79..0.
        // Overlap x 104..133, y -79..-71 -> hit, with positive contact area.
        assert!(detect_hit(
            &[jab],
            a_pos,
            Facing::Right,
            &[stand_hurt],
            d_pos,
            Facing::Left,
        ));
        let c = detect_hit_contact(
            &[jab],
            a_pos,
            Facing::Right,
            &[stand_hurt],
            d_pos,
            Facing::Left,
        )
        .expect("KFM jab should connect");
        assert!(c.w > 0.0 && c.h > 0.0);

        // Push the defender far away -> clean miss.
        assert!(!detect_hit(
            &[jab],
            a_pos,
            Facing::Right,
            &[stand_hurt],
            Vec2::new(400.0, 0.0),
            Facing::Left,
        ));
    }

    /// The fixture also lets us validate AttackAttr against attribute strings
    /// authored in real CNS-style data shapes (the controller in 6.2 feeds these
    /// to `parse`). Synthetic but matching MUGEN's exact attr syntax.
    #[test]
    fn real_world_attr_strings_parse() {
        // Forms taken verbatim from typical KFM-era HitDef attr declarations.
        assert_eq!(AttackAttr::parse("S, NA").power, AttackPower::Normal);
        assert_eq!(AttackAttr::parse("C, NA").class, StateClass::Crouching);
        assert_eq!(AttackAttr::parse("A, NA").class, StateClass::Air);
        assert_eq!(AttackAttr::parse("S, SP").kind, AttackKind::Projectile);
        assert_eq!(AttackAttr::parse("S, HA").power, AttackPower::Hyper);
        assert_eq!(AttackAttr::parse("S, ST").kind, AttackKind::Throw);
    }
}
