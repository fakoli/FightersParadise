//! Attack-attribute invulnerability windows — the `NotHitBy` / `HitBy` mask
//! (faithfulness audit P9).
//!
//! MUGEN's `NotHitBy` and `HitBy` controllers install a temporary
//! *attack-attribute filter* on a character: for a number of ticks, hits are
//! accepted or rejected based on the **attacker's** `HitDef` `attr` (its
//! state-class + 2-char attack string). `common1.cns` uses these heavily for
//! get-up / throw invulnerability (e.g. "can't be thrown right after getting
//! up": `NotHitBy value = , NT,ST,HT`).
//!
//! This module owns the data model and the pure parse/match logic. The executor
//! ([`crate::executor`]) sets the slots from the controllers and decrements them
//! each tick; hit resolution ([`crate::combat::resolve_attack`]) consults the
//! defender's active slots against the attacker's `HitDef.attr` before applying a
//! hit, dropping the hit (so the attack passes through, exactly like MUGEN) when
//! any active slot blocks it.
//!
//! # The grammar
//!
//! A `NotHitBy`/`HitBy` `value` (slot 1) or `value2` (slot 2) is a
//! comma-separated list whose:
//!
//! - **first token** is a *state-type letter group* — any subset of `S`/`C`/`A`
//!   (standing / crouching / air), e.g. `SCA` or `SA`. An **empty** first token
//!   (the `value = , NT,ST,HT` form) means **all** state-types.
//! - **remaining tokens** are 2-character *attack-class* pairs, each a
//!   [power letter](fp_combat::AttackPower) (`N`/`S`/`H` = Normal/Special/Hyper)
//!   followed by a [kind letter](fp_combat::AttackKind) (`A`/`T`/`P` =
//!   Attack/Throw/Projectile): `NA`, `SA`, `HA`, `NT`, `ST`, `HT`, `NP`, `SP`,
//!   `HP`. **No** pair tokens means **all** attack classes.
//!
//! This is the same letter grammar as a `HitDef` `attr` (which is a *single*
//! class + a *single* PK pair), but a `NotHitBy`/`HitBy` value is a *set*: one
//! state-type group plus a set of PK pairs. A `*` token (or an entirely empty
//! value) is treated as "match everything".
//!
//! # The match rule
//!
//! An attacker's [`fp_combat::AttackAttr`] (its `attr`) **matches** a parsed
//! [`AttackAttrSet`] when **both**:
//!
//! - the attacker's [`StateClass`] is in the set's state-type group, **and**
//! - the attacker's (power, kind) pair is in the set's PK list (an empty PK list
//!   matches every pair).
//!
//! Given a match decision, an active slot **blocks** the hit when:
//!
//! - [`InvulnMode::NotHitBy`] (exclude): the attr **matches** the set — the
//!   listed attributes are exactly the ones that cannot hit.
//! - [`InvulnMode::HitBy`] (include): the attr does **not** match the set — only
//!   the listed attributes *can* hit, so anything outside is blocked.
//!
//! # Edge / safety semantics (never panics)
//!
//! Parsing always yields a value; a malformed token is dropped with a
//! `tracing::debug!` rather than failing. The MUGEN-safe interpretation of an
//! **empty** parsed set (no state-types parsed) is mode-dependent and is the
//! crux of "fail safe":
//!
//! - For [`InvulnMode::NotHitBy`] an empty set **matches nothing**, so it
//!   **blocks nothing** — an unparseable `NotHitBy` is inert (the character is
//!   *not* accidentally made invulnerable).
//! - For [`InvulnMode::HitBy`] an empty set also **matches nothing**, so —
//!   because `HitBy` blocks the *non-matching* — it would block **everything**.
//!   That is the literal reading of "you can only be hit by `nothing`", and it
//!   matches MUGEN, where a `HitBy` with no admissible attributes makes the
//!   character fully invulnerable for its `time`. An author who writes a garbage
//!   `HitBy` gets total invulnerability for the window; this is documented and
//!   intentional rather than silently downgraded.
//!
//! The empty-set state-type group is normalized to "all state-types" only when
//! the first token was *explicitly empty* (the `, NT,ST,HT` form) or a `*`
//! wildcard. The state-type group token must otherwise consist of **only** the
//! letters `S`/`C`/`A` (plus whitespace); a token carrying any other character
//! (e.g. `value = garbage`) is malformed and the whole group is dropped to the
//! empty set, which then follows the mode-dependent fail-safe rule above (rather
//! than silently extracting the stray `a` from `garbage` as "air").

