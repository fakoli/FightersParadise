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
use serde::{Deserialize, Serialize};

// Re-export the geometry types callers need so they can build the slices for
// [`detect_hit`] without depending on `fp-physics` directly.
pub use fp_physics::{Clsn as ClsnBox, Facing as ClsnFacing};

/// MUGEN state-class of an attack: which stance the *attacker* is in.
///
/// This is the first token of a HitDef `attr` string (e.g. the `S` in `S, NA`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// The MUGEN `animtype` / `air.animtype` of a hit — which **get-hit reaction
/// animation** the defender plays.
///
/// This is distinct from [`HitType`] (`ground.type`, which gates *whether* a hit
/// lands at a given height): `animtype` only selects the *reaction pose*. MUGEN's
/// common1 get-hit states (`5000`-`5xxx`) branch on `GetHitVar(animtype)` ~20
/// times to pick the right hurt animation, so authoring `animtype = Hard` on a
/// HitDef is what makes a heavy attack produce a heavy reaction.
///
/// # Integer encoding (`GetHitVar(animtype)`)
///
/// [`AnimType::code`] returns the canonical MUGEN integer for each variant. The
/// encoding is the standard documented in the MUGEN trigger reference (Elecbyte)
/// and used by the common1 `5xxx` states:
///
/// | variant            | `code()` |
/// |--------------------|----------|
/// | [`Light`](Self::Light)       | `0` |
/// | [`Medium`](Self::Medium)     | `1` |
/// | [`Hard`](Self::Hard)         | `2` |
/// | [`Back`](Self::Back)         | `3` |
/// | [`Up`](Self::Up)             | `4` |
/// | [`DiagUp`](Self::DiagUp)     | `5` |
/// | [`DiagDown`](Self::DiagDown) | `6` |
///
/// `Light`/`Medium`/`Hard` are the three ordinary ground reactions; `Back` is the
/// "flung backwards" reaction; `Up`/`DiagUp`/`DiagDown` are the launched
/// reactions. Defaults to [`AnimType::Light`] (MUGEN's default, code `0`) — which
/// is exactly the bug this fixes: an unset `animtype` previously made *every* hit
/// read back as `Light`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AnimType {
    /// `Light` — a light hit reaction (MUGEN default; code `0`).
    Light,
    /// `Medium` (authored as `Med` or `Medium`) — a medium hit reaction (code `1`).
    Medium,
    /// `Hard` — a heavy hit reaction (code `2`).
    Hard,
    /// `Back` — a flung-backwards reaction (code `3`).
    Back,
    /// `Up` — a launched-straight-up reaction (code `4`).
    Up,
    /// `DiagUp` — a launched diagonally-up reaction (code `5`).
    DiagUp,
    /// `DiagDown` — a launched diagonally-down reaction (code `6`).
    DiagDown,
}

impl Default for AnimType {
    /// Defaults to [`AnimType::Light`] — MUGEN's default `animtype` and the safe
    /// fallback for unrecognized / absent values.
    fn default() -> Self {
        AnimType::Light
    }
}

impl AnimType {
    /// Parses a MUGEN `animtype` / `air.animtype` token into an [`AnimType`].
    ///
    /// Matching is **case-insensitive** and whitespace-tolerant. Both the short
    /// `Med` and the long `Medium` spellings map to [`AnimType::Medium`] (real
    /// content uses both). The launched forms `DiagUp` / `DiagDown` are accepted.
    ///
    /// An **empty** token, or any **unrecognized** token, falls back to
    /// [`AnimType::Light`] (MUGEN's default). A *non-empty* unrecognized token also
    /// logs a [`tracing::warn!`]; an empty token is silent (it just means "use the
    /// default"). Never panics.
    ///
    /// # Examples
    ///
    /// ```
    /// use fp_combat::AnimType;
    ///
    /// assert_eq!(AnimType::parse("Light"), AnimType::Light);
    /// assert_eq!(AnimType::parse("Hard"), AnimType::Hard);
    /// // Both Med spellings map to Medium.
    /// assert_eq!(AnimType::parse("Med"), AnimType::Medium);
    /// assert_eq!(AnimType::parse("medium"), AnimType::Medium);
    /// // Launched forms.
    /// assert_eq!(AnimType::parse("DiagUp"), AnimType::DiagUp);
    /// assert_eq!(AnimType::parse("diagdown"), AnimType::DiagDown);
    /// // Case / whitespace tolerant.
    /// assert_eq!(AnimType::parse("  bAcK "), AnimType::Back);
    /// // Unknown / empty -> Light (the default).
    /// assert_eq!(AnimType::parse("nonsense"), AnimType::Light);
    /// assert_eq!(AnimType::parse(""), AnimType::Light);
    /// ```
    #[must_use]
    pub fn parse(s: &str) -> Self {
        let t = s.trim();
        if t.is_empty() {
            // Absent value: silently use the default (this is the common case
            // when `air.animtype` is omitted and the caller substitutes "").
            return AnimType::Light;
        }
        if t.eq_ignore_ascii_case("light") {
            AnimType::Light
        } else if t.eq_ignore_ascii_case("med") || t.eq_ignore_ascii_case("medium") {
            AnimType::Medium
        } else if t.eq_ignore_ascii_case("hard") {
            AnimType::Hard
        } else if t.eq_ignore_ascii_case("back") {
            AnimType::Back
        } else if t.eq_ignore_ascii_case("up") {
            AnimType::Up
        } else if t.eq_ignore_ascii_case("diagup") {
            AnimType::DiagUp
        } else if t.eq_ignore_ascii_case("diagdown") {
            AnimType::DiagDown
        } else {
            tracing::warn!(input = %s, "unrecognized HitDef animtype; defaulting to Light");
            AnimType::Light
        }
    }

    /// Returns MUGEN's `GetHitVar(animtype)` integer encoding for this variant.
    ///
    /// See the [type-level table](AnimType#integer-encoding-gethitvaranimtype):
    /// `Light=0`, `Medium=1`, `Hard=2`, `Back=3`, `Up=4`, `DiagUp=5`,
    /// `DiagDown=6`. This is the value the defender's common1 get-hit states read
    /// to branch to the correct reaction animation.
    ///
    /// # Examples
    ///
    /// ```
    /// use fp_combat::AnimType;
    ///
    /// assert_eq!(AnimType::Light.code(), 0);
    /// assert_eq!(AnimType::Hard.code(), 2);
    /// assert_eq!(AnimType::DiagDown.code(), 6);
    /// ```
    #[must_use]
    pub fn code(self) -> i32 {
        match self {
            AnimType::Light => 0,
            AnimType::Medium => 1,
            AnimType::Hard => 2,
            AnimType::Back => 3,
            AnimType::Up => 4,
            AnimType::DiagUp => 5,
            AnimType::DiagDown => 6,
        }
    }
}

/// The MUGEN `priority` type, used to resolve simultaneous (trading) hits.
///
/// Resolution itself is task 6.3; this enum is just the data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// The decision of a priority *clash* between two simultaneously-connecting
/// [`HitDef`]s, produced by [`resolve_clash`].
///
/// In MUGEN, when both fighters have an active `HitDef` whose attack boxes
/// connect on the **same tick**, the engine does not simply let both land:
/// it compares the two [`Priority`] values and resolves a trade. This enum is
/// the verdict — which side(s) actually apply their hit this tick — from the
/// point of view of the **first** [`HitDef`] argument to [`resolve_clash`].
///
/// The result is always symmetric: `resolve_clash(a, b)` and `resolve_clash(b, a)`
/// agree about each side (the [`FirstWins`](Self::FirstWins) /
/// [`SecondWins`](Self::SecondWins) pair simply swaps).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClashOutcome {
    /// Both hits land — a **trade**. Each attacker connects with the other.
    /// Produced when the higher-priority side is itself a [`PriorityType::Hit`]
    /// against another [`PriorityType::Hit`] at the *same* value, the
    /// MUGEN "Hit vs Hit = trade" rule.
    Trade,
    /// Only the **first** `HitDef` lands; the second is cancelled this tick.
    /// Produced when the first side has the strictly higher priority value, or
    /// (at equal value) the type table favours the first side only.
    FirstWins,
    /// Only the **second** `HitDef` lands; the first is cancelled this tick.
    /// The mirror of [`FirstWins`](Self::FirstWins).
    SecondWins,
    /// **Neither** hit lands this tick — both attacks whiff. Produced by the
    /// equal-value type combinations that involve a [`PriorityType::Dodge`] or
    /// two [`PriorityType::Miss`]es, where MUGEN suppresses both attacks.
    NeitherHits,
}

