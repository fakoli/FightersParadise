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

> **Status (2026-06-15):** Fighters Paradise is a playable two-character fighter
> driven by real Kung Fu Man (KFM) data. **No crate is a stub anymore** — stages,
> screenpacks, and storyboards all render now, and SFF v1 + PNG sprites decode.
> The app also now **discovers content from directories** (a `chars/` roster, a
> `stages/` folder, and `data/<motif>/` screenpack sets) and supports
> **HUD/life-power-bar customization** and **key remapping** from an in-game
> options screen — all documented in §8 below. What's left is mostly *fidelity
> sub-features* on the newly-landed presentation paths (called out plainly below)
> and the forward-looking modes — all tracked in [Known Issues](known-issues.md).

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
All six core formats parse **real KFM content end to end** (182 parser tests).
The text formats share one discipline: case-insensitive keys, BOM/CRLF tolerant,
`;` comments stripped, **never crash** — malformed lines are warn-logged and
skipped, not fatal.

| File   | What it holds                                    | Parser module                          | Status |
| ------ | ------------------------------------------------ | -------------------------------------- | ------ |
| `.def` | The manifest: `[Info]`, `[Files]`, sections      | `fp-formats/src/def.rs`                | Full — INI sections + `key=value`, quote stripping, `resolve_path` for `.def`-relative refs |
| `.sff` | Sprite archive (indexed-color images)            | `fp-formats/src/sff/`                  | **v2 full** (RLE8/RLE5/LZ5 + uncompressed **+ PNG8/24/32 decode**); **v1 (PCX) now decodes pixels *and* its trailing inline palette** — both eras render in color |
| `.air` | Animations: frame timing, flip/blend, Clsn boxes | `fp-formats/src/air.rs`                | Full for `group,image,x,y,ticks[,flip[,blend]]`, Loopstart, Clsn1/Clsn2; extended `scale`/`angle`/`Interpolate` now **parse** into the typed `Frame` (renderer doesn't apply them yet) |
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
| `cargo run -p fp-app`                              | **No args → the in-app Title menu** over the shipped clean-room motif (`assets/data/{system,select}.def`, a roster pointing at `assets/trainingdummy`). No KFM needed. |
| `cargo run -p fp-app -- <dir>/`                    | **Directory discovery** — scan a folder for a character roster (`chars/<name>/<name>.def` layout, or a flat dir of `*.def`) and launch the Title menu over the discovered roster. See §8. |
| `cargo run -p fp-app -- p1.def`                    | Direct two-player match, **same** character on both sides (skips the menu). |
| `cargo run -p fp-app -- p1.def p2.def`             | Direct two-player match, **two different** characters.        |
| `cargo run -p fp-app -- char.sff char.air`         | **Legacy animation viewer** — plays the `.sff` sprites through the `.air` timeline. No states, no combat. |
| `cargo run -p fp-app -- char.sff`                  | **Legacy static viewer** — shows the first sprite.    |
| `cargo run -p fp-app -- validate char.def`         | **Headless validator / linter** — loads the `.def` and prints actionable author problems (missing sprites/animations, unresolved state refs, expressions that failed to compile, no-op controllers). No window. |

Two **flags** layer on top of any of the launch modes above (both are parsed and
*stripped* before file routing, so they compose with every command):

| Flag                       | What it does                                                       |
| -------------------------- | ----------------------------------------------------------------- |
| `--motif <name\|path>`     | Pick a discovered/explicit screenpack motif (see §8.3). Resolves a discovered motif **name** under `assets/data/`, a `system.def` path, or a motif **directory** holding a `system.def`. An unresolvable value warns and falls back to the default motif — a typo never crashes the app. |
| `--simul`                  | Launch a **Simul** team match (both sides field all fighters at once). Default is the classic 1v1. See §8.4. |
| `--turns`                  | Launch a **Turns** team match (sequential KO hand-off). The last team flag wins if both `--simul` and `--turns` are given. |

What each combo *enables*:

- **No-arg / directory (menu modes)** boot the in-app **Title menu**
  (`cli_route` → `CliRoute::Menu` for no args, `CliRoute::Directory` for a folder;
  `crates/fp-app/src/main.rs`). The menu walks Title → character-select →
  stage-select → fight over the shipped clean-room motif's roster, augmented with
  any characters discovered from the directory argument (§8). The setup/options
  screen (key remap + HUD customization, §8.5) hangs off the Title menu.
- **`.def` (direct match modes)** are the real engine: full state machine,
  commands, combat, hitpause, throws, super meter, best-of-3 rounds. **This is the
  fast path to play a single character** — it bypasses the menu
  (`CliRoute::Direct`). P1 is keyboard-driven; P2 is the baseline CPU AI (or a
  second keyboard).
- **`.sff` + `.air` (viewer)** is a quick visual sanity check for your sprites and
  animations *before* wiring up states — no `.cns`/`.cmd` needed.
- **`.sff` (static)** verifies the sprite archive decodes at all.
- **`validate` (linter)** is the fastest way to find why a character loads wrong —
  run it before filing a bug; it surfaces exactly what the loader skipped or
  could not resolve.

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
- **Sprites of any era render in color:** SFF v1 inline palettes and SFF v2
  PNG8/24/32 sprites both decode now (so WinMUGEN-era and modern HD art show).
- **Presentation layer:** a parallax **stage** (from a stage `.def`), a real
  **`fight.def` screenpack** (life + power bars, names, round announcer, timer as
  glyphs, with a quad-HUD fallback), a drawn **power/meter bar**, and
  **intro/ending storyboard** playback.
- **~40 state controllers** dispatched, including `AssertSpecial`, `Width`,
  `SprPriority`, `Pause`/`SuperPause`, `PalFX`/`AfterImage`, `HitOverride`, and
  the get-hit-velocity set; plus priority/trade **clash** resolution and a
  **real `random`** trigger (`[0, 999]`).

### Current limits — call these out before you file a bug

| Limit | Effect on your character | Tracking |
| ----- | ------------------------ | -------- |
| **Stage tiling / animation / vertical camera — *Partial*** | The parallax stage renders, but `tile`/`velocity`/`mask`/`type=anim` layers and the camera's vertical-follow are *parsed but not yet drawn* — the camera only scrolls horizontally. | [#29](known-issues.md) |
| **Screenpack combo/face + layered bars — *Partial*** | The `fight.def` screenpack renders life/power bars, names, round announcer, and timer, but `[Combo]`/`[Face]` are parsed-not-drawn and only one `bg0` layer per bar renders. | [#31](known-issues.md) |
| **FNT text on the legacy quad HUD — *Partial*** | The FNT v1 parser + `draw_text` glyph path feed the *screenpack*; the fallback quad HUD (used when no `fight.def`) still shows only a solid-color KO/win marker, no glyph text. FNT is also asset-blocked (no real `.fnt` fixture ships). | [#30](known-issues.md) |
| **Hit sparks — infrastructure only — *Partial*** | The effect-entity path exists (own-spark spawn/tick/expire), but KFM (and conventional content) authors *common*-`fightfx` sparks and **no `fightfx.sff` loader exists yet**, so KFM shows **no visible spark**. The hit still lands. | [#17](known-issues.md) |
| **AfterImage trail is approximate — *Partial*** | `PalFX`/`AfterImage` dispatch and the color-tint render uniform work, but the trail is a motion-smear approximation (no true frame-history ghost ring); `sinadd`/`TimeGap`/`Trans` etc. aren't modeled. | [#33](known-issues.md) |
| **Storyboard fade/clearcolor/BGM — *Partial*** | `intro.def`/`ending.def` now play (a `StoryboardPlayer` ticks scenes; `fp-app` overlays them), but per-scene fadein/fadeout, `clearcolor`, and BGM are not yet applied. | [#32](known-issues.md) |
| **`.act` palette + extended AIR are parser-only — *Partial*** | `.act` external palettes and AIR `scale`/`angle`/`Interpolate` parse into the typed model but are **not yet consumed at runtime** (no `.act` palette swaps, no per-frame scale/rotation). | [#39](known-issues.md) |
| **A few CNS controllers are still no-ops — *Missing*** | `LifeAdd`, `Helper`, `Projectile`, `Explod`, `EnvShake` are still not dispatched — they debug-log and do nothing. (~40 controllers *are* dispatched now, including the formerly-missing `Width`/`AssertSpecial`/`SprPriority`/`Pause`/`SuperPause`/`PalFX`/`AfterImage`/`HitOverride`/get-hit-vel set.) | [#39](known-issues.md) |
| **No helpers / projectiles / teams — *Missing*** | `Helper`, `Projectile`, and the `parent/helper/target/partner/playerid` redirects resolve to nothing; team/turns/tag *modes* are forward-looking. | [#39](known-issues.md) |
| **No replay / save-state — *Missing*** | No state serialization, replay capture, or rollback yet (the deterministic-design groundwork and in-state RNG are in place). | [#38](known-issues.md) |

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

The sprites decoded but produced no visible pixels. SFF v1 inline palettes and
SFF v2 PNG sprites both decode now, so the most common cause is the palette
convention rather than a missing decoder:

- Remember palette **index 0 is always transparent** by MUGEN convention — sprites
  authored against index 0 as a visible color will show holes (or render
  fully transparent if index 0 is their only color).
- A truly empty/zero-size sprite, or one that linked to a missing data sprite,
  renders as nothing.

Sanity-check the sprites in isolation with the viewer:
`cargo run -p fp-app -- my-fighter.sff my-fighter.air`. If they show there, the
issue is in the character's states/positions, not the sprite decode.

### Parse warnings in the log

Warnings like "skipping malformed line" or "unknown trigger" are **non-fatal by
design**. The parser keeps going; the offending line/controller is dropped. A
trigger or parameter that fails to compile becomes a `const 0` and the character
keeps running. If a move misbehaves, grep the logs for the controller/trigger name
to see whether it was skipped or compiled to `0`.

### My character loads but a move does nothing

- The move may rely on a **still-unimplemented controller** (`LifeAdd`, `Helper`,
  `Projectile`, `Explod`, `EnvShake` — see the limits table and the
  [compatibility matrix](mugen-compatibility.md)). Unimplemented controllers
  debug-log and no-op. (~40 controllers *are* dispatched now, so most moves work.)
- Helper/projectile-based moves won't spawn anything yet ([#39](known-issues.md)).
- The `random` trigger now returns a real value in `[0, 999]`
  ([#28](known-issues.md)), so RNG-gated branches *do* fire — if one misbehaves,
  it's a logic issue, not the old fixed-`0` stub.

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
and have them just work.** Today that's largely true: characters of *any* era
render in color (SFF v1 palettes + PNG sprites both decode), ~40 controllers run,
and fights show with a parallax stage, a `fight.def` screenpack, glyph text, a
power bar, and intro/ending storyboards. What the roadmap still fills in is the
*forward-looking* scope — helpers/projectiles, team/turns/tag modes,
`.act`/extended-AIR *runtime* consumption, replay/determinism, and the
render-fidelity polish on the newly-landed presentation paths (stage tiling,
screenpack combo/face, true afterimage ghosts). Track that progress in the
**[Roadmap](roadmap.md)** and **[Known Issues](known-issues.md)**, and check the
**[Compatibility Matrix](mugen-compatibility.md)** before porting a specific
character.

---

## 8. The 1.0 content model — directory discovery, motifs, teams & HUD

Beyond pointing the app at a single `.def`, Fighters Paradise now reads a whole
**content tree** the way MUGEN organizes one — by directory convention. Point the
app at a folder and it auto-discovers a character roster; drop in a `stages/`
folder and stages appear in the stage-select; drop in `data/<motif>/` folders and
each becomes a selectable screenpack. None of this needs a single index file, and
every scan is **never-crash**: a missing or malformed directory yields an empty
list with a `tracing::warn!`, not a panic.

> All examples below reference only the **shipped clean-room content**
> (`assets/data/`, `assets/trainingdummy/`, `assets/stages/dojo/`). Bring-your-own
> bundles (KFM, third-party characters, …) live anywhere on disk and are passed by
> path — never committed into this tree (see §6).

### 8.1 The character roster — `chars/<name>/<name>.def`

Pass a **directory** instead of a `.def` and the app scans it for characters via
`fp_ui::discovery::discover_chars` (`crates/fp-ui/src/discovery.rs`), then boots
the Title menu over the discovered roster (`CliRoute::Directory`,
`crates/fp-app/src/main.rs`):

```sh
cargo run -p fp-app -- /path/to/content/chars/
```

Two layouts are recognized (they may coexist in one folder):

1. **Nested (the MUGEN-standard layout).** Each immediate subfolder `<name>/` that
   contains a same-named `<name>.def` is one character:

   ```
   chars/
   ├── trainingdummy/
   │   └── trainingdummy.def      ; discovered as "trainingdummy"
   └── my-fighter/
       └── my-fighter.def         ; discovered as "my-fighter"
   ```

2. **Flat.** Any loose `<dir>/<file>.def` is a character whose name is the file
   stem — a convenience for a scratch folder of bare `.def`s.

Discovery is deterministic: results are sorted **case-insensitively by name**
(then by path) so the roster order is stable regardless of the filesystem's
listing order, and duplicates are collapsed. A subfolder with no matching `.def`
is **skipped** (`tracing::debug!`), never fatal — an asset-less folder is normal,
not an error. The directory argument is absolutized up-front so the discovered
`.def` paths resolve correctly when the menu loads a pick.

### 8.2 Stages — a sibling `stages/` directory

Stages are discovered by `fp_stage::discover_stages` (`crates/fp-stage/src/lib.rs`)
scanning a `stages/` folder (in the app, the directory sibling to the motif's
`select.def`). Every `*.def` directly under it is read and accepted **only if it
actually looks like a stage** — i.e. it carries at least one stage-defining
section (`[BGdef]`, `[Camera]`, `[StageInfo]`, or `[PlayerInfo]`). A stray
character `.def` or other unrelated `.def` in the folder is warn-and-skipped, so
the stage-select never offers a non-stage by mistake:

```
content/
├── chars/…
└── stages/
    ├── dojo.def                   ; a real stage -> offered in stage-select
    └── arena.def
```

Each discovered stage's display name is its `[Info] name`, falling back to the
`.def` file stem when the stage authors no name. The shipped clean-room backdrop
(`assets/stages/dojo/bg.png`, drawn as a full-window image when no `[BGdef]`
stage is loaded) is always available as a fallback entry.

### 8.3 Motif / screenpack sets — `data/<motif>/` + the `--motif` flag

A **motif** (screenpack set) is a folder under a `data/` directory holding a
`system.def`. They are discovered by `fp_ui::discovery::discover_motifs`
(`crates/fp-ui/src/discovery.rs`), which scans the motif directory
(`DEFAULT_MOTIF_DIR = "assets/data"`) and returns each subfolder that contains a
`system.def`:

```
data/
├── default/
│   └── system.def                ; the shipped clean-room motif (= assets/data/)
└── neon/
    └── system.def
```

Pick one with the **`--motif <name|path>`** flag, parsed by `parse_motif_flag` and
resolved by `resolve_motif_system_def` (`crates/fp-app/src/main.rs`). The selector
is resolved in three forms, in order:

1. a **direct `.def` path** that exists (used verbatim — e.g. `--motif
   assets/data/system.def`);
2. a **directory path** holding a `system.def` (resolves to `<dir>/system.def`);
3. a **discovered motif name** matched case-insensitively against the motifs found
   under `assets/data/` (e.g. `--motif neon`).

An unresolvable selector (a typo, a missing folder, a dangling `--motif` with no
value) drops with a `tracing::warn!` and the app loads the **default motif**
(`assets/data/system.def`) — so a bad `--motif` never crashes the app. The motif's
`select.def` (its `[Files] select`, resolved relative to `system.def`) supplies
the roster; if it's missing the shipped `assets/data/select.def` is used as the
fallback roster.

### 8.4 Team modes — `--simul` / `--turns`

The direct-CLI match path runs a `TeamMatch` whose mode comes from
`parse_team_flag` (`crates/fp-app/src/main.rs`):

| Flag        | `TeamMode` | Meaning                                          |
| ----------- | ---------- | ------------------------------------------------ |
| *(none)*    | `Single`   | The classic 1v1 — byte-identical to before the flag existed. |
| `--simul`   | `Simul`    | Both sides field all their fighters at once.     |
| `--turns`   | `Turns`    | Sequential KO hand-off (one fighter at a time per side). |

Both flags take **no value** and are stripped before file routing; an unknown
`--…` token passes through untouched. If both are given, the **last one wins**.

```sh
# A simul team match of the shipped trainingdummy on both sides:
cargo run -p fp-app -- --simul assets/trainingdummy/trainingdummy.def
```

### 8.5 HUD customization & key remapping (the options screen)

From the Title menu, **`SETUP` / `OPTIONS`** opens the setup screen
(`screens::SetupScreen`, `crates/fp-app/src/screens.rs`), which edits the live
input/HUD config in place — a change takes effect immediately, no restart.

**Key remapping (T042).** The setup screen lists the input-device preference
(Keyboard / Controller) and one row per remappable action. The remappable actions
are the four absolute screen directions (`UP DOWN LEFT RIGHT`) and the six MUGEN
attack buttons (`A B C` punches, `X Y Z` kicks) — `screens::InputAction`. Confirm
on an action row enters *capture* mode ("PRESS A KEY"); the next key you press is
bound to that action. The default keyboard map (`crates/fp-app/src/main.rs`) is:

| Action            | Default key       | MUGEN button |
| ----------------- | ----------------- | ------------ |
| Up / Down / Left / Right | `W` / `S` / `A` / `D` | directions |
| A / B / C         | `U` / `I` / `O`   | a / b / c (punches) |
| X / Y / Z         | `J` / `K` / `L`   | x / y / z (kicks)   |

(The arrow keys remain wired alongside `WASD` for movement; remapping rebinds the
configurable action keys.)

**HUD customization (T046).** A `HUD CUSTOMIZATION` row on the setup screen opens
the HUD-customization screen (`screens::HudCustomizeScreen`), which edits an
`fp_ui::HudConfig` (`crates/fp-ui/src/hud_config.rs`) layered **on top of** the
screenpack-authored layout. The override model is deliberately *additive* — its
default is a guaranteed no-op (the HUD draws exactly as the `fight.def` authored
it), and the renderer (`crates/fp-ui/src/renderer.rs`) reads it each frame. The
options exposed are:

- **Life-bar color** and **power-bar color** — cycle each bar's front-fill tint
  through a preset palette: `WHITE` (the neutral no-op) → `RED` → `GREEN` →
  `BLUE` → `YELLOW` → `CYAN`, wrapping. The tint is applied as a `PalFX`-style
  per-channel multiply, so `WHITE` leaves the bar exactly as drawn.
- **Per-element visibility** — toggle each of `LIFE`, `POWER`, `NAME`, `TIMER`,
  and `COMBO` on/off independently (`fp_ui::HudElement`). Hidden elements simply
  aren't drawn.

Two further knobs exist in the `HudConfig` **model** and are honored by the
renderer but are **not yet surfaced** on the in-game screen (they are intended for
a future config-file loader): a global `(dx, dy)` **position offset** that nudges
every HUD element's anchor, and a global **scale** multiplier (clamped to
`0.1..=4.0`) applied to the HUD bars. A bad value can never collapse or explode
the HUD — out-of-range colors are clamped into gamut and a non-finite scale falls
back to `1.0`.
