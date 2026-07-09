# Twister Splitter

Takes a single DXF of a layered/stacked design — the kind where every piece sits
on top of the others at the same spot (a typical Illustrator "Export As DXF" of a
stacked laser-cut model) — and does two things:

1. **Splits** it into the individual pieces that need cutting.
2. **Arranges** those pieces onto laser-bed–sized sheets so they fit and don't
   waste material, writing one cut-ready DXF per sheet.

Default sheet size is **400 × 400 mm**. Measurements are in millimetres.

---

## Running it

### The app (easiest)

Double-click nothing — open a terminal in this folder and run:

```
cargo run
```

That opens the desktop window. Load your DXF, adjust the settings on the side if
you want, click **NEST**, then **Export** to save the sheets.

### The command line (for repeat jobs)

```
cargo run -- your-design.dxf
```

The finished sheets land in a folder called `out` (one file per sheet, named
`your-design_sheet_00.dxf`, `_sheet_01.dxf`, and so on).

---

## Options (all optional)

You only need these if the defaults don't suit a particular job. On the command
line they go after the file name; in the app they're the controls on the panel.

| What you want | Command-line flag | Default |
|---|---|---|
| Different sheet size | `--size 600x300` | 400 × 400 mm |
| Save somewhere else | `--out-dir my-folder` | `out` |
| More gap between parts | `--kerf 3` | 2 mm |
| Keep parts away from the sheet edge | `--margin 6` | 0 (off) |
| **Cut more than one of each part** | `--copies 3` | 1 |
| **Engrave a number on each part** (so a stacked model can be re-stacked in order) | `--engrave` | off |
| **Leave little "tabs" so parts don't fall out while cutting** | `--tab-width 3` | off |
| Make finished parts exactly to size (compensate for the laser beam width) | `--kerf-comp 0.15` | off |
| Pack faster but looser | `--packer rect` | dense packing |
| Give the packer more time for a tighter fit | `--time 20` | 12 seconds/sheet |

Everything the output produces is already on a **`Cut`** layer with a cut colour,
so laser software (LightBurn and similar) recognises it as a cut with no extra
setup on your end.

---

## Good to know

- **It never changes your original file.** It only writes new sheet files.
- The pieces are packed tightly and rotated to fit; on a typical design it uses
  a sheet or two fewer than a naive layout.
- If a single piece is bigger than one sheet, it gets its own sheet and a warning
  — that piece just can't fit the laser bed at that size.
- Some fill/hatch shapes in the original are ignored on purpose — they duplicate
  the outlines that actually get cut.

---

*Questions or something looks off? The person who set this up can help — there's a
technical writeup in `CLAUDE.md` and `docs/` if they need it.*
