# Fighters Paradise — Architecture Overview

## What Is This?

Fighters Paradise is a modern reimplementation of the [MUGEN](https://en.wikipedia.org/wiki/Mugen_(game_engine)) 2D fighting game engine, written in Rust. It provides full backward compatibility with existing MUGEN content files (.sff, .air, .cns, .cmd, .def, .snd).

## Crate Dependency Graph

```
fp-app (binary)
  ├── fp-engine         (game loop, round flow)
  │     ├── fp-character  (character struct, state machine)
  │     │     ├── fp-vm       (bytecode VM for expressions)
  │     │     ├── fp-combat   (HitDef, damage, guard)
  │     │     ├── fp-physics  (gravity, friction, AABB)
  │     │     └── fp-input    (command recognition)
  │     ├── fp-stage      (backgrounds, camera)
  │     ├── fp-ui         (lifebars, menus, motif)
  │     └── fp-audio      (SFX, BGM)
  ├── fp-render         (wgpu sprite renderer)
  ├── fp-formats        (all file parsers)
  └── fp-core           (shared types, math, errors)
```

## Core Architecture Decisions

### Fixed 60Hz Timestep
MUGEN runs at exactly 60 ticks/second. We match this for fighting game determinism.

### Struct-Based Entities (Not ECS)
MUGEN entities have fixed, well-known properties. The bytecode VM needs direct field access. Characters use fixed array slots; helpers/projectiles use `SlotMap`.

### Bytecode-Compiled Expression VM
MUGEN expressions are compiled at load time into stack-based bytecode, then executed at runtime. A Pratt parser handles all 14 operator precedence levels.

### GPU Palette Lookup
MUGEN sprites are 256-color indexed. The GPU shader performs palette lookup, enabling cheap palette swaps without texture re-upload.

### Never Crash on Bad Content
Parsers collect warnings and substitute safe defaults. Missing sprites become invisible, bad expressions evaluate to 0.

## Game Loop

```
┌─────────────────────────────────────────┐
│ Fixed 60Hz Timestep Loop                │
│                                         │
│  1. Poll input                          │
│  2. Update command buffers              │
│  3. Evaluate state machine (per char)   │
│     a. Common states (-3, -2, -1)       │
│     b. Current state controllers        │
│     c. Process state changes            │
│  4. Physics integration                 │
│  5. Collision detection                 │
│  6. Hit resolution                      │
│  7. Camera update                       │
│  8. Render                              │
│     a. Stage background                 │
│     b. Characters (sorted by Z)         │
│     c. UI (lifebars, timer)             │
│     d. Present frame                    │
└─────────────────────────────────────────┘
```

## File Format Summary

| Format | Extension | Type | Purpose |
|--------|-----------|------|---------|
| SFF | .sff | Binary | Sprite container (indexed-color images + palettes) |
| AIR | .air | Text | Animation definitions (frame sequences, collision boxes) |
| CNS | .cns | Text | Character state definitions (state controllers, triggers) |
| CMD | .cmd | Text | Input command definitions (special move sequences) |
| DEF | .def | Text | Configuration (character/stage/system metadata) |
| SND | .snd | Binary | Sound container (WAV samples) |
| FNT | .fnt | Binary | Font definitions |
