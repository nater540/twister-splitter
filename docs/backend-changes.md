# Backend changes the GUI needs

Audience: the `nesting-backend` agent (owner of `extract` / `flatten` / `nest` /
`optimize` / `pack` / `emit` / `geom`).

Author: the `egui-designer` agent, while wiring the desktop UI (`src/gui.rs`,
binary `twister-gui`, behind the `gui` feature).

The GUI is built and wired to the current library API, so **none of these are
hard blockers** — the app runs today by duplicating a little CLI logic and
recomputing derived data on the UI thread. Each item below removes a workaround,
a duplication, or a UX limitation. Priorities:

- **P0** — needed for a genuinely good first release (cancellation, live
  feedback, shared oversized logic, surfaced diagnostics).
- **P1** — polish the UI clearly wants (per-piece / per-sheet metadata).
- **P2** — stretch features (pin-and-re-nest, in-memory emit).

Where a workaround exists today it is called out so you can see exactly what the
UI is doing in the meantime.

## Implementation status (updated 2026-07-09)

**Everything in the P0/P1/P2 and S-1…S-8 sections below is now built** in the
library — the "why / suggested API" text is kept for the record, but the
workarounds it describes are gone. Concretely:

- **P0-1 cancellation** — `nest_sparrow`/`nest_sheets` take `cancel:
  Option<Arc<AtomicBool>>`; `CancelTerminator` (`src/optimize.rs`) stops mid-run
  and returns a partial `NestResult`. `nest::nest` checks the flag too.
- **P0-2 live events** — `NestEvent::{SheetCompleted, Progress}` callback
  (`src/optimize.rs`) replaces the bare `usize`; sheets stream as finalized.
- **P0-3 shared oversized logic** — `optimize::nest_sheets` owns the
  nest-then-append-oversized pipeline; CLI and GUI both call it.
- **P0-4 diagnostics** — `diag::{Diagnostic, Severity}`; the library returns them
  instead of `eprintln!`.
- **P1-5 piece metadata** — `Piece.{area, source: PieceSource, id}` all present.
- **P1-6 / S-1 per-sheet stats** — `stats::{SheetStats, sheet_stats,
  all_sheet_stats}` with yield, `cut_len_mm`, `pierces`, `est_run_secs`.
- **P1-7 hull fallback** — `NestItem.hull_fallback: bool`.
- **P1-8 Clone** — `Placed`, `NestItem`, `NestOutcome`, `SheetStats` all derive it.
- **P2-9 pin-and-re-nest** — `optimize::nest_sheets_pinned` + `nest::nest_pinned`.
- **P2-10 in-memory emit** — `emit::build_sheet_drawings`; `emit::emit` wraps it.
- **S-2 spacing/margin/kerf-comp** — DONE (see its section).
- **S-3 nest-into-holes** — DEFERRED, blocked upstream (see its section).
- **S-4 mirror** — DONE, manual flip (see its section).
- **S-5 placement ops** — `Placed::{move_to_sheet, flip_h, flip_v}`, `locked`.
- **S-6 stable id** — `Piece.id` (content hash + occurrence counter).
- **S-7 SVG export** — `svg::{write_svg, sheet_svgs, combined_svg}`.
- **S-8 material** — `stats::{Material, CutProfile}`.

**The only genuinely unbuilt work is the Roadmap (R0–R3) at the bottom** of this
file, plus the two upstream-blocked nesting items (S-3, auto-mirror).

---

## P0-1. Cooperative cancellation for nesting

**What:** Let a caller stop an in-flight `optimize::nest_sparrow` (and
`nest::nest`) promptly, mid-sheet.

**Why:** Nesting runs on a background thread and can take `time × sheets`
seconds (default 12 s/sheet). The UI has a **Cancel** button, but today it is
only a *soft* cancel: the UI stops listening on the result channel and the
worker keeps burning a CPU core to completion (`src/gui.rs`,
`Intent::CancelNest`). `nest_sparrow` constructs `BasicTerminator::new()`
internally (`src/optimize.rs:187`) with no external stop signal, and `time` also
can't be shortened once started.

**Suggested API:** thread an optional cancellation flag through, checked by the
terminator between sparrow iterations:

```rust
pub fn nest_sparrow(
    items: &[NestItem],
    sheet_w: f64, sheet_h: f64, kerf: f64, seed: u64,
    explore: Duration, compress: Duration,
    cancel: Option<Arc<AtomicBool>>,   // NEW: set true to stop ASAP
    progress: impl FnMut(usize),
) -> NestResult
```

Internally, replace `BasicTerminator::new()` with a terminator that also returns
"terminate" when `cancel` is set (sparrow's `Terminator` is a trait, so a small
wrapper works). On cancel, return whatever has been placed so far plus the
still-unplaced pieces (a partial `NestResult` is fine — the UI will label it
"partial"). `nest::nest` (the greedy fallback) should check the same flag at the
top of its per-piece loop (`src/nest.rs:196`).

---

## P0-2. Live progress + streamed per-sheet placements

**What:** A richer progress signal, and — ideally — placements delivered *as each
sheet is finalized* rather than only in the final `NestResult`.