/// Resolves a MUGEN priority *clash* between two simultaneously-connecting
/// [`HitDef`]s, given each side's [`Priority`].
///
/// This is the pure decision the round coordinator consults when **both**
/// fighters have an active `HitDef` whose boxes connect on the same tick (see
/// [`detect_hit`]). It compares the two priorities and returns a
/// [`ClashOutcome`] saying which side(s) apply their hit:
///
/// # Rules
///
/// 1. **Strictly higher [`Priority::value`] wins.** The side with the larger
///    numeric value lands its hit; the loser's hit is cancelled this tick
///    ([`FirstWins`](ClashOutcome::FirstWins) /
///    [`SecondWins`](ClashOutcome::SecondWins)). The [`PriorityType`] is
///    ignored when the values differ — value dominates.
/// 2. **Equal values consult the [`PriorityType`] table.** With both
///    [`Priority::value`]s equal, the combination of the two
///    [`PriorityType`]s decides (symmetric in the two sides):
///
///    | first \ second | `Hit`         | `Miss`        | `Dodge`       |
///    |----------------|---------------|---------------|---------------|
///    | **`Hit`**      | `Trade`       | `FirstWins`   | `NeitherHits` |
///    | **`Miss`**     | `SecondWins`  | `NeitherHits` | `NeitherHits` |
///    | **`Dodge`**    | `NeitherHits` | `NeitherHits` | `NeitherHits` |
///
///    The intuition follows MUGEN: two `Hit`s **trade** (both connect); a
///    `Hit` against a `Miss` lands while the `Miss` side is ignored; a
///    `Dodge` makes **both** attacks whiff (a clean dodge avoids the trade);
///    and two `Miss`es cancel each other out.
///
/// Pure, deterministic, total, and **never panics**.
///
/// # Examples
///
/// ```
/// use fp_combat::{resolve_clash, ClashOutcome, Priority, PriorityType};
///
/// // Strictly higher value wins regardless of type.
/// let hi = Priority { value: 6, kind: PriorityType::Hit };
/// let lo = Priority { value: 3, kind: PriorityType::Hit };
/// assert_eq!(resolve_clash(hi, lo), ClashOutcome::FirstWins);
/// assert_eq!(resolve_clash(lo, hi), ClashOutcome::SecondWins);
///
/// // Equal value, both Hit -> a trade (both land).
/// let a = Priority { value: 4, kind: PriorityType::Hit };
/// let b = Priority { value: 4, kind: PriorityType::Hit };
/// assert_eq!(resolve_clash(a, b), ClashOutcome::Trade);
///
/// // Equal value, Hit vs Dodge -> neither lands.
/// let dodge = Priority { value: 4, kind: PriorityType::Dodge };
/// assert_eq!(resolve_clash(a, dodge), ClashOutcome::NeitherHits);
/// ```
#[must_use]
pub fn resolve_clash(a: Priority, b: Priority) -> ClashOutcome {
    use std::cmp::Ordering;
    use ClashOutcome::*;
    use PriorityType::*;

    // (1) Value dominates: a strictly higher numeric priority wins outright,
    //     ignoring the types.
    match a.value.cmp(&b.value) {
        Ordering::Greater => return FirstWins,
        Ordering::Less => return SecondWins,
        Ordering::Equal => {}
    }

    // (2) Equal value: consult the symmetric PriorityType table.
    match (a.kind, b.kind) {
        (Hit, Hit) => Trade,
        (Hit, Miss) => FirstWins,
        (Miss, Hit) => SecondWins,
        // Any combination involving a Dodge, or two Misses, suppresses both.
        (Hit, Dodge)
        | (Dodge, Hit)
        | (Miss, Miss)
        | (Miss, Dodge)
        | (Dodge, Miss)
        | (Dodge, Dodge) => NeitherHits,
    }
}

/// A `(hit, guard)` pair of damage values: damage dealt on a clean hit vs. on block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Damage {
    /// Damage dealt when the attack lands cleanly.
    pub hit: i32,
    /// Damage dealt when the attack is guarded (blocked).
    pub guard: i32,
}

/// A `(p1, p2)` pause-time pair, in ticks.
///
/// `p1` is how long the attacker pauses; `p2` is the defender's shake time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct PauseTime {
    /// Ticks the attacker (P1) is paused.
    pub p1: i32,
    /// Ticks the defender (P2) shakes.
    pub p2: i32,
}

/// A `(hit, guard)` pair of power-meter amounts: power gained on a clean hit vs.
/// when the attack is guarded.
///
/// Used by both [`HitDef::getpower`] (power given to the **attacker**, P1) and
/// [`HitDef::givepower`] (power given to the **defender**, P2). MUGEN's documented
/// defaults are damage-proportional: when the author omits the value it derives
/// from the HitDef's [`Damage`] via a *life-to-power* multiplier (see
/// [`HitDef::default_getpower`] / [`HitDef::default_givepower`]), and the guard
/// amount defaults to half the hit amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct PowerGain {
    /// Power added when the attack lands cleanly (`getpower`/`givepower` first
    /// component, MUGEN `p1power`/`p2power`).
    pub hit: i32,
    /// Power added when the attack is guarded (`getpower`/`givepower` second
    /// component, MUGEN `p1gpower`/`p2gpower`). Defaults to [`hit`](Self::hit) / 2.
    pub guard: i32,
}

/// Per-situation hit-time values (ticks the defender stays in hit-stun), in ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// A MUGEN sound reference: a `(group, sample)` pair into a `.snd` file, plus a
/// flag selecting which file.
///
/// MUGEN's `hitsound` / `guardsound` parameters are a `group, sample` pair (e.g.
/// `hitsound = 5, 0` is sample `0` of group `5`), not a single id — the older
/// single-`i32` model dropped the sample. For these HitDef parameters the sound
/// comes from the **common / fight** sound file (`fight.snd`) **by default**; a
/// leading `S` (case-insensitive) on the group token selects the **character's
/// own** `.snd` instead. (This is the inverse of the `PlaySnd` controller, where
/// the character's own file is the default and `F` selects the common file — see
/// [03-engine-architecture.md §state-controllers]). That distinction is captured
/// in [`common`](SoundId::common). This type is pure data and carries no playback
/// logic — a downstream player (`fp-audio`) resolves and plays it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SoundId {
    /// The sound *group* number (with any leading `S` flag already stripped and
    /// reflected in [`common`](SoundId::common)).
    pub group: i32,
    /// The sound *sample* number within [`group`](SoundId::group). Defaults to `0`
    /// when the authored value omits it (`hitsound = 5` ≡ `5, 0`).
    pub sample: i32,
    /// `true` (the default for `hitsound`/`guardsound`) when the sound comes from
    /// the **common / fight** sound file (`fight.snd`); `false` when the group
    /// token was `S`-prefixed, meaning the **character's own** `.snd`.
    pub common: bool,
}

impl SoundId {
    /// Parses a MUGEN `hitsound` / `guardsound` value into an optional [`SoundId`].
    ///
    /// The value is a comma list `group [, sample]`:
    /// - The **group** token selects the **common / fight** sound file by default
    ///   ([`common`](SoundId::common) `= true`); a leading `S`/`s` flag instead
    ///   selects the **character's own** `.snd` (`common = false`) and is stripped
    ///   before the integer is parsed. (Inverse of `PlaySnd`, where the default is
    ///   the character's own file and `F` selects the common one.)
    /// - The **sample** defaults to `0` when absent (`"5"` ≡ `"5, 0"`) or when the
    ///   sample token cannot be parsed as an integer.
    ///
    /// Returns [`None`] (the MUGEN "no sound" sentinel) when the group is the
    /// literal `-1`, or when the group token is empty / cannot be parsed as an
    /// integer (garbage). Never panics.
    ///
    /// # Examples
    ///
    /// ```
    /// use fp_combat::SoundId;
    ///
    /// // No prefix → the common / fight sound file (the hitsound default).
    /// assert_eq!(
    ///     SoundId::parse("5, 0"),
    ///     Some(SoundId { group: 5, sample: 0, common: true })
    /// );
    /// // Leading `S` → the character's own .snd.
    /// assert_eq!(
    ///     SoundId::parse("S5, 2"),
    ///     Some(SoundId { group: 5, sample: 2, common: false })
    /// );
    /// // Sample defaults to 0 when omitted.
    /// assert_eq!(
    ///     SoundId::parse("7"),
    ///     Some(SoundId { group: 7, sample: 0, common: true })
    /// );
    /// // The `-1` sentinel and garbage both mean "no sound".
    /// assert_eq!(SoundId::parse("-1"), None);
    /// assert_eq!(SoundId::parse("nope"), None);
    /// ```
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.split(',');
        let group_tok = parts.next().unwrap_or("").trim();

        // hitsound/guardsound default to the common/fight sound file; a leading
        // `S`/`s` flag selects the character's own `.snd` instead.
        let (common, group_digits) = match group_tok.strip_prefix(['S', 's']) {
            Some(rest) => (false, rest.trim()),
            None => (true, group_tok),
        };

        let group = group_digits.parse::<i32>().ok()?;
        // `-1` is MUGEN's explicit "no sound" sentinel.
        if group == -1 {
            return None;
        }

        // Sample defaults to 0 when absent or unparseable.
        let sample = parts
            .next()
            .map(str::trim)
            .and_then(|t| t.parse::<i32>().ok())
            .unwrap_or(0);

        Some(Self {
            group,
            sample,
            common,
        })
    }
}

/// Resource fields attached to a hit: spark id, hit sound, guard sound.
///
/// `sparkno` is a raw numeric id (`-1` = none). The two sounds are
/// [`SoundId`]s wrapped in [`Option`]: [`None`] is the MUGEN "no sound" sentinel
/// (an authored `-1`, or an absent / unparseable value). The `S`-prefix on
/// `sparkno` (use the character's own AIR rather than the common set) is not
/// modelled here — it is resolved by the controller in task 6.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HitResources {
    /// `sparkno` — spark animation action id (`-1` = none).
    pub sparkno: i32,
    /// `hitsound` — sound played on a clean hit (`None` = no sound).
    pub hitsound: Option<SoundId>,
    /// `guardsound` — sound played when the hit is guarded (`None` = no sound).
    pub guardsound: Option<SoundId>,
}

