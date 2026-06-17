//! Hash-keyed, on-disk IR cache for parsed character content (F034 T087).
//!
//! Parsing and compiling a character's `.def` + every file it references
//! (`.cns`/`.cmd`/`.air`/`.sff`/`.snd`/`.act`) is the slow part of
//! [`LoadedCharacter::load`](crate::loader::LoadedCharacter::load) — and it
//! floods the log with `bad expression -> const 0` / `CNS:` warnings every time.
//! This module provides a **content-addressed** cache so an unchanged character
//! loads from a single deserialize instead of re-parsing the whole bundle.
//!
//! # How the key is computed
//!
//! The cache key is *airtight* against edits: every referenced source file is
//! hashed, not just the `.def`. For each referenced file we record
//! `(relpath, sha256(bytes))`, sort the list by relpath for a stable order, and
//! fold the encoded list together with [`PARSER_FORMAT_VERSION`] and
//! [`COMPILER_IR_VERSION`] into one [`blake3`] key:
//!
//! ```text
//! inputs = sorted([(relpath, sha256(bytes)) for f in def_referenced_files])
//! key    = blake3(encode(inputs) || PARSER_FORMAT_VERSION || COMPILER_IR_VERSION)
//! ```
//!
//! Editing **any** input changes that file's sha256, which changes the key, so a
//! stale entry is simply never found — automatic invalidation. Bumping either
//! version const invalidates *every* entry at once (used when the parser or the
//! compiled-IR layout changes).
//!
//! # Safety contract
//!
//! Mirrors the engine-wide "never crash on bad content" rule, extended to the
//! cache itself: **any** cache error — missing dir, unreadable file, truncated
//! blob, version mismatch, mutated bytes — is a *silent fall-through* to the full
//! load path ([`IrCache::read`] returns `None`, [`IrCache::write`] returns
//! `Err` that the caller ignores). The cache can only ever make a load faster,
//! never make it fail.
//!
//! # Controls
//!
//! - `$FP_CACHE_DIR` overrides the cache root (default `<workspace>/.fp-cache/`).
//! - `FP_NO_CACHE=1` disables the cache entirely (both read and write).
//! - The cache **refuses to write inside `assets/`** (the clean-room tracked
//!   tree) so generated blobs never land in version control. The default root
//!   `.fp-cache/` is gitignored.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Version of the text/binary **parsers** (`.def`/`.cns`/`.cmd`/`.air`/`.sff`/…).
///
/// Bump this whenever a parser change alters the parsed representation so old
/// cache entries (which encode the previous representation) are discarded.
pub const PARSER_FORMAT_VERSION: u32 = 1;

/// Version of the **compiled IR** layout (the merged [`CompiledState`] graph and
/// everything else baked into a cached [`LoadedCharacter`]).
///
/// [`CompiledState`]: crate::loader::CompiledState
///
/// Bump this whenever the compiled-IR layout changes so old cache entries are
/// discarded even when the on-disk source files are byte-identical.
pub const COMPILER_IR_VERSION: u32 = 1;

/// File extension for cache blobs (Fighters Paradise IR).
const CACHE_EXT: &str = "fpir";

/// Process-wide cache hit/miss counters, used by tests (and available to callers
/// for diagnostics) to prove a second load came from the cache rather than a
/// re-parse. See [`cache_stats`] / [`reset_cache_stats`].
static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static CACHE_MISSES: AtomicU64 = AtomicU64::new(0);

/// A snapshot of the process-wide cache `(hits, misses)` counters.
#[must_use]
pub fn cache_stats() -> (u64, u64) {
    (
        CACHE_HITS.load(Ordering::Relaxed),
        CACHE_MISSES.load(Ordering::Relaxed),
    )
}

/// Resets the process-wide cache hit/miss counters to zero (test helper).
pub fn reset_cache_stats() {
    CACHE_HITS.store(0, Ordering::Relaxed);
    CACHE_MISSES.store(0, Ordering::Relaxed);
}

/// One referenced source input: its path (as written in the `.def`, used as the
/// stable sort key) and the sha256 of its current bytes.
///
/// A referenced file that does not exist or cannot be read still contributes to
/// the key (with an all-zero hash) so that *creating* a previously missing
/// optional asset also invalidates the entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceInput {
    /// The reference exactly as it appears in the `.def` (the stable sort key).
    pub relpath: String,
    /// sha256 of the file's current bytes; all-zero when the file is absent.
    pub sha256: [u8; 32],
}

