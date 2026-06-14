# Content Guide — Bring Your Own Character

Fighters Paradise is built around one idea: **bring your own characters.** It is a
clean-room reimplementation of the MUGEN 2D fighting engine in Rust, and it reads
the *same* `.def` / `.sff` / `.air` / `.cmd` / `.cns` / `.snd` files that MUGEN
characters ship with. The long-term vision is a fully customizable fighting-game
engine: drop in your favorite character bundle, point the app at it, and fight.

This guide is the practical, example-driven path to doing that **today** — what a
character bundle is made of, how each file maps to our parsers, exactly how to load
one with `cargo run`, what works versus what is still missing, and how to debug a
character that loads wrong.

> **Status (2026-06-14):** Fighters Paradise is a playable two-character fighter
> driven by real Kung Fu Man (KFM) data. It is **not** a stub. Some content does
> not yet render (SFF v1 art, stage backgrounds, real screenpacks) — those limits
> are called out plainly below and tracked in [Known Issues](known-issues.md).

See also: [Architecture](architecture.md) · [MUGEN Compatibility Matrix](mugen-compatibility.md) · [Roadmap](roadmap.md) · [Known Issues](known-issues.md) · [Development](development.md) · root [README](../README.md)

---

## 1. Anatomy of a MUGEN character bundle

A MUGEN character is a *folder of files* held together by a single `.def`. The
`.def` is the manifest: it names every other file and a few key constants. Here is
the real Kung Fu Man `.def` (`test-assets/kfm/kfm.def`), trimmed to the parts that
matter:

```ini
[Info]
name = "Kung Fu Man"
localcoord = 320,240        ; the coordinate space the sprites/positions assume

[Files]
cmd      = kfm.cmd          ; command set (motions -> command names)
cns      = kfm.cns          ; constants + state logic
st       = kfm.cns          ; states (KFM points st at the same file)
stcommon = common1.cns      ; shared common states (filled in last)
sprite   = kfm.sff          ; sprite archive
anim     = kfm.air          ; animations
sound    = kfm.snd          ; sound archive
ai       = kfm.ai           ; AI hints (not used by this engine)
```

### The six core formats and how they map to our parsers

Every format below is parsed by the [`fp-formats`](../crates/fp-formats) crate.
All six core formats parse **real KFM content end to end** (142 parser tests).
The text formats share one discipline: case-insensitive keys, BOM/CRLF tolerant,
`;` comments stripped, **never crash** — malformed lines are warn-logged and
skipped, not fatal.

| File   | What it holds                                    | Parser module                          | Status |
| ------ | ------------------------------------------------ | -------------------------------------- | ------ |
| `.def` | The manifest: `[Info]`, `[Files]`, sections      | `fp-formats/src/def.rs`                | Full — INI sections + `key=value`, quote stripping, `resolve_path` for `.def`-relative refs |
| `.sff` | Sprite archive (indexed-color images)            | `fp-formats/src/sff/`                  | **v2 full** (RLE8/RLE5/LZ5 + uncompressed); **v1 (PCX) decodes pixels but not its palette**; PNG-embedded sprites unsupported |
| `.air` | Animations: frame timing, flip/blend, Clsn boxes | `fp-formats/src/air.rs`                | Full for `group,image,x,y,ticks[,flip[,blend]]`, Loopstart, Clsn1/Clsn2; **scale/angle/Interpolate not parsed** |
| `.cmd` | Commands: motions (`~D, DF, F, x`) -> names      | `fp-formats/src/cmd.rs`                | Full — `[Command]` blocks + `[Defaults]`; the `[Statedef -1]` AI section is intentionally read by the CNS path instead |
| `.cns` | Constants (`[Data]`, `[Size]`, …) + state logic  | `fp-formats/src/cns.rs`                | Full — `[Statedef N]` headers + `[State N,…]` controllers, raw trigger expressions preserved |
| `.snd` | Sound archive (RIFF/WAVE payloads)               | `fp-formats/src/snd.rs`                | Full — directory walk, `(group,sample)` lookup; PCM decode happens in `fp-audio` |