**Why:** The UI wants (a) a real progress indicator and (b) to fill the preview
canvas sheet-by-sheet as the run proceeds, which is much better UX for a
multi-sheet job. Today the callback is `FnMut(usize)` receiving only the count of
sheets peeled so far (`src/optimize.rs:111`, `nest_sparrow`'s `progress`
argument), and all `Placed`s arrive together at the end. The UI can currently
only show a spinner + "sheet N".

Note that `nest_sparrow` *already* peels one sheet at a time
(`src/optimize.rs:136` loop) — so the placements for a completed sheet exist
before the next sheet starts. Surfacing them is cheap.

**Suggested API:** replace the bare `usize` callback with an event callback:

```rust
pub enum NestEvent<'a> {
    /// A sheet was finalized; these placements will not change.
    SheetCompleted { sheet: usize, placed: &'a [Placed] },
    /// Coarse progress for a determinate bar, 0.0..=1.0 (optional/best-effort).
    Progress { fraction: f32 },
}

pub fn nest_sparrow(/* … */, on_event: impl FnMut(NestEvent)) -> NestResult
```

Keeping it a callback (not a channel) preserves the "library stays UI-free"
property the code already values (`src/optimize.rs` doc comment, and the
`indicatif` spinner in `main.rs` is driven the same way). The UI will forward
these events to the canvas via its own channel.

---

## P0-3. Move the "oversized pieces on their own sheets" logic into the library

**What:** One library function that does the *whole* nest-to-fixed-sheets job the
CLI and GUI both want, including appending oversized pieces on dedicated sheets.

**Why:** That post-processing loop — take `NestResult.oversized`, append each as
its own `Placed` on a new sheet index with a recentring transform — currently
lives in `main.rs` (`pack_nest`, `src/main.rs:228`) and I had to **duplicate it
verbatim** in the GUI worker (`src/gui.rs`, `run_pack`, `Packer::Nest` arm). Two
copies of identical placement logic will drift.

**Suggested API:** a thin wrapper next to `nest_sparrow`:

```rust
/// Full pipeline: nest onto fixed sheets, then place oversized pieces on their
/// own sheets. Returns everything a caller needs to emit or preview.
pub fn nest_sheets(
    items: &[NestItem],
    piece_bboxes: &[Bbox],      // for the oversized recentring transform
    sheet_w: f64, sheet_h: f64, kerf: f64, seed: u64,
    explore: Duration, compress: Duration,
    cancel: Option<Arc<AtomicBool>>,
    on_event: impl FnMut(NestEvent),
) -> NestOutcome;               // { placed: Vec<Placed>, oversized: Vec<usize>, sheets: usize }
```

Then `main.rs` and `gui.rs` both call `nest_sheets` and delete their copies.

---

## P0-4. Return structured diagnostics instead of `eprintln!`

**What:** Have `extract` and the nesters *return* their warnings rather than
printing them.

**Why:** The pipeline emits several user-relevant warnings straight to stderr,
which the GUI does not own and cannot show:

- non-unit INSERT scale → footprint may be wrong (`src/extract.rs:150`)
- INSERT of a block with no cut geometry, skipped (`src/extract.rs:139`)
- dropped degenerate parts with no cuttable area (`src/extract.rs:195`)
- pieces nested by convex hull because the outline was non-simple
  (`src/nest.rs:157`)