/// The full set of referenced inputs that key a cache entry, kept sorted by
/// [`relpath`](SourceInput::relpath) for a deterministic encoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrCacheKey {
    /// The sorted referenced inputs (the `.def` itself plus every file it names).
    pub inputs: Vec<SourceInput>,
}

impl IrCacheKey {
    /// Builds a key from the `.def` path and the resolved paths of every file it
    /// references, hashing each file's current bytes.
    ///
    /// `def_path` is the character's `.def`; `referenced` is the list of
    /// `(relpath, resolved_path)` pairs the loader resolved from `[Files]` — the
    /// `relpath` is what was written in the `.def` (used verbatim as the sort /
    /// identity key), `resolved_path` is what is actually hashed. The `.def`
    /// itself is included automatically (so editing the `.def` re-imports too).
    ///
    /// Never fails: an unreadable / absent referenced file contributes an
    /// all-zero hash rather than aborting key computation.
    #[must_use]
    pub fn build(def_path: &Path, referenced: &[(String, PathBuf)]) -> Self {
        let mut inputs: Vec<SourceInput> = Vec::with_capacity(referenced.len() + 1);
        // The `.def` itself, keyed by its file name so it sorts deterministically
        // alongside the referenced relpaths.
        let def_key = def_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| def_path.to_string_lossy().into_owned());
        inputs.push(SourceInput {
            relpath: def_key,
            sha256: hash_file(def_path),
        });
        for (relpath, resolved) in referenced {
            inputs.push(SourceInput {
                relpath: relpath.clone(),
                sha256: hash_file(resolved),
            });
        }
        // Stable order for a deterministic key. A character cannot legitimately
        // reference the same relpath twice with different bytes, but if the list
        // somehow contains duplicates the sort keeps the encoding stable.
        inputs.sort_by(|a, b| a.relpath.cmp(&b.relpath));
        Self { inputs }
    }

    /// Folds the sorted inputs and both version consts into the final 32-byte
    /// blake3 digest, rendered as the lowercase-hex cache-file stem.
    #[must_use]
    pub fn digest_hex(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        for input in &self.inputs {
            // length-prefix the relpath so `("ab","")` and `("a","b")` cannot
            // collide, then the raw 32-byte hash.
            let bytes = input.relpath.as_bytes();
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
            hasher.update(&input.sha256);
        }
        hasher.update(&PARSER_FORMAT_VERSION.to_le_bytes());
        hasher.update(&COMPILER_IR_VERSION.to_le_bytes());
        hasher.finalize().to_hex().to_string()
    }
}

/// The payload an [`LoadedCharacter`](crate::loader::LoadedCharacter) load
/// currently caches.
///
/// The full compiled-IR graph (the merged [`CompiledState`] map, the decoded
/// sprites, and the AIR animations) is **not yet** serde-serializable — that
/// lands with T086 ("serde on the static load graph"). Until then this manifest
/// records the lightweight, already-serializable shape of the loaded character so
/// the cache is exercised end-to-end through the public `load` path: a first load
/// writes it, a second unchanged load deserializes it (a hit), and any edit to a
/// referenced source re-keys it (a miss). When the graph gains serde, this is the
/// type to widen into the real cached IR — the surrounding key / probe / write /
/// invalidation machinery does not change.
///
/// [`CompiledState`]: crate::loader::CompiledState
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedManifest {
    /// `[Info] name` of the cached character.
    pub name: String,
    /// Number of compiled states in the merged graph.
    pub compiled_states: u32,
    /// Number of sprites in the loaded SFF.
    pub sprite_count: u32,
    /// Number of animations in the loaded AIR.
    pub anim_count: u32,
}

/// On-disk header prepended to every cache payload.
///
/// The header is checked before the payload is deserialized so a blob written by
/// an older engine (different version consts) — or one whose key does not match
/// the recomputed key — is discarded as a *miss* without ever touching the
/// payload bytes. This is belt-and-suspenders over the key already being in the
/// file *name*: it survives an attacker (or a hash collision) renaming a blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrCacheHeader {
    /// Parser format version this blob was written with.
    pub format_version: u32,
    /// Compiled-IR layout version this blob was written with.
    pub compiler_ir_version: u32,
    /// The full key (sorted inputs + their hashes) this blob is valid for.
    pub key: IrCacheKey,
}