impl Default for HitResources {
    /// `sparkno` defaults to `-1` (unset); both sounds default to [`None`] (no
    /// sound), matching MUGEN's "no resource" convention.
    fn default() -> Self {
        Self {
            sparkno: -1,
            hitsound: None,
            guardsound: None,
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
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
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
    /// `animtype` — the get-hit **reaction animation** for a grounded defender
    /// (MUGEN default `Light`). Distinct from [`ground_type`](Self::ground_type):
    /// this is what `GetHitVar(animtype)` reports and the common1 `5xxx` get-hit
    /// states branch on to pick the hurt pose.
    pub animtype: AnimType,
    /// `air.animtype` — the get-hit reaction animation for an **airborne**
    /// defender. At HitDef parse time MUGEN defaults this to whatever
    /// [`animtype`](Self::animtype) was set to when the `air.animtype` key is
    /// absent; the struct-level [`Default`] is [`AnimType::Light`].
    pub air_animtype: AnimType,
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
    /// `fall.xvelocity` — initial X velocity when entering the falling state
    /// (MUGEN `fall.xvelocity`; surfaces via `GetHitVar(fall.xvel)`).
    ///
    /// MUGEN's documented default is "no change" (the defender keeps its current X
    /// velocity); we model that absence as `None`. Authored values (e.g. evilken's
    /// `fall.xvelocity`) are carried through to `GetHitVar(fall.xvel)`.
    pub fall_xvelocity: Option<f32>,
    /// `fall.yvelocity` — initial upward Y velocity when entering the falling state.
    pub fall_yvelocity: f32,
    /// `fall.damage` — extra life damage dealt to the defender **when it lands**
    /// from the fall (MUGEN `fall.damage`; surfaces via `GetHitVar(fall.damage)`,
    /// applied by the `HitFallDamage` controller in the authored get-hit state).
    ///
    /// Default `0` (no landing damage). KFM authors `fall.damage = 70` on its
    /// sweep so common1's `HitFallDamage` drops 70 life on landing.
    pub fall_damage: i32,
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
    /// `getpower` — power-meter `(hit, guard)` granted to the **attacker** (P1)
    /// when this HitDef connects (MUGEN `p1power`/`p1gpower`).
    ///
    /// When the author omits `getpower`, MUGEN derives it from the hit damage via
    /// the `Default.Attack.LifeToPowerMul` config (default `0.7`), with the guard
    /// amount half the hit amount. The HitDef *controller* fills this in via
    /// [`HitDef::default_getpower`] when the param is absent; the struct-level
    /// [`Default`] is `(0, 0)` (no gain), since the controller — not the bare
    /// struct — knows the damage at parse time. KFM authors `getpower = 0` on
    /// every attack (13×) to **suppress** this default gain.
    pub getpower: PowerGain,
    /// `givepower` — power-meter `(hit, guard)` granted to the **defender** (P2)
    /// when this HitDef connects (MUGEN `p2power`/`p2gpower`).
    ///
    /// When omitted, MUGEN derives it from the hit damage via the
    /// `Default.GetHit.LifeToPowerMul` config (default `0.6`), guard amount half
    /// the hit amount — see [`HitDef::default_givepower`]. Struct-level [`Default`]
    /// is `(0, 0)`.
    pub givepower: PowerGain,
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
            animtype: AnimType::default(),
            air_animtype: AnimType::default(),
            ground_velocity: Vec2::default(),
            air_velocity: Vec2::default(),
            guard_velocity: 0.0,
            hittimes: HitTimes::default(),
            fall: false,
            fall_xvelocity: None,
            fall_yvelocity: 0.0,
            fall_damage: 0,
            p1stateno: None,
            p2stateno: None,
            priority: Priority::default(),
            id: 0,
            chainid: -1,
            getpower: PowerGain::default(),
            givepower: PowerGain::default(),
        }
    }
}

/// MUGEN's default `Default.Attack.LifeToPowerMul` — the multiplier applied to a
/// HitDef's hit damage to derive the **attacker's** default `getpower` when the
/// param is omitted (`data/mugen.cfg`, default `0.7`).
pub const DEFAULT_ATTACK_LIFE_TO_POWER_MUL: f32 = 0.7;

/// MUGEN's default `Default.GetHit.LifeToPowerMul` — the multiplier applied to a
/// HitDef's hit damage to derive the **defender's** default `givepower` when the
/// param is omitted (`data/mugen.cfg`, default `0.6`).
pub const DEFAULT_GETHIT_LIFE_TO_POWER_MUL: f32 = 0.6;

impl HitDef {
    /// MUGEN's documented default `getpower` (attacker power gain), derived from
    /// this HitDef's [`Damage::hit`].
    ///
    /// `hit = round(damage.hit * `[`DEFAULT_ATTACK_LIFE_TO_POWER_MUL`]`)` and
    /// `guard = hit / 2` (integer division, matching MUGEN's "`p1gpower` defaults
    /// to `p1power / 2`"). Negative damage clamps the gain to `0`. The HitDef
    /// *controller* calls this only when the `getpower` param is **absent**; an
    /// authored `getpower = 0` keeps the explicit `(0, 0)` (KFM's suppression).
    #[must_use]
    pub fn default_getpower(&self) -> PowerGain {
        let hit = (self.damage.hit as f32 * DEFAULT_ATTACK_LIFE_TO_POWER_MUL)
            .round()
            .max(0.0) as i32;
        PowerGain {
            hit,
            guard: hit / 2,
        }
    }

    /// MUGEN's documented default `givepower` (defender power gain), derived from
    /// this HitDef's [`Damage::hit`].
    ///
    /// `hit = round(damage.hit * `[`DEFAULT_GETHIT_LIFE_TO_POWER_MUL`]`)` and
    /// `guard = hit / 2`. See [`HitDef::default_getpower`] for the symmetric notes.
    #[must_use]
    pub fn default_givepower(&self) -> PowerGain {
        let hit = (self.damage.hit as f32 * DEFAULT_GETHIT_LIFE_TO_POWER_MUL)
            .round()
            .max(0.0) as i32;
        PowerGain {
            hit,
            guard: hit / 2,
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

/// Which sprite/animation set a HitDef's `sparkno` resolves against, plus the
/// (non-negative) animation id to play from that set — the MUGEN spark-source rule
/// **as it actually arrives in this codebase**.
///
/// In *authored* MUGEN, `sparkno` selects the hit-spark animation when an attack
/// connects, and an `S` prefix (`S2`) means "use my **own** set" while a bare
/// number (`2`) means "use the **common** `fightfx` set". The magnitude is the
/// action id either way, and `-1` is the documented "no spark" sentinel.
///
/// **Important — what reaches this type is *not* the authored string.** The
/// `HitDef` controller's `parse_resource_id` (in `fp-character`) **strips** the
/// leading `S`/`s` and keeps the bare *positive* magnitude — it does **not** fold
/// an `S` prefix into a negative integer. So given the current parser:
///
/// - `sparkno = -1` arrives as `-1` → [`SparkSource::None`] (no spark).
/// - Any other **negative** value (a *literal* `sparkno = -N` in the CNS) arrives
///   negative → [`SparkSource::Own`] at id `N` (attacker's own SFF/AIR).
/// - Every **non-negative** value — **including the `S`-prefixed own-spark form**
///   `S2`, which the parser flattens to `2` — arrives non-negative →
///   [`SparkSource::Common`] at that id.
///
/// Consequence (tracked in `docs/known-issues.md`, audit #17): because the `S`
/// prefix is lost upstream, the [`SparkSource::Own`] path is reachable **only** by
/// a literal-negative `sparkno`, which authored content rarely uses. Conventional
/// characters (Kung Fu Man included — its `sparkno` values are all `0/1/2/3/40`
/// plus one `-1`) therefore classify as [`SparkSource::Common`], and with no
/// `fightfx.sff` loaded those spawn no visible spark today. Restoring the
/// `S`-prefix → own-spark distinction is an `fp-character` parser change (out of
/// this type's scope); this enum classifies faithfully *given the value it is
/// handed*.
///
/// Use [`SparkSource::classify`] to turn a raw `sparkno` into one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SparkSource {
    /// No spark at all (the `-1` sentinel, or any value MUGEN treats as "none").
    None,
    /// Play action `anim` from the **attacker's own** SFF/AIR. Reached by a
    /// *literal* negative `sparkno` (e.g. `-2` → own action `2`); the `S`-prefixed
    /// authoring form does **not** reach here today (the parser strips the `S` and
    /// keeps a positive id — see the type-level note). `anim` is the non-negative
    /// action id.
    Own {
        /// The attacker-own animation (action) id to play.
        anim: i32,
    },
    /// Play action `anim` from the **common** `fightfx` set (any non-negative
    /// `sparkno`, including the `S`-flattened own-spark form). `anim` is that
    /// action id.
    Common {
        /// The common-`fightfx` animation (action) id to play.
        anim: i32,
    },
}

impl SparkSource {
    /// Classifies a raw [`HitResources::sparkno`] into a [`SparkSource`].
    ///
    /// `-1` (and only `-1`) is the MUGEN "no spark" sentinel → [`SparkSource::None`].
    /// Any other **negative** value (a *literal* `sparkno = -N`) is an attacker-own
    /// spark whose action id is the magnitude (`-2` → own action `2`). A
    /// **non-negative** value is a common `fightfx` spark at that action id — note
    /// the `S`-prefixed own-spark form arrives here non-negative (the parser strips
    /// the `S`), so it classifies as `Common`, not `Own` (see the type-level note).
    /// Total and never panics.
    ///
    /// # Examples
    ///
    /// ```
    /// use fp_combat::SparkSource;
    ///
    /// // The default / sentinel: no spark.
    /// assert_eq!(SparkSource::classify(-1), SparkSource::None);
    /// // A LITERAL negative → attacker's own set, id = magnitude.
    /// assert_eq!(SparkSource::classify(-2), SparkSource::Own { anim: 2 });
    /// // Non-negative → the common fightfx set (this is also where an
    /// // `S`-prefixed own-spark lands today, since the parser strips the `S`).
    /// assert_eq!(SparkSource::classify(0), SparkSource::Common { anim: 0 });
    /// assert_eq!(SparkSource::classify(5), SparkSource::Common { anim: 5 });
    /// ```
    #[must_use]
    pub const fn classify(sparkno: i32) -> Self {
        if sparkno == -1 {
            SparkSource::None
        } else if sparkno < 0 {
            // Own spark: the action id is the magnitude. `-i32::MIN` would
            // overflow, so use `unsigned_abs` semantics via wrapping then cast —
            // but a const fn can't call that, so guard the extreme explicitly.
            let anim = if sparkno == i32::MIN {
                i32::MAX
            } else {
                -sparkno
            };
            SparkSource::Own { anim }
        } else {
            SparkSource::Common { anim: sparkno }
        }
    }
}

/// The defender's stance at the moment of a hit — which body height they occupy.
///
/// This maps to the MUGEN guardflag/hitflag height letters: standing -> `H`,
/// crouching -> `L`, airborne -> `A`. (An `M` guardflag admits both ground stances.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stance {
    /// Standing (matches the `H` flag letter).
    Stand,
    /// Crouching (matches the `L` flag letter).
    Crouch,
    /// Airborne (matches the `A` flag letter).
    Air,
}

impl Default for Stance {
    /// Defaults to [`Stance::Stand`] — the most common defender stance and a safe
    /// fallback.
    fn default() -> Self {
        Stance::Stand
    }
}

impl Stance {
    /// The [`HitFlags`] bit-set this stance must be admitted by for a flag to apply.
    ///
    /// - [`Stance::Stand`] -> [`HitFlags::HIGH`] (`H`)
    /// - [`Stance::Crouch`] -> [`HitFlags::LOW`] (`L`)
    /// - [`Stance::Air`] -> [`HitFlags::AIR`] (`A`)
    ///
    /// A flag set "admits" the stance when it [`HitFlags::contains`] this single bit.
    /// Note `M` is stored as `HIGH | LOW`, so it admits both ground stances.
    fn flag(self) -> HitFlags {
        match self {
            Stance::Stand => HitFlags::from_bits_truncate(HitFlags::HIGH),
            Stance::Crouch => HitFlags::from_bits_truncate(HitFlags::LOW),
            Stance::Air => HitFlags::from_bits_truncate(HitFlags::AIR),
        }
    }

