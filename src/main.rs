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
use twister_splitter::geom::Affine;
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
  let cli = Cli::parse();

  let drawing = match Drawing::load_file(&cli.input) {
    Ok(d) => d,
    Err(e) => {
      eprintln!("error: failed to load '{}': {e}", cli.input.display());
      return ExitCode::FAILURE;
    }
  };

  let pieces = extract::extract(&drawing, cli.sources.into());
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
  match emit::emit(&drawing, &pieces, &placed, &cli.out_dir, stem) {
    Ok(report) => {
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
  let items = nest::build_items(drawing, pieces);

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

  let explore = std::time::Duration::from_secs_f64(cli.time * 0.8);
  let compress = std::time::Duration::from_secs_f64(cli.time * 0.2);
  let result = twister_splitter::optimize::nest_sparrow(
    &items,
    cli.size.0,
    cli.size.1,
    cli.kerf,
    0x9E37_79B9_7F4A_7C15,
    explore,
    compress,
    |done_sheets| bar.set_message(format!("optimizing sheet {}…", done_sheets + 1)),
  );
  bar.finish_and_clear();

  let mut placed = result.placed;
  // Pieces too large for any sheet each get their own sheet, recentred to the
  // sheet origin like the rectangle packer's oversized path.
  let mut oversized_labels = Vec::new();
  for (k, &pi) in result.oversized.iter().enumerate() {
    let piece = &pieces[pi];
    placed.push(Placed {
      piece_index: pi,
      sheet: result.sheets + k,
      transform: Affine::place(&piece.bbox, 0.0, 0.0, 0.0),
      oversized: true,
    });
    oversized_labels.push(piece.label.clone());
  }

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
