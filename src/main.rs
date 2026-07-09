//! twister-splitter — split a DXF of overlapping/stacked objects into separate
//! laser-cuttable sheets, bin-packing the pieces so none exceeds a sheet size.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use dxf::Drawing;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

use twister_splitter::emit::{self, Placed};
use twister_splitter::extract::{self, Piece, Sources};
use twister_splitter::nest;
use twister_splitter::pack::{self, PackConfig};

/// Split a DXF into laser-cuttable sheets no larger than a given size.
#[derive(Parser)]
#[command(name = "twister-splitter", version, about)]
struct Cli {
  /// Input DXF file.
  input: PathBuf,

  /// Output directory for the generated sheet files.
  #[arg(short, long, default_value = "out")]
  out_dir: PathBuf,

  /// Sheet size as WxH in DXF units (millimetres for Illustrator exports).
  #[arg(short, long, default_value = "400x400", value_parser = parse_size)]
  size: (f64, f64),

  /// Spacing (kerf) between neighbouring parts, in DXF units.
  #[arg(short, long, default_value_t = 2.0)]
  kerf: f64,

  /// Usable inset kept clear on every sheet edge, in DXF units.
  #[arg(long, default_value_t = 0.0)]
  margin: f64,

  /// Kerf compensation: offset each cut outline outward by half this value so
  /// finished parts are dimensionally correct (0 = off). When on, outlines are
  /// emitted as polylines (curved outlines approximated).
  #[arg(long, default_value_t = 0.0)]
  kerf_comp: f64,

  /// Disallow 90° rotation of pieces during nesting.
  #[arg(long)]
  no_rotate: bool,

  /// Which piece sources to pack: layer, block, or both.
  #[arg(long, value_enum, default_value_t = SourcesArg::Both)]
  sources: SourcesArg,

  /// Packing engine: `nest` (shape-aware, dense) or `rect` (bounding-box).
  #[arg(long, value_enum, default_value_t = PackerArg::Nest)]
  packer: PackerArg,

  /// Nesting optimizer time budget per sheet, in seconds (nest packer only).
  /// Higher = tighter packing, fewer sheets.
  #[arg(long, default_value_t = 12.0)]
  time: f64,

  /// Nest this many copies of every piece (R3-1; nest packer only). 1 = one of
  /// each. The GUI can set copies per-piece; this flag sets a uniform count.
  #[arg(long, default_value_t = 1)]
  copies: usize,

  /// Engrave each piece's assembly number as TEXT on the `Engrave` layer (R3-2),
  /// so a stacked/layered model can be re-stacked in order.
  #[arg(long)]
  engrave: bool,

  /// Micro-tab (holding-bridge) length in DXF units (0 = off): leaves uncut gaps
  /// so fully-cut parts stay attached to the sheet (R3-3). Emits outlines as open
  /// polylines (curved outlines approximated). Use with `--tab-count`.
  #[arg(long, default_value_t = 0.0)]
  tab_width: f64,

  /// Number of micro-tabs per outline ring (R3-3; used when `--tab-width` > 0).
  #[arg(long, default_value_t = 4)]
  tab_count: usize,