    /// The common MUGEN get-hit state number for this stance, used as the suggested
    /// defender state when the [`HitDef`] does not override it with `p2stateno`.
    ///
    /// - [`Stance::Stand`] -> `5000`
    /// - [`Stance::Crouch`] -> `5010`
    /// - [`Stance::Air`] -> `5020`
    fn common_gethit_state(self) -> i32 {
        match self {
            Stance::Stand => 5000,
            Stance::Crouch => 5010,
            Stance::Air => 5020,
        }
    }
}

/// The defender's situation at the instant an attack connects.
///
/// This is everything the *pure* resolution logic ([`resolve_hit`]) needs to know about
/// the defender. It is intentionally tiny and engine-agnostic: it carries no `Character`
/// reference, so this crate stays a leaf. `fp-character` (task 6.3b) builds this from the
/// live defender and then applies the returned [`HitOutcome`].
///
/// # Fields
///
/// - `stance` — the defender's body height ([`Stance::Stand`] / [`Stance::Crouch`] /
///   [`Stance::Air`]). Used both to gate guard/hit (via the flag letters) and to pick the
///   stance-based common get-hit state.
/// - `holding_back` — whether the defender is holding the "back" direction (away from the
///   attacker), i.e. attempting to guard. Guarding only succeeds if the guardflag also
///   admits the stance.
/// - `airborne` — whether the defender is off the ground. This selects ground vs. air
///   knockback velocity. It is tracked separately from `stance` because a character can be
///   knocked into the air yet still be processed as e.g. an air stance; in normal play
///   `airborne == (stance == Stance::Air)`, but [`resolve_hit`] does not assume that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DefenderState {
    /// The defender's body height / stance.
    pub stance: Stance,
    /// `true` if the defender is holding back (a guard attempt).
    pub holding_back: bool,
    /// `true` if the defender is airborne (selects air vs. ground knockback).
    pub airborne: bool,
}

impl Default for DefenderState {
    /// A standing, grounded defender who is **not** guarding — the simplest case.
    fn default() -> Self {
        Self {
            stance: Stance::Stand,
            holding_back: false,
            airborne: false,
        }
    }
}

impl DefenderState {
    /// Convenience constructor.
    ///
    /// # Examples
    ///
    /// ```
    /// use fp_combat::{DefenderState, Stance};
    ///
    /// // A crouching defender holding back, on the ground.
    /// let d = DefenderState::new(Stance::Crouch, true, false);
    /// assert_eq!(d.stance, Stance::Crouch);
    /// assert!(d.holding_back);
    /// assert!(!d.airborne);
    /// ```
    pub fn new(stance: Stance, holding_back: bool, airborne: bool) -> Self {
        Self {
            stance,
            holding_back,
            airborne,
        }
    }
}

/// The three possible outcomes of resolving an attack against a defender.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HitResult {
    /// The attack landed cleanly (the defender takes a hit reaction).
    Hit,
    /// The attack was blocked (the defender guarded successfully).
    Guard,
    /// The attack had no effect on this defender (the hitflag excluded their state).
    Miss,
}

/// The fully-resolved effects of an attack on a defender — the output of [`resolve_hit`].
///
/// This is **pure data**: a recipe of what should happen, with no side effects applied.
/// `fp-character` (task 6.3b) is responsible for actually mutating the defender (and
/// attacker) from these values.
///
/// # Knockback orientation (important)
///
/// [`HitOutcome::knockback`] is expressed **attacker-facing-relative**: a positive `x`
/// pushes the defender *away* from the attacker, in the attacker's forward direction; a
/// negative `y` (MUGEN convention, Y points down) lifts the defender upward. Task 6.3b
/// must **mirror this by the attacker's facing** before applying it to the defender — when
/// the attacker faces left, negate the `x` component. The pure logic here cannot know the
/// attacker's facing, so it leaves the value in this canonical, facing-relative frame.
///
/// # Empty outcome on [`HitResult::Miss`]
///
/// A miss produces a zeroed outcome: no damage, no knockback, no times, no fall, and the
/// suggested get-hit state is left as the stance-based common state (callers should ignore
/// effects when `result == HitResult::Miss`). Use [`HitOutcome::is_effective`] to check.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HitOutcome {
    /// Which of [`HitResult::Hit`] / [`HitResult::Guard`] / [`HitResult::Miss`] occurred.
    pub result: HitResult,
    /// Damage to apply to the defender: the [`Damage::hit`] value on a hit, the
    /// [`Damage::guard`] value on a guard, and `0` on a miss.
    pub damage: i32,
    /// Knockback velocity to apply to the defender, **attacker-facing-relative** (see the
    /// type-level docs): positive `x` = away from the attacker in its forward direction.
    /// On a guard this is `(guard_velocity, 0)`; on a hit it is the ground or air velocity
    /// per [`DefenderState::airborne`]; on a miss it is `(0, 0)`.
    pub knockback: Vec2<f32>,
    /// Ticks the **attacker** (P1) is paused (`pausetime.p1`). `0` on a miss.
    pub pausetime: i32,
    /// Ticks the **defender** (P2) shakes (`pausetime.p2`). `0` on a miss.
    pub shaketime: i32,
    /// Hit-stun / guard-stun the defender stays in, in ticks. On a hit this is the
    /// air or ground hittime per [`DefenderState::airborne`]; on a guard it is the guard
    /// hittime; `0` on a miss.
    pub hittime: i32,
    /// Slide time — extra ticks the defender slides after stun. Modelled to mirror MUGEN's
    /// `ground.slidetime` and `guard.slidetime`; this crate has no distinct slidetime
    /// field on [`HitDef`] yet, so it defaults to the relevant hittime and `0` on a miss.
    pub slidetime: i32,
    /// Control time — ticks before the defender regains control (MUGEN `airHitCtrlTime` /
    /// `guard.ctrltime`). Mirrors the relevant hittime here and is `0` on a miss.
    pub ctrltime: i32,
    /// Whether this hit knocks the defender into a falling state. Only ever `true` on a
    /// [`HitResult::Hit`] when [`HitDef::fall`] is set; always `false` on guard/miss.
    pub fall: bool,
    /// Initial upward Y velocity for the falling state ([`HitDef::fall_yvelocity`]). Only
    /// meaningful when `fall` is `true`; `0.0` otherwise.
    pub fall_yvelocity: f32,
    /// Suggested state number to put the **defender** into: [`HitDef::p2stateno`] if set,
    /// otherwise the stance-based common get-hit state (`5000` standing / `5010` crouching
    /// / `5020` air). On a miss this is the stance-based common state but should be ignored
    /// (the defender's state does not change on a miss).
    pub gethit_state: i32,
}

impl HitOutcome {
    /// Builds the zeroed "no effect" outcome for a [`HitResult::Miss`].
    ///
    /// The suggested get-hit state is still the stance-based common state for
    /// completeness, but callers must ignore all effects when the result is a miss.
    fn miss(stance: Stance) -> Self {
        Self {
            result: HitResult::Miss,
            damage: 0,
            knockback: Vec2::new(0.0, 0.0),
            pausetime: 0,
            shaketime: 0,
            hittime: 0,
            slidetime: 0,
            ctrltime: 0,
            fall: false,
            fall_yvelocity: 0.0,
            gethit_state: stance.common_gethit_state(),
        }
    }

    /// `true` when the outcome actually affects the defender (a hit or a guard); `false`
    /// for a [`HitResult::Miss`].
    pub fn is_effective(self) -> bool {
        !matches!(self.result, HitResult::Miss)
    }
}