impl IrCacheHeader {
    /// Builds the current-version header for a given key.
    #[must_use]
    pub fn current(key: IrCacheKey) -> Self {
        Self {
            format_version: PARSER_FORMAT_VERSION,
            compiler_ir_version: COMPILER_IR_VERSION,
            key,
        }
    }

    /// `true` when this header matches the running engine's version consts *and*
    /// the supplied freshly-computed key (so a mutated/stale blob is rejected).
    #[must_use]
    pub fn is_valid_for(&self, key: &IrCacheKey) -> bool {
        self.format_version == PARSER_FORMAT_VERSION
            && self.compiler_ir_version == COMPILER_IR_VERSION
            && &self.key == key
    }
}

/// What gets bincode-encoded into a `.fpir` blob: the header followed by the
/// caller's payload `T`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEnvelope<T> {
    header: IrCacheHeader,
    payload: T,
}

/// A handle to the on-disk IR cache rooted at a resolved directory.
///
/// Construct with [`IrCache::resolve`] (which honors `$FP_CACHE_DIR` /
/// `FP_NO_CACHE` and refuses to root inside `assets/`). When the cache is
/// disabled, [`resolve`](IrCache::resolve) returns `None` and the caller takes
/// the full load path.
#[derive(Debug, Clone)]
pub struct IrCache {
    root: PathBuf,
}

impl IrCache {
    /// Resolves the cache root from the environment, or `None` when caching is
    /// disabled / not permitted.
    ///
    /// - `FP_NO_CACHE=1` → `None` (caching off).
    /// - `$FP_CACHE_DIR` if set → that directory.
    /// - else `<workspace>/.fp-cache/`, where `<workspace>` is `workspace_root`.
    ///
    /// Refuses (returns `None`) any root that resolves **inside `assets/`** so the
    /// clean-room tracked tree never receives generated blobs.
    #[must_use]
    pub fn resolve(workspace_root: &Path) -> Option<Self> {
        if env_flag("FP_NO_CACHE") {
            return None;
        }
        let root = match std::env::var_os("FP_CACHE_DIR") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => workspace_root.join(".fp-cache"),
        };
        if path_is_inside_assets(&root) {
            tracing::warn!(
                "IR cache root {} resolves inside assets/; refusing to cache there",
                root.display()
            );
            return None;
        }
        Some(Self { root })
    }

    /// Constructs a cache handle rooted at an explicit directory, bypassing the
    /// environment (used by tests and callers that already resolved a root).
    ///
    /// Still refuses a root inside `assets/`.
    #[must_use]
    pub fn with_root(root: PathBuf) -> Option<Self> {
        if path_is_inside_assets(&root) {
            return None;
        }
        Some(Self { root })
    }

    /// The resolved cache root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Full path to the blob for `key`.
    fn blob_path(&self, key: &IrCacheKey) -> PathBuf {
        self.root.join(format!("{}.{CACHE_EXT}", key.digest_hex()))
    }

    /// Probes the cache for `key`, deserializing the payload on a verified hit.
    ///
    /// Returns `None` (a *miss*, counted) on any of: file absent, unreadable,
    /// truncated/corrupt, header version mismatch, or a header key that does not
    /// match the freshly-computed `key`. Never panics.
    #[must_use]
    pub fn read<T: DeserializeOwned>(&self, key: &IrCacheKey) -> Option<T> {
        let path = self.blob_path(key);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => {
                CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        match bincode::deserialize::<CacheEnvelope<T>>(&bytes) {
            Ok(env) if env.header.is_valid_for(key) => {
                CACHE_HITS.fetch_add(1, Ordering::Relaxed);
                Some(env.payload)
            }
            Ok(_) => {
                // Decoded, but the header is stale (version bump) or the key does
                // not match (mutated bytes / renamed blob). Treat as a miss and
                // remove the bad blob so it does not linger.
                tracing::debug!("IR cache {}: header mismatch, discarding", path.display());
                let _ = std::fs::remove_file(&path);
                CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
                None
            }
            Err(e) => {
                tracing::debug!(
                    "IR cache {}: corrupt blob ({e}), discarding",
                    path.display()
                );
                let _ = std::fs::remove_file(&path);
                CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Writes `payload` for `key` to the cache via an atomic temp-write + rename.
    ///
    /// # Errors
    ///
    /// Returns an [`IrCacheError`] when the cache directory cannot be created,
    /// the payload cannot be encoded, or the temp file cannot be written/renamed.
    /// Callers treat any error as a no-op (the full load already succeeded), so
    /// this never propagates into a failed character load.
    pub fn write<T: Serialize>(&self, key: &IrCacheKey, payload: &T) -> Result<(), IrCacheError> {
        std::fs::create_dir_all(&self.root)
            .map_err(|e| IrCacheError::Io(format!("create {}: {e}", self.root.display())))?;
        let envelope = CacheEnvelope {
            header: IrCacheHeader::current(key.clone()),
            payload,
        };
        let bytes = bincode::serialize(&envelope)
            .map_err(|e| IrCacheError::Encode(format!("serialize: {e}")))?;

        // Atomic publish: write to a unique temp file in the same directory, then
        // rename over the final path. A crash mid-write leaves only the temp file
        // (a future read misses cleanly); a successful rename is atomic on POSIX.
        let final_path = self.blob_path(key);
        let tmp_path = self
            .root
            .join(format!(".{}.{}.tmp", key.digest_hex(), std::process::id()));
        std::fs::write(&tmp_path, &bytes)
            .map_err(|e| IrCacheError::Io(format!("write {}: {e}", tmp_path.display())))?;
        std::fs::rename(&tmp_path, &final_path).map_err(|e| {
            // Best-effort cleanup of the temp file on a failed rename.
            let _ = std::fs::remove_file(&tmp_path);
            IrCacheError::Io(format!("rename into {}: {e}", final_path.display()))
        })?;
        Ok(())
    }
}

/// Errors raised by the IR cache write path.
///
/// Every variant is non-fatal: callers treat any [`IrCacheError`] as "skip the
/// cache" and proceed with the already-completed full load.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IrCacheError {
    /// A filesystem operation (create dir / write / rename) failed.
    #[error("IR cache I/O error: {0}")]
    Io(String),
    /// The payload could not be serialized.
    #[error("IR cache encode error: {0}")]
    Encode(String),
}

/// Reads `path` and returns the sha256 of its bytes, or an all-zero hash when the
/// file is absent or unreadable (so creating a missing file still changes the key).
fn hash_file(path: &Path) -> [u8; 32] {
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            hasher.finalize().into()
        }
        Err(_) => [0u8; 32],
    }
}