These are exactly the things a user needs to see ("why did my piece vanish / why
does this one look wrong"), and they belong in the UI's status/diagnostics area.

**Suggested API:** collect into a returned list (or a `&mut Vec<Diagnostic>`
sink, to avoid changing every signature's return type):

```rust
pub struct Diagnostic {
    pub severity: Severity,          // Info | Warning
    pub piece_label: Option<String>, // ties the message to a list row when possible
    pub message: String,
}

pub fn extract(drawing: &Drawing, sources: Sources) -> (Vec<Piece>, Vec<Diagnostic>);
```

Keep `eprintln!` out of the library entirely; let the CLI print the returned
diagnostics and the GUI render them.

---

## P1-5. Per-piece metadata on `Piece`

**What:** Add `area`, a source category, and a stable id to `Piece`.

**Why:** The UI's piece list (`src/gui.rs`, `controls`) shows label + an
oversized flag, and would like to **sort by area**, **group Layer/part vs
Block**, and key selection on a **stable id**. Today it only has `label` and
`bbox` (both already `pub` on `src/extract.rs:32`). Labels like `part:0` are
index-derived and get reused across re-extraction, so they are not stable ids.

**Suggested API:**

```rust
pub struct Piece {
    pub label: String,
    pub kind: PieceKind,
    pub bbox: Bbox,
    pub area: f64,             // NEW: true outline area (you already flatten rings)
    pub source: PieceSource,   // NEW: Part | Block  (grouping/filtering)
    // optional: pub id: u64,  // stable across re-extraction
}

pub enum PieceSource { Part, Block }
```

`area` is essentially free — `flatten::area` already exists (`src/flatten.rs:18`)
and `piece_polygon` is computed during nesting anyway.

---

## P1-6. Per-sheet utilization / stats in the nest result

**What:** Report packing quality per sheet.

**Why:** The single most useful number for this tool's user is **sheet
utilization %** (how full each sheet is). The CLI's own docs quote it ("nest
fills sheet 0 to ~75%"), so the value clearly exists conceptually, but the UI
would have to recompute every piece's area and sum them per sheet to show it.

**Suggested API:** attach per-sheet stats to the outcome:

```rust
pub struct SheetStats {
    pub sheet: usize,
    pub piece_count: usize,
    pub used_area: f64,
    pub sheet_area: f64,
    pub utilization: f32,   // used_area / sheet_area
}
// e.g. NestOutcome { placed, oversized, sheets, stats: Vec<SheetStats> }
```

---

## P1-7. Tell the UI which pieces were nested by their convex hull

**What:** Per-piece flag: "the flattened outline was non-simple, so this piece
was nested by its convex hull."

**Why:** When a piece is packed by its hull it reserves *more* than its real
outline, and the preview's real-outline overlay can look like it under-fills its
slot. The UI wants to badge these pieces so the user understands. Today
`valid_ring_or_hull` only bumps an aggregate counter (`src/nest.rs:90`,
`src/optimize.rs:69`); it doesn't record *which* items fell back.

**Suggested API:** either add `hull_fallback: bool` to `NestItem` (set in
`build_items`, `src/nest.rs:44`), or return the set of piece indices that used a
hull from the nest run. The UI already has `NestItem.polygon` (public) and uses
`flatten::entity_rings` to draw the true outline — this flag is the missing bit.

---

## P1-8. Derive `Clone` on the shared result types

**What:** `#[derive(Clone)]` on `emit::Placed`, `nest::NestItem`, and any new
`NestOutcome` / `SheetStats`.

**Why:** The UI keeps the last successful result while a new nest runs (to keep
the preview populated), and would like to snapshot/diff placements. `Placed`
(`src/emit.rs:88`) and `NestItem` (`src/nest.rs:35`) are plain data; `Affine`
and `Bbox` are already `Copy`. Cheap, no downside. (`Send` is already satisfied,
which is why the UI can move `Vec<Placed>`/`Vec<NestItem>` to its worker today.)

---

## P2-9. Pin placements and re-nest the rest

**What:** Accept a set of already-placed pieces the packer must treat as fixed
obstacles, and nest only the remaining pieces around them.

**Why:** A high-value power feature: user drags/locks a piece, then re-nests
everything else without disturbing it; or adds a few new pieces and packs them
onto existing sheets. Today every run packs from scratch.

**Suggested API (sketch):** `nest_sheets(..., fixed: &[Placed])` where `fixed`
pieces are inserted into each layout as pre-placed hazards before optimizing.
jagua models fixed items/hazards, so the bin-packing path (`nest::nest`) is the
more natural home than sparrow strip-packing. Design-dependent — flag for
discussion before building.

---

## P2-10. In-memory emit (build `Drawing`s without touching the filesystem)

**What:** Factor the per-sheet `Drawing` construction out of `emit::emit` so a
caller can get the drawings in memory.

**Why:** `emit::emit` writes files into a directory (`src/emit.rs:104`), which is
fine for the GUI's "Export…" action (it already calls `emit` with a chosen
folder). But an "export to a single chosen path / zip" flow, or previewing the
*exact* output DXF, wants the `Drawing`s without a directory side effect.

**Suggested API:**

```rust
pub fn build_sheet_drawings(
    source: &Drawing, pieces: &[Piece], placed: &[Placed],
) -> Vec<Drawing>;   // one per sheet, in sheet order
```

Keep `emit::emit` as a thin wrapper that calls this and writes each drawing to
`{stem}_sheet_{NN}.dxf`. Preserves the R13+ version-inheritance fix
(`src/emit.rs:121`) in one place.

---

# Additional needs surfaced by the Slate UI

The full **Slate** design (workspace with a NEST control bar, setup panel,
packing-stats panel, per-part context actions, and DXF/SVG export) exposes more
of the domain than the first scaffold did. These are the extra backend seams it
implies. The UI ships today by displaying UI-only values or computing stats from
geometry on the render thread — each item below says what the workaround is.

## S-1. Per-sheet stats owned by the library (yield / cut-length / pierces / est-run)

**What:** Return per-sheet packing metrics from the nest run.

**Why:** Slate's stats panel shows **material yield %**, **cut length**,
**pierces**, and **estimated run time** per sheet, plus used/waste m². The GUI
currently computes yield, cut length (Σ ring perimeters) and pierces (ring count)
itself in `sheet_stats()` (`src/gui.rs`) by walking `piece_rings` each frame —
correct but duplicated and it belongs with the packer. **Est. run** it cannot
compute at all: it needs a **feed-rate / pierce-time model** (mm/s cut speed +
seconds/pierce, likely per material), so the UI shows `—` for it today.

**Suggested API:** extend the P1-6 `SheetStats` with `cut_len_mm`, `pierces`, and
(if a feed model is added) `est_run_secs`; provide a
`fn cut_metrics(pieces, placed, sheet) -> (cut_len_mm, pierces)` helper so the UI
stops recomputing. A feed-rate model can live behind a
`CutProfile { feed_mm_s, pierce_s }` the caller supplies.

## S-2. Distinguish part **spacing** vs **kerf compensation** vs **sheet margin**

**STATUS (2026-07-09): DONE.** Spacing and **sheet margin** enforced (the nester
insets the container). **Kerf compensation** implemented as **Option A**:
`flatten::compensate_rings` offsets the flattened rings (outer +kerf/2, holes
−kerf/2, classified by containment-depth parity; `flatten::offset_ring` with a
mitre limit does the offset), and `emit`/`build_sheet_drawings` take a
`kerf_comp: f64` (0 = off) that emits the compensated outline as closed
POLYLINES instead of the original entities. Flag-gated/off-by-default (faithful
splines stay the default). `EmitReport` now carries `diagnostics: Vec<Diagnostic>`
with the explicit "kerf compensation on … emitted as polylines (curved outlines
approximated)" notice. Containment is preserved by building the nesting
reservation from the compensated rings (`nest::build_items_with(drawing, pieces,
kerf_comp)` → `piece_polygon` over the grown outline). CLI flag `--kerf-comp`.
Tested in `tests/kerf_comp.rs` (outer grows / holes shrink by kerf/2; compensated
outline stays within its reservation on the fixture; emit end-to-end + diagnostic).
Chosen because this fixture's outlines are almost all SPLINES and a faithful
offset of a spline is not a spline — Option A is the only one that actually
compensates spline outlines, with the fidelity tradeoff surfaced, not silent.