use fp_combat::{AttackAttr, AttackKind, AttackPower, StateClass};
use serde::{Deserialize, Serialize};

/// Which way an [`InvulnSlot`] filters the attacker's attack attribute.
///
/// See the [module docs](crate::invuln) for the full block rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum InvulnMode {
    /// `NotHitBy` — **exclude** the listed attributes: a hit whose attr **is in**
    /// the set is blocked. This is the default (an inactive slot reads as a
    /// `NotHitBy` with an empty set, which blocks nothing).
    #[default]
    NotHitBy,
    /// `HitBy` — **include** only the listed attributes: a hit whose attr is
    /// **not in** the set is blocked.
    HitBy,
}

/// A parsed `NotHitBy`/`HitBy` attack-attribute set: a state-type group plus a
/// list of (power, kind) attack-class pairs.
///
/// Built with [`AttackAttrSet::parse`]. An empty `state_types` paired with an
/// empty `pairs` is the "matches nothing" set; the `any` flag records that the
/// value was an explicit wildcard (`*` or empty), which matches *everything*.
///
/// See the [module docs](crate::invuln) for the grammar and match rule.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AttackAttrSet {
    /// The admitted attacker state-classes (`S`/`C`/`A`). Empty means **no**
    /// state-type matches (the fail-safe set), unless [`any`](Self::any) is set.
    pub state_types: Vec<StateClass>,
    /// The admitted attack-class pairs `(power, kind)`. Empty means **every**
    /// pair matches (only the state-type group then constrains the match).
    pub pairs: Vec<(AttackPower, AttackKind)>,
    /// `true` when the value was an explicit wildcard (`*` or wholly empty):
    /// [`matches`](Self::matches) then returns `true` for any attr.
    pub any: bool,
}

impl AttackAttrSet {
    /// Parses a `NotHitBy`/`HitBy` value string into an [`AttackAttrSet`].
    ///
    /// Accepts the MUGEN grammar described in the [module docs](crate::invuln):
    /// a leading state-type letter group (subset of `SCA`, possibly empty) and
    /// any number of trailing 2-char attack-class pairs. Whitespace- and
    /// case-tolerant. An empty value or a `*` token is the explicit wildcard
    /// ([`any`](Self::any) = `true`). Unrecognized pair tokens are dropped with a
    /// `tracing::debug!`. **Never panics.**
    ///
    /// # Examples
    ///
    /// ```
    /// use fp_character::invuln::AttackAttrSet;
    /// use fp_combat::{StateClass, AttackPower, AttackKind};
    ///
    /// // `SCA` — all three state-types, all attack classes.
    /// let s = AttackAttrSet::parse("SCA");
    /// assert_eq!(s.state_types, vec![StateClass::Standing, StateClass::Crouching, StateClass::Air]);
    /// assert!(s.pairs.is_empty());
    ///
    /// // `, NT,ST,HT` — empty state-type group means ALL state-types (stored as
    /// // a full S/C/A group), with throws only.
    /// let throws = AttackAttrSet::parse(", NT,ST,HT");
    /// assert_eq!(throws.state_types.len(), 3); // S, C, A — all state-types
    /// assert_eq!(throws.pairs.len(), 3);
    /// assert!(throws.pairs.contains(&(AttackPower::Normal, AttackKind::Throw)));
    ///
    /// // `*` and empty are wildcards.
    /// assert!(AttackAttrSet::parse("*").any);
    /// assert!(AttackAttrSet::parse("").any);
    /// ```
    #[must_use]
    pub fn parse(value: &str) -> Self {
        let trimmed = value.trim();
        // An entirely empty value is the explicit wildcard. (An empty *first
        // token* with trailing pairs — `, NT` — is NOT a wildcard; that is the
        // "all state-types, these pairs" form handled below.)
        if trimmed.is_empty() || trimmed == "*" {
            return Self {
                state_types: Vec::new(),
                pairs: Vec::new(),
                any: true,
            };
        }

        let mut tokens = trimmed.split(',');
        // First token: the state-type letter group (subset of S/C/A). An empty
        // first token means "all state-types" — recorded by leaving the vec
        // empty but flagging it so the match treats an empty group as "all".
        let class_tok = tokens.next().unwrap_or("").trim();
        let mut state_types = Vec::new();
        let mut explicit_all_statetypes = false;
        if class_tok.is_empty() || class_tok == "*" {
            // An explicitly-empty first token (the `, NT,ST,HT` form) or a `*`
            // means "all state-types".
            explicit_all_statetypes = true;
        } else {
            // The state-type group must consist ONLY of S/C/A letters (whitespace
            // allowed). A token carrying any other character is malformed, so the
            // whole group is dropped (→ empty = matches nothing, the fail-safe
            // set) rather than silently extracting any stray S/C/A letters from a
            // garbage token like `garbage` (whose `a` would otherwise read as Air).
            let mut malformed = false;
            for c in class_tok.chars() {
                if c.is_whitespace() {
                    continue;
                }
                match c.to_ascii_uppercase() {
                    'S' => push_unique(&mut state_types, StateClass::Standing),
                    'C' => push_unique(&mut state_types, StateClass::Crouching),
                    'A' => push_unique(&mut state_types, StateClass::Air),
                    other => {
                        tracing::debug!(
                            "NotHitBy/HitBy: bad state-type letter {other:?} in {value:?}; \
                             dropping the whole state-type group (matches nothing)"
                        );
                        malformed = true;
                        break;
                    }
                }
            }
            if malformed {
                state_types.clear();
            }
        }

        // Remaining tokens: 2-char attack-class pairs (power + kind).
        let mut pairs = Vec::new();
        for tok in tokens {
            let t = tok.trim();
            if t.is_empty() {
                continue;
            }
            match parse_pair(t) {
                Some(pair) => push_unique(&mut pairs, pair),
                None => {
                    tracing::debug!(
                        "NotHitBy/HitBy: unrecognized attack-class pair {t:?} in {value:?}; ignored"
                    );
                }
            }
        }

        // If the first token explicitly meant "all state-types", record it as a
        // full S/C/A group so the match logic does not see an empty (= matches
        // nothing) group.
        if explicit_all_statetypes {
            state_types = vec![StateClass::Standing, StateClass::Crouching, StateClass::Air];
        }

        Self {
            state_types,
            pairs,
            any: false,
        }
    }