How they fit together inside the engine:

- **`.sff` + `.air`** give you a character you can *see* move. The renderer
  ([`fp-render`](../crates/fp-render)) does a palette lookup in a WGSL shader and
  treats palette index 0 as transparent.
- **`.cmd`** turns joystick/keyboard motions into named commands. The 60-frame
  input ring buffer and the command matcher live in
  [`fp-input`](../crates/fp-input) and understand the MUGEN command symbols
  `~` (release), `/` (hold), `$` (direction-detect), `>` (strict-immediate),
  `+` (simultaneous).
- **`.cns`** is the brain. Every trigger and every controller parameter is
  **compiled to expression form at load** by [`fp-vm`](../crates/fp-vm) and
  executed once per 60Hz tick by [`fp-character`](../crates/fp-character)'s
  state-machine executor. A parse failure becomes a `const 0` fallback with a
  warning — never a panic.
- **`.snd`** is decoded on demand by [`fp-audio`](../crates/fp-audio) and played
  through a channel mixer with MUGEN cut-off semantics. With no audio device it
  falls back silently and never panics.

> **Naming note on `fp-vm`:** despite the crate name, it is a *tree-walk
> evaluator* over a parsed expression tree, not a bytecode/stack VM. The behavior
> (per-tick evaluation of compiled CNS expressions) is what matters here.

### How the loader assembles a character

When you point Fighters Paradise at a `.def`, `LoadedCharacter::load`
(`crates/fp-character/src/loader.rs:438`) does this in order:

1. Parse the `.def`; read `[Info] name` and `localcoord`.
2. **Require** `[Files] sprite` (`.sff`) and `[Files] anim` (`.air`) — missing
   either is a hard load error.
3. **Optionally** load `cmd` and `sound`. Missing/bad ones warn and are skipped;
   the character still loads.
4. Merge the CNS state files **first-wins** — the character's own
   `st`/`st0..st9`/`cns` first, then `stcommon` last (fill-missing only).
5. Merge the `.cmd` file's `[Statedef -1]` command→state bridge as a *supplement*
   (its controllers are appended, not dropped).
6. Append the engine's built-in ground locomotion (stand↔walk↔crouch↔jumpstart)
   and auto-land transitions, so **every** character gets basic movement even if,
   like stock KFM, it authors none.
7. Read constants from the first file with a `[Data]` group.

If the CNS/CMD files produce **no** state data at all, the load fails on purpose —
a character with zero states is broken, and the built-ins must not mask that.

---

## 2. Laying out your files

Keep a character in **one folder**, with the `.def` referencing the others by
relative name. File refs in `[Files]` are resolved relative to the `.def`'s own
directory (`DefFile::resolve_path`), so a self-contained folder "just works":

```
my-fighter/
├── my-fighter.def      ; the manifest you point the app at
├── my-fighter.sff      ; sprites      ([Files] sprite = my-fighter.sff)
├── my-fighter.air      ; animations   ([Files] anim   = my-fighter.air)
├── my-fighter.cmd      ; commands     ([Files] cmd    = my-fighter.cmd)
├── my-fighter.cns      ; constants + states
├── common1.cns         ; common states ([Files] stcommon = common1.cns)
└── my-fighter.snd      ; sounds       ([Files] sound  = my-fighter.snd)
```

Minimum to load: a `.def` whose `[Files]` names a valid `sprite` and `anim`, plus
at least one CNS file that defines states. The bundled `test-assets/kfm/` folder
is the canonical reference layout — copy its shape.

---

## 3. Loading a character today

Everything ultimately runs through plain **Cargo**. The repo also ships two
convenience wrappers around it — both untracked, local-only files: a **`Makefile`**
(`make build`/`run`/`run-kfm`/`run-sprite`/`test`/`test-fast`/`check`/`clippy`/`fmt`/
`fmt-check`/`doc`/`clean`, plus `make ci` = clippy + fmt-check + test) whose header
calls itself "the canonical dev-workstation interface," and **`scripts/fp.sh`**, a
process-control wrapper that can start/stop/restart/status the windowed game (it
launches `fp-app` detached and tracks its PID — something a Makefile target can't do
cleanly). Every `make` target is a thin shell over a `cargo` command, so you can run
any of the commands below with `cargo` directly if you prefer.

