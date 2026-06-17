//! Stable per-character identity fingerprint for snapshot / replay guards (#38).
//!
//! A snapshot ([`crate::CharacterSnapshot`]) or a recorded replay carries **only**
//! runtime state; it is meant to be restored into an *already-loaded* character
//! built from the **same** `.def`. Restoring a snapshot into a character loaded
//! from a *different* `.def` silently corrupts the simulation (the state-machine
//! cursor, variable banks, and combat bookkeeping all assume the original loaded
//! data). [`CharacterFingerprint`] is the cheap, stable identity stamp that lets a
//! restore path *detect* that mismatch and refuse, instead of corrupting state.
//!
//! # Determinism (important)
//!
//! The fingerprint must be **identical across processes and runs of the same
//! build** so it can be persisted in a replay log and validated later. It is
//! therefore computed with a hand-rolled [FNV-1a] 64-bit hasher over a fixed,
//! stable subset of each character's *static* identity — **never** with
//! [`std::collections::hash_map::DefaultHasher`], whose `RandomState` seed is
//! per-process random and would make the fingerprint differ every run.
//!
//! # What goes into the fingerprint
//!
//! Only fields that are stable for a given `.def` and cheap to read:
//!
//! - the `[Info] name` ([`LoadedCharacter::name`](crate::LoadedCharacter)),
//! - the **integer** [`CharacterConstants`](crate::CharacterConstants) (life/power
//!   maxima, attack/defence, airjuggle, the `[Size]` widths/height, and the
//!   `localcoord`) — integer fields only, so the hash never depends on float bit
//!   patterns,
//! - the **set of compiled state numbers** (the sorted keys of
//!   [`LoadedCharacter::states`](crate::LoadedCharacter)), which distinguishes two
//!   characters that share a name but have different move sets.
//!
//! [FNV-1a]: https://en.wikipedia.org/wiki/Fowler%E2%80%93Noll%E2%80%93Vo_hash_function

use serde::{Deserialize, Serialize};

use crate::LoadedCharacter;

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A small, deterministic [FNV-1a] hasher used to derive a stable identity
/// fingerprint that is the **same across runs of the same build** (unlike
/// [`std::collections::hash_map::DefaultHasher`], whose seed is process-random).
///
/// [FNV-1a]: https://en.wikipedia.org/wiki/Fowler%E2%80%93Noll%E2%80%93Vo_hash_function
#[derive(Debug, Clone, Copy)]
struct Fnv1a {
    state: u64,
}

impl Fnv1a {
    /// A fresh hasher seeded with the FNV-1a offset basis.
    const fn new() -> Self {
        Self {
            state: FNV_OFFSET_BASIS,
        }
    }

    /// Folds one byte into the running hash.
    fn write_u8(&mut self, byte: u8) {
        self.state ^= u64::from(byte);
        self.state = self.state.wrapping_mul(FNV_PRIME);
    }

    /// Folds a byte slice into the running hash (length-independent; callers that
    /// need framing hash an explicit length first).
    fn write_bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u8(b);
        }
    }

    /// Folds an `i32` (little-endian) into the running hash.
    fn write_i32(&mut self, v: i32) {
        self.write_bytes(&v.to_le_bytes());
    }

    /// Folds a `u32` (little-endian) into the running hash.
    fn write_u32(&mut self, v: u32) {
        self.write_bytes(&v.to_le_bytes());
    }

    /// The accumulated 64-bit hash.
    const fn finish(self) -> u64 {
        self.state
    }
}

/// A cheap, stable identity stamp for one loaded character (#38).
///
/// Derived deterministically from a [`LoadedCharacter`]'s `.def`-name, its integer
/// [`CharacterConstants`](crate::CharacterConstants), and the set of its compiled
/// state numbers via [`CharacterFingerprint::of`]. Two characters loaded from the
/// same `.def` (same build) always produce the **same** fingerprint; two visibly
/// different characters produce different ones with overwhelming probability.
///
/// It is stored in a [`crate::CharacterSnapshot`]-bearing match snapshot and in a
/// replay log so the restore / replay path can validate that the snapshot is being
/// applied to a match built from the same characters, refusing a mismatch instead
/// of silently corrupting state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CharacterFingerprint(pub u64);