    /// Returns `true` if the attacker's [`AttackAttr`] is a member of this set.
    ///
    /// Membership requires **both** the attacker's [`StateClass`] to be in
    /// [`state_types`](Self::state_types) **and** the attacker's `(power, kind)`
    /// pair to be in [`pairs`](Self::pairs) (an empty `pairs` admits every pair).
    /// A wildcard set ([`any`](Self::any)) matches any attr. See the
    /// [module docs](crate::invuln) for how a match maps to a block.
    #[must_use]
    pub fn matches(&self, attr: &AttackAttr) -> bool {
        if self.any {
            return true;
        }
        // State-type group: an empty group matches nothing (the fail-safe set).
        // The explicit "all state-types" form was normalized to a full S/C/A vec
        // at parse time, so an empty group here is genuinely "no state-types".
        if !self.state_types.contains(&attr.class) {
            return false;
        }
        // Pair list: empty means "all pairs"; otherwise the attacker's pair must
        // be present.
        if self.pairs.is_empty() {
            return true;
        }
        self.pairs.contains(&(attr.power, attr.kind))
    }
}

/// One invulnerability slot: a parsed [`AttackAttrSet`], its filtering
/// [`InvulnMode`], and the remaining ticks it stays active.
///
/// MUGEN gives `NotHitBy`/`HitBy` two independent slots (`value` = slot 1,
/// `value2` = slot 2), each with its own `time`. A slot is **active** while
/// [`time_remaining`](Self::time_remaining) `> 0`; an inactive slot blocks
/// nothing. A hit must pass **both** slots (see [`InvulnMask::blocks`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct InvulnSlot {
    /// The parsed attack-attribute set this slot filters against.
    pub attrs: AttackAttrSet,
    /// Whether the set is an exclude ([`InvulnMode::NotHitBy`]) or include
    /// ([`InvulnMode::HitBy`]) filter.
    pub mode: InvulnMode,
    /// Remaining ticks this slot stays active. `<= 0` means inactive.
    pub time_remaining: i32,
    /// Whether this slot keeps counting down during a hit-pause freeze
    /// (`ignorehitpause = 1` on the controller that set it). A slot without it
    /// is **frozen** (not decremented) while the character is hit-paused, like
    /// the other per-tick timers.
    pub ignore_hitpause: bool,
}

