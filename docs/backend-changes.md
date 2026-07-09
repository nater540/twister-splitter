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