### Prerequisites

| Platform | One-time setup                                  |
| -------- | ----------------------------------------------- |
| macOS    | `brew install sdl2` — the `-L /opt/homebrew/lib` linker flag is applied automatically by `.cargo/config.toml` for Apple Silicon |
| Linux    | `sudo apt install libsdl2-dev`                  |

Build once to confirm your toolchain and SDL2 are wired up:

```sh
cargo build --workspace
```

### The CLI combos (verified against `select_mode`, `main.rs:972`)

The app picks its mode from the number and kind of arguments. **A `.def` launches
a real match; a bare `.sff`/`.air` opens a legacy viewer.**

| Command                                            | What it does                                          |
| -------------------------------------------------- | ----------------------------------------------------- |
| `cargo run -p fp-app`                              | Default: a two-KFM match from `test-assets/kfm/kfm.def`. Falls back to a checkerboard test pattern if KFM is absent. |
| `cargo run -p fp-app -- p1.def`                    | Two-player match, **same** character on both sides.   |
| `cargo run -p fp-app -- p1.def p2.def`             | Two-player match, **two different** characters.        |
| `cargo run -p fp-app -- char.sff char.air`         | **Legacy animation viewer** — plays the `.sff` sprites through the `.air` timeline. No states, no combat. |
| `cargo run -p fp-app -- char.sff`                  | **Legacy static viewer** — shows the first sprite.    |

What each combo *enables*:

- **`.def` (match modes)** are the real engine: full state machine, commands,
  combat, hitpause, throws, super meter, best-of-3 rounds. **This is how you play
  a character.** P1 is keyboard-driven; P2 is a stationary dummy in this milestone
  (no second-player input map and no AI yet).
- **`.sff` + `.air` (viewer)** is a quick visual sanity check for your sprites and
  animations *before* wiring up states — no `.cns`/`.cmd` needed.
- **`.sff` (static)** verifies the sprite archive decodes at all.

> **There is no `sff + air + cmd` mode.** A third non-`.def` argument is ignored —
> `select_mode` runs the 2-argument viewer and never reads a standalone `.cmd`.
> Commands only take effect when a character is loaded from its `.def`. (The header
> comment in `main.rs` showing a 3-file form is stale.)

### Example: load your own character

```sh
# Drop your bundle anywhere on disk, then point the app at its .def:
cargo run -p fp-app -- /path/to/my-fighter/my-fighter.def

# Or fight it against KFM:
cargo run -p fp-app -- /path/to/my-fighter/my-fighter.def test-assets/kfm/kfm.def
```

### Controls (Player 1)

| Action  | Keys                          | MUGEN button |
| ------- | ----------------------------- | ------------ |
| Move    | Arrow keys **or** `W A S D`   | up/down/left/right |
| Attack  | `U` / `I` / `O`               | a / b / c    |
| Attack  | `J` / `K` / `L`               | x / y / z    |
| Quit    | `Esc`                         | —            |

(Mapping read from `match_input_from_keyboard`, `crates/fp-app/src/main.rs:565`.)

---

## 4. What works vs. what doesn't

Fighters Paradise already runs a complete fight loop on real data; the gaps are
mostly in *presentation* (stages, screenpacks, fonts, some sprite formats) and in
a known set of less-common CNS controllers. For the authoritative,
controller-by-controller breakdown, see the
**[MUGEN Compatibility Matrix](mugen-compatibility.md)**.

### Works today (driven end to end by real KFM data)

- Loading a full character from its `.def` (sprites, animations, commands, states,
  sounds) and merging CNS/CMD in MUGEN order.
- A fixed-60Hz two-player `Match`: round/best-of-3 flow, KO and time-over,
  winner readout.