  /// Split each loose part's outer contour onto a `Cut-Outer` layer and its holes
  /// onto `Cut-Inner` (R1-1b), for separate laser operations. Loose parts only;
  /// block pieces stay on `Cut`.
  #[arg(long)]
  split_layers: bool,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum SourcesArg {
  Layer,
  Block,
  Both,
}

#[derive(Clone, Copy, PartialEq, clap::ValueEnum)]
enum PackerArg {
  /// Shape-aware polygon nesting (jagua-rs): tightest material usage.
  Nest,
  /// Axis-aligned bounding-box packing (MaxRects): fast, looser.
  Rect,
}

impl From<SourcesArg> for Sources {
  fn from(a: SourcesArg) -> Self {
    match a {
      SourcesArg::Layer => Sources::Layer,
      SourcesArg::Block => Sources::Block,
      SourcesArg::Both => Sources::Both,
    }
  }
}

fn parse_size(s: &str) -> Result<(f64, f64), String> {
  let (w, h) = s
    .split_once(['x', 'X', '*'])
    .ok_or_else(|| format!("expected WxH, got '{s}'"))?;
  let w: f64 = w.trim().parse().map_err(|_| format!("bad width in '{s}'"))?;
  let h: f64 = h.trim().parse().map_err(|_| format!("bad height in '{s}'"))?;
  if w <= 0.0 || h <= 0.0 {
    return Err("width and height must be positive".into());
  }
  Ok((w, h))
}

fn main() -> ExitCode {
  // Bare launch (no CLI args) opens the desktop GUI; any args run the CLI below.
  // With `--no-default-features` (the CLI-only Windows/headless build) this block
  // is compiled out, so a bare invocation falls through to clap's usage output.
  #[cfg(feature = "gui")]
  if std::env::args_os().len() <= 1 {
    return match twister_splitter::gui::run() {
      Ok(()) => ExitCode::SUCCESS,
      Err(e) => {
        eprintln!("error: GUI failed to start: {e}");
        ExitCode::FAILURE
      }
    };
  }

  let cli = Cli::parse();

  let drawing = match Drawing::load_file(&cli.input) {
    Ok(d) => d,
    Err(e) => {
      eprintln!("error: failed to load '{}': {e}", cli.input.display());
      return ExitCode::FAILURE;
    }
  };

  let (mut pieces, diags) = extract::extract(&drawing, cli.sources.into());
  // Apply a uniform copy count (R3-1). The nest packer's `build_items` reserves
  // one item per copy; the rect packer ignores it.
  if cli.copies != 1 {
    for p in &mut pieces {
      p.quantity = cli.copies;
    }
  }
  for d in &diags {
    let prefix = match d.severity {
      twister_splitter::diag::Severity::Warning => "warning",
      twister_splitter::diag::Severity::Info => "note",
    };
    match &d.piece_label {
      Some(label) => eprintln!("{prefix}: [{label}] {}", d.message),
      None => eprintln!("{prefix}: {}", d.message),
    }
  }
  if pieces.is_empty() {
    eprintln!("error: no cuttable pieces found in '{}'", cli.input.display());
    return ExitCode::FAILURE;
  }
  println!("Extracted {} piece(s).", pieces.len());

  let (placed, oversized) = match cli.packer {
    PackerArg::Rect => pack_rect(&pieces, &cli),
    PackerArg::Nest => pack_nest(&drawing, &pieces, &cli),
  };

  let sheet_count = placed.iter().map(|p| p.sheet).max().map_or(0, |m| m + 1);
  println!(
    "Packed into {sheet_count} sheet(s) of {}x{} (kerf {}) via {}.",
    cli.size.0,
    cli.size.1,
    cli.kerf,
    match cli.packer {
      PackerArg::Nest => "shape nesting",
      PackerArg::Rect => "bbox packing",
    }
  );
  for label in &oversized {
    eprintln!("warning: piece '{label}' exceeds the sheet size; placed on its own sheet uncut-to-fit");
  }

  let stem = cli
    .input
    .file_stem()
    .and_then(|s| s.to_str())
    .unwrap_or("out");
  let emit_opts = emit::EmitOptions {
    kerf_comp: cli.kerf_comp,
    engrave_numbers: cli.engrave,
    tab_width: cli.tab_width,
    tab_count: cli.tab_count,
    split_cut_layers: cli.split_layers,
  };
  match emit::emit_opts(&drawing, &pieces, &placed, &cli.out_dir, stem, emit_opts) {
    Ok(report) => {
      for d in &report.diagnostics {
        println!("  note: {}", d.message);
      }
      for f in &report.files {
        println!("  wrote {}", f.display());
      }
      ExitCode::SUCCESS
    }
    Err(e) => {
      eprintln!("error: writing output: {e}");
      ExitCode::FAILURE
    }
  }
}

/// Bounding-box packing (MaxRects). Returns per-piece placements and the labels
/// of oversized pieces.
fn pack_rect(pieces: &[Piece], cli: &Cli) -> (Vec<Placed>, Vec<String>) {
  let cfg = PackConfig {
    sheet_w: cli.size.0,
    sheet_h: cli.size.1,
    kerf: cli.kerf,
    allow_rotation: !cli.no_rotate,
  };
  let bboxes: Vec<_> = pieces.iter().map(|p| p.bbox).collect();
  let placements = pack::pack(&bboxes, &cfg);
  let oversized = placements
    .iter()
    .filter(|p| p.oversized)
    .map(|p| pieces[p.piece_index].label.clone())
    .collect();
  let placed = placements
    .iter()
    .map(|p| p.to_placed(&pieces[p.piece_index].bbox))
    .collect();
  (placed, oversized)
}

/// Shape-aware nesting (jagua-rs). Flattens each piece to a polygon, nests, then
/// appends any piece too large for a sheet on its own sheet.
fn pack_nest(drawing: &Drawing, pieces: &[Piece], cli: &Cli) -> (Vec<Placed>, Vec<String>) {
  let items = nest::build_items_with(drawing, pieces, cli.kerf_comp);

  // Debug: dump the piece polygons in sparrow/jagua strip-packing JSON format.
  if let Some(path) = std::env::var_os("TS_NEST_JSON") {
    let mut s = format!("{{\"name\":\"twister\",\"strip_height\":{},\"items\":[", cli.size.1);
    for (i, it) in items.iter().enumerate() {
      if i > 0 {
        s.push(',');
      }
      s.push_str(&format!("{{\"id\":{i},\"demand\":1,\"shape\":{{\"type\":\"simple_polygon\",\"data\":["));
      for (k, p) in it.polygon.iter().enumerate() {
        if k > 0 {
          s.push(',');
        }
        s.push_str(&format!("[{},{}]", p[0], p[1]));
      }
      s.push_str("]}}");
    }
    s.push_str("]}");
    std::fs::write(&path, s).unwrap();
    eprintln!("wrote nest JSON ({} items) to {:?}", items.len(), path);
    std::process::exit(0);
  }

  // Animated spinner while sparrow optimizes each sheet (seconds per sheet).
  // Hidden when stderr is not a terminal (piped/redirected).
  let bar = ProgressBar::new_spinner();
  if std::io::stderr().is_terminal() {
    bar.set_style(ProgressStyle::with_template("  nesting {spinner:.green} [{elapsed}] {msg}").unwrap());
    bar.enable_steady_tick(std::time::Duration::from_millis(120));
    bar.set_message("optimizing sheet 1…");
  } else {
    bar.set_draw_target(ProgressDrawTarget::hidden());
  }

  // Surface hull-fallback pieces (non-simple outline nested by convex hull).
  // With `--copies`, a piece yields several identical items, so warn once each.
  let mut warned_hull = std::collections::HashSet::new();
  for it in &items {
    if it.hull_fallback && warned_hull.insert(it.piece_index) {
      eprintln!(
        "note: [{}] outline was non-simple; nested by its convex hull",
        pieces[it.piece_index].label
      );
    }
  }

  let explore = std::time::Duration::from_secs_f64(cli.time * 0.8);
  let compress = std::time::Duration::from_secs_f64(cli.time * 0.2);
  let bboxes: Vec<_> = pieces.iter().map(|p| p.bbox).collect();
  let outcome = twister_splitter::optimize::nest_sheets(
    &items,
    &bboxes,
    cli.size.0,
    cli.size.1,
    cli.kerf,
    cli.margin,
    0x9E37_79B9_7F4A_7C15,
    explore,
    compress,
    None, // CLI has no cancellation signal
    |event| {
      if let twister_splitter::optimize::NestEvent::SheetCompleted { sheet, .. } = event {
        bar.set_message(format!("optimizing sheet {}…", sheet + 2));
      }
    },
  );
  bar.finish_and_clear();

  let placed = outcome.placed;
  let oversized_labels: Vec<String> = outcome.oversized.iter().map(|&pi| pieces[pi].label.clone()).collect();

  if std::env::var_os("TS_DEBUG").is_some() {
    for p in &placed {
      eprintln!(
        "[dbg] sheet {:2} rot={:6.1}° {}{}",
        p.sheet,
        p.transform.rotation().to_degrees(),
        pieces[p.piece_index].label,
        if p.oversized { "  [OVERSIZED]" } else { "" },
      );
    }
  }

  (placed, oversized_labels)
}
