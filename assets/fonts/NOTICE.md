# Bundled fonts

## JetBrains Mono (`JetBrainsMono-Regular.ttf`, `JetBrainsMono-Bold.ttf`)

Used for all numerics, file paths, dimensions and keyboard shortcuts per the
Slate design. Licensed under the **SIL Open Font License 1.1** (OFL), which
permits bundling and redistribution inside an application. Upstream + full
license text: https://github.com/JetBrains/JetBrainsMono (see its `OFL.txt`).

These are embedded into the binary via `include_bytes!` in `src/gui.rs`
(`fonts::install`).

## Sans / UI proportional font — NOT bundled

The Slate spec calls for a Helvetica-substitute sans as the primary UI face.
Helvetica/Arial are proprietary and cannot be redistributed, so the proportional
family currently falls back to egui's built-in default. To ship a specific sans,
drop a redistributable TTF here (e.g. `Inter-Regular.ttf`) and add it to
`fonts::install` — the `FontDefinitions` wiring already reserves the slot.