- Command recognition (`~ / $ > +`) → state transitions; `~30` quarter-circles
  etc. work.
- Combat: `HitDef` resolution, Clsn1×Clsn2 hit detection, knockback, hitpause,
  i-frames (`NotHitBy`/`HitBy`), damage multipliers (`AttackMulSet`/`DefenceMulSet`).
- KFM's signature **throw** (via `Target*` controllers), **supers** (power/meter
  carried across rounds), jump + air-jump + auto-land, hit reactions.
- `.snd` playback: `PlaySnd` and `HitDef` impact sounds.
- Animation: per-element timing, flip, and the three blend modes
  (normal/additive/subtractive).

### Current limits — call these out before you file a bug

| Limit | Effect on your character | Tracking |
| ----- | ------------------------ | -------- |
| **SFF v1 palettes not extracted** | WinMUGEN-era (`.sff` v1) sprites decode their pixels but have **no colors to look up**, so they render **invisible/transparent**. Modern SFF v2 characters (like KFM) are fine. | [#25](known-issues.md) |
| **PNG-embedded sprites unsupported** | SFF v2 sprites stored as PNG (Png8/24/32 — common in HD characters) fail to decode. RLE8/RLE5/LZ5/uncompressed all work. | [#35](known-issues.md) |
| **No stage backgrounds** | `fp-stage` is a stub; matches render over a flat clear color, not a `[BGDef]`/`[BG]` stage. | [#29](known-issues.md) |
| **HUD is not a real screenpack** | Life bars + KO/win marker are hand-drawn colored quads, not a `fight.def`/`fight.sff`. No power/meter bar drawn, no font text. | [#26](known-issues.md), [#30](known-issues.md), [#31](known-issues.md) |
| **No hit sparks** | The spark anchor is computed but no spark sprite is spawned/rendered. | [#17](known-issues.md) |
| **Some CNS controllers are no-ops** | `Width`, `AssertSpecial`, `SprPriority`, `Pause`/`SuperPause`, `AfterImage`/`PalFX`, and the get-hit velocity controllers are not yet dispatched — they log a debug message and do nothing. ~30 common controllers *are* implemented. | [#10/#13/#16/#23/#24/#33](known-issues.md) |
| **No helpers/projectiles/teams** | `Helper`, `Projectile`, and the `parent/helper/target/partner/playerid` redirects resolve to nothing yet. | [#39](known-issues.md) |
| **No RNG** | `random` returns a fixed `0`, so `Random`-driven behavior is deterministic-but-constant. | [#28](known-issues.md) |
| **Storyboards parse but don't play** | `intro.def`/`ending.def` are parsed into a typed model but not ticked or rendered. | [#32](known-issues.md) |
| **No FNT/ACT support** | Font and external-palette files have no parser. | [#30](known-issues.md) |

A character that uses an unimplemented controller still **loads and fights** — the
unsupported controller is simply skipped, so you get a degraded-but-playable
character rather than a crash.

---

## 5. Troubleshooting

The whole stack follows a **never-crash** discipline: bad input warns and is
skipped, missing assets degrade gracefully, and the app falls back to a
checkerboard test pattern rather than panicking. Run with logs visible to see
exactly what was skipped:

```sh
RUST_LOG=info cargo run -p fp-app -- /path/to/my-fighter.def
# more detail (per-line skip warnings, redirect/eval notes):
RUST_LOG=debug cargo run -p fp-app -- /path/to/my-fighter.def
```

### My character shows a checkerboard / test pattern

The match failed to *load* and the app degraded. Look for a
`match failed to load: …` warning. Common causes:

- The `.def` has no `[Files] sprite` or no `[Files] anim` (both are **required**).
- The referenced `.sff`/`.air` path doesn't resolve relative to the `.def`.
- The CNS/CMD files defined **no states** (`loaded no CNS states`).

### My character loads but is invisible

The sprites decoded but produced no visible pixels. Most likely:

- **SFF v1 character** — its inline palette isn't extracted yet, so every pixel
  maps to transparent ([#25](known-issues.md)). This is expected for WinMUGEN-era
  art; there is currently no workaround short of the palette fix.
- **PNG-embedded SFF v2 sprites** — unsupported decode ([#35](known-issues.md)).
- Remember palette **index 0 is always transparent** by MUGEN convention — sprites
  authored against index 0 as a visible color will show holes.

Sanity-check the sprites in isolation with the viewer:
`cargo run -p fp-app -- my-fighter.sff my-fighter.air`.

### Parse warnings in the log

Warnings like "skipping malformed line" or "unknown trigger" are **non-fatal by
design**. The parser keeps going; the offending line/controller is dropped. A
trigger or parameter that fails to compile becomes a `const 0` and the character
keeps running. If a move misbehaves, grep the logs for the controller/trigger name
to see whether it was skipped or compiled to `0`.

### My character loads but a move does nothing

- The move may rely on a **not-yet-implemented controller** (see the limits table
  and the [compatibility matrix](mugen-compatibility.md)). Unimplemented
  controllers debug-log and no-op.
- The move may need a `random`/RNG branch — `random` currently returns `0`
  ([#28](known-issues.md)), so RNG-gated branches never fire.
- Helper/projectile-based moves won't spawn anything yet ([#39](known-issues.md)).

### Sound doesn't play

Audio degrades silently to a null backend when no device is present, so a headless
or device-less run is normal. WAV formats rodio can't decode are rejected up front
(hardened decoder), and `PlaySnd` looping is read but currently plays once. Common
(`F`-prefixed) sounds fall back to the character's own `.snd` because no shared
common sound file is loaded yet.

---

## 6. Where the bundled character lives — and the clean-room rule

The bundled Kung Fu Man is what `cargo run -p fp-app` loads by default. It lives
behind a **gitignored symlink**:

```
fp-work/test-assets        ->  …/FightersParadise/test-assets   (local-only symlink)
fp-work/test-assets/kfm/   ->  kfm.def, kfm.sff, kfm.air, kfm.cmd,
                               kfm.cns, common1.cns, kfm.snd, intro/ending storyboards
```

The default character path is hardcoded as `test-assets/kfm/kfm.def`
(`DEFAULT_DEF`, `crates/fp-app/src/main.rs:71`). If that path is absent the app
shows the test pattern instead.

### Clean-room: do not commit copyrighted content

Fighters Paradise is a **clean-room** reimplementation. Two rules are
non-negotiable and enforced by `.gitignore`:

1. **No Elecbyte/MUGEN engine source or copyrighted assets** are shipped or
   tracked. `git ls-files` shows **zero** `.sff/.air/.cmd/.cns/.def/.snd` files;
   the only tracked binary is the project's own `assets/banner.png`.
2. **Kung Fu Man is content under CC BY-NC 3.0** (© Elecbyte), used **locally for
   testing only**. It is reached through the gitignored `test-assets` symlink and
   is **never** committed or distributed with this engine.

When you bring your own characters: **keep them out of the repo.** Put them
anywhere on disk and pass the absolute path on the command line. Do not add
character bundles to version control unless you own the rights and intend to ship
them — and even then, not into this engine's tree.

Fighters Paradise is an independent project. MUGEN is a trademark of Elecbyte.
This project does not include any Elecbyte code or assets. The engine itself is
licensed under [MIT](../LICENSE).

---

## 7. The vision — full customization

The headline goal is simple: **bring your own characters, in real MUGEN format,
and have them just work.** Today that means SFF v2 characters with implemented
controllers run as a full fight; the roadmap fills in the rest — SFF v1 palettes
and PNG sprites so *any* era of character renders, stages and real screenpacks so
fights look complete, fonts/text, helpers/projectiles/teams, and the remaining
controllers. Track that progress in the **[Roadmap](roadmap.md)** and
**[Known Issues](known-issues.md)**, and check the
**[Compatibility Matrix](mugen-compatibility.md)** before porting a specific
character.