GUI wiring (for the egui-designer): pass the **Kerf** knob as `kerf_comp` into
`nest::build_items_with(..., kerf_comp)` and `emit::emit(..., kerf_comp)` (and/or
`build_sheet_drawings(..., kerf_comp)`), and surface `EmitReport.diagnostics`.
Signature deltas relayed separately.

**What:** Three separate parameters the design treats as distinct; the backend
has only one (`kerf` = gap between parts).

**Why:** Slate's NEST bar has **Spacing** (2.0), **Margin** (6.0) and **Kerf**
(0.15) as three different knobs. Today the UI maps **Spacing → the backend's
`kerf` separation** and treats the other two as display-only:
- **Sheet margin** — a usable inset the nester should keep parts inside. The UI
  *draws* the dashed margin per the canvas spec, but **the nester doesn't enforce
  it**, so parts can currently sit in the margin. Needs the packer to inset the
  container (jagua bins can take an inset/zone) by `sheet_margin_mm`.
- **Kerf compensation** — offsetting each cut outline outward by half the laser
  beam width so finished parts are dimensionally correct. This is a **geometry
  offset** applied to the emitted outlines, distinct from inter-part spacing.
  Not applied anywhere today.

**Suggested API:** rename the nesting-gap parameter to `spacing` for clarity and
add `sheet_margin`, plus a `kerf_compensation` applied in `flatten`/`emit` (offset
the polygon). Keep them independent.

## S-3. Nest small parts into holes of larger parts

**STATUS (2026-07-09): DEFERRED — blocked upstream.** jagua-rs 0.7.2 is the
latest release (crates.io, May 2026) and its importer drops polygon holes
(`warn!("No native support for polygons yet, ignoring the holes")`), keeping only
the outer ring — so a part's void cannot be exposed to the collision engine.
Native polygon/hole support is an open, unscheduled upstream enhancement
(JeroenGar/jagua-rs issue #5 "Polygon and MultiPolygon support?"). No jagua
version supports holes, so no upgrade path exists; the limited pinned-host-only
workaround was declined as a partial hack. Revisit when jagua ships native hole
support. The GUI's "Nest Parts Into Holes" toggle stays disabled.

**What:** Allow a part to be placed inside the concave hole/void of another part.

**Why:** Slate has a **Nest Parts Into Holes** toggle and the canvas spec shows
discs seated inside circular bores. This materially improves yield for framed
parts. The current nesters treat each part's outer outline as solid.

**Suggested API:** a `nest_into_holes: bool` on the nest call; when set, expose
each part's holes as usable regions to the collision engine (jagua can represent
a bin/part interior with holes). Non-trivial — flag for design discussion.

## S-4. Mirror / reflected placements

**STATUS (2026-07-09): DONE (manual per-part flip).** `emit` now renders
reflected (det<0) placements correctly — arc sweep reversed, ellipse parameters
handled, and a mirrored INSERT emits `y_scale_factor = −1`; `geom::Affine` gained
`determinant`/`reflect_x`/`reflect_y`, and `Placed` gained `flip_h`/`flip_v`
(mirror about the footprint centre). Proven by `tests/mirror_emit.rs`. The GUI
wires **Flip Horizontal / Flip Vertical** (Arrange + context menus). **Automatic
mirror-DURING-nesting is NOT supported** and was declined: the default sparrow
optimizer is rotation-only, and jagua's bpp demand model can't express "place the
part OR its mirror, exactly once." The "Mirror Allowed" toggle therefore stays
disabled; manual Flip H/V is the reachable path.

**What:** Allow parts to be flipped (mirrored) as well as rotated during nesting.

**Why:** Slate has a **Mirror Allowed** toggle. For symmetric stock this unlocks
extra fits. Today only rotation is offered (`allow_rotation`). Needs the
placement transform + `emit` to support a reflection (negative determinant),
which also means `geom::Affine` must round-trip a mirror and `emit` must handle
mirrored INSERT/geometry correctly.

## S-5. Mutable per-part placement ops (rotate / flip / move-to-sheet / lock)