/// `true` when `name` is set to a truthy value (`1`/`true`/`yes`, case-insensitive).
fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "1" || v == "true" || v == "yes"
        })
        .unwrap_or(false)
}

/// `true` when any normalized component of `path` is exactly `assets` — the
/// clean-room tracked tree the cache must never write into.
fn path_is_inside_assets(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(
            c,
            std::path::Component::Normal(s) if s.eq_ignore_ascii_case("assets")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Per-test scratch dir under the OS temp root (unique per test name + pid).
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fp_ir_cache_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(contents).unwrap();
    }

    /// Lays out a minimal `.def` + a couple of referenced files, returning the
    /// def path and the `(relpath, resolved)` reference list the loader would
    /// build.
    fn fixture(dir: &Path) -> (PathBuf, Vec<(String, PathBuf)>) {
        let def = dir.join("c.def");
        write_file(&def, b"[Files]\nsprite=c.sff\ncns=c.cns\n");
        write_file(&dir.join("c.sff"), b"SFF-BYTES-v1");
        write_file(&dir.join("c.cns"), b"[Statedef 0]\n");
        let refs = vec![
            ("c.sff".to_string(), dir.join("c.sff")),
            ("c.cns".to_string(), dir.join("c.cns")),
        ];
        (def, refs)
    }

    // ---- AC1: write-then-read hit; mutating cached bytes => miss -------------
    #[test]
    fn ir_cache_write_then_read_round_trips_payload() {
        let dir = scratch_dir("roundtrip");
        let (def, refs) = fixture(&dir);
        let key = IrCacheKey::build(&def, &refs);
        let cache = IrCache::with_root(dir.join(".fp-cache")).unwrap();

        // Cold probe is a miss (delta-based: the counters are process-global and
        // other tests run concurrently, so assert the *change*, not absolutes).
        let (_, m0) = cache_stats();
        assert!(cache.read::<Vec<i32>>(&key).is_none());
        let (_, m1) = cache_stats();
        assert_eq!(m1 - m0, 1, "cold probe increments the miss counter by one");

        // Write, then a second probe hits and returns the exact payload.
        let payload = vec![1, 2, 3, 4];
        cache.write(&key, &payload).unwrap();
        let (h0, _) = cache_stats();
        let got: Vec<i32> = cache.read(&key).expect("warm probe must hit");
        assert_eq!(got, payload);
        let (h1, _) = cache_stats();
        assert_eq!(h1 - h0, 1, "warm probe increments the hit counter by one");
    }

    #[test]
    fn ir_cache_mutated_bytes_is_discarded_as_miss() {
        let dir = scratch_dir("mutated");
        let (def, refs) = fixture(&dir);
        let key = IrCacheKey::build(&def, &refs);
        let cache = IrCache::with_root(dir.join(".fp-cache")).unwrap();
        cache.write(&key, &vec![9u8, 8, 7]).unwrap();

        // Corrupt the blob's bytes in place.
        let blob = cache.blob_path(&key);
        let mut bytes = std::fs::read(&blob).unwrap();
        for b in bytes.iter_mut() {
            *b ^= 0xFF;
        }
        std::fs::write(&blob, &bytes).unwrap();

        assert!(
            cache.read::<Vec<u8>>(&key).is_none(),
            "mutated bytes must not be served as a hit"
        );
        assert!(!blob.exists(), "corrupt blob is removed");
    }

    // ---- AC2: editing ANY referenced source changes the key -----------------
    #[test]
    fn ir_cache_editing_any_input_changes_key() {
        let dir = scratch_dir("invalidate");
        let (def, refs) = fixture(&dir);
        let original = IrCacheKey::build(&def, &refs).digest_hex();

        // Edit the .def itself.
        write_file(&def, b"[Files]\nsprite=c.sff\ncns=c.cns\n;edited\n");
        let after_def = IrCacheKey::build(&def, &refs).digest_hex();
        assert_ne!(original, after_def, "editing the .def re-keys");

        // Edit a referenced .cns.
        write_file(&dir.join("c.cns"), b"[Statedef 0]\n; changed\n");
        let after_cns = IrCacheKey::build(&def, &refs).digest_hex();
        assert_ne!(after_def, after_cns, "editing a referenced file re-keys");

        // Edit a referenced .sff (binary).
        write_file(&dir.join("c.sff"), b"SFF-BYTES-v2-different");
        let after_sff = IrCacheKey::build(&def, &refs).digest_hex();
        assert_ne!(after_cns, after_sff, "editing a binary asset re-keys");
    }

    #[test]
    fn ir_cache_creating_a_missing_input_changes_key() {
        let dir = scratch_dir("create_missing");
        let (def, _) = fixture(&dir);
        // Reference an optional .snd that does not exist yet.
        let refs = vec![("c.snd".to_string(), dir.join("c.snd"))];
        let before = IrCacheKey::build(&def, &refs).digest_hex();
        write_file(&dir.join("c.snd"), b"SND-BYTES");
        let after = IrCacheKey::build(&def, &refs).digest_hex();
        assert_ne!(before, after, "creating a missing optional file re-keys");
    }

    #[test]
    fn ir_cache_a_hit_for_unchanged_inputs() {
        let dir = scratch_dir("unchanged");
        let (def, refs) = fixture(&dir);
        let k1 = IrCacheKey::build(&def, &refs);
        let k2 = IrCacheKey::build(&def, &refs);
        assert_eq!(k1, k2);
        assert_eq!(k1.digest_hex(), k2.digest_hex());
    }

    // ---- AC3: corrupt / older-version cache discarded without panic ---------
    #[test]
    fn ir_cache_garbage_blob_never_panics() {
        let dir = scratch_dir("garbage");
        let (def, refs) = fixture(&dir);
        let key = IrCacheKey::build(&def, &refs);
        let cache = IrCache::with_root(dir.join(".fp-cache")).unwrap();
        std::fs::create_dir_all(cache.root()).unwrap();
        // A truncated / non-bincode blob at the exact expected path.
        std::fs::write(cache.blob_path(&key), b"not a valid envelope").unwrap();

        assert!(cache.read::<Vec<i32>>(&key).is_none());
    }

    #[test]
    fn ir_cache_older_format_version_is_discarded() {
        let dir = scratch_dir("oldversion");
        let (def, refs) = fixture(&dir);
        let key = IrCacheKey::build(&def, &refs);
        let cache = IrCache::with_root(dir.join(".fp-cache")).unwrap();
        std::fs::create_dir_all(cache.root()).unwrap();

        // Hand-build an envelope with a stale header (older versions) and write
        // it at the current key's path.
        let stale = CacheEnvelope {
            header: IrCacheHeader {
                format_version: PARSER_FORMAT_VERSION.wrapping_sub(1),
                compiler_ir_version: COMPILER_IR_VERSION,
                key: key.clone(),
            },
            payload: vec![1i32, 2, 3],
        };
        std::fs::write(cache.blob_path(&key), bincode::serialize(&stale).unwrap()).unwrap();

        assert!(
            cache.read::<Vec<i32>>(&key).is_none(),
            "older format_version must be discarded"
        );
    }

    #[test]
    fn ir_cache_header_key_mismatch_is_discarded() {
        let dir = scratch_dir("keymismatch");
        let (def, refs) = fixture(&dir);
        let key = IrCacheKey::build(&def, &refs);
        let cache = IrCache::with_root(dir.join(".fp-cache")).unwrap();
        std::fs::create_dir_all(cache.root()).unwrap();

        // A current-version envelope but for a DIFFERENT key, parked at this
        // key's path (simulates a renamed blob / hash collision).
        let other_key = IrCacheKey {
            inputs: vec![SourceInput {
                relpath: "other".to_string(),
                sha256: [7u8; 32],
            }],
        };
        let envelope = CacheEnvelope {
            header: IrCacheHeader::current(other_key),
            payload: vec![5i32],
        };
        std::fs::write(
            cache.blob_path(&key),
            bincode::serialize(&envelope).unwrap(),
        )
        .unwrap();

        assert!(cache.read::<Vec<i32>>(&key).is_none());
    }

    // ---- AC3: FP_NO_CACHE disables; assets/ root refused --------------------
    #[test]
    fn ir_cache_refuses_to_root_inside_assets() {
        assert!(IrCache::with_root(PathBuf::from("/some/repo/assets/.fp-cache")).is_none());
        assert!(IrCache::with_root(PathBuf::from("assets/data/cache")).is_none());
        // A sibling that merely starts with "assets" is fine.
        assert!(IrCache::with_root(PathBuf::from("/repo/assets-cache")).is_some());
        assert!(IrCache::with_root(PathBuf::from("/repo/.fp-cache")).is_some());
    }

    #[test]
    fn ir_cache_path_inside_assets_detection() {
        assert!(path_is_inside_assets(Path::new("assets")));
        assert!(path_is_inside_assets(Path::new("foo/Assets/bar")));
        assert!(!path_is_inside_assets(Path::new("foo/assets-x/bar")));
        assert!(!path_is_inside_assets(Path::new("foo/myassets")));
    }

    #[test]
    fn ir_cache_env_flag_parsing() {
        // Use a uniquely-named var so we don't race other tests on FP_NO_CACHE.
        let name = "FP_IR_CACHE_TEST_FLAG";
        std::env::remove_var(name);
        assert!(!env_flag(name));
        std::env::set_var(name, "1");
        assert!(env_flag(name));
        std::env::set_var(name, "TRUE");
        assert!(env_flag(name));
        std::env::set_var(name, "0");
        assert!(!env_flag(name));
        std::env::remove_var(name);
    }

    #[test]
    fn ir_cache_atomic_write_leaves_no_temp_files() {
        let dir = scratch_dir("atomic");
        let (def, refs) = fixture(&dir);
        let key = IrCacheKey::build(&def, &refs);
        let cache = IrCache::with_root(dir.join(".fp-cache")).unwrap();
        cache.write(&key, &vec![1i32, 2, 3]).unwrap();

        let entries: Vec<_> = std::fs::read_dir(cache.root())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries.len(), 1, "exactly one blob, no leftover temp");
        assert!(entries[0].ends_with(".fpir"));
    }
}
