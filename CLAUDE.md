# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`twister-splitter` takes a single DXF (typically an Illustrator "Export As DXF"
of a layered/stacked laser-cut model) whose objects all overlap at the origin,
splits it into individual cuttable **pieces**, and bin-packs those pieces onto
fixed-size sheets (default **400×400 mm**) so each sheet fits a laser bed. It
writes one output DXF per sheet.

Run it:

```
# Desktop GUI (bare launch, no args) — `gui` is a default feature:
cargo run
# CLI (any args switch to the command-line pipeline):
cargo run -- <input.dxf> [--out-dir out] [--size 400x400] [--kerf 2] \
            [--margin 0] [--kerf-comp 0] \
            [--packer nest|rect] [--no-rotate] [--sources both|layer|block]
```

`main` dispatches at runtime: **no CLI args → the GUI window**; **any args → the
CLI**. So `cargo run` opens the app while `cargo run -- fixtures/…` (and the test
suite) stay GUI-free. `cargo run --bin twister-gui` launches the GUI explicitly
(no `--features gui` needed now). Build the lean, CLI-only binary with
`cargo build --no-default-features` — there the GUI dispatch is compiled out, so a
bare invocation just prints CLI usage.

By default it uses **shape-aware nesting** (`--packer nest`): it flattens each
piece to its true concave outline and nests with free rotation via the **real
`sparrow` optimizer** (a SOTA irregular-packing heuristic on `jagua-rs`), which
packs each sheet densely. `--time <secs>` sets the optimizer budget per sheet
(default 12; higher = tighter). `--packer rect` selects the axis-aligned MaxRects
packer (instant, looser). On the Gengar fixture: nest fills sheet 0 to ~75%
(vs ~34% for the older greedy) across 3 sheets; rect = 4 sheets.

## Commands

- Build: `cargo build` (release: `cargo build --release`)
- Run on the fixture: `cargo run -- fixtures/gengar-stacked.dxf`
- Test all: `cargo test`
- Run a single test: `cargo test <name>` (e.g. `cargo test piece_only_fits_when_rotated`)
- Lint (must stay clean): `cargo clippy --all-targets`
- Format: `cargo fmt` (see the 2-space note below before relying on it)
- `TS_NEST_JSON=path` dumps the piece polygons in sparrow/jagua strip-packing
  JSON (for debugging or feeding the standalone sparrow binary), then exits.
- Nesting shows an `indicatif` spinner while sparrow optimizes each sheet, gated
  on `stderr().is_terminal()` so it's hidden when piped. The library reports
  progress through an `FnMut` callback, staying UI-free.

## Pipeline / architecture

The flow is `parse → extract → pack/nest → emit`, wired together in `src/main.rs`
(thin CLI) over the library crate (`src/lib.rs`). The interesting design lives
in these modules:

- **`extract.rs`** — turns a `dxf::Drawing` into `Vec<Piece>`. A loose piece is a
  **connected part** (`group_parts`): outline entities grouped by ring
  containment (even/odd depth — an outer contour plus its holes = one part;
  disjoint shapes = separate parts), so a layer holding several shapes splits
  into individually-nestable parts and zero-area degenerate artifacts are
  dropped. A block **INSERT** is one piece. Both sources by default
  (`Sources::Both`). Bboxes from spline control points (safe over-estimate).
- **`flatten.rs`** — turns a piece's entities into ONE simple polygon for
  nesting. Splines are evaluated on the actual curve via **De Boor** (clamped,
  rational-capable), not the control hull (which self-intersects). Key
  invariant: the returned polygon must **contain all of the piece's geometry** —
  emit transforms every entity rigidly, so anything outside the reserved polygon
  would spill off its slot and overlap neighbours. It keeps the concave outer
  ring only when that ring encloses every other ring's vertices; otherwise it
  falls back to the convex hull of all vertices.
- **`optimize.rs`** — the DEFAULT nester: drives the real **`sparrow`** optimizer
  (pinned git dep; unifies to one `jagua-rs 0.7.2`, callable via `BasicTerminator`
  + `DummySolListener`). sparrow does *strip* packing, so `nest_sparrow` **peels**
  sheets: strip-pack the remaining pieces (strip height = sheet height), keep
  pieces landing fully within the first sheet width, re-pack the rest (each round
  realigns to x=0, so nothing crosses a sheet edge). Reads back each placement
  via `int_to_ext_transformation` → `geom::Affine`; pieces too big for a sheet at
  any rotation are returned `oversized`. Trade-off: peeling can't reach the
  ~2-sheet true-bin optimum (strip→bin gap), but packs each sheet densely.