**What:** Edit an individual placement after nesting.

**Why:** Slate's Arrange menu and the part right-click context menu offer
**Rotate 90° CW/CCW**, **Flip H/V**, **Move to Sheet ▸**, and **Lock Position**.
These require a placement to be individually transformable and re-emittable, and
**Lock** ties directly into P2-9 (pin-and-re-nest). The UI has selection wired
(click-to-select a part) but these actions are currently no-ops that set a status
line.

**Suggested API:** make `Placed` the editable source of truth (it already carries
`transform` + `sheet`); add helpers `rotate_placement(&mut Placed, quarter_turns)`,
`move_to_sheet(&mut Placed, sheet)`, and a `locked: bool` flag consumed by the
pinned-re-nest path. Re-emit is already placement-driven, so edits flow through
`emit` unchanged.

## S-6. Stable `PartId` / selection identity

**What:** A stable per-part identifier the UI can use for selection, context
menus, and "move to sheet".

**Why:** Slate selects a part ("PART 6 · FRAME_04") and acts on it. The UI keys
selection on the piece **index** today, which is stable only until re-extraction.
This is the same ask as **P1-5** (`Piece.id`); calling it out again because the
per-part context actions make it load-bearing, not just cosmetic.

## S-7. SVG export (and combine-into-single-file)

**What:** Emit sheets as **SVG** in addition to DXF, and optionally combine all
sheets into one file.

**Why:** Slate's Export menu has **Format: DXF ✓ / SVG** and **Combine Into
Single File**. `emit` writes DXF only (`src/emit.rs`), so the UI's SVG path is
disabled with a status message today, and combine is display-only.

**Suggested API:** building on P2-10's `build_sheet_drawings`, add
`fn write_svg(drawings: &[Drawing], out, stem, combined: bool)` (or an
`emit::Format` enum passed to `emit`). SVG is convenient because the outlines are
already flattened polylines; the writer can reuse `flatten`'s rings.

## S-8. Material / thickness as first-class job metadata

**What:** Let the job carry a material + thickness the backend can act on.

**Why:** Slate's setup panel selects **Material (Plywood · 3 mm)**. The UI stores
`material`/`thickness_mm` but nothing consumes them. They naturally drive S-1's
cut profile (feed/pierce) and S-2's kerf compensation. Low priority until those
land, but worth threading a `Material { name, thickness_mm, cut: CutProfile }`
through the job so the stats/kerf features have a home.

---

## Non-asks (things that already work — recorded so they aren't "fixed")

- **Progress callback pattern.** `nest_sparrow` taking `impl FnMut` and the
  library staying UI-free is exactly right; keep it. The P0-2 change *extends*
  this pattern, it doesn't replace it.
- **Seed is already a parameter** (`nest_sparrow(..., seed, ...)`), so a
  "re-roll layout" button needs no backend change.
- **`NestItem.polygon` is already public**, so the UI can draw nesting outlines.
- **The types are already `Send`**, so moving work to a background thread needs
  nothing from you.

---

# Roadmap: features beyond nesting (proposed 2026-07-09)

A prioritized backlog of non-nesting features surfaced after the core app
shipped. Owner tags: **[gui]** = egui-designer (`src/gui.rs`), **[lib]** =
nesting-backend (library), **[both]**. Effort: **S** (hours) / **M** (a focused
session) / **L** (multi-session).

**Progress (2026-07-09):** the backend track is essentially complete. DONE:
**R1-1a** (single `Cut` layer + colour), **R1-1b** (outer/inner cut-layer split),
**R3-1** (part quantity), **R3-2** (engraved assembly numbers, lib core), **R3-3**
(micro-tabs), **R3-4** (holes-before-outline cut order). All are surfaced through
`emit::EmitOptions` + `emit::emit_opts` and per-piece `Piece.quantity`, with CLI
flags for each. Remaining lib item: **R2-3** (arc/polyline offsetter — flagged a
drop candidate). Everything else — **R0** quick wins, **R1-2** save/load, **R1-3**
undo, **R2-1** canvas manipulation, **R2-2** units, and the GUI halves of **R3-2**
(toggle + user-driven number) and the new emit knobs — is **[gui]**. See the
**GUI wiring hand-off** at the end of this file. Each item's STATUS line has
details.

## R0 — Quick wins (wire existing stubs)

These are `menu_todo` placeholders in `src/gui.rs` today; small, mostly [gui].

- **R0-1 Zoom + keyboard nav** [gui] **S** — Zoom In/Out (⌘±), Fit Sheet (⌘0),
  Actual Size (⌘1). The zoom HUD + scroll-zoom already drive `zoom`/`pan`; just
  wire the menu items and keyboard shortcuts to that existing state. "Fit Sheet"
  = compute zoom so the active sheet fills the canvas; "Actual Size" = 1 px/mm.
- **R0-2 Export All Sheets (⇧⌘E)** [gui] **S** — `emit::emit` already writes
  every sheet; the current Export is effectively all-sheets. Wire the menu item
  and make "Export this sheet" the filtered case.