impl InvulnSlot {
    /// Returns `true` while this slot is active (`time_remaining > 0`).
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.time_remaining > 0
    }

    /// Decrements this slot's [`time_remaining`](Self::time_remaining) by one
    /// tick, saturating at `0` (never goes negative, never panics).
    ///
    /// Inactive slots are left untouched.
    pub fn decrement(&mut self) {
        if self.time_remaining > 0 {
            self.time_remaining = self.time_remaining.saturating_sub(1);
        }
    }

    /// Returns `true` if this slot is **active and blocks** a hit whose attacker
    /// attribute is `attr`.
    ///
    /// An inactive slot never blocks. An active slot blocks per its mode:
    /// `NotHitBy` blocks when `attr` **is in** the set; `HitBy` blocks when
    /// `attr` is **not in** the set.
    #[must_use]
    pub fn blocks(&self, attr: &AttackAttr) -> bool {
        if !self.is_active() {
            return false;
        }
        let in_set = self.attrs.matches(attr);
        match self.mode {
            InvulnMode::NotHitBy => in_set,
            InvulnMode::HitBy => !in_set,
        }
    }
}

/// A character's two-slot attack-attribute invulnerability mask.
///
/// Slot 0 is set from the controller's `value`, slot 1 from `value2`. Both
/// slots default to inactive (`time_remaining = 0`, an empty `NotHitBy` set),
/// which blocks nothing. A hit is blocked if **either** active slot blocks it
/// (so a hit must pass *both* active slots — exactly MUGEN's "both must allow"
/// rule).
///
/// See the [module docs](crate::invuln) for the grammar, match rule, and
/// fail-safe edge semantics.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct InvulnMask {
    /// Slot 1 — set from the controller's `value` parameter.
    pub slot1: InvulnSlot,
    /// Slot 2 — set from the controller's `value2` parameter.
    pub slot2: InvulnSlot,
}

impl InvulnMask {
    /// Returns `true` if **any** active slot blocks a hit whose attacker
    /// attribute is `attr` — i.e. the hit must be **dropped** (it passes through
    /// the defender with no effect, like MUGEN).
    ///
    /// A hit is allowed only when it passes **both** slots; either slot blocking
    /// is enough to drop the hit.
    #[must_use]
    pub fn blocks(&self, attr: &AttackAttr) -> bool {
        self.slot1.blocks(attr) || self.slot2.blocks(attr)
    }

    /// Advances both slots by one tick.
    ///
    /// `hitpaused` is `true` while the character is frozen by a connecting hit;
    /// in that case only slots flagged
    /// [`ignore_hitpause`](InvulnSlot::ignore_hitpause) count down (the others
    /// are frozen, like every other per-tick timer). When not hit-paused, both
    /// active slots count down. Never panics.
    pub fn tick(&mut self, hitpaused: bool) {
        for slot in [&mut self.slot1, &mut self.slot2] {
            if hitpaused && !slot.ignore_hitpause {
                continue;
            }
            slot.decrement();
        }
    }
}

/// Pushes `item` into `vec` only if it is not already present (small N; the
/// O(N) scan is cheaper than a hash set for the at-most-handful of entries).
fn push_unique<T: PartialEq>(vec: &mut Vec<T>, item: T) {
    if !vec.contains(&item) {
        vec.push(item);
    }
}

