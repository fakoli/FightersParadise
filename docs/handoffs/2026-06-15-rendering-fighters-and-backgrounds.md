# Session handoff — fighters + backgrounds now render on screen (2026-06-15)

> Newest handoff = current pickup point. Supersedes
> [2026-06-15-orphan-recovery-and-evilken-debug.md](2026-06-15-orphan-recovery-and-evilken-debug.md).

## Repo state (verified)

- **`main` tip: `d8177c0`.** No open PRs; no leftover worktrees/branches.
- `cargo test --workspace` + `cargo clippy --workspace --all-targets -D warnings` green with
  `test-assets/` linked (~2210+ tests).
- This session merged **#40** and **#41** (plus this handoff).

## The headline: the app actually renders a match now

A live run (this session enabled compute use) revealed the match was rendering **nothing but a
single HUD digit** — no fighters, no life bars. Root-caused and fixed two layers:

### #40 — `fix(render)`: sprite vertex-buffer bump-allocation (the keystone)
The sprite `vertex_buffer` held exactly **one quad** and was overwritten on every `draw_sprite`,
but each draw is its **own render pass**. Because all `queue.write_buffer` calls in a frame are
applied *before any* command-buffer pass executes at submit, every per-quad pass read the **last**
quad written — so only the final `draw_sprite` (a HUD glyph) survived and every fighter / life-bar /
earlier glyph vanished. (The debug-box path already worked around this; the sprite path didn't.)
Fix: enlarge the buffer to `MAX_SPRITE_QUADS`, bump-allocate per quad, draw via `base_vertex`.
→ **evilken renders in full color.** This is a GPU-output bug **CPU tests can't see** (fp-render has
no headless readback), which is why it went unnoticed — the verification was a screenshot.

### #41 — `feat(render,app)`: full-color stage backgrounds + clean-room dojo
The renderer was palette-indexed only. Added an RGBA image path: `fp_render::ImageTexture` +
`shaders/image.wgsl` + `pipeline_image` + `RenderFrame::draw_image` (reuses the bump-allocated
vertex buffer). `fp-app` decodes a PNG (`png` crate, `EXPAND`→8-bit RGBA), holds
`MatchRun.background`, draws it first when no MUGEN `[BGdef]` stage is loaded. Asset:
**original nanobanana-generated** `assets/stages/dojo/bg.png` (960×720, MIT, clean-room — CLAUDE.md
note added). A real MUGEN stage still takes precedence; absent asset → flat clear color (no regression).

Each PR went through the full pipeline: isolated worktree → local `clippy + test --workspace` →
independent adversarial AI review (`fakoli-crew:critic`, both PASS) → green CI → squash-merge.
Both reviewers' SHOULD-FIX items were folded in (overflow/16-bit guards, `ImageTexture::new`
length assertion, etc.).

## ⚠️ OPEN FOLLOW-UPS (investigated, NOT done — precise root causes)

### KFM invisible — SFF **v2** palette-PARSING bug (the next real task)
`kfm.sff` is SFF **v2** (evilken is v1). Investigated with probes + a live experiment:
- Sprite (0,0) is `Lz5`, decodes to valid indices 0-31 — **shapes render fine** (the pipeline works
  for v2; an experiment routing v2 → the act palette rendered KFM as correct-shape **black
  silhouettes**).
- The palette table has 7 entries: **PAL#0-5** (the 32-color per-sprite palettes, LData offsets
  0/128/.../640) all read **identical black garbage** `[0,0,0,124,...]`; **PAL#6** (256-color
  group-9000 *act* palette, offset 768) reads real `[255,255,255,...]`.
- So the bug is that the small per-sprite palettes' LData `data_offset` points at a wrong/degenerate
  region — the real per-sprite palette color data is **not** at `ldata[0..768]`.
- **FIX direction:** hex-diff `kfm.sff`'s LData layout against the SFF v2 spec to find the correct
  palette-data base + the sprite→palette index/color mapping, then make `SffFile::palette()`
  version-aware (v2 = RGBA, `num_colors`-sized). DO NOT ship the act-palette override — it
  mis-renders every v2 character (wrong colors). Substantial dedicated task, not a quick decode tweak.
- Where it bites: `fp-app` `get_or_create_sprite` calls `decode_sprite` + `palette(pal_idx)`;
  `palette()` (`crates/fp-formats/src/sff/mod.rs:~443`) always uses `rgb_to_rgba` (768 RGB).

### Per-sprite PalFX tint (deferred from #40)
`palfx_buffer` is still a single buffer written per draw → all passes read the last draw's palfx
(identity in the common case, so fighters render fine, but super-flash / AfterImage tints don't
apply). Bump-allocate it like the vertex buffer (uniform dynamic-offset, 256B align) when wiring
per-sprite tints.

## How to SEE / drive the live app (compute-use gotchas)

- The SDL2/wgpu window is a **cargo binary**, so it **can't be added to the computer-use allowlist**
  (`request_access` → `not_installed`) and computer-use screenshots filter it out. **Use macOS
  `screencapture`** instead: `osascript -e 'tell application "System Events" to tell process "fp-app"
  to get {position, size} of window 1'` for geometry, then `screencapture -x -R x,y,w,h /tmp/s.png`
  and Read the PNG. (Needs Screen Recording granted to Claude — survives a relaunch.)
- `cargo run -p fp-app -- <char.def>` boots a **direct match** (no-arg → the Title menu instead).
- **nanobanana** (for background/asset gen): plugin venv at
  `~/.claude/plugins/marketplaces/fakoli-plugins/plugins/nano-banana-pro/.venv/bin/python`, script
  `.../skills/generate/scripts/nanobanana.py`; subcommand is **`gen`** (not `generate`);
  `--out x.png --aspect 4:3 --size 2K`; GEMINI_API_KEY is in env; it writes JPEG-in-`.png`, so
  `sips -s format png ...` to get a real PNG the `png` crate can decode.

## Infra (unchanged)
env -u GITHUB_TOKEN on ALL gh/git-network; sibling worktrees need dangerouslyDisableSandbox for
cargo/git; CoW-clone target + symlink test-assets per worktree; merge gate = local clippy+test
--workspace + AI critic PASS + CI green, squash-merge (`--delete-branch` warns while a worktree
holds the branch — harmless); NEVER add a field to LoadedCharacter/CompiledState/AnimFrame
(literal-construction in multiple crates' tests → derive a method instead).