- **R0-3 Select All / Deselect / Delete From Nest** [both] **M** *(was S–M —
  nemesis)* — selection is a single `sel_id` today; add a multi-select set.
  **Delete** must model removal as a `removed: HashSet<u64>` keyed on stable
  `Piece.id`, applied only when building the nest input — **never** filter/shrink
  the `pieces` vec: `Placed.piece_index`, `bboxes[]`, `labels[]`,
  `NestItem.piece_index`, and `model.piece_rings[]` are all positional indices
  into one `pieces` vec, so shrinking it misaligns every placement. Must also
  survive `reextract()` (which rebuilds pieces + clears selection).
- **R0-4 About / Keyboard Shortcuts dialogs** [gui] **S** — static modal content.

> **Nemesis note (R0-1):** wiring real keyboard shortcuts surfaces an existing
> **⌘R collision** — "Reload Source" and "Re-nest Current Sheet" share the chord.
> Reassign one first. **(R0-2):** the "Export Active Sheet…" menu label is
> currently a lie — it runs `emit::emit`, which writes ALL sheets; R0-2 must add
> real per-sheet filtering, not just a rename.

## R1 — High value

- **R1-1a Single "Cut" layer + color** [lib] **S** — **STATUS (2026-07-09):
  DONE.** `emit` now routes every emitted entity through `add_cut`, which stamps
  `common.layer = "Cut"` and `common.color = ACI 1` (red); `register_cut_layer`
  adds the `Cut` layer to each sheet's table with that colour, and copied block
  definitions have their sub-entities re-layered too (so INSERT geometry cuts even
  when the consumer doesn't inherit the INSERT's layer). Laser software
  (LightBurn, etc.) imports the output as one cut op with no manual layer
  assignment. Tested in `tests/quantity.rs` (layer + ACI on emitted geometry and
  in the layer table). (No "engrave/score" layer: the loader drops all HATCH
  fills, so every surviving entity is already a cut — an engrave layer would
  always be empty. The real engrave need is R3-2, engraved part numbers.)
- **R1-1b Outer-cut vs inner-cut layer split** [lib] **M** — **STATUS
  (2026-07-09): DONE.** `EmitOptions.split_cut_layers` (CLI `--split-layers`) puts
  each loose part's outer contour on `Cut-Outer` (ACI 1) and its holes on
  `Cut-Inner` (ACI 3), so the two can carry different laser ops. Role is decided
  by outline area — the largest ring/entity is the outer contour, the rest are
  holes (`emit::cut_role`), applied on the faithful, kerf-comp, and tab paths.
  **Block (INSERT) pieces are not split** — a block is one INSERT entity, so it
  stays on `Cut` and a warning diagnostic reports how many did. Off by default
  (single `Cut` layer, R1-1a). Tested in `tests/split_layers.rs`.
- **R1-2 Save / Load a job + Recent files + drag-drop DXF** [both] **M–L** —
  serialize a job: source DXF path **+ content hash**, params **including
  `sources`/extraction settings** (the piece *set* depends on `params.sources`, so
  reloading with a different `sources` orphans placements even on an unchanged
  file), per-part placements, and locks. **Identity is the hard part:** `Placed`
  stores `piece_index`, not `id`, so persist an explicit `piece_index → Piece.id`
  map. `Piece.id` (quantized bbox+area hash + occurrence counter) is stable
  **only across re-extraction of the same file** — any DXF edit that nudges a
  piece's extent orphans its placements. So define explicit **reload-on-hash-
  mismatch** behavior: re-extract, re-attach by id, warn, and drop unmatched
  placements. Restore `sources`/params *before* re-extracting.
