//! MUGEN-specific identifier types.
//!
//! These newtypes provide type safety for the various ID pairs used throughout
//! MUGEN file formats. Each wraps a (group, index) pair that references content
//! within container files (SFF for sprites, SND for sounds, etc.).

/// Identifies a sprite within an SFF (Sprite File Format) container.
///
/// MUGEN sprites are organized by group number (e.g., 0 = idle, 200 = walk)
/// and image number within that group. This pair uniquely identifies a sprite
/// in a character's or stage's SFF file.
///
/// # Examples
///
/// ```
/// use fp_core::SpriteId;
///
/// let idle_frame_0 = SpriteId::new(0, 0);
/// assert_eq!(idle_frame_0.group(), 0);
/// assert_eq!(idle_frame_0.image(), 0);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SpriteId {
    group: u16,
    image: u16,
}

impl SpriteId {
    /// Creates a new sprite identifier from group and image numbers.
    pub const fn new(group: u16, image: u16) -> Self {
        Self { group, image }
    }

    /// Returns the sprite group number.
    ///
    /// Common MUGEN groups: 0=idle, 5=turn, 10=crouch, 20=stand-walk,
    /// 40=jump, 100-199=standing attacks, 200-599=crouching/air attacks.
    pub const fn group(&self) -> u16 {
        self.group
    }

    /// Returns the image number within the group.
    pub const fn image(&self) -> u16 {
        self.image
    }
}

impl std::fmt::Display for SpriteId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "({}, {})", self.group, self.image)
    }
}

/// Identifies an animation action defined in an AIR (Animation) file.
///
/// Each animation action number corresponds to a sequence of sprite frames
/// with timing and collision box data. Action numbers map to character states
/// in MUGEN (e.g., action 0 = idle animation, action 5 = turn animation).
///
/// # Examples
///
/// ```
/// use fp_core::AnimId;
///
/// let idle_anim = AnimId::new(0);
/// assert_eq!(idle_anim.action(), 0);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct AnimId {
    action: i32,
}

impl AnimId {
    /// Creates a new animation identifier from an action number.
    pub const fn new(action: i32) -> Self {
        Self { action }
    }

    /// Returns the animation action number.
    pub const fn action(&self) -> i32 {
        self.action
    }
}

impl std::fmt::Display for AnimId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Anim({})", self.action)
    }
}

/// Identifies a sound sample within an SND (Sound) container file.
///
/// Similar to [`SpriteId`], sounds are organized by group and sample number.
/// Common groups: 0=common sounds, 1=hit sounds, etc.
///
/// # Examples
///
/// ```
/// use fp_core::SoundId;
///
/// let hit_sound = SoundId::new(1, 0);
/// assert_eq!(hit_sound.group(), 1);
/// assert_eq!(hit_sound.sample(), 0);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SoundId {
    group: u32,
    sample: u32,
}

impl SoundId {
    /// Creates a new sound identifier from group and sample numbers.
    pub const fn new(group: u32, sample: u32) -> Self {
        Self { group, sample }
    }

    /// Returns the sound group number.
    pub const fn group(&self) -> u32 {
        self.group
    }

    /// Returns the sample number within the group.
    pub const fn sample(&self) -> u32 {
        self.sample
    }
}

impl std::fmt::Display for SoundId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Snd({}, {})", self.group, self.sample)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sprite_id_basics() {
        let id = SpriteId::new(200, 5);
        assert_eq!(id.group(), 200);
        assert_eq!(id.image(), 5);
        assert_eq!(id.to_string(), "(200, 5)");
    }

    #[test]
    fn anim_id_basics() {
        let id = AnimId::new(200);
        assert_eq!(id.action(), 200);
        assert_eq!(id.to_string(), "Anim(200)");
    }

    #[test]
    fn sound_id_basics() {
        let id = SoundId::new(1, 3);
        assert_eq!(id.group(), 1);
        assert_eq!(id.sample(), 3);
        assert_eq!(id.to_string(), "Snd(1, 3)");
    }

    #[test]
    fn sprite_id_equality_and_hash() {
        use std::collections::HashSet;
        let a = SpriteId::new(0, 0);
        let b = SpriteId::new(0, 0);
        let c = SpriteId::new(0, 1);
        assert_eq!(a, b);
        assert_ne!(a, c);

        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
        assert!(!set.contains(&c));
    }
}