/// Resolves a single attack against a defender, deciding the outcome and its effects.
///
/// This is the **pure** core of the combat system: given a fully-populated [`HitDef`] and
/// the defender's [`DefenderState`], it computes a [`HitOutcome`] *recipe* without
/// touching any `Character`. Applying that recipe (mutating health, velocity, state) is
/// `fp-character`'s job in task 6.3b.
///
/// # Decision logic
///
/// 1. **Guard** when the defender is holding back **and** the [`HitDef::guardflag`] admits
///    the defender's [`Stance`] (`H` for standing, `L` for crouching, `A` for air; `M`
///    admits both ground heights). An **empty** guardflag means the attack is
///    *unblockable* — it can never be guarded, so holding back does not help.
/// 2. Otherwise it is a **Hit** if the [`HitDef::hitflag`] admits the defender's stance.
/// 3. Otherwise it is a **Miss** (the hitflag excludes the defender's state) — an empty
///    outcome with no effects.
///
/// # Effects
///
/// - On **Hit**: applies [`Damage::hit`]; knockback is [`HitDef::air_velocity`] when the
///   defender is [`DefenderState::airborne`], else [`HitDef::ground_velocity`]; hit-stun is
///   the air or ground hittime accordingly; [`HitOutcome::fall`] is set iff
///   [`HitDef::fall`] is set, carrying [`HitDef::fall_yvelocity`].
/// - On **Guard**: applies [`Damage::guard`]; knockback is `(guard_velocity, 0)`; stun is
///   the guard hittime; never falls.
/// - On **Miss**: a zeroed outcome (see [`HitOutcome`]).
///
/// The suggested defender get-hit state is [`HitDef::p2stateno`] when set, otherwise the
/// stance-based common state (`5000` / `5010` / `5020`).
///
/// Knockback is **attacker-facing-relative** — see [`HitOutcome`]'s type docs; 6.3b mirrors
/// it by the attacker's facing.
///
/// Pure, deterministic, and **never panics**.
///
/// # Examples
///
/// ```
/// use fp_combat::{resolve_hit, DefenderState, HitDef, HitFlags, Damage, Stance, HitResult};
///
/// // A standing defender holding back blocks an `H`-guardable attack.
/// let hd = HitDef {
///     guardflag: HitFlags::parse("MA"),
///     hitflag: HitFlags::parse("MAF"),
///     damage: Damage { hit: 30, guard: 4 },
///     ..HitDef::default()
/// };
/// let blocking = DefenderState::new(Stance::Stand, true, false);
/// let out = resolve_hit(&hd, blocking);
/// assert_eq!(out.result, HitResult::Guard);
/// assert_eq!(out.damage, 4); // guard damage
///
/// // Not holding back -> the same attack lands.
/// let open = DefenderState::new(Stance::Stand, false, false);
/// assert_eq!(resolve_hit(&hd, open).result, HitResult::Hit);
/// ```
pub fn resolve_hit(hitdef: &HitDef, defender: DefenderState) -> HitOutcome {
    let stance_flag = defender.stance.flag();

    // (1) Guard? Only if the defender is holding back, the guardflag is non-empty
    // (empty == unblockable), and the guardflag admits this stance.
    let can_guard = defender.holding_back
        && !hitdef.guardflag.is_empty()
        && hitdef.guardflag.contains(stance_flag);

    if can_guard {
        return HitOutcome {
            result: HitResult::Guard,
            damage: hitdef.damage.guard,
            // Guard pushback is purely horizontal in MUGEN.
            knockback: Vec2::new(hitdef.guard_velocity, 0.0),
            pausetime: hitdef.pausetime.p1,
            shaketime: hitdef.pausetime.p2,
            hittime: hitdef.hittimes.guard,
            // No distinct guard slide/ctrl fields on HitDef yet; mirror guard hittime.
            slidetime: hitdef.hittimes.guard,
            ctrltime: hitdef.hittimes.guard,
            // Guarding never triggers a fall.
            fall: false,
            fall_yvelocity: 0.0,
            gethit_state: suggested_gethit_state(hitdef, defender.stance),
        };
    }

    // (2) Hit? Only if the hitflag admits the defender's stance.
    if !hitdef.hitflag.contains(stance_flag) {
        // (3) Miss: the hit does not affect this defender state.
        return HitOutcome::miss(defender.stance);
    }

    // Clean hit. Pick ground vs. air knockback + hittime per the defender's airborne-ness.
    let (knockback, hittime) = if defender.airborne {
        (hitdef.air_velocity, hitdef.hittimes.air)
    } else {
        (hitdef.ground_velocity, hitdef.hittimes.ground)
    };

    // Fall only applies on a clean hit, and only when the HitDef requests it.
    let fall = hitdef.fall;

    HitOutcome {
        result: HitResult::Hit,
        damage: hitdef.damage.hit,
        knockback,
        pausetime: hitdef.pausetime.p1,
        shaketime: hitdef.pausetime.p2,
        hittime,
        // No distinct slide/ctrl fields on HitDef yet; mirror the applicable hittime.
        slidetime: hittime,
        ctrltime: hittime,
        fall,
        fall_yvelocity: if fall { hitdef.fall_yvelocity } else { 0.0 },
        gethit_state: suggested_gethit_state(hitdef, defender.stance),
    }
}