impl CharacterFingerprint {
    /// Computes the stable fingerprint of a loaded character.
    ///
    /// Hashes — with a fixed [FNV-1a] hasher, deterministic across runs — the
    /// `.def` name, the integer constants, and the **sorted** set of compiled
    /// state numbers. Each variable-length field (the name, the state-number list)
    /// is length-prefixed so two distinct field layouts cannot collide by
    /// concatenation. Reads only; never mutates the character.
    ///
    /// [FNV-1a]: https://en.wikipedia.org/wiki/Fowler%E2%80%93Noll%E2%80%93Vo_hash_function
    #[must_use]
    pub fn of(loaded: &LoadedCharacter) -> Self {
        let mut h = Fnv1a::new();

        // ---- [Info] name (length-prefixed so "ab"+"c" != "a"+"bc") -----------
        let name = loaded.name.as_bytes();
        h.write_u32(name.len() as u32);
        h.write_bytes(name);

        // ---- Integer constants (skip floats: no bit-pattern dependence) ------
        let c = &loaded.constants;
        h.write_i32(c.life_max);
        h.write_i32(c.power_max);
        h.write_i32(c.attack);
        h.write_i32(c.defence);
        h.write_i32(c.airjuggle);
        h.write_i32(c.size.ground_front);
        h.write_i32(c.size.ground_back);
        h.write_i32(c.size.height);
        h.write_i32(c.localcoord.0);
        h.write_i32(c.localcoord.1);

        // ---- Compiled state-number set (sorted -> order-independent) ---------
        // HashMap iteration order is process-randomized, so collect + sort the
        // keys first; the fingerprint must not depend on insertion / iteration
        // order. Length-prefixed for the same framing reason as the name.
        let mut state_nums: Vec<i32> = loaded.states.keys().copied().collect();
        state_nums.sort_unstable();
        h.write_u32(state_nums.len() as u32);
        for n in state_nums {
            h.write_i32(n);
        }

        Self(h.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::CompiledState;
    use crate::{Character, CharacterConstants};
    use fp_formats::air::AirFile;
    use fp_formats::sff::SffFile;
    use std::collections::HashMap;

    /// Builds a minimal valid SFF v1 container in memory carrying a single linked
    /// (data-less) sprite, so a headless [`LoadedCharacter`] can be built without a
    /// sprite asset on disk. Mirrors `fp-engine`'s test `empty_sff`; the
    /// fingerprint never reads the sprites, this is only to satisfy construction.
    fn empty_sff() -> SffFile {
        const SUBHEADER_OFFSET: usize = 64;
        let mut buf = vec![0u8; SUBHEADER_OFFSET + 32];
        buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
        buf[15] = 1; // SFF v1
        buf[16..20].copy_from_slice(&1u32.to_le_bytes()); // num_groups
        buf[20..24].copy_from_slice(&1u32.to_le_bytes()); // num_images
        buf[24..28].copy_from_slice(&(SUBHEADER_OFFSET as u32).to_le_bytes());
        SffFile::from_bytes(&buf).expect("synthetic SFF v1 must parse")
    }

    /// Builds a minimal `LoadedCharacter` with a given name, constants, and state
    /// numbers, without touching the filesystem. The state values are empty
    /// defaults (only their *numbers* feed the fingerprint) and the anim container
    /// is the default; the fingerprint reads none of the assets.
    fn synth(name: &str, constants: CharacterConstants, state_nums: &[i32]) -> LoadedCharacter {
        let mut states = HashMap::new();
        for &n in state_nums {
            states.insert(
                n,
                CompiledState {
                    number: n,
                    ..Default::default()
                },
            );
        }
        LoadedCharacter {
            name: name.to_string(),
            displayname: name.to_string(),
            author: String::new(),
            localcoord: constants.localcoord,
            constants,
            states,
            sff: empty_sff(),
            air: AirFile {
                actions: HashMap::new(),
            },
            cmd: None,
            snd: None,
            palettes: Vec::new(),
        }
    }

    #[test]
    fn fingerprint_is_stable_and_order_independent() {
        let c = CharacterConstants::default();
        // Same identity, state numbers inserted in different orders.
        let a = synth("KFM", c, &[0, 200, 1000, 5900]);
        let b = synth("KFM", c, &[5900, 0, 1000, 200]);
        assert_eq!(
            CharacterFingerprint::of(&a),
            CharacterFingerprint::of(&b),
            "fingerprint must not depend on HashMap insertion order"
        );
    }

    #[test]
    fn fingerprint_differs_on_name() {
        let c = CharacterConstants::default();
        let a = synth("KFM", c, &[0, 200]);
        let b = synth("Suave", c, &[0, 200]);
        assert_ne!(CharacterFingerprint::of(&a), CharacterFingerprint::of(&b));
    }

    #[test]
    fn fingerprint_differs_on_constants() {
        let a = synth("KFM", CharacterConstants::default(), &[0]);
        let c2 = CharacterConstants {
            life_max: 1234,
            ..Default::default()
        };
        let b = synth("KFM", c2, &[0]);
        assert_ne!(CharacterFingerprint::of(&a), CharacterFingerprint::of(&b));
    }

    #[test]
    fn fingerprint_differs_on_state_set() {
        let c = CharacterConstants::default();
        let a = synth("KFM", c, &[0, 200]);
        let b = synth("KFM", c, &[0, 200, 201]);
        assert_ne!(CharacterFingerprint::of(&a), CharacterFingerprint::of(&b));
    }

    #[test]
    fn character_does_not_carry_identity() {
        // Sanity: a bare Character has no fingerprint of its own (the fingerprint
        // lives on the loaded static data, not the runtime entity).
        let _ = Character::new();
    }
}