- **`nest.rs`** — `build_items` (flatten each piece to a nesting polygon, used by
  both nesters) + `valid_ring_or_hull` (gate polygons through jagua's
  `SPolygon::new`, hull-repair on rejection). Also `nest::nest`, a self-contained
  greedy bottom-left-fill nester over jagua's CDE — kept as a tested fallback
  (not the default path).
- **`pack.rs`** — the alternative MaxRects (best-short-side-fit) bin packer:
  bounding-box packing with 90° rotation and a kerf gap. Oversized pieces get
  their own sheet (free-list cleared so nothing nests on top). `Placement::to_placed`
  converts its output to the shared `emit::Placed`.
- **`emit.rs`** — one output `Drawing` per sheet. Both packers produce a common
  `Placed { piece_index, sheet, transform: Affine, oversized }`; emit applies the
  transform to loose entities in place, and for INSERTs sets `rotation =
  xf.rotation()` and `location = xf(base_point)` (so a block vertex `v` renders
  at `xf(v)`), copying each block definition once per sheet.
- **`geom.rs`** — `Bbox` and a rotation+translation-only `Affine` (no scale, so
  splines/blocks stay geometrically faithful); `Affine::rotation()` recovers the
  angle for INSERT placement.

## Non-obvious constraints (learned from the fixture — read before changing I/O)

- **The `dxf` crate (v0.6) has no HATCH entity type.** It silently drops all
  HATCH entities on load. In the fixture these are all `SOLID` fills that just
  duplicate the spline outlines, so dropping them is intentional and correct for
  laser cutting — but do not assume "load then save" preserves a file.
- **`Drawing::new()` defaults to ACAD R12, whose writer silently drops SPLINE**
  (an R13+ entity). `emit.rs` sets `out.header.version = source.header.version`
  to avoid losing every outline. If splines ever vanish from output, check this
  first.
- **Illustrator stacked exports overlap at the origin**, so spatial/connected
  clustering collapses the whole design into one object. Pieces are therefore
  defined by layer or block membership, never by geometry proximity.
- Units are implicit (Illustrator leaves `$INSUNITS` unset); `--size`/`--kerf`
  are in whatever units the file uses. For these exports that is millimetres.

## Tests

- `tests/packing.rs` — rectangle-packer invariants on synthetic inputs (no
  overlap at full `f64` precision, in-bounds, rotation-only-when-needed,
  oversized isolation).
- `tests/pipeline.rs` — rectangle path end-to-end over the fixture: 63 pieces,
  `Block_0` flagged oversized (~405 mm tall), no overlaps, and a save→reload
  round-trip preserving 141 splines / 6 inserts / 1 polyline and 0 hatches.
- `tests/flatten_fixture.rs` — every fixture piece flattens to a *simple*
  (non-self-intersecting) polygon (the reason for De Boor over control points).
- `tests/nest_pipeline.rs` — nesting path end-to-end: all 63 pieces placed once,
  ≤ 4 sheets, every non-oversized sheet's rendered geometry within bounds (the
  containment guarantee), geometry preserved. Slow (~27 s debug; ~1 s release).

## Cross-compiling for Windows

The tool also runs on Windows 10. A Docker build cross-compiles a **self-contained
x86_64 .exe** (no DLLs to ship):

```
./build-windows.sh          # -> ./dist/twister-splitter.exe
```

- The Windows build is **CLI-only**: `Dockerfile.windows` builds with
  `cargo build --release --no-default-features --bin twister-splitter …`, which
  drops the default `gui` feature (eframe/winit/glow). Without this the GUI stack
  would pull MinGW C deps into the exe and break the self-contained guarantee (and
  likely fail to link). The exe is the same lean CLI it has always been.
- Target is `x86_64-pc-windows-gnu` (MinGW); works because the CLI crate has no C
  dependencies. `.cargo/config.toml` sets the linker and `-C link-arg=-static`
  to statically link the MinGW runtime, so the exe needs no `libgcc`/
  `libwinpthread`/`libstdc++` DLLs on the target machine.
- `Dockerfile.windows` asserts self-containedness at build time: it dumps the
  exe's DLL imports and **fails the build** if any MinGW runtime DLL leaked in.
- The `.cargo/config.toml` settings are scoped to the Windows target triple, so
  native `cargo build`/`cargo test` on macOS/Linux are unaffected.

## Notes

- Rust **edition 2024** (needs Rust 1.85+). `pack.rs`/`emit.rs` use let-chains.
- `.editorconfig` asks for **2-space** indent, but `rustfmt` defaults to 4 and
  there is no `rustfmt.toml`, so `cargo fmt` will reformat to 4 spaces. Add
  `rustfmt.toml` with `tab_spaces = 2` before relying on `cargo fmt`.