/// Picks the suggested defender get-hit state: the HitDef's `p2stateno` override if set,
/// else the stance-based common get-hit state (`5000` / `5010` / `5020`).
fn suggested_gethit_state(hitdef: &HitDef, stance: Stance) -> i32 {
    hitdef.p2stateno.unwrap_or_else(|| stance.common_gethit_state())
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
    fn hitdef_default_power_gain_is_zero() {
        // Struct-level default: no automatic power gain (the controller fills the
        // damage-proportional defaults; the bare struct does not).
        let hd = HitDef::default();
        assert_eq!(hd.getpower, PowerGain { hit: 0, guard: 0 });
        assert_eq!(hd.givepower, PowerGain { hit: 0, guard: 0 });
    }

    #[test]
    fn default_getpower_givepower_are_damage_proportional() {
        // MUGEN defaults: getpower = round(damage*0.7), givepower = round(damage*0.6);
        // each guard amount is the hit amount / 2 (integer division).
        let hd = HitDef {
            damage: Damage { hit: 100, guard: 10 },
            ..HitDef::default()
        };
        // 100 * 0.7 = 70, guard 35.
        assert_eq!(hd.default_getpower(), PowerGain { hit: 70, guard: 35 });
        // 100 * 0.6 = 60, guard 30.
        assert_eq!(hd.default_givepower(), PowerGain { hit: 60, guard: 30 });
    }

    #[test]
    fn default_power_gain_rounds_and_clamps_nonnegative() {
        // 23 * 0.7 = 16.1 -> 16; 23 * 0.6 = 13.8 -> 14.
        let hd = HitDef {
            damage: Damage { hit: 23, guard: 0 },
            ..HitDef::default()
        };
        assert_eq!(hd.default_getpower().hit, 16);
        assert_eq!(hd.default_givepower().hit, 14);

        // Negative damage never yields negative power gain.
        let neg = HitDef {
            damage: Damage { hit: -50, guard: 0 },
            ..HitDef::default()
        };
        assert_eq!(neg.default_getpower(), PowerGain { hit: 0, guard: 0 });
        assert_eq!(neg.default_givepower(), PowerGain { hit: 0, guard: 0 });
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

    #[test]
    fn anim_type_parses_all_known_spellings() {
        // Canonical spellings, case-insensitive, whitespace-tolerant.
        assert_eq!(AnimType::parse("Light"), AnimType::Light);
        assert_eq!(AnimType::parse("light"), AnimType::Light);
        assert_eq!(AnimType::parse("  hard  "), AnimType::Hard);
        assert_eq!(AnimType::parse("BACK"), AnimType::Back);
        assert_eq!(AnimType::parse("Up"), AnimType::Up);
        assert_eq!(AnimType::parse("DiagUp"), AnimType::DiagUp);
        assert_eq!(AnimType::parse("diagdown"), AnimType::DiagDown);

        // BOTH `Med` and `Medium` map to Medium (real KFM content uses both).
        assert_eq!(AnimType::parse("Med"), AnimType::Medium);
        assert_eq!(AnimType::parse("med"), AnimType::Medium);
        assert_eq!(AnimType::parse("Medium"), AnimType::Medium);
        assert_eq!(AnimType::parse("MEDIUM"), AnimType::Medium);
    }

    #[test]
    fn anim_type_unknown_and_empty_default_to_light() {
        // Unknown non-empty token -> Light (and would log a warn).
        assert_eq!(AnimType::parse("nonsense"), AnimType::Light);
        // Empty / whitespace -> Light (silent: "use the default").
        assert_eq!(AnimType::parse(""), AnimType::Light);
        assert_eq!(AnimType::parse("   "), AnimType::Light);
        // Default is Light.
        assert_eq!(AnimType::default(), AnimType::Light);
    }

    #[test]
    fn anim_type_code_mapping_is_mugen_standard() {
        assert_eq!(AnimType::Light.code(), 0);
        assert_eq!(AnimType::Medium.code(), 1);
        assert_eq!(AnimType::Hard.code(), 2);
        assert_eq!(AnimType::Back.code(), 3);
        assert_eq!(AnimType::Up.code(), 4);
        assert_eq!(AnimType::DiagUp.code(), 5);
        assert_eq!(AnimType::DiagDown.code(), 6);
        // Parse + code round-trips for the three ordinary ground reactions.
        assert_eq!(AnimType::parse("Light").code(), 0);
        assert_eq!(AnimType::parse("Med").code(), 1);
        assert_eq!(AnimType::parse("Hard").code(), 2);
    }

    #[test]
    fn hitdef_default_animtype_is_light() {
        let hd = HitDef::default();
        assert_eq!(hd.animtype, AnimType::Light);
        assert_eq!(hd.air_animtype, AnimType::Light);
        assert_eq!(hd.animtype.code(), 0);
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
        // Spark id is -1 (none); both sounds are None (no sound).
        assert_eq!(
            hd.resources,
            HitResources { sparkno: -1, hitsound: None, guardsound: None }
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
            animtype: AnimType::Hard,
            air_animtype: AnimType::Up,
            ground_velocity: Vec2::new(-4.0, 0.0),
            air_velocity: Vec2::new(-3.0, -6.0),
            guard_velocity: -2.0,
            hittimes: HitTimes { ground: 15, air: 18, guard: 9 },
            fall: true,
            fall_xvelocity: Some(-2.5),
            fall_yvelocity: -4.5,
            fall_damage: 70,
            p1stateno: Some(1000),
            p2stateno: Some(5050),
            priority: Priority { value: 5, kind: PriorityType::Miss },
            id: 7,
            chainid: -1,
            resources: HitResources {
                sparkno: 2,
                hitsound: Some(SoundId { group: 5, sample: 0, common: false }),
                guardsound: Some(SoundId { group: 6, sample: 0, common: false }),
            },
            getpower: PowerGain { hit: 63, guard: 31 },
            givepower: PowerGain { hit: 54, guard: 27 },
        };
        let copy = base; // Copy, not move.
        assert_eq!(base, copy); // original still usable -> proves Copy
        assert_eq!(copy.damage.hit, 90);
        assert_eq!(copy.p2stateno, Some(5050));
        assert_eq!(copy.ground_type, HitType::Low);
        assert_eq!(copy.animtype, AnimType::Hard);
        assert_eq!(copy.air_animtype, AnimType::Up);
        assert_eq!(copy.priority.kind, PriorityType::Miss);
    }

    /// All the leaf enums must round-trip through their `Default` and be the
    /// MUGEN-faithful defaults.
    #[test]
    fn enum_defaults_match_mugen() {
        assert_eq!(StateClass::default(), StateClass::Standing);
        assert_eq!(AttackPower::default(), AttackPower::Normal);
        assert_eq!(AttackKind::default(), AttackKind::Attack);
        assert_eq!(AnimType::default(), AnimType::Light);
        assert_eq!(HitType::default(), HitType::High);
        assert_eq!(PriorityType::default(), PriorityType::Hit);
        assert_eq!(Priority::default(), Priority { value: 4, kind: PriorityType::Hit });
        assert_eq!(AttackAttr::default(), AttackAttr::parse("S, NA"));
    }

    // ---- Priority / trade clash resolution (audit #20) ----

    /// Helper: build a [`Priority`] from a value and type.
    fn prio(value: i32, kind: PriorityType) -> Priority {
        Priority { value, kind }
    }

    /// Rule 1: a strictly higher numeric value wins outright, regardless of the
    /// [`PriorityType`] on either side (value dominates).
    #[test]
    fn clash_higher_value_wins_ignoring_type() {
        // First higher.
        assert_eq!(
            resolve_clash(prio(7, PriorityType::Hit), prio(4, PriorityType::Hit)),
            ClashOutcome::FirstWins
        );
        // Second higher.
        assert_eq!(
            resolve_clash(prio(2, PriorityType::Hit), prio(5, PriorityType::Hit)),
            ClashOutcome::SecondWins
        );
        // The loser's type does not rescue it: a higher Miss still beats a lower
        // Hit, and a higher Dodge still beats a lower Hit (value dominates).
        assert_eq!(
            resolve_clash(prio(6, PriorityType::Miss), prio(3, PriorityType::Hit)),
            ClashOutcome::FirstWins
        );
        assert_eq!(
            resolve_clash(prio(3, PriorityType::Hit), prio(6, PriorityType::Dodge)),
            ClashOutcome::SecondWins
        );
    }

    /// Rule 2: equal value, both `Hit` -> a **trade** (both attacks land). This is
    /// the KFM case (priority = 3, Hit on both sides).
    #[test]
    fn clash_equal_hit_vs_hit_is_a_trade() {
        assert_eq!(
            resolve_clash(prio(4, PriorityType::Hit), prio(4, PriorityType::Hit)),
            ClashOutcome::Trade
        );
        // KFM authors priority = 3, Hit.
        assert_eq!(
            resolve_clash(prio(3, PriorityType::Hit), prio(3, PriorityType::Hit)),
            ClashOutcome::Trade
        );
    }

    /// Rule 2: equal value, `Hit` vs `Miss` -> the `Hit` side lands and the `Miss`
    /// side is ignored (symmetric).
    #[test]
    fn clash_equal_hit_vs_miss_hit_lands() {
        assert_eq!(
            resolve_clash(prio(4, PriorityType::Hit), prio(4, PriorityType::Miss)),
            ClashOutcome::FirstWins
        );
        assert_eq!(
            resolve_clash(prio(4, PriorityType::Miss), prio(4, PriorityType::Hit)),
            ClashOutcome::SecondWins
        );
    }

    /// Rule 2: equal value, any combination involving a `Dodge`, or two `Miss`es,
    /// suppresses **both** attacks (neither lands).
    #[test]
    fn clash_equal_dodge_and_double_miss_suppress_both() {
        let cases = [
            (PriorityType::Hit, PriorityType::Dodge),
            (PriorityType::Dodge, PriorityType::Hit),
            (PriorityType::Miss, PriorityType::Dodge),
            (PriorityType::Dodge, PriorityType::Miss),
            (PriorityType::Dodge, PriorityType::Dodge),
            (PriorityType::Miss, PriorityType::Miss),
        ];
        for (ka, kb) in cases {
            assert_eq!(
                resolve_clash(prio(5, ka), prio(5, kb)),
                ClashOutcome::NeitherHits,
                "equal-value {ka:?} vs {kb:?} should suppress both"
            );
        }
    }

    /// `resolve_clash` is symmetric: swapping the two arguments swaps `FirstWins`
    /// and `SecondWins` and leaves `Trade` / `NeitherHits` unchanged, for every
    /// pair of priorities drawn from a small grid.
    #[test]
    fn clash_is_symmetric() {
        fn mirror(o: ClashOutcome) -> ClashOutcome {
            match o {
                ClashOutcome::FirstWins => ClashOutcome::SecondWins,
                ClashOutcome::SecondWins => ClashOutcome::FirstWins,
                other => other,
            }
        }
        let kinds = [PriorityType::Hit, PriorityType::Miss, PriorityType::Dodge];
        let values = [3, 4, 5];
        for &va in &values {
            for &vb in &values {
                for ka in kinds {
                    for kb in kinds {
                        let a = prio(va, ka);
                        let b = prio(vb, kb);
                        assert_eq!(
                            resolve_clash(a, b),
                            mirror(resolve_clash(b, a)),
                            "resolve_clash must be symmetric for {a:?} / {b:?}"
                        );
                    }
                }
            }
        }
    }

    /// The default priority (`4, Hit`) clashed with itself is a trade — the
    /// baseline both-default case both attacks should trade.
    #[test]
    fn clash_default_priority_self_is_trade() {
        assert_eq!(
            resolve_clash(Priority::default(), Priority::default()),
            ClashOutcome::Trade
        );
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

    // ---------------------------------------------------------------------
    // (6.3a) resolve_hit: Guard / Hit / Miss decision + effects.
    // ---------------------------------------------------------------------

    /// A standing defender holding back, vs an attack whose guardflag admits high
    /// (`H` via `M`) -> Guard with guard damage, guard velocity, no fall.
    #[test]
    fn resolve_standing_block_guards() {
        let hd = HitDef {
            guardflag: HitFlags::parse("M"), // admits H and L (standing & crouching)
            hitflag: HitFlags::parse("MAF"),
            damage: Damage { hit: 50, guard: 7 },
            guard_velocity: -3.0,
            ground_velocity: Vec2::new(8.0, 0.0),
            hittimes: HitTimes { ground: 12, air: 20, guard: 9 },
            pausetime: PauseTime { p1: 11, p2: 11 },
            fall: true,
            fall_yvelocity: -5.0,
            ..HitDef::default()
        };
        let d = DefenderState::new(Stance::Stand, true, false);
        let out = resolve_hit(&hd, d);
        assert_eq!(out.result, HitResult::Guard);
        assert_eq!(out.damage, 7); // guard damage, not hit damage
        assert_eq!(out.knockback, Vec2::new(-3.0, 0.0)); // guard velocity, Y unused
        assert_eq!(out.hittime, 9); // guard hittime
        assert_eq!(out.slidetime, 9);
        assert_eq!(out.ctrltime, 9);
        assert_eq!(out.pausetime, 11);
        assert_eq!(out.shaketime, 11);
        assert!(!out.fall); // guarding never falls, even though hitdef.fall = true
        assert_eq!(out.fall_yvelocity, 0.0);
        assert_eq!(out.gethit_state, 5000); // standing common get-hit state
        assert!(out.is_effective());
    }

    /// A non-blocking defender (not holding back) vs a normal attack -> Hit with hit
    /// damage and ground knockback + ground hittime + fall (since hitdef.fall).
    #[test]
    fn resolve_non_blocking_hit() {
        let hd = HitDef {
            guardflag: HitFlags::parse("M"),
            hitflag: HitFlags::parse("MAF"),
            damage: Damage { hit: 50, guard: 7 },
            ground_velocity: Vec2::new(8.0, -2.0),
            air_velocity: Vec2::new(6.0, -9.0),
            hittimes: HitTimes { ground: 12, air: 25, guard: 9 },
            pausetime: PauseTime { p1: 10, p2: 10 },
            fall: true,
            fall_yvelocity: -6.5,
            ..HitDef::default()
        };
        // Not holding back -> no guard even though guardflag would admit the stance.
        let d = DefenderState::new(Stance::Stand, false, false);
        let out = resolve_hit(&hd, d);
        assert_eq!(out.result, HitResult::Hit);
        assert_eq!(out.damage, 50); // hit damage
        assert_eq!(out.knockback, Vec2::new(8.0, -2.0)); // GROUND velocity (not airborne)
        assert_eq!(out.hittime, 12); // ground hittime
        assert_eq!(out.slidetime, 12);
        assert_eq!(out.ctrltime, 12);
        assert!(out.fall);
        assert_eq!(out.fall_yvelocity, -6.5);
        assert_eq!(out.gethit_state, 5000);
    }

    /// An airborne defender selects AIR knockback + air hittime.
    #[test]
    fn resolve_airborne_hit_uses_air_velocity() {
        let hd = HitDef {
            guardflag: HitFlags::parse("A"),
            hitflag: HitFlags::parse("MAF"),
            damage: Damage { hit: 40, guard: 6 },
            ground_velocity: Vec2::new(8.0, 0.0),
            air_velocity: Vec2::new(5.0, -8.0),
            hittimes: HitTimes { ground: 12, air: 25, guard: 9 },
            ..HitDef::default()
        };
        // Airborne, not holding back -> air hit.
        let d = DefenderState::new(Stance::Air, false, true);
        let out = resolve_hit(&hd, d);
        assert_eq!(out.result, HitResult::Hit);
        assert_eq!(out.knockback, Vec2::new(5.0, -8.0)); // air velocity
        assert_eq!(out.hittime, 25); // air hittime
        assert_eq!(out.gethit_state, 5020); // air common get-hit state
    }

    /// An unblockable attack (empty guardflag) hits even when the defender holds back.
    #[test]
    fn resolve_unblockable_hits_even_holding_back() {
        let hd = HitDef {
            guardflag: HitFlags::empty(), // empty == unblockable
            hitflag: HitFlags::parse("MAF"),
            damage: Damage { hit: 99, guard: 0 },
            ground_velocity: Vec2::new(4.0, 0.0),
            ..HitDef::default()
        };
        let d = DefenderState::new(Stance::Stand, true, false); // holding back, but...
        let out = resolve_hit(&hd, d);
        assert_eq!(out.result, HitResult::Hit); // ...unblockable -> still a hit
        assert_eq!(out.damage, 99);
    }

    /// A hitflag that excludes the defender's stance -> Miss (empty outcome).
    #[test]
    fn resolve_hitflag_excludes_stance_misses() {
        // hitflag admits only air; a grounded standing defender is excluded.
        let hd = HitDef {
            guardflag: HitFlags::parse("M"),
            hitflag: HitFlags::parse("A"), // air-only
            damage: Damage { hit: 30, guard: 5 },
            ground_velocity: Vec2::new(7.0, 0.0),
            fall: true,
            fall_yvelocity: -4.0,
            ..HitDef::default()
        };
        let d = DefenderState::new(Stance::Stand, false, false);
        let out = resolve_hit(&hd, d);
        assert_eq!(out.result, HitResult::Miss);
        assert_eq!(out.damage, 0);
        assert_eq!(out.knockback, Vec2::new(0.0, 0.0));
        assert_eq!(out.hittime, 0);
        assert_eq!(out.slidetime, 0);
        assert_eq!(out.ctrltime, 0);
        assert_eq!(out.pausetime, 0);
        assert_eq!(out.shaketime, 0);
        assert!(!out.fall);
        assert_eq!(out.fall_yvelocity, 0.0);
        assert!(!out.is_effective());
    }

    /// Holding back but the guardflag does NOT admit the stance -> falls through to Hit
    /// (guard requires a matching guardflag, per the KB).
    #[test]
    fn resolve_holding_back_wrong_guardflag_hits() {
        // guardflag admits only HIGH (standing); a crouching defender holding back
        // cannot block -> hit (hitflag admits low via M).
        let hd = HitDef {
            guardflag: HitFlags::from_bits_truncate(HitFlags::HIGH), // H only
            hitflag: HitFlags::parse("MAF"),
            damage: Damage { hit: 33, guard: 4 },
            ground_velocity: Vec2::new(6.0, 0.0),
            hittimes: HitTimes { ground: 10, air: 20, guard: 8 },
            ..HitDef::default()
        };
        let d = DefenderState::new(Stance::Crouch, true, false);
        let out = resolve_hit(&hd, d);
        assert_eq!(out.result, HitResult::Hit);
        assert_eq!(out.damage, 33);
        assert_eq!(out.hittime, 10); // ground hittime
        assert_eq!(out.gethit_state, 5010); // crouch common get-hit state
    }

    /// `p2stateno`, when set, overrides the stance-based common get-hit state, for
    /// both Hit and Guard.
    #[test]
    fn resolve_p2stateno_overrides_common_state() {
        let hd = HitDef {
            guardflag: HitFlags::parse("M"),
            hitflag: HitFlags::parse("MAF"),
            damage: Damage { hit: 20, guard: 3 },
            p2stateno: Some(5050),
            ..HitDef::default()
        };
        // Hit case.
        let hit = resolve_hit(&hd, DefenderState::new(Stance::Crouch, false, false));
        assert_eq!(hit.result, HitResult::Hit);
        assert_eq!(hit.gethit_state, 5050); // override, not 5010
        // Guard case.
        let guard = resolve_hit(&hd, DefenderState::new(Stance::Crouch, true, false));
        assert_eq!(guard.result, HitResult::Guard);
        assert_eq!(guard.gethit_state, 5050);
    }

    /// `fall` is suppressed when the HitDef does not request it, even on a clean hit.
    #[test]
    fn resolve_no_fall_when_hitdef_fall_false() {
        let hd = HitDef {
            hitflag: HitFlags::parse("MAF"),
            damage: Damage { hit: 10, guard: 1 },
            fall: false,
            fall_yvelocity: -7.0, // present but must be ignored when fall = false
            ..HitDef::default()
        };
        let out = resolve_hit(&hd, DefenderState::new(Stance::Stand, false, false));
        assert_eq!(out.result, HitResult::Hit);
        assert!(!out.fall);
        assert_eq!(out.fall_yvelocity, 0.0);
    }

    /// `resolve_hit` is pure & deterministic: identical inputs yield identical outcomes.
    #[test]
    fn resolve_hit_is_deterministic() {
        let hd = HitDef {
            guardflag: HitFlags::parse("MA"),
            hitflag: HitFlags::parse("MAF"),
            damage: Damage { hit: 25, guard: 4 },
            ground_velocity: Vec2::new(5.0, -1.0),
            air_velocity: Vec2::new(4.0, -7.0),
            ..HitDef::default()
        };
        let d = DefenderState::new(Stance::Air, false, true);
        let first = resolve_hit(&hd, d);
        for _ in 0..16 {
            assert_eq!(resolve_hit(&hd, d), first);
        }
    }

    // ------------------------------------------------------------------
    // Proctor 6.3a gap-fill: M-guardflag crouch block, air block, guard
    // velocity precedence, stance↔airborne decoupling, guard-before-
    // hitflag ordering, per-stance common states, and value passthrough.
    // ------------------------------------------------------------------

    /// AC2 ("M = both ground"): a CROUCHING defender holding back vs a `M`
    /// guardflag blocks — the low (`L`) bit of `M` admits the crouch stance.
    /// This is the positive counterpart to `resolve_holding_back_wrong_guardflag_hits`.
    #[test]
    fn resolve_crouch_block_with_m_guardflag_guards() {
        let hd = HitDef {
            guardflag: HitFlags::parse("M"), // M = H | L, so admits crouch (L)
            hitflag: HitFlags::parse("MAF"),
            damage: Damage { hit: 44, guard: 6 },
            guard_velocity: -2.5,
            hittimes: HitTimes { ground: 10, air: 20, guard: 7 },
            ..HitDef::default()
        };
        let d = DefenderState::new(Stance::Crouch, true, false);
        let out = resolve_hit(&hd, d);
        assert_eq!(out.result, HitResult::Guard);
        assert_eq!(out.damage, 6); // guard damage
        assert_eq!(out.knockback, Vec2::new(-2.5, 0.0));
        assert_eq!(out.hittime, 7); // guard hittime
        assert_eq!(out.gethit_state, 5010); // crouch common get-hit state
    }

    /// AC2/AC3: an AIRBORNE defender holding back vs a guardflag admitting `A`
    /// blocks in the air — and crucially uses the GUARD velocity (horizontal
    /// only), NOT the air knockback velocity, despite being airborne.
    #[test]
    fn resolve_air_block_uses_guard_velocity_not_air_velocity() {
        let hd = HitDef {
            guardflag: HitFlags::parse("A"), // air-guardable
            hitflag: HitFlags::parse("MAF"),
            damage: Damage { hit: 60, guard: 9 },
            air_velocity: Vec2::new(7.0, -10.0), // must NOT be used on a guard
            guard_velocity: -4.0,
            hittimes: HitTimes { ground: 12, air: 30, guard: 11 },
            fall: true, // must be suppressed on guard
            fall_yvelocity: -8.0,
            ..HitDef::default()
        };
        let d = DefenderState::new(Stance::Air, true, true);
        let out = resolve_hit(&hd, d);
        assert_eq!(out.result, HitResult::Guard);
        assert_eq!(out.damage, 9);
        // Guard pushback is purely horizontal; air_velocity is ignored entirely.
        assert_eq!(out.knockback, Vec2::new(-4.0, 0.0));
        assert_eq!(out.hittime, 11); // guard hittime, not air hittime (30)
        assert!(!out.fall);
        assert_eq!(out.fall_yvelocity, 0.0);
        assert_eq!(out.gethit_state, 5020); // air common get-hit state
    }

    /// AC1/AC3: the documented decoupling of `airborne` from `stance`. The pure
    /// logic selects knockback/hittime purely from `airborne`, and the get-hit
    /// state purely from `stance` — they are not assumed to agree.
    #[test]
    fn resolve_airborne_flag_independent_of_stance() {
        let hd = HitDef {
            guardflag: HitFlags::empty(), // unblockable -> always a hit when admitted
            hitflag: HitFlags::parse("MAF"),
            damage: Damage { hit: 20, guard: 2 },
            ground_velocity: Vec2::new(3.0, 0.0),
            air_velocity: Vec2::new(9.0, -9.0),
            hittimes: HitTimes { ground: 5, air: 40, guard: 0 },
            ..HitDef::default()
        };

        // Stance says Stand (gethit_state 5000) but airborne = true -> AIR knockback/hittime.
        let stand_but_airborne = resolve_hit(&hd, DefenderState::new(Stance::Stand, false, true));
        assert_eq!(stand_but_airborne.result, HitResult::Hit);
        assert_eq!(stand_but_airborne.knockback, Vec2::new(9.0, -9.0)); // air velocity
        assert_eq!(stand_but_airborne.hittime, 40); // air hittime
        assert_eq!(stand_but_airborne.gethit_state, 5000); // stance-driven (Stand)

        // Stance says Air (gethit_state 5020) but airborne = false -> GROUND knockback/hittime.
        let air_but_grounded = resolve_hit(&hd, DefenderState::new(Stance::Air, false, false));
        assert_eq!(air_but_grounded.result, HitResult::Hit);
        assert_eq!(air_but_grounded.knockback, Vec2::new(3.0, 0.0)); // ground velocity
        assert_eq!(air_but_grounded.hittime, 5); // ground hittime
        assert_eq!(air_but_grounded.gethit_state, 5020); // stance-driven (Air)
    }

    /// Decision ordering: guard is evaluated BEFORE the hitflag gate. A defender
    /// whose stance the hitflag would EXCLUDE, but who is holding back with a
    /// matching guardflag, still GUARDS (it does not fall through to Miss).
    #[test]
    fn resolve_guard_takes_precedence_over_excluding_hitflag() {
        let hd = HitDef {
            guardflag: HitFlags::parse("H"), // standing-guardable
            hitflag: HitFlags::parse("A"),   // hitflag admits ONLY air
            damage: Damage { hit: 50, guard: 5 },
            guard_velocity: -1.0,
            ..HitDef::default()
        };
        // Standing defender holding back: hitflag (A) excludes Stand, but the
        // guardflag (H) admits it and the defender is holding back -> Guard.
        let d = DefenderState::new(Stance::Stand, true, false);
        let out = resolve_hit(&hd, d);
        assert_eq!(out.result, HitResult::Guard);
        assert_eq!(out.damage, 5); // guard damage, NOT a miss
        assert!(out.is_effective());

        // Same HitDef, NOT holding back: now the hitflag gate applies -> Miss.
        let open = DefenderState::new(Stance::Stand, false, false);
        assert_eq!(resolve_hit(&hd, open).result, HitResult::Miss);
    }

    /// Each stance maps to its own common get-hit state on a clean (non-override)
    /// hit: 5000 / 5010 / 5020. Exercised in one place for the full mapping.
    #[test]
    fn resolve_per_stance_common_gethit_states() {
        let hd = HitDef {
            guardflag: HitFlags::empty(), // unblockable: holding-back is irrelevant
            hitflag: HitFlags::parse("MAF"),
            damage: Damage { hit: 1, guard: 0 },
            ..HitDef::default()
        };
        assert_eq!(
            resolve_hit(&hd, DefenderState::new(Stance::Stand, false, false)).gethit_state,
            5000
        );
        assert_eq!(
            resolve_hit(&hd, DefenderState::new(Stance::Crouch, false, false)).gethit_state,
            5010
        );
        assert_eq!(
            resolve_hit(&hd, DefenderState::new(Stance::Air, false, true)).gethit_state,
            5020
        );
    }

    /// A clean hit reports `is_effective() == true` (the Guard/Miss cases are
    /// covered elsewhere; this closes the trio).
    #[test]
    fn resolve_hit_is_effective() {
        let hd = HitDef {
            hitflag: HitFlags::parse("MAF"),
            damage: Damage { hit: 5, guard: 1 },
            ..HitDef::default()
        };
        let out = resolve_hit(&hd, DefenderState::new(Stance::Stand, false, false));
        assert_eq!(out.result, HitResult::Hit);
        assert!(out.is_effective());
    }

    /// `resolve_hit` is pure passthrough for numeric values: it never clamps,
    /// rejects, or normalizes damage / velocities / times. Zero and negative
    /// damage (e.g. a purely-knockback move) flow through verbatim, and it never
    /// panics on extreme/non-finite velocity components.
    #[test]
    fn resolve_hit_passes_values_through_without_clamping() {
        let hd = HitDef {
            guardflag: HitFlags::parse("M"),
            hitflag: HitFlags::parse("MAF"),
            damage: Damage { hit: -10, guard: -3 }, // negative (heal-on-hit / odd content)
            ground_velocity: Vec2::new(f32::INFINITY, f32::NEG_INFINITY),
            hittimes: HitTimes { ground: -5, air: 20, guard: -2 },
            ..HitDef::default()
        };
        // Hit path: negative hit damage and non-finite velocity pass through, no panic.
        let hit = resolve_hit(&hd, DefenderState::new(Stance::Stand, false, false));
        assert_eq!(hit.result, HitResult::Hit);
        assert_eq!(hit.damage, -10);
        assert_eq!(hit.hittime, -5);
        assert!(hit.knockback.x.is_infinite() && hit.knockback.y.is_infinite());

        // Guard path: negative guard damage passes through verbatim.
        let guard = resolve_hit(&hd, DefenderState::new(Stance::Stand, true, false));
        assert_eq!(guard.result, HitResult::Guard);
        assert_eq!(guard.damage, -3);
        assert_eq!(guard.hittime, -2);
    }

    /// On a Miss the suggested get-hit state is still the stance-based common
    /// state (documented as "should be ignored by callers"), and remains
    /// stance-specific. Locks the documented Miss-outcome contract per stance.
    #[test]
    fn resolve_miss_keeps_stance_common_state_but_no_effects() {
        // hitflag admits only air -> a grounded crouch/stand defender misses.
        let hd = HitDef {
            guardflag: HitFlags::parse("A"), // does not admit ground stances
            hitflag: HitFlags::parse("A"),
            damage: Damage { hit: 30, guard: 5 },
            p2stateno: None,
            ..HitDef::default()
        };
        let stand = resolve_hit(&hd, DefenderState::new(Stance::Stand, false, false));
        assert_eq!(stand.result, HitResult::Miss);
        assert_eq!(stand.gethit_state, 5000); // stance common, even on miss
        assert!(!stand.is_effective());

        let crouch = resolve_hit(&hd, DefenderState::new(Stance::Crouch, true, false));
        // Holding back, but guardflag A does not admit crouch, and hitflag A
        // excludes crouch -> Miss (not Guard).
        assert_eq!(crouch.result, HitResult::Miss);
        assert_eq!(crouch.gethit_state, 5010);
    }

    /// Documented Miss contract: a Miss's suggested get-hit state is ALWAYS the
    /// stance-based common state and does NOT honor `p2stateno` — per the
    /// `HitOutcome::gethit_state` docs ("on a miss this is the stance-based common
    /// state but should be ignored"). This distinguishes Miss from Hit/Guard,
    /// which DO consult the `p2stateno` override (see
    /// `resolve_p2stateno_overrides_common_state`).
    #[test]
    fn resolve_miss_ignores_p2stateno_override() {
        let hd = HitDef {
            guardflag: HitFlags::parse("A"),
            hitflag: HitFlags::parse("A"), // excludes a grounded standing defender
            p2stateno: Some(5070),
            ..HitDef::default()
        };
        let out = resolve_hit(&hd, DefenderState::new(Stance::Stand, false, false));
        assert_eq!(out.result, HitResult::Miss);
        // p2stateno is NOT consulted on a miss: the field falls back to the
        // stance common state (5000), which callers must ignore anyway.
        assert_eq!(out.gethit_state, 5000);
        assert_eq!(out.damage, 0);
        assert!(!out.is_effective());
    }

    /// `DefenderState::default` is a standing, grounded, non-guarding defender —
    /// the documented simplest case — and resolves to a plain ground hit.
    #[test]
    fn resolve_with_default_defender_state() {
        assert_eq!(
            DefenderState::default(),
            DefenderState::new(Stance::Stand, false, false)
        );
        let hd = HitDef {
            hitflag: HitFlags::parse("MAF"),
            damage: Damage { hit: 12, guard: 2 },
            ground_velocity: Vec2::new(4.0, 0.0),
            ..HitDef::default()
        };
        let out = resolve_hit(&hd, DefenderState::default());
        assert_eq!(out.result, HitResult::Hit);
        assert_eq!(out.damage, 12);
        assert_eq!(out.knockback, Vec2::new(4.0, 0.0));
        assert_eq!(out.gethit_state, 5000);
    }

    // ---- SoundId::parse (hitsound / guardsound faithful parsing) -------------

    /// `group, sample` parses both components; no flag → the common/fight file
    /// (the `hitsound`/`guardsound` default).
    #[test]
    fn sound_id_parse_group_and_sample() {
        assert_eq!(
            SoundId::parse("5, 0"),
            Some(SoundId { group: 5, sample: 0, common: true })
        );
        assert_eq!(
            SoundId::parse("5, 3"),
            Some(SoundId { group: 5, sample: 3, common: true })
        );
    }

    /// A leading `S`/`s` flag selects the character's own `.snd` (`common = false`)
    /// and is stripped before the group integer is parsed.
    #[test]
    fn sound_id_parse_s_prefix_is_own() {
        assert_eq!(
            SoundId::parse("S5, 2"),
            Some(SoundId { group: 5, sample: 2, common: false })
        );
        // Lower-case flag too.
        assert_eq!(
            SoundId::parse("s10, 1"),
            Some(SoundId { group: 10, sample: 1, common: false })
        );
    }

    /// The sample defaults to `0` when absent or unparseable; whitespace is
    /// tolerated around both tokens.
    #[test]
    fn sound_id_parse_sample_defaults_to_zero() {
        assert_eq!(
            SoundId::parse("7"),
            Some(SoundId { group: 7, sample: 0, common: true })
        );
        assert_eq!(
            SoundId::parse("  9  "),
            Some(SoundId { group: 9, sample: 0, common: true })
        );
        // Garbage sample → defaults to 0 (group still valid).
        assert_eq!(
            SoundId::parse("4, nope"),
            Some(SoundId { group: 4, sample: 0, common: true })
        );
    }

    /// `-1`, empty, and non-numeric groups all mean "no sound" ([`None`]).
    #[test]
    fn sound_id_parse_sentinel_and_garbage_are_none() {
        assert_eq!(SoundId::parse("-1"), None);
        assert_eq!(SoundId::parse("-1, 4"), None);
        assert_eq!(SoundId::parse(""), None);
        assert_eq!(SoundId::parse("nope"), None);
        // A bare `S` flag with no digits → unparseable group → None.
        assert_eq!(SoundId::parse("S"), None);
    }

    /// `SparkSource::classify` maps a raw `sparkno` onto the MUGEN spark-source
    /// rule: `-1` = none, other negatives = attacker-own (id = magnitude),
    /// non-negative = the common `fightfx` set.
    #[test]
    fn spark_source_classify_maps_mugen_rule() {
        // The default / sentinel.
        assert_eq!(SparkSource::classify(-1), SparkSource::None);
        // Negative → attacker's own set, id = magnitude.
        assert_eq!(SparkSource::classify(-2), SparkSource::Own { anim: 2 });
        assert_eq!(SparkSource::classify(-50), SparkSource::Own { anim: 50 });
        // Non-negative → the common fightfx set at that id.
        assert_eq!(SparkSource::classify(0), SparkSource::Common { anim: 0 });
        assert_eq!(SparkSource::classify(5), SparkSource::Common { anim: 5 });
    }

    /// Even the saturating `i32::MIN` edge classifies as an own spark without
    /// panicking (no `-i32::MIN` overflow).
    #[test]
    fn spark_source_classify_extreme_negative_does_not_overflow() {
        assert_eq!(
            SparkSource::classify(i32::MIN),
            SparkSource::Own { anim: i32::MAX }
        );
    }

    /// The discarded-no-longer contact anchor: `detect_hit_contact` reports the
    /// center of the overlap region, the world-space point a hit spark anchors to.
    #[test]
    fn hit_contact_point_is_overlap_center() {
        // Attacker at x=0 facing right; a punch box reaching x=10..55, y=-60..-40.
        let attack = [ClsnBox::new(10.0, -60.0, 55.0, -40.0)];
        // Defender at x=60 facing left; hurt box ±18 about its axis -> world 42..78.
        let hurt = [ClsnBox::new(-18.0, -70.0, 18.0, 0.0)];
        let c = detect_hit_contact(
            &attack,
            Vec2::new(0.0, 0.0),
            ClsnFacing::Right,
            &hurt,
            Vec2::new(60.0, 0.0),
            ClsnFacing::Left,
        )
        .expect("boxes overlap");
        // Overlap X is [42, 55], Y is [-60, -40]; the point is the center.
        assert!((c.point.x - 48.5).abs() < 1e-4, "x center {}", c.point.x);
        assert!((c.point.y - (-50.0)).abs() < 1e-4, "y center {}", c.point.y);
        assert!(c.w > 0.0 && c.h > 0.0);
    }
}