- **R1-3 Undo / Redo** [gui] **M** — a command-history stack over the existing
  unidirectional intent loop; snapshot the `Placed` set (it's `Clone`) or record
  inverse ops. Land this **before/with** R2-1 so manual moves are reversible.

## R2 — Deeper

- **R2-1 Direct canvas manipulation** [gui] **M–L** — drag a part to reposition,
  arrow-key nudge, snap. Hit-test via point-in-polygon on the rendered outline;
  drag updates `Placed.transform`. Depends on **R1-3** (undo). Manual moves aren't
  overlap-checked (documented caller responsibility — a re-nest cleans up).
- **R2-2 Units (user-asserted mm/inch) + material catalog** [both] **M** — a units
  *reinterpretation* toggle (input `$INSUNITS` is unset, so the tool assumes mm;
  the toggle only relabels on the user's assertion — it can't make units "real")
  plus a per-material catalog driving `CutProfile` (feed/pierce) and kerf. Est.
  run stays a labelled **estimate** (feed rates are inherently approximate). This
  is really the UI for **S-8** (`Material`) + **S-1** (`CutProfile`) — build on
  that seam, don't duplicate it.
- **R2-3 Arc/polyline kerf compensation (denser joins)** [lib] **L** *(renamed
  from "faithful curve" — nemesis: unachievable)* — a NURBS offset is not a NURBS,
  so a "faithful curve offset" can't exist; the best upgrade over the shipped
  Option-A miter offset is a proper offsetter that emits clean arc/polyline joins.
  **Hard acceptance criterion: pure-Rust offsetter only (e.g. `cavalier_contours`)
  — NO new C dependency**, or it breaks the Windows static cross-compile guarantee
  (Clipper2 is C++). Marginal gain for L effort — **candidate to drop** unless a
  concrete dimensional-accuracy complaint appears.

## R3 — Missing domain features (nemesis-surfaced; a laser user expects these)

- **R3-1 Part quantity / copies** [lib] **M** — **STATUS (2026-07-09): DONE.**
  `Piece.quantity` (default 1, raised by the GUI's per-part knob) drives
  `nest::build_items`, which now reserves one nesting item per copy (sharing the
  piece index). Placements, `emit`, and stats all resolve each copy back to the
  same source piece, so N copies nest, cut, and count correctly. Implemented by
  item expansion rather than jagua `demand` because the sheet-peeling loop rebuilds
  its instance each round (demand wouldn't survive the peel), and expansion also
  composes with the greedy `nest` fallback. Reachable from the CLI via `--copies N`
  (uniform; nest packer). Tested in `tests/quantity.rs` (3 copies → 3 items → 3
  placements → 3 emitted outlines). *Caveat:* combining `quantity > 1` with the
  pinned re-nest path (`nest_pinned`) is not supported — its `piece_index → item`
  map assumes one item per piece; unaffected while all quantities are 1.
- **R3-2 Assembly numbering + engraved part labels** [both] **M** — **STATUS
  (2026-07-09): lib core DONE.** `EmitOptions.engrave_numbers` (CLI `--engrave`)
  adds a centred TEXT label of each piece's 1-based assembly number at its
  footprint centre, on a registered `Engrave` layer (ACI 5, kept off the cut
  layer so it runs as a separate op). Surfaced via `EmitReport.diagnostics`.
  Tested in `tests/engrave_sequence.rs`. *Remaining [gui]:* a toggle, and
  optionally a user-driven number instead of `piece_index + 1`.
- **R3-3 Micro-tabs / holding bridges** [lib] **M** — **STATUS (2026-07-09):
  DONE.** `EmitOptions.{tab_width, tab_count}` (CLI `--tab-width`/`--tab-count`)
  breaks each outline ring into open polyline segments separated by `tab_count`
  uncut gaps of `tab_width`, distributed evenly by arc length (`emit::tab_ring`),
  so fully-cut parts stay attached. Like kerf-comp it's an opt-in
  polyline-approximation mode (curved outlines flattened) with the tradeoff
  surfaced as a diagnostic; degenerate/over-tabbed rings fall back to a whole
  outline. Composes with kerf compensation (tabs the compensated rings). Tested
  in `tests/micro_tabs.rs` (cut length = perimeter − tab_count·tab_width).
- **R3-4 Cut sequencing (holes before outline)** [lib] **S** — **STATUS
  (2026-07-09): DONE.** Within each piece, `emit` now orders cuts by ascending
  outline area (`emit::entity_area`) so interior rings/holes cut before the outer
  contour — applied to loose entities, copied block sub-entities, and the
  kerf-comp/tab ring paths. Always on (no fidelity cost; pure reorder). Verified
  in `tests/engrave_sequence.rs` (hole emitted before the outer square).

## Still blocked (nesting — listed for completeness)

- **Nest-into-holes** (S-3): blocked upstream by jagua-rs dropping polygon holes.
- **Automatic mirror during nesting** (S-4): sparrow is rotation-only; jagua's bpp
  demand model can't express "place the part OR its mirror, exactly once."

## Nemesis review

An adversarial red-team pass (2026-07-09) against the codebase. The premise held
(R0–R2 are all genuinely unbuilt `menu_todo` stubs), but framing and hidden
prerequisites needed fixing — folded into the items above. Key findings:

- **Critical — R2-3 "faithful curve" is unachievable + risks the Windows build.**
  A NURBS offset isn't a NURBS; and a Clipper2 offsetter is C++, which would break
  the "no C dependency / static MinGW" Windows guarantee. → Renamed to arc/polyline
  joins; pure-Rust-offsetter-only made a hard acceptance criterion; flagged as a
  drop candidate.
- **Major — R1-1 overstated.** No engrave geometry exists (HATCH fills are dropped
  on load), INSERT/Block pieces can't be per-ring layered, and the claimed
  outer/hole classification isn't present on the default emit path. → Split into
  R1-1a (single Cut layer + color, correct & cheap — ship first) and R1-1b (outer/
  inner split, loose parts only); engrave need repurposed as R3-2.
- **Major — R1-2 / R0-3 identity.** `Piece.id` is stable only across re-extraction
  of the *same* file, not across DXF edits or `sources` changes; `Placed` keys on
  `piece_index` (positional), so deletion-by-filtering misaligns everything. →
  Deletion modelled as a `removed`-by-`id` set (never shrink `pieces`); Save/Load
  persists `sources`/params + an explicit index→id map with defined
  reload-on-hash-mismatch behavior.
- **Major — R2-2 units.** Input units are unknowable ($INSUNITS unset), so a toggle
  can only *reinterpret* on the user's assertion; Est. run stays an estimate. →
  Reframed; cross-referenced to S-8/S-1.
- **Minor** — ⌘R shortcut collision (Reload Source vs Re-nest); the "Export Active
  Sheet" label already writes all sheets; shipped manual ops already mutate without
  undo (noted, not reordered).
- **Missing (added as R3-1…R3-4):** part quantity (jagua `demand` hardcoded to 1),
  engraved assembly numbers (the real engrave use case for a stacked model),
  micro-tabs/holding bridges, and hole-before-outline cut sequencing.

**Top 3 changes it insisted on (all applied):** (1) rewrite R2-3 to drop "faithful
curve" and forbid a C dependency; (2) split R1-1 and delete the empty engrave
layer, repurposing it as R3-2; (3) harden R1-2/R0-3 around stable-`id` identity
rather than betting on more stability than `Piece.id` provides.

---

# GUI wiring hand-off (backend → egui-designer, 2026-07-09)

The backend track is done. Six shipped features now need UI. This is what to wire;
none of it is a blocker (the app still runs), each item just exposes a finished
backend seam. All emit-time features flow through **one** new entry point.

## 1. Switch export to `emit::emit_opts`

The export path currently calls `emit::emit(&drawing, &pieces, &placed, &dir,
stem, kerf)` (`src/gui.rs`, `Intent::Export`, ~line 721). `emit` still works (it's
now a thin `f64`→kerf-comp wrapper), but the new knobs live on:

```rust
pub struct EmitOptions {
    pub kerf_comp: f64,        // existing `kerf_mm` knob (already wired via `emit`)
    pub engrave_numbers: bool, // R3-2
    pub tab_width: f64,        // R3-3 (0 = off)
    pub tab_count: usize,      // R3-3 (tabs per ring)
    pub split_cut_layers: bool,// R1-1b
}   // #[derive(Clone, Copy, Debug, Default)] — Default = faithful cut, nothing extra

pub fn emit_opts(source, pieces, placed, out_dir, stem, opts: EmitOptions)
    -> std::io::Result<EmitReport>;
```

Replace the `emit(...)` call with `emit_opts(..., EmitOptions { kerf_comp: kerf,
..from the controls below })`. Keep surfacing `EmitReport.diagnostics` (you already
do) — every new mode pushes a `Diagnostic` there (see §4).

`build_sheet_drawings(source, pieces, placed, opts)` also now takes `EmitOptions`
(was `f64`) if you use it for preview.

## 2. New controls to add (map to `EmitOptions` / `Params`)

| Control (Slate NEST bar / setup) | Field | Notes |
|---|---|---|
| **Engrave part numbers** toggle | `engrave_numbers` | TEXT of `piece_index+1` on the `Engrave` layer, centred on each part. |
| **Micro-tabs**: width + count | `tab_width`, `tab_count` | `tab_width > 0 && tab_count > 0` ⇒ tabs on. **Forces polyline mode** (splines flattened) — same fidelity trade as kerf-comp; the diagnostic says so. |
| **Split outer/inner cut layers** toggle | `split_cut_layers` | Loose parts only. Outer→`Cut-Outer` (red), holes→`Cut-Inner` (green). Blocks stay on `Cut` and emit a warning. |

`Params` already has `kerf_mm` wired as `kerf_comp`. The three new toggles/knobs
are net-new `Params` fields + widgets. `nest_into_holes`, `mirror_allowed`,
`combine_single` remain as-is (combine is real for SVG; the other two stay
disabled — still blocked upstream).

**Emitted layers** are now `Cut` (+ `Cut-Outer`/`Cut-Inner` when split, `Engrave`
when engraving). If the canvas legend or a layer chip reflects output layers, it
can read these names.

## 3. Per-piece quantity (R3-1)

`Piece.quantity: usize` (default 1) is the copy count. Add a per-part **quantity
stepper** to the piece list / context menu that sets `model.pieces[i].quantity`.
`nest::build_items` reserves one nesting item per copy, so the **nest packer**
places/cuts/counts N copies automatically — no other call changes.

Caveats (same identity story as `locked`/selection):
- **Reset to 1 on `reextract()`** — re-apply saved quantities by `Piece.id` after
  re-extraction, exactly like you plan to for locks (R1-2).
- The **rect packer ignores** quantity (bbox path); it's a nest-packer feature.
- **`nest_sheets_pinned` + quantity > 1 is unsupported** (its `piece_index → item`
  map assumes one item per piece). Fine while pinning is used with quantity 1.

## 4. Diagnostics to render

`EmitReport.diagnostics` now carries an `Info`/`Warning` line for each active
mode: kerf-comp, engraving, micro-tabs (all `Info`, noting the polyline
approximation), split-layers (`Info`), and a **`Warning`** counting block pieces
that couldn't be split. Route them to the status/diagnostics area as you already
do for the kerf-comp notice.

## 5. Already automatic (no control needed)

- **R3-4 cut sequencing** — holes are always emitted before the outer contour.
- **R1-1a single `Cut` layer + colour** — always on when the split is off.

## 6. Remaining GUI-only roadmap

Unchanged and still yours: **R0-1..R0-4** (zoom/nav, export-all, multi-select +
delete-by-`id`, about/shortcuts), **R1-2** (save/load + recent + drag-drop),
**R1-3** (undo/redo), **R2-1** (canvas manipulation), **R2-2** (units + material
catalog UI over `stats::Material`/`CutProfile`), and the GUI half of **R3-2** (the
engrave toggle above, plus optionally a user-assigned number instead of
`piece_index+1`). Watch the two nemesis notes: the **⌘R collision** (Reload Source
vs Re-nest) and the **"Export Active Sheet" label** that actually writes all sheets.