/// Parses a single 2-character attack-class pair (`power` letter + `kind`
/// letter), case-insensitively. Returns `None` for anything that is not exactly
/// two recognized letters.
fn parse_pair(tok: &str) -> Option<(AttackPower, AttackKind)> {
    let upper = tok.trim().to_ascii_uppercase();
    if upper.chars().count() != 2 {
        return None;
    }
    let mut chars = upper.chars();
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
    Some((power, kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `HitDef`-style attr from class/power/kind letters, for terse tests.
    fn attr(class: StateClass, power: AttackPower, kind: AttackKind) -> AttackAttr {
        AttackAttr { class, power, kind }
    }

    #[test]
    fn parse_sca_is_all_statetypes_all_pairs() {
        let s = AttackAttrSet::parse("SCA");
        assert_eq!(
            s.state_types,
            vec![StateClass::Standing, StateClass::Crouching, StateClass::Air]
        );
        assert!(s.pairs.is_empty(), "no pair tokens => all pairs");
        assert!(!s.any);
        // Matches any class with any pair.
        assert!(s.matches(&attr(
            StateClass::Standing,
            AttackPower::Normal,
            AttackKind::Attack
        )));
        assert!(s.matches(&attr(
            StateClass::Air,
            AttackPower::Hyper,
            AttackKind::Projectile
        )));
    }

    #[test]
    fn parse_empty_first_token_means_all_statetypes_with_pairs() {
        // The real KFM "can't be thrown after get-up" value.
        let s = AttackAttrSet::parse(", NT,ST,HT");
        assert!(s.state_types.contains(&StateClass::Standing));
        assert!(s.state_types.contains(&StateClass::Crouching));
        assert!(s.state_types.contains(&StateClass::Air));
        assert_eq!(s.pairs.len(), 3);
        // Throws of any power, any state-type, match.
        assert!(s.matches(&attr(
            StateClass::Standing,
            AttackPower::Normal,
            AttackKind::Throw
        )));
        assert!(s.matches(&attr(
            StateClass::Crouching,
            AttackPower::Hyper,
            AttackKind::Throw
        )));
        // A normal strike does NOT match (only throws are listed).
        assert!(!s.matches(&attr(
            StateClass::Standing,
            AttackPower::Normal,
            AttackKind::Attack
        )));
    }

    #[test]
    fn parse_subset_statetype_group_constrains_class() {
        // `SA, NA` — standing or air normal-attacks only.
        let s = AttackAttrSet::parse("SA, NA");
        assert!(s.matches(&attr(
            StateClass::Standing,
            AttackPower::Normal,
            AttackKind::Attack
        )));
        assert!(s.matches(&attr(
            StateClass::Air,
            AttackPower::Normal,
            AttackKind::Attack
        )));
        // Crouching is not in the group.
        assert!(!s.matches(&attr(
            StateClass::Crouching,
            AttackPower::Normal,
            AttackKind::Attack
        )));
        // Wrong pair.
        assert!(!s.matches(&attr(
            StateClass::Standing,
            AttackPower::Special,
            AttackKind::Attack
        )));
    }

    #[test]
    fn wildcard_matches_everything() {
        for v in ["*", "", "   "] {
            let s = AttackAttrSet::parse(v);
            assert!(s.any, "{v:?} should be a wildcard");
            assert!(s.matches(&attr(
                StateClass::Crouching,
                AttackPower::Special,
                AttackKind::Throw
            )));
        }
    }

    #[test]
    fn garbage_value_is_empty_set_matches_nothing() {
        // Unparseable -> empty state-types, empty pairs, not wildcard.
        let s = AttackAttrSet::parse("garbage");
        assert!(!s.any);
        assert!(
            s.state_types.is_empty(),
            "no valid state-type letters parsed"
        );
        // Matches nothing (empty state-type group).
        assert!(!s.matches(&attr(
            StateClass::Standing,
            AttackPower::Normal,
            AttackKind::Attack
        )));
    }

    #[test]
    fn nothitby_empty_set_blocks_nothing_hitby_empty_set_blocks_everything() {
        let any_attr = attr(
            StateClass::Standing,
            AttackPower::Normal,
            AttackKind::Attack,
        );
        let empty = AttackAttrSet::parse("garbage"); // empty, not wildcard

        // NotHitBy with an empty set: matches nothing => blocks nothing.
        let nhb = InvulnSlot {
            attrs: empty.clone(),
            mode: InvulnMode::NotHitBy,
            time_remaining: 5,
            ignore_hitpause: false,
        };
        assert!(!nhb.blocks(&any_attr), "empty NotHitBy is inert");

        // HitBy with an empty set: matches nothing => blocks everything.
        let hby = InvulnSlot {
            attrs: empty,
            mode: InvulnMode::HitBy,
            time_remaining: 5,
            ignore_hitpause: false,
        };
        assert!(
            hby.blocks(&any_attr),
            "empty HitBy blocks everything (full invuln)"
        );
    }

    #[test]
    fn nothitby_blocks_matching_hitby_blocks_nonmatching() {
        let punch = attr(
            StateClass::Standing,
            AttackPower::Normal,
            AttackKind::Attack,
        );
        let throw = attr(StateClass::Standing, AttackPower::Normal, AttackKind::Throw);

        // NotHitBy covering throws: blocks the throw, allows the punch.
        let nhb = InvulnSlot {
            attrs: AttackAttrSet::parse(", NT,ST,HT"),
            mode: InvulnMode::NotHitBy,
            time_remaining: 12,
            ignore_hitpause: false,
        };
        assert!(nhb.blocks(&throw), "NotHitBy blocks a listed throw");
        assert!(!nhb.blocks(&punch), "NotHitBy allows an unlisted punch");

        // HitBy admitting only throws: allows the throw, blocks the punch.
        let hby = InvulnSlot {
            attrs: AttackAttrSet::parse(", NT,ST,HT"),
            mode: InvulnMode::HitBy,
            time_remaining: 12,
            ignore_hitpause: false,
        };
        assert!(!hby.blocks(&throw), "HitBy allows a listed throw");
        assert!(hby.blocks(&punch), "HitBy blocks an unlisted punch");
    }

    #[test]
    fn inactive_slot_never_blocks() {
        let punch = attr(
            StateClass::Standing,
            AttackPower::Normal,
            AttackKind::Attack,
        );
        let slot = InvulnSlot {
            attrs: AttackAttrSet::parse("SCA"),
            mode: InvulnMode::NotHitBy,
            time_remaining: 0,
            ignore_hitpause: false,
        };
        assert!(!slot.is_active());
        assert!(!slot.blocks(&punch), "expired slot blocks nothing");
    }

    #[test]
    fn decrement_expires_and_saturates() {
        let mut slot = InvulnSlot {
            attrs: AttackAttrSet::parse("SCA"),
            mode: InvulnMode::NotHitBy,
            time_remaining: 2,
            ignore_hitpause: false,
        };
        slot.decrement();
        assert_eq!(slot.time_remaining, 1);
        assert!(slot.is_active());
        slot.decrement();
        assert_eq!(slot.time_remaining, 0);
        assert!(!slot.is_active(), "expired at 0");
        // Saturating: further decrements stay at 0.
        slot.decrement();
        assert_eq!(slot.time_remaining, 0);
    }

    #[test]
    fn mask_blocks_if_either_slot_blocks() {
        let punch = attr(
            StateClass::Standing,
            AttackPower::Normal,
            AttackKind::Attack,
        );
        let throw = attr(StateClass::Standing, AttackPower::Normal, AttackKind::Throw);

        // Slot 1 blocks punches, slot 2 blocks throws — a hit must pass BOTH.
        let mask = InvulnMask {
            slot1: InvulnSlot {
                attrs: AttackAttrSet::parse(", NA,SA,HA"),
                mode: InvulnMode::NotHitBy,
                time_remaining: 10,
                ignore_hitpause: false,
            },
            slot2: InvulnSlot {
                attrs: AttackAttrSet::parse(", NT,ST,HT"),
                mode: InvulnMode::NotHitBy,
                time_remaining: 10,
                ignore_hitpause: false,
            },
        };
        assert!(mask.blocks(&punch), "slot1 blocks the punch");
        assert!(mask.blocks(&throw), "slot2 blocks the throw");

        // A special projectile passes both (neither slot lists it).
        let proj = attr(
            StateClass::Standing,
            AttackPower::Special,
            AttackKind::Projectile,
        );
        assert!(
            !mask.blocks(&proj),
            "an unlisted projectile passes both slots"
        );
    }

    #[test]
    fn mask_tick_freezes_non_ignorehitpause_slots_during_pause() {
        let mut mask = InvulnMask {
            slot1: InvulnSlot {
                attrs: AttackAttrSet::parse("SCA"),
                mode: InvulnMode::NotHitBy,
                time_remaining: 5,
                ignore_hitpause: false, // frozen during pause
            },
            slot2: InvulnSlot {
                attrs: AttackAttrSet::parse("SCA"),
                mode: InvulnMode::NotHitBy,
                time_remaining: 5,
                ignore_hitpause: true, // keeps counting during pause
            },
        };

        // During a hit-pause: slot1 frozen, slot2 still counts down.
        mask.tick(true);
        assert_eq!(mask.slot1.time_remaining, 5, "frozen during pause");
        assert_eq!(
            mask.slot2.time_remaining, 4,
            "ignorehitpause counts during pause"
        );

        // Not paused: both count down.
        mask.tick(false);
        assert_eq!(mask.slot1.time_remaining, 4);
        assert_eq!(mask.slot2.time_remaining, 3);
    }

    #[test]
    fn parse_is_case_and_whitespace_tolerant() {
        let a = AttackAttrSet::parse("  sca ,  na , hp ");
        let b = AttackAttrSet::parse("SCA, NA, HP");
        assert_eq!(a, b);
    }
}
