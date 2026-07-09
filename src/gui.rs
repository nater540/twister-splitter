//! Desktop GUI for twister-splitter (eframe/egui) — the **Slate** design.
//!
//! Slate is an Adobe-Illustrator-inspired dark workspace: a single main window
//! with a brand strip, menu bar, a contextual NEST control bar, a left setup
//! panel, a right packing-stats panel, a bottom status bar, and a central area
//! holding sheet tabs over a custom-painted nesting canvas.
//!
//! ## Shape
//!
//! * All visual tokens (colours, fonts, metrics) live in [`theme`] — the design
//!   is skinned in one place.
//! * State lives in [`App`]. Actions that *do* something (open/nest/export/…)
//!   are pushed as [`Intent`]s and applied after layout in one unidirectional
//!   pass; pure view toggles (show-grid, params) mutate state directly since
//!   they only affect the next frame or the next nest, never mid-render work.
//! * [`layout`] is the single panel-layout function (shared with any future
//!   kittest harness, so the tested layout can't drift from the shipped one).
//! * Nesting is CPU-heavy, so it runs on a background thread and reports through
//!   the library's UI-free `FnMut` callback. Backend gaps this UI still needs
//!   are tracked in `docs/backend-changes.md`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Vec2};

use dxf::Drawing;

use crate::emit::{self, Placed};
use crate::extract::{self, Piece, PieceKind, Sources};
use crate::flatten;
use crate::geom::Bbox;
use crate::nest::{self, NestItem};
use crate::optimize;
use crate::pack::{self, PackConfig};
use crate::stats::{self, CutProfile};
use crate::svg;

/// Launch the desktop app.
pub fn run() -> eframe::Result<()> {
  let native_options = eframe::NativeOptions {
    viewport: egui::ViewportBuilder::default()
      .with_inner_size([1280.0, 820.0])
      .with_min_inner_size([980.0, 640.0])
      .with_title("Twister Splitter"),
    ..Default::default()
  };
  eframe::run_native(
    "Twister Splitter",
    native_options,
    Box::new(|cc| {
      // Fonts and visuals must be applied inside the creation closure so the
      // first frame is already skinned.
      let font_report = theme::install_fonts(&cc.egui_ctx);
      cc.egui_ctx.set_visuals(theme::visuals());
      Ok(Box::new(App::new(font_report)))
    }),
  )
}

// ---------------------------------------------------------------------------
// Theme: the single place the Slate design is expressed.
// ---------------------------------------------------------------------------

mod theme {
  use super::*;
  use egui::{FontData, FontDefinitions, FontFamily};

  // Embedded JetBrains Mono (OFL-1.1). Used for numerics / paths / shortcuts.
  const JBMONO_REGULAR: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf");
  const JBMONO_BOLD: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf");

  /// A named family for the bold mono face (stat/yield numbers).
  pub const MONO_BOLD: &str = "jbmono-bold";

  // --- Slate colour tokens (from the design spec) ------------------------
  const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
  }
  /// White at straight-alpha `a` (0..=255).
  pub const fn white_a(a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied_const(255, 255, 255, a)
  }
  /// Accent blue at straight-alpha `a`.
  pub const fn accent_a(a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied_const(47, 143, 237, a)
  }

  pub const ACCENT: Color32 = rgb(0x49, 0x90, 0xe0);
  pub const ACCENT_HOVER: Color32 = rgb(0x5a, 0x9f, 0xf0);
  pub const PANEL: Color32 = rgb(0x2b, 0x2c, 0x31);
  pub const WINDOW_BODY: Color32 = rgb(0x21, 0x22, 0x26);
  pub const BAR_DARK: Color32 = rgb(0x1a, 0x1b, 0x1e);
  pub const MENU_BAR: Color32 = rgb(0x26, 0x27, 0x2b);
  pub const CANVAS_BG: Color32 = rgb(0x10, 0x11, 0x14);
  pub const BOARD: Color32 = rgb(0x14, 0x14, 0x16);
  pub const INPUT_INSET: Color32 = rgb(0x16, 0x17, 0x1a);
  pub const CARD: Color32 = rgb(0x23, 0x24, 0x29);
  pub const BORDER_STRONG: Color32 = rgb(0x3a, 0x3b, 0x40);
  pub const BORDER_HAIRLINE: Color32 = rgb(0x13, 0x13, 0x16);
  pub const BORDER_CARD: Color32 = rgb(0x34, 0x35, 0x3a);
  pub const TEXT_PRIMARY: Color32 = rgb(0xe6, 0xe6, 0xe6);
  pub const TEXT_SECONDARY: Color32 = white_a(140);
  pub const TEXT_MUTED: Color32 = white_a(107);
  pub const OK_GREEN: Color32 = rgb(0x5b, 0xb6, 0x7a);
  pub const WARN_AMBER: Color32 = rgb(0xe0, 0xa5, 0x4a);
  pub const CUT_INNER: Color32 = white_a(82);
  pub const GRID_LINE: Color32 = white_a(11);
  pub const TAB_ACTIVE: Color32 = rgb(0x2f, 0x30, 0x36);
  pub const TAB_INACTIVE: Color32 = rgb(0x19, 0x1a, 0x1d);

  // --- Font helpers ------------------------------------------------------
  pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
  }
  pub fn mono_bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(MONO_BOLD.into()))
  }
  pub fn sans(size: f32) -> FontId {
    FontId::proportional(size)
  }

  /// Whether a custom sans (proportional) face was loaded, so the app can flag
  /// that it fell back to egui's default sans.
  pub struct FontReport {
    pub sans_is_default: bool,
  }

  /// Register fonts. JetBrains Mono is embedded; the proportional/sans face
  /// stays on egui's default unless a redistributable TTF is dropped into
  /// `assets/fonts/Sans-Regular.ttf` (the slot is wired so that's all it takes).
  pub fn install_fonts(ctx: &egui::Context) -> FontReport {
    let mut defs = FontDefinitions::default();

    defs
      .font_data
      .insert("jbmono".into(), Arc::new(FontData::from_static(JBMONO_REGULAR)));
    defs
      .font_data
      .insert(MONO_BOLD.into(), Arc::new(FontData::from_static(JBMONO_BOLD)));

    // JetBrains Mono leads the Monospace family (egui's default mono + emoji
    // fallback stay behind it).
    defs
      .families
      .entry(FontFamily::Monospace)
      .or_default()
      .insert(0, "jbmono".into());
    // A dedicated bold-mono family for numbers, with regular as fallback.
    defs
      .families
      .insert(FontFamily::Name(MONO_BOLD.into()), vec![MONO_BOLD.into(), "jbmono".into()]);

    // Optional runtime sans: dev builds can pick up a TTF from the source tree;
    // shipped builds should embed one instead.
    let mut sans_is_default = true;
    let sans_path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/fonts/Sans-Regular.ttf");
    if let Ok(bytes) = std::fs::read(sans_path) {
      defs.font_data.insert("sans".into(), Arc::new(FontData::from_owned(bytes)));
      defs
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "sans".into());
      sans_is_default = false;
    }

    ctx.set_fonts(defs);
    FontReport { sans_is_default }
  }

  /// Base Visuals for the Slate palette.
  pub fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.override_text_color = Some(TEXT_PRIMARY);
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.window_stroke = Stroke::new(1.0, BORDER_STRONG);
    v.window_corner_radius = CornerRadius::same(8);
    v.extreme_bg_color = INPUT_INSET;
    v.faint_bg_color = WINDOW_BODY;
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0, ACCENT);

    let cr = CornerRadius::same(5);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_SECONDARY);
    v.widgets.noninteractive.corner_radius = cr;

    v.widgets.inactive.bg_fill = INPUT_INSET;
    v.widgets.inactive.weak_bg_fill = INPUT_INSET;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT_SECONDARY);
    v.widgets.inactive.corner_radius = cr;

    v.widgets.hovered.bg_fill = ACCENT;
    v.widgets.hovered.weak_bg_fill = MENU_BAR;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    v.widgets.hovered.corner_radius = cr;

    v.widgets.active.bg_fill = ACCENT_HOVER;
    v.widgets.active.weak_bg_fill = ACCENT;
    v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT_HOVER);
    v.widgets.active.corner_radius = cr;

    v.widgets.open.bg_fill = ACCENT;
    v.widgets.open.weak_bg_fill = MENU_BAR;
    v.widgets.open.corner_radius = cr;
    v
  }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Packer {
  Nest,
  Rect,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExportFormat {
  Dxf,
  Svg,
}

/// User-tunable job parameters. Some (marked "display/future") are surfaced by
/// the UI but not yet consumed by the backend — see `docs/backend-changes.md`.
#[derive(Clone)]
struct Params {
  stock_w: f64,
  stock_h: f64,
  material: String,
  thickness_mm: f64,
  /// Gap between parts; wired to the backend's `kerf` nesting separation.
  spacing_mm: f64,
  /// Usable sheet inset; now enforced by the nester (S-2) and drawn on the canvas.
  sheet_margin_mm: f64,
  /// Laser kerf compensation (display/future — not yet applied to geometry).
  kerf_mm: f64,
  allow_rotation: bool,
  nest_into_holes: bool, // display/future
  mirror_allowed: bool,  // display/future
  packer: Packer,
  time: f64,
  sources: Sources,
  export_format: ExportFormat,
  combine_single: bool, // display/future
}

impl Default for Params {
  fn default() -> Self {
    Params {
      stock_w: 400.0,
      stock_h: 400.0,
      material: "Plywood · 3 mm".into(),
      thickness_mm: 3.0,
      spacing_mm: 2.0,
      sheet_margin_mm: 6.0,
      kerf_mm: 0.0,
      allow_rotation: true,
      nest_into_holes: true,
      mirror_allowed: false,
      packer: Packer::Nest,
      time: 12.0,
      sources: Sources::Both,
      export_format: ExportFormat::Dxf,
      combine_single: false,
    }
  }
}

/// A loaded drawing plus everything derived from it that the UI reads while
/// painting. Rings are pre-flattened once at load so repaint stays cheap.
struct Model {
  path: PathBuf,
  drawing: Drawing,
  pieces: Vec<Piece>,
  /// `piece_rings[i]` = outline rings of piece `i`, in the piece's own frame.
  piece_rings: Vec<Vec<Vec<[f64; 2]>>>,
  /// Index of the largest-area ring of each piece (its outer contour), or
  /// `usize::MAX` when the piece has no rings.
  outer_ring: Vec<usize>,
  input_bbox: Bbox,
  entity_count: usize,
  layer_count: usize,
  /// Warnings from extraction (skipped inserts, non-unit scale, dropped parts)
  /// plus hull-fallback notes, for the diagnostics area.
  diagnostics: Vec<crate::diag::Diagnostic>,
}

struct Outcome {
  placed: Vec<Placed>,
  oversized_labels: Vec<String>,
  sheets: usize,
  elapsed: Duration,
}

enum Job {
  Idle,
  Running {
    progress: Arc<AtomicUsize>,
    /// Set true to ask the worker to stop the in-flight nest promptly.
    cancel: Arc<AtomicBool>,
    started: Instant,
    rx: Receiver<Outcome>,
  },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
  Input,
  Sheets,
}

/// How a nest run treats existing placements (P2-9 pin-and-re-nest).
#[derive(Clone, Copy, PartialEq, Eq)]
enum NestMode {
  /// Re-pack everything; only `locked` parts stay pinned. Empty pin set on a
  /// fresh file ⇒ a from-scratch pack.
  Global,
  /// Re-pack only the active sheet's unlocked parts; every other sheet and any
  /// locked part on the active sheet stays put.
  CurrentSheet,
}

enum Intent {
  OpenFileDialog,
  Load(PathBuf),
  ReExtract,
  StartNest(NestMode),
  CancelNest,
  SetView(View),
  ShowSheet(usize),
  SelectPiece(Option<usize>),
  Export,
  ShowNewJob(bool),
  Status(String),
  /// Rotate the selected placement by `quarter_turns` × 90° (S-5).
  RotateSelected(i32),
  /// Mirror the selected placement about its footprint centre (S-4).
  FlipHSelected,
  FlipVSelected,
  MoveSelectedToSheet(usize),
  ToggleLockSelected,
  RecomputeStats,
}

pub struct App {
  model: Option<Model>,
  params: Params,
  job: Job,
  outcome: Option<Outcome>,
  view: View,
  active_sheet: usize,
  /// Selection keyed on the stable `Piece::id` so it survives re-extraction.
  sel_id: Option<u64>,
  /// Per-sheet stats from `crate::stats`, cached at nest completion — NOT
  /// recomputed every frame.
  stats_cache: Vec<stats::SheetStats>,
  /// Notices from the most recent export (e.g. the kerf-comp polyline note).
  export_diagnostics: Vec<crate::diag::Diagnostic>,
  status: String,
  show_new_job: bool,
  // Canvas view state.
  zoom: f32,
  pan: Vec2,
  show_grid: bool,
  show_labels: bool,
  show_margins: bool,
  sans_is_default: bool,
  intents: Vec<Intent>,
}

impl App {
  fn new(fonts: theme::FontReport) -> Self {
    let mut app = App {
      model: None,
      params: Params::default(),
      job: Job::Idle,
      outcome: None,
      view: View::Input,
      active_sheet: 0,
      sel_id: None,
      stats_cache: Vec::new(),
      export_diagnostics: Vec::new(),
      status: "Open a DXF to begin — File ▸ Open DXF…".into(),
      show_new_job: false,
      zoom: 1.0,
      pan: Vec2::ZERO,
      show_grid: true,
      show_labels: true,
      show_margins: true,
      sans_is_default: fonts.sans_is_default,
      intents: Vec::new(),
    };
    app.apply_demo_hook();
    app
  }

  /// Non-interactive launch into a populated state, for screenshots / smoke
  /// testing (no effect unless the env var is set):
  ///
  /// * `TS_GUI_DEMO=<path.dxf>` — auto-load that DXF and immediately Split &
  ///   Nest, so the app opens straight into the packed Slate workspace. Uses a
  ///   short optimizer budget so the result appears within a few seconds.
  /// * `TS_GUI_DEMO_MODAL=1` — additionally open the New Nesting Job modal.
  ///
  /// The queued intents drain on the first frame (Load runs synchronously, then
  /// StartNest spawns the worker).
  fn apply_demo_hook(&mut self) {
    let Some(path) = std::env::var_os("TS_GUI_DEMO") else { return };
    self.params.time = 1.5; // short per-sheet budget for a fast populated capture
    self.intents.push(Intent::Load(PathBuf::from(path)));
    self.intents.push(Intent::StartNest(NestMode::Global));
    if std::env::var_os("TS_GUI_DEMO_MODAL").is_some() {
      self.intents.push(Intent::ShowNewJob(true));
    }
  }

  fn is_running(&self) -> bool {
    matches!(self.job, Job::Running { .. })
  }
}

// ---------------------------------------------------------------------------
// eframe wiring + unidirectional loop
// ---------------------------------------------------------------------------

impl eframe::App for App {
  fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
    let ctx = ui.ctx().clone();
    self.poll_job(&ctx);
    layout(self, ui);
    let intents = std::mem::take(&mut self.intents);
    for intent in intents {
      self.apply_intent(intent, &ctx);
    }
  }
}

impl App {
  fn poll_job(&mut self, ctx: &egui::Context) {
    if let Job::Running { rx, .. } = &self.job {
      match rx.try_recv() {
        Ok(outcome) => {
          let over = outcome.oversized_labels.len();
          self.status = format!(
            "Nested {} part(s) onto {} sheet(s) in {:.1}s{}.",
            outcome.placed.len(),
            outcome.sheets,
            outcome.elapsed.as_secs_f32(),
            if over > 0 { format!(" · {over} oversized") } else { String::new() },
          );
          self.outcome = Some(outcome);
          self.view = View::Sheets;
          self.active_sheet = 0;
          self.job = Job::Idle;
          self.recompute_stats();
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => {
          ctx.request_repaint_after(Duration::from_millis(100));
        }
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
          self.status = "Nesting thread ended unexpectedly.".into();
          self.job = Job::Idle;
        }
      }
    }
  }

  fn apply_intent(&mut self, intent: Intent, ctx: &egui::Context) {
    match intent {
      Intent::OpenFileDialog => {
        if let Some(path) = rfd::FileDialog::new().add_filter("DXF", &["dxf"]).pick_file() {
          self.intents.push(Intent::Load(path));
        }
      }
      Intent::Load(path) => self.load(path),
      Intent::ReExtract => self.reextract(),
      Intent::StartNest(mode) => self.start_nest(ctx, mode),
      Intent::CancelNest => {
        // Cooperative cancel: signal the worker to stop promptly, then drop it.
        // It observes the flag between sparrow iterations and stops burning CPU.
        if let Job::Running { cancel, .. } = &self.job {
          cancel.store(true, Ordering::Relaxed);
        }
        self.job = Job::Idle;
        self.status = "Nesting cancelled.".into();
      }
      Intent::SetView(v) => self.view = v,
      Intent::ShowSheet(s) => {
        self.active_sheet = s;
        self.view = View::Sheets;
      }
      Intent::SelectPiece(p) => {
        // Map the picked index to its stable id so selection survives re-nesting.
        self.sel_id = p.and_then(|i| self.model.as_ref().and_then(|m| m.pieces.get(i)).map(|pc| pc.id));
      }
      Intent::Export => self.export(),
      Intent::ShowNewJob(v) => self.show_new_job = v,
      Intent::Status(s) => self.status = s,
      Intent::RotateSelected(qt) => {
        if self.edit_selected(|pl, bbox| pl.rotate(bbox, qt)) {
          self.status = "Rotated selected part.".into();
        }
      }
      Intent::FlipHSelected => {
        if self.edit_selected(|pl, bbox| pl.flip_h(bbox)) {
          self.status = "Flipped selected part horizontally.".into();
        }
      }
      Intent::FlipVSelected => {
        if self.edit_selected(|pl, bbox| pl.flip_v(bbox)) {
          self.status = "Flipped selected part vertically.".into();
        }
      }
      Intent::MoveSelectedToSheet(s) => {
        if self.edit_selected(|pl, _| pl.move_to_sheet(s)) {
          self.status = format!("Moved selected part to sheet {}.", s + 1);
        }
      }
      Intent::ToggleLockSelected => {
        let mut locked = false;
        if self.edit_selected(|pl, _| {
          pl.locked = !pl.locked;
          locked = pl.locked;
        }) {
          self.status = if locked { "Locked selected part." } else { "Unlocked selected part." }.into();
        }
      }
      Intent::RecomputeStats => self.recompute_stats(),
    }
  }

  /// Apply `f` to the placement of the currently-selected piece (found by its
  /// stable id), then refresh the stats cache. Returns false with a hint if
  /// nothing is selected or the piece isn't placed on any sheet.
  fn edit_selected(&mut self, f: impl FnOnce(&mut Placed, &Bbox)) -> bool {
    let Some(id) = self.sel_id else {
      self.status = "Select a part first (click it on the canvas).".into();
      return false;
    };
    let mut changed = false;
    if let Some(model) = &self.model
      && let Some(pi) = model.pieces.iter().position(|p| p.id == id)
    {
      let bbox = model.pieces[pi].bbox;
      if let Some(outcome) = &mut self.outcome
        && let Some(pl) = outcome.placed.iter_mut().find(|pl| pl.piece_index == pi)
      {
        f(pl, &bbox);
        changed = true;
      }
    }
    if changed {
      self.recompute_stats();
    }
    changed
  }

  fn load(&mut self, path: PathBuf) {
    match Drawing::load_file(&path) {
      Ok(drawing) => {
        let entity_count = drawing.entities().count();
        let mut layers = std::collections::BTreeSet::new();
        for e in drawing.entities() {
          layers.insert(e.common.layer.clone());
        }
        self.model = Some(Model {
          path,
          drawing,
          pieces: Vec::new(),
          piece_rings: Vec::new(),
          outer_ring: Vec::new(),
          input_bbox: Bbox::empty(),
          entity_count,
          layer_count: layers.len(),
          diagnostics: Vec::new(),
        });
        self.outcome = None;
        self.sel_id = None;
        self.stats_cache.clear();
        self.export_diagnostics.clear();
        self.view = View::Input;
        self.zoom = 1.0;
        self.pan = Vec2::ZERO;
        self.reextract();
      }
      Err(e) => self.status = format!("Failed to load: {e}"),
    }
  }

  fn reextract(&mut self) {
    let Some(model) = &mut self.model else { return };
    let (pieces, mut diagnostics) = extract::extract(&model.drawing, self.params.sources);
    // Add hull-fallback notes (non-simple outline nested by convex hull).
    for it in nest::build_items(&model.drawing, &pieces) {
      if it.hull_fallback {
        diagnostics.push(
          crate::diag::Diagnostic::info("outline was non-simple; nested by its convex hull")
            .for_piece(pieces[it.piece_index].label.clone()),
        );
      }
    }
    let mut bbox = Bbox::empty();
    let mut outer_ring = Vec::with_capacity(pieces.len());
    let piece_rings: Vec<Vec<Vec<[f64; 2]>>> = pieces
      .iter()
      .map(|p| {
        let rings = gather_piece_rings(&model.drawing, p);
        // Outer contour = largest-area ring; the rest are treated as holes.
        let mut best = (usize::MAX, 0.0f64);
        for (ri, ring) in rings.iter().enumerate() {
          let a = if ring.len() >= 3 { flatten::area(ring).abs() } else { 0.0 };
          if a > best.1 {
            best = (ri, a);
          }
          for &[x, y] in ring {
            bbox.add_point(x, y);
          }
        }
        outer_ring.push(best.0);
        rings
      })
      .collect();
    let warn_count = diagnostics
      .iter()
      .filter(|d| d.severity == crate::diag::Severity::Warning)
      .count();
    self.status = format!(
      "{} — {} part(s) · {} entities · {} layers{}.",
      model.path.file_name().and_then(|s| s.to_str()).unwrap_or("input.dxf"),
      pieces.len(),
      model.entity_count,
      model.layer_count,
      if warn_count > 0 { format!(" · {warn_count} warning(s)") } else { String::new() },
    );
    model.pieces = pieces;
    model.piece_rings = piece_rings;
    model.outer_ring = outer_ring;
    model.input_bbox = bbox;
    model.diagnostics = diagnostics;
    self.outcome = None;
    self.stats_cache.clear();
    self.sel_id = None;
  }

  fn start_nest(&mut self, ctx: &egui::Context, mode: NestMode) {
    let Some(model) = &self.model else { return };
    if self.is_running() {
      return;
    }
    // Compose the pin set from the current placements (P2-9). Global pins only
    // locked parts; CurrentSheet also pins every part on the other sheets so
    // only the active sheet's unlocked parts move. Empty ⇒ from-scratch pack.
    let fixed: Vec<Placed> = match (&self.outcome, mode) {
      (Some(o), NestMode::Global) => o.placed.iter().filter(|p| p.locked).cloned().collect(),
      (Some(o), NestMode::CurrentSheet) => o
        .placed
        .iter()
        .filter(|p| p.sheet != self.active_sheet || p.locked)
        .cloned()
        .collect(),
      (None, _) => Vec::new(),
    };
    let params = self.params.clone();
    let (tx, rx) = channel();
    let progress = Arc::new(AtomicUsize::new(0));
    let cancel = Arc::new(AtomicBool::new(false));
    let started = Instant::now();

    // Everything the worker needs is prepared here and moved in.
    // Grow each reservation to contain the kerf-compensated (enlarged) outline
    // so the nest layout matches what `emit` cuts with the same `kerf_mm`.
    let items: Vec<NestItem> = nest::build_items_with(&model.drawing, &model.pieces, self.params.kerf_mm);
    let bboxes: Vec<Bbox> = model.pieces.iter().map(|p| p.bbox).collect();
    let labels: Vec<String> = model.pieces.iter().map(|p| p.label.clone()).collect();

    let pin_count = fixed.len();
    let prog = progress.clone();
    let cancel_worker = cancel.clone();
    let ctx_worker = ctx.clone();
    std::thread::spawn(move || {
      let t0 = Instant::now();
      let mut outcome = run_pack(&params, &items, &bboxes, &labels, &fixed, &prog, cancel_worker, &ctx_worker);
      outcome.elapsed = t0.elapsed();
      let _ = tx.send(outcome);
      ctx_worker.request_repaint();
    });

    self.status = if pin_count == 0 {
      "Nesting…".into()
    } else {
      format!("Re-nesting around {pin_count} pinned part(s)…")
    };
    self.job = Job::Running { progress, cancel, started, rx };
    ctx.request_repaint();
  }

  fn export(&mut self) {
    let (Some(model), Some(outcome)) = (&self.model, &self.outcome) else {
      self.status = "Nothing to export yet — run Split & Nest first.".into();
      return;
    };
    let Some(dir) = rfd::FileDialog::new().pick_folder() else { return };
    let stem = model.path.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
    let (w, h) = (self.params.stock_w, self.params.stock_h);
    let kerf = self.params.kerf_mm;
    let result: std::io::Result<(usize, Vec<crate::diag::Diagnostic>)> = match self.params.export_format {
      ExportFormat::Dxf => {
        // `emit`'s trailing `kerf_comp`: > 0 applies compensation (curved cuts
        // approximated as polylines, flagged via diagnostics); 0 keeps splines.
        emit::emit(&model.drawing, &model.pieces, &outcome.placed, &dir, stem, kerf).map(|r| (r.files.len(), r.diagnostics))
      }
      ExportFormat::Svg => {
        // SVG keeps faithful outlines — the kerf-comp polyline path is DXF-only.
        svg::write_svg(&model.drawing, &model.pieces, &outcome.placed, &dir, stem, w, h, self.params.combine_single)
          .map(|files| (files.len(), Vec::new()))
      }
    };
    match result {
      Ok((n, diags)) => {
        let note = diags.first().map(|d| format!(" · {}", d.message)).unwrap_or_default();
        self.status = format!("Wrote {n} file(s) to {}{note}.", dir.display());
        self.export_diagnostics = diags;
      }
      Err(e) => self.status = format!("Export failed: {e}"),
    }
  }

  /// Recompute the per-sheet stats cache from the current model + outcome.
  /// Called once when a nest finishes, not per frame.
  fn recompute_stats(&mut self) {
    let cut = self.material().cut;
    self.stats_cache = match (&self.model, &self.outcome) {
      (Some(m), Some(o)) => stats::all_sheet_stats(
        &m.pieces,
        &m.drawing,
        &o.placed,
        o.sheets,
        self.params.stock_w,
        self.params.stock_h,
        cut,
      ),
      _ => Vec::new(),
    };
  }

  /// Build a [`stats::Material`] from the setup-panel selection. Feed rates are a
  /// UI-side table until the backend ships a material catalog (S-8); its
  /// `CutProfile` drives the Est. run estimate.
  fn material(&self) -> stats::Material {
    let cut = match self.params.material.as_str() {
      "Plywood · 6 mm" => CutProfile { feed_mm_s: 12.0, pierce_s: 0.8 },
      "MDF · 3 mm" => CutProfile { feed_mm_s: 18.0, pierce_s: 0.5 },
      "Acrylic · 3 mm" => CutProfile { feed_mm_s: 15.0, pierce_s: 0.4 },
      _ => CutProfile { feed_mm_s: 20.0, pierce_s: 0.5 }, // Plywood · 3 mm
    };
    stats::Material { name: self.params.material.clone(), thickness_mm: self.params.thickness_mm, cut }
  }

  /// Cached stats for the active sheet (see [`App::recompute_stats`]).
  fn active_stats(&self) -> Option<&stats::SheetStats> {
    self.stats_cache.get(self.active_sheet)
  }
}

#[allow(clippy::too_many_arguments)]
fn run_pack(
  params: &Params,
  items: &[NestItem],
  bboxes: &[Bbox],
  labels: &[String],
  fixed: &[Placed],
  progress: &AtomicUsize,
  cancel: Arc<AtomicBool>,
  ctx: &egui::Context,
) -> Outcome {
  match params.packer {
    Packer::Rect => {
      let cfg = PackConfig {
        sheet_w: params.stock_w,
        sheet_h: params.stock_h,
        kerf: params.spacing_mm,
        allow_rotation: params.allow_rotation,
      };
      let placements = pack::pack(bboxes, &cfg);
      let oversized_labels = placements
        .iter()
        .filter(|p| p.oversized)
        .map(|p| labels[p.piece_index].clone())
        .collect();
      let placed: Vec<Placed> = placements.iter().map(|p| p.to_placed(&bboxes[p.piece_index])).collect();
      let sheets = placed.iter().map(|p| p.sheet).max().map_or(0, |m| m + 1);
      Outcome { placed, oversized_labels, sheets, elapsed: Duration::ZERO }
    }
    Packer::Nest => {
      // With pins → the deterministic jagua path that keeps `fixed` parts put
      // (P2-9); otherwise a from-scratch sparrow pack. Both stream the same
      // events and append oversized free pieces on their own sheets. The event
      // closure is inlined per branch so it stays higher-ranked over the
      // `NestEvent<'_>` borrow (a shared closure fails HRTB inference).
      let outcome = if fixed.is_empty() {
        let explore = Duration::from_secs_f64(params.time * 0.8);
        let compress = Duration::from_secs_f64(params.time * 0.2);
        optimize::nest_sheets(
          items,
          bboxes,
          params.stock_w,
          params.stock_h,
          params.spacing_mm,
          params.sheet_margin_mm,
          0x9E37_79B9_7F4A_7C15,
          explore,
          compress,
          Some(cancel),
          |event| {
            if let optimize::NestEvent::SheetCompleted { sheet, .. } = event {
              progress.store(sheet, Ordering::Relaxed);
              ctx.request_repaint();
            }
          },
        )
      } else {
        optimize::nest_sheets_pinned(
          items,
          bboxes,
          fixed,
          params.stock_w,
          params.stock_h,
          params.spacing_mm,
          params.sheet_margin_mm,
          Some(cancel),
          |event| {
            if let optimize::NestEvent::SheetCompleted { sheet, .. } = event {
              progress.store(sheet, Ordering::Relaxed);
              ctx.request_repaint();
            }
          },
        )
      };
      let oversized_labels = outcome.oversized.iter().map(|&pi| labels[pi].clone()).collect();
      Outcome { placed: outcome.placed, oversized_labels, sheets: outcome.sheets, elapsed: Duration::ZERO }
    }
  }
}

fn gather_piece_rings(drawing: &Drawing, piece: &Piece) -> Vec<Vec<[f64; 2]>> {
  let mut rings = Vec::new();
  match &piece.kind {
    PieceKind::Loose(entities) => {
      for e in entities {
        flatten::entity_rings(e, &mut rings);
      }
    }
    PieceKind::Insert { block_name, .. } => {
      if let Some(b) = drawing.blocks().find(|b| &b.name == block_name) {
        for e in &b.entities {
          flatten::entity_rings(e, &mut rings);
        }
      }
    }
  }
  rings
}

/// Format a duration in seconds as `M:SS` for the run-time estimate.
fn fmt_duration(secs: f64) -> String {
  let s = secs.round().max(0.0) as u64;
  format!("{}:{:02}", s / 60, s % 60)
}

// ---------------------------------------------------------------------------
// Layout (single shared panel-layout function)
// ---------------------------------------------------------------------------

fn frame(fill: Color32, mx: i8, my: i8) -> egui::Frame {
  egui::Frame::NONE.fill(fill).inner_margin(egui::Margin::symmetric(mx, my))
}

/// Lay out every panel. Add order (spec): title → menu → control → status →
/// left → right → central.
fn layout(app: &mut App, ui: &mut egui::Ui) {
  egui::Panel::top("titlebar")
    .exact_size(38.0)
    .frame(frame(theme::BAR_DARK, 12, 0))
    .show(ui, |ui| title_bar(app, ui));
  egui::Panel::top("menubar")
    .exact_size(30.0)
    .frame(frame(theme::MENU_BAR, 6, 0))
    .show(ui, |ui| menu_bar(app, ui));
  egui::Panel::top("controlbar")
    .exact_size(40.0)
    .frame(frame(theme::PANEL, 14, 0))
    .show(ui, |ui| control_bar(app, ui));
  egui::Panel::bottom("statusbar")
    .exact_size(26.0)
    .frame(frame(theme::BAR_DARK, 12, 0))
    .show(ui, |ui| status_bar(app, ui));
  egui::Panel::left("setup")
    .exact_size(250.0)
    .resizable(false)
    .frame(frame(theme::PANEL, 0, 0))
    .show(ui, |ui| setup_panel(app, ui));
  egui::Panel::right("stats")
    .exact_size(256.0)
    .resizable(false)
    .show_separator_line(false)
    .frame(frame(theme::PANEL, 14, 0))
    .show(ui, |ui| stats_panel(app, ui));
  // Sheet tabs sit above the canvas but only across the central column — added
  // after the side panels, this top strip spans just their gap (nesting a
  // CentralPanel inside another made the inner one overdraw the right panel).
  egui::Panel::top("sheettabs")
    .exact_size(36.0)
    .frame(frame(theme::WINDOW_BODY, 8, 0))
    .show(ui, |ui| sheet_tabs(app, ui));
  egui::CentralPanel::default()
    .frame(egui::Frame::NONE.fill(theme::CANVAS_BG))
    .show(ui, |ui| canvas(app, ui));

  if app.show_new_job {
    new_job_dialog(app, ui.ctx());
  }
}

// ---- Title bar ------------------------------------------------------------

fn title_bar(app: &mut App, ui: &mut egui::Ui) {
  ui.horizontal_centered(|ui| {
    // App glyph: rounded accent square with a white "twist" mark.
    let (r, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(r, CornerRadius::same(4), theme::ACCENT);
    let c = r.center();
    let s = 3.0;
    p.line_segment([c + Vec2::new(-s, -s), c + Vec2::new(s, s)], Stroke::new(1.6, Color32::WHITE));
    p.line_segment([c + Vec2::new(s, -s), c + Vec2::new(-s, s)], Stroke::new(1.6, Color32::WHITE));

    ui.add_space(8.0);
    ui.label(egui::RichText::new("Twister Splitter").font(theme::sans(12.5)).strong().color(theme::TEXT_PRIMARY));
    if let Some(model) = &app.model {
      let name = model.path.file_name().and_then(|s| s.to_str()).unwrap_or("");
      ui.label(egui::RichText::new(format!("— {name}")).font(theme::sans(11.0)).color(theme::white_a(90)));
    }

    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
      if app.sans_is_default {
        ui.label(egui::RichText::new("sans: egui default").font(theme::mono(10.0)).color(theme::TEXT_MUTED))
          .on_hover_text("Drop a redistributable sans TTF at assets/fonts/Sans-Regular.ttf to replace egui's default sans.");
      }
    });
  });
}

// ---- Menu bar -------------------------------------------------------------

/// A menu action row with a right-aligned shortcut; returns true when clicked.
fn menu_item(ui: &mut egui::Ui, label: &str, shortcut: &str) -> bool {
  let clicked = ui.add(egui::Button::new(label).shortcut_text(shortcut).min_size(Vec2::new(190.0, 22.0))).clicked();
  if clicked {
    ui.close();
  }
  clicked
}

/// A menu toggle row (checkbox). Mutates `flag` directly (view-only state).
fn menu_toggle(ui: &mut egui::Ui, flag: &mut bool, label: &str) {
  if ui.checkbox(flag, label).clicked() {
    ui.close();
  }
}

/// A placeholder menu row for actions not yet wired to the backend.
fn menu_todo(app: &mut App, ui: &mut egui::Ui, label: &str, shortcut: &str) {
  if menu_item(ui, label, shortcut) {
    app.intents.push(Intent::Status(format!("'{}' isn't implemented yet.", label.trim_end_matches('…'))));
  }
}

fn menu_bar(app: &mut App, ui: &mut egui::Ui) {
  ui.horizontal_centered(|ui| {
    ui.menu_button("File", |ui| {
      if menu_item(ui, "New Nesting Job…", "⌘N") {
        app.intents.push(Intent::ShowNewJob(true));
      }
      if menu_item(ui, "Open DXF…", "⌘O") {
        app.intents.push(Intent::OpenFileDialog);
      }
      ui.separator();
      if menu_item(ui, "Import DXF…", "⇧⌘I") {
        app.intents.push(Intent::OpenFileDialog);
      }
      if menu_item(ui, "Reload Source", "⌘R") {
        app.intents.push(Intent::ReExtract);
      }
      ui.separator();
      if menu_item(ui, "Export Active Sheet…", "⌘E") {
        app.intents.push(Intent::Export);
      }
      menu_todo(app, ui, "Export All Sheets…", "⇧⌘E");
      ui.separator();
      menu_todo(app, ui, "Save Layout", "⌘S");
      if menu_item(ui, "Quit", "⌘Q") {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
      }
    });
    ui.menu_button("Edit", |ui| {
      menu_todo(app, ui, "Undo", "⌘Z");
      menu_todo(app, ui, "Redo", "⇧⌘Z");
      ui.separator();
      menu_todo(app, ui, "Select All Parts", "⌘A");
      if menu_item(ui, "Deselect", "⌘D") {
        app.intents.push(Intent::SelectPiece(None));
      }
      ui.separator();
      menu_todo(app, ui, "Delete From Nest", "⌫");
    });
    ui.menu_button("View", |ui| {
      menu_todo(app, ui, "Zoom In", "⌘+");
      menu_todo(app, ui, "Zoom Out", "⌘−");
      menu_todo(app, ui, "Fit Sheet", "⌘0");
      menu_todo(app, ui, "Actual Size", "⌘1");
      ui.separator();
      menu_toggle(ui, &mut app.show_grid, "Show Grid");
      menu_toggle(ui, &mut app.show_labels, "Show Part Labels");
      menu_toggle(ui, &mut app.show_margins, "Show Sheet Margins");
    });
    ui.menu_button("Nest", |ui| {
      if menu_item(ui, "Split & Nest", "⌘⏎") {
        app.intents.push(Intent::StartNest(NestMode::Global));
      }
      ui.add_enabled_ui(app.outcome.is_some(), |ui| {
        if menu_item(ui, "Re-nest Current Sheet", "⌘R") {
          app.intents.push(Intent::StartNest(NestMode::CurrentSheet));
        }
      });
      ui.separator();
      menu_toggle(ui, &mut app.params.allow_rotation, "Allow Part Rotation");
      // Shown disabled with an honest reason (per the user's decisions): jagua
      // can't model holes, and the sparrow optimizer is rotation-only so it
      // can't mirror during nesting (manual Flip H/V is the mirroring path).
      ui.add_enabled_ui(false, |ui| ui.checkbox(&mut app.params.nest_into_holes, "Nest Parts Into Holes"))
        .response
        .on_hover_text("Requires nester hole support — not yet available.");
      ui.add_enabled_ui(false, |ui| ui.checkbox(&mut app.params.mirror_allowed, "Mirror Allowed"))
        .response
        .on_hover_text("Manual Flip H/V only — automatic mirroring isn't supported by the nester.");
      ui.separator();
      // Engine choice (an extension beyond the spec's menu): the dense shape
      // nester vs. the instant bounding-box packer for a quick preview.
      let mut engine = app.params.packer;
      ui.radio_value(&mut engine, Packer::Nest, "Engine: Dense (shape nest)");
      ui.radio_value(&mut engine, Packer::Rect, "Engine: Fast (bounding box)");
      app.params.packer = engine;
    });
    ui.menu_button("Arrange", |ui| {
      let has_sel = app.sel_id.is_some();
      let sheets = app.outcome.as_ref().map_or(0, |o| o.sheets);
      ui.add_enabled_ui(has_sel, |ui| {
        if menu_item(ui, "Rotate 90° CW", "⌘]") {
          app.intents.push(Intent::RotateSelected(-1));
        }
        if menu_item(ui, "Rotate 90° CCW", "⌘[") {
          app.intents.push(Intent::RotateSelected(1));
        }
      });
      ui.add_enabled_ui(has_sel, |ui| {
        if menu_item(ui, "Flip Horizontal", "⌘H") {
          app.intents.push(Intent::FlipHSelected);
        }
        if menu_item(ui, "Flip Vertical", "") {
          app.intents.push(Intent::FlipVSelected);
        }
      });
      ui.separator();
      ui.add_enabled_ui(has_sel && sheets > 1, |ui| {
        ui.menu_button("Move to Sheet", |ui| {
          for s in 0..sheets {
            if ui.button(format!("Sheet {}", s + 1)).clicked() {
              app.intents.push(Intent::MoveSelectedToSheet(s));
              ui.close();
            }
          }
        });
      });
      ui.add_enabled_ui(has_sel, |ui| {
        if menu_item(ui, "Lock Position", "⌘L") {
          app.intents.push(Intent::ToggleLockSelected);
        }
      });
    });
    ui.menu_button("Export", |ui| {
      if menu_item(ui, "Export Active Sheet…", "⌘E") {
        app.intents.push(Intent::Export);
      }
      menu_todo(app, ui, "Export All Sheets…", "⇧⌘E");
      ui.separator();
      let mut fmt = app.params.export_format;
      if ui.radio_value(&mut fmt, ExportFormat::Dxf, "Format: DXF").clicked() {
        ui.close();
      }
      if ui.radio_value(&mut fmt, ExportFormat::Svg, "Format: SVG").clicked() {
        ui.close();
      }
      app.params.export_format = fmt;
      ui.separator();
      menu_toggle(ui, &mut app.params.combine_single, "Combine Into Single File");
    });
    ui.menu_button("Help", |ui| {
      menu_todo(app, ui, "Keyboard Shortcuts", "");
      menu_todo(app, ui, "About Twister Splitter", "");
    });
  });
}

// ---- Control bar ----------------------------------------------------------

fn control_bar(app: &mut App, ui: &mut egui::Ui) {
  ui.horizontal_centered(|ui| {
    ui.label(egui::RichText::new("NEST").font(theme::sans(11.0)).strong().color(theme::white_a(115)));
    ui.add_space(10.0);
    numeric_chip(ui, "Spacing", &mut app.params.spacing_mm, 0.0..=50.0, 0.1);
    numeric_chip(ui, "Margin", &mut app.params.sheet_margin_mm, 0.0..=100.0, 0.5);
    numeric_chip(ui, "Kerf", &mut app.params.kerf_mm, 0.0..=5.0, 0.01);

    ui.add_space(6.0);
    ui.label(egui::RichText::new("Rotation").font(theme::sans(12.0)).color(theme::TEXT_SECONDARY));
    toggle_pill(ui, &mut app.params.allow_rotation);

    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
      if outline_button(ui, "Export", 28.0).clicked() {
        app.intents.push(Intent::Export);
      }
      ui.add_space(8.0);
      let can_nest = app.model.is_some() && !app.is_running();
      ui.add_enabled_ui(can_nest, |ui| {
        if primary_button(ui, "Re-nest", 28.0).clicked() {
          app.intents.push(Intent::StartNest(NestMode::Global));
        }
      });
      if app.is_running() && outline_button(ui, "Cancel", 28.0).clicked() {
        app.intents.push(Intent::CancelNest);
      }
    });
  });
}

// ---- Setup panel (left) ---------------------------------------------------

fn setup_panel(app: &mut App, ui: &mut egui::Ui) {
  // Reserve the pinned bottom button, then scroll the sections above it.
  let full = ui.available_rect_before_wrap();
  let btn_h = 36.0;
  let btn_rect = Rect::from_min_max(
    Pos2::new(full.left() + 14.0, full.bottom() - btn_h - 14.0),
    Pos2::new(full.right() - 14.0, full.bottom() - 14.0),
  );

  let mut body = ui.new_child(
    egui::UiBuilder::new().max_rect(Rect::from_min_max(full.min, Pos2::new(full.right(), btn_rect.top() - 8.0))),
  );
  egui::ScrollArea::vertical().auto_shrink([false, false]).show(&mut body, |ui| {
    ui.add_space(6.0);
    section_header(ui, "Stock");
    ui.horizontal(|ui| {
      ui.add_space(14.0);
      labeled_number(ui, "Width", &mut app.params.stock_w, "mm", 100.0);
      labeled_number(ui, "Height", &mut app.params.stock_h, "mm", 100.0);
    });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
      ui.add_space(14.0);
      field_label(ui, "Material");
    });
    ui.horizontal(|ui| {
      ui.add_space(14.0);
      egui::ComboBox::from_id_salt("material")
        .selected_text(egui::RichText::new(&app.params.material).font(theme::sans(12.0)))
        .width(210.0)
        .show_ui(ui, |ui| {
          for m in ["Plywood · 3 mm", "Plywood · 6 mm", "MDF · 3 mm", "Acrylic · 3 mm"] {
            if ui.selectable_label(app.params.material == m, m).clicked() {
              app.params.material = m.into();
              app.params.thickness_mm = m
                .rsplit("· ")
                .next()
                .and_then(|s| s.trim_end_matches(" mm").parse().ok())
                .unwrap_or(app.params.thickness_mm);
              // Material feeds the cut profile → refresh Est. run immediately.
              app.intents.push(Intent::RecomputeStats);
            }
          }
        });
    });

    divider(ui);
    section_header(ui, "Source DXF");
    let src_name = app
      .model
      .as_ref()
      .and_then(|m| m.path.file_name().and_then(|s| s.to_str()))
      .unwrap_or("no file loaded")
      .to_string();
    ui.horizontal(|ui| {
      ui.add_space(14.0);
      inset_text(ui, &src_name, 148.0);
      if browse_pill(ui, "Browse").clicked() {
        app.intents.push(Intent::OpenFileDialog);
      }
    });
    if let Some(model) = &app.model {
      ui.horizontal(|ui| {
        ui.add_space(14.0);
        ui.label(
          egui::RichText::new(format!("{} entities · {} layers", model.entity_count, model.layer_count))
            .font(theme::mono(10.5))
            .color(theme::TEXT_MUTED),
        );
      });
      // Diagnostics from extraction / flattening (skipped inserts, non-unit
      // scales, hull fallbacks). Full text on hover.
      if !model.diagnostics.is_empty() {
        let warns = model
          .diagnostics
          .iter()
          .filter(|d| d.severity == crate::diag::Severity::Warning)
          .count();
        let (color, label) = if warns > 0 {
          (theme::WARN_AMBER, format!("⚠ {warns} warning(s), {} note(s)", model.diagnostics.len() - warns))
        } else {
          (theme::TEXT_MUTED, format!("{} note(s)", model.diagnostics.len()))
        };
        ui.horizontal(|ui| {
          ui.add_space(14.0);
          let resp = ui.label(egui::RichText::new(label).font(theme::mono(10.5)).color(color));
          resp.on_hover_ui(|ui| {
            for d in &model.diagnostics {
              let text = match &d.piece_label {
                Some(l) => format!("[{l}] {}", d.message),
                None => d.message.clone(),
              };
              ui.label(egui::RichText::new(text).font(theme::mono(10.5)).color(theme::TEXT_SECONDARY));
            }
          });
        });
      }
      // Notices from the last export (e.g. kerf-comp emitted outlines as
      // polylines) — surfaced so the fidelity tradeoff is visible.
      if !app.export_diagnostics.is_empty() {
        ui.horizontal(|ui| {
          ui.add_space(14.0);
          let label = format!("⚑ export: {} note(s)", app.export_diagnostics.len());
          let resp = ui.label(egui::RichText::new(label).font(theme::mono(10.5)).color(theme::WARN_AMBER));
          resp.on_hover_ui(|ui| {
            for d in &app.export_diagnostics {
              ui.label(egui::RichText::new(&d.message).font(theme::mono(10.5)).color(theme::TEXT_SECONDARY));
            }
          });
        });
      }
    }

    divider(ui);
    section_header(ui, "Output");
    ui.horizontal(|ui| {
      ui.add_space(14.0);
      let out = app
        .model
        .as_ref()
        .and_then(|m| m.path.parent().and_then(|p| p.to_str()))
        .unwrap_or("~/cut/")
        .to_string();
      inset_text(ui, &out, 148.0);
      if browse_pill(ui, "Browse").clicked() {
        app.intents.push(Intent::Status("Output directory is chosen at export time.".into()));
      }
    });
    ui.add_space(10.0);
  });

  // Pinned primary button.
  let enabled = app.model.is_some() && !app.is_running();
  let resp = ui.put(
    btn_rect,
    egui::Button::new(egui::RichText::new("Split & Nest").font(theme::sans(13.0)).strong().color(Color32::WHITE))
      .fill(if enabled { theme::ACCENT } else { theme::BORDER_STRONG })
      .corner_radius(CornerRadius::same(6)),
  );
  if enabled && resp.clicked() {
    app.intents.push(Intent::StartNest(NestMode::Global));
  }
}

// ---- Stats panel (right) --------------------------------------------------

fn stats_panel(app: &mut App, ui: &mut egui::Ui) {
  // The panel frame supplies the 14px side margins, so content flows edge-to-edge
  // within the ~228px content width — no per-widget insets, and stat rows stay
  // bounded (no overflow past the window edge).
  ui.add_space(6.0);
  section_header_inline(ui, "Packing Summary");
  ui.add_space(6.0);

  let stats = app.active_stats();
  let sheets = app.outcome.as_ref().map_or(0, |o| o.sheets);

  let card = egui::Frame::NONE
    .fill(theme::CARD)
    .stroke(Stroke::new(1.0, theme::BORDER_CARD))
    .corner_radius(CornerRadius::same(7))
    .inner_margin(egui::Margin::same(14));
  card.show(ui, |ui| {
    let y = stats.map_or(0.0, |s| s.utilization * 100.0);
    ui.label(egui::RichText::new(format!("{y:.0}%")).font(theme::mono_bold(30.0)).color(theme::TEXT_PRIMARY));
    ui.label(egui::RichText::new("material yield").font(theme::sans(11.0)).color(theme::TEXT_MUTED));
    ui.add_space(8.0);
    progress_bar(ui, y / 100.0);
    ui.add_space(6.0);
    let (used, waste) = stats.map_or((0.0, 0.0), |s| {
      let used_m2 = s.used_area / 1_000_000.0;
      (used_m2, (s.sheet_area / 1_000_000.0 - used_m2).max(0.0))
    });
    ui.label(
      egui::RichText::new(format!("used {used:.3} m²   waste {waste:.3} m²"))
        .font(theme::mono(10.5))
        .color(theme::TEXT_MUTED),
    );
  });

  ui.add_space(10.0);
  ui.horizontal(|ui| {
    stat_tile(ui, stats.map_or(0, |s| s.piece_count), "parts on sheet");
    ui.add_space(8.0);
    stat_tile(ui, sheets, "total sheets");
  });

  ui.add_space(12.0);
  section_header_inline(ui, "Active Sheet");
  ui.add_space(4.0);
  stat_row(ui, "Dimensions", &format!("{:.0} × {:.0}", app.params.stock_w, app.params.stock_h));
  stat_row(ui, "Cut length", &stats.map_or("—".into(), |s| format!("{:.1} m", s.cut_len_mm / 1000.0)));
  stat_row(ui, "Pierces", &stats.map_or("—".into(), |s| s.pierces.to_string()));
  stat_row(ui, "Est. run", &stats.map_or("—".into(), |s| fmt_duration(s.est_run_secs)));

  // Pinned bottom secondary button.
  let full = ui.available_rect_before_wrap();
  let btn_rect = Rect::from_min_max(
    Pos2::new(full.left(), full.bottom() - 34.0 - 14.0),
    Pos2::new(full.right(), full.bottom() - 14.0),
  );
  let resp = ui.put(
    btn_rect,
    egui::Button::new(egui::RichText::new("Export this sheet").font(theme::sans(12.5)).color(theme::TEXT_PRIMARY))
      .fill(theme::INPUT_INSET)
      .stroke(Stroke::new(1.0, theme::BORDER_STRONG))
      .corner_radius(CornerRadius::same(6)),
  );
  if resp.clicked() {
    app.intents.push(Intent::Export);
  }
}

// ---- Sheet tabs -----------------------------------------------------------

fn sheet_tabs(app: &mut App, ui: &mut egui::Ui) {
  ui.horizontal_centered(|ui| {
    let sheets = app.outcome.as_ref().map_or(0, |o| o.sheets);
    if sheets == 0 {
      ui.label(
        egui::RichText::new("Input preview — run Split & Nest to produce sheets")
          .font(theme::sans(12.0))
          .color(theme::TEXT_MUTED),
      );
      return;
    }
    for s in 0..sheets {
      let active = app.view == View::Sheets && app.active_sheet == s;
      let good = app.stats_cache.get(s).is_none_or(|st| st.utilization >= 0.55);
      if sheet_tab(ui, s, active, good).clicked() {
        app.intents.push(Intent::ShowSheet(s));
      }
      ui.add_space(6.0);
    }
    ui.label(egui::RichText::new("+").font(theme::sans(15.0)).color(theme::TEXT_MUTED));

    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
      let mut v = app.view;
      ui.selectable_value(&mut v, View::Sheets, "Sheets");
      ui.selectable_value(&mut v, View::Input, "Input");
      if v != app.view {
        app.intents.push(Intent::SetView(v));
      }
    });
  });
}

fn sheet_tab(ui: &mut egui::Ui, index: usize, active: bool, good: bool) -> egui::Response {
  let size = Vec2::new(78.0, 26.0);
  let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
  let p = ui.painter();
  let (fill, border, text) = if active {
    (theme::TAB_ACTIVE, theme::ACCENT, theme::TEXT_PRIMARY)
  } else {
    (theme::TAB_INACTIVE, theme::BORDER_HAIRLINE, theme::white_a(140))
  };
  p.rect(rect, CornerRadius { nw: 6, ne: 6, sw: 0, se: 0 }, fill, Stroke::new(1.0, border), StrokeKind::Inside);
  let sq = Rect::from_min_size(Pos2::new(rect.left() + 9.0, rect.center().y - 4.0), Vec2::splat(8.0));
  p.rect_filled(sq, CornerRadius::same(2), if good { theme::OK_GREEN } else { theme::WARN_AMBER });
  p.text(
    Pos2::new(sq.right() + 7.0, rect.center().y),
    Align2::LEFT_CENTER,
    format!("Sheet {}", index + 1),
    theme::sans(12.0),
    text,
  );
  resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, active, format!("sheet {}", index + 1)));
  resp
}

// ---- Status bar -----------------------------------------------------------

fn status_bar(app: &mut App, ui: &mut egui::Ui) {
  ui.horizontal_centered(|ui| {
    if let Job::Running { progress, started, .. } = &app.job {
      ui.spinner();
      let done = progress.load(Ordering::Relaxed);
      ui.label(
        egui::RichText::new(format!("Nesting — sheet {} — {:.0}s", done + 1, started.elapsed().as_secs_f32()))
          .font(theme::mono(11.0))
          .color(theme::TEXT_SECONDARY),
      );
    } else {
      let y = app.active_stats().map_or(0.0, |s| s.utilization * 100.0);
      let sheets = app.outcome.as_ref().map_or(0, |o| o.sheets);
      if sheets > 0 {
        ui.label(egui::RichText::new(format!("{y:.0}%")).font(theme::mono(11.0)).color(theme::TEXT_SECONDARY));
        ui.label(egui::RichText::new("·").color(theme::TEXT_MUTED));
        ui.label(
          egui::RichText::new(format!("Sheet {} of {}", app.active_sheet + 1, sheets))
            .font(theme::mono(11.0))
            .color(theme::TEXT_SECONDARY),
        );
      } else {
        ui.label(egui::RichText::new(&app.status).font(theme::mono(11.0)).color(theme::TEXT_MUTED));
      }
    }

    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
      let (dot, text) = if app.is_running() { (theme::WARN_AMBER, "Working") } else { (theme::OK_GREEN, "Ready") };
      ui.label(egui::RichText::new(&app.status).font(theme::mono(11.0)).color(theme::TEXT_MUTED));
      ui.add_space(8.0);
      // Hand-paint the status dot — the "●" glyph is missing from the mono face.
      let (dot_rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
      ui.painter().circle_filled(dot_rect.center(), 4.0, dot);
      ui.label(egui::RichText::new(text).font(theme::mono(11.0)).color(dot));
    });
  });
}

// ---------------------------------------------------------------------------
// Canvas (custom-painted nesting preview, per the SheetCanvas spec)
// ---------------------------------------------------------------------------

fn canvas(app: &mut App, ui: &mut egui::Ui) {
  let size = ui.available_size();
  let (response, painter) = ui.allocate_painter(size, Sense::click_and_drag());
  response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Other, true, "nesting canvas"));
  let rect = response.rect;
  painter.rect_filled(rect, CornerRadius::ZERO, theme::CANVAS_BG);
  paint_dot_grid(&painter, rect);

  let Some(model) = &app.model else {
    painter.text(rect.center(), Align2::CENTER_CENTER, "Open a DXF to preview", theme::sans(15.0), theme::TEXT_MUTED);
    return;
  };

  // Pan / zoom interaction.
  if response.dragged() {
    app.pan += response.drag_delta();
  }
  if response.hovered() {
    let scroll = ui.input(|i| i.smooth_scroll_delta.y);
    if scroll != 0.0 {
      app.zoom = (app.zoom * (1.0 + scroll * 0.001)).clamp(0.2, 8.0);
    }
  }

  let (fit_box, placements): (Bbox, Vec<&Placed>) = match app.view {
    View::Input => (model.input_bbox, Vec::new()),
    View::Sheets => (
      Bbox { min_x: 0.0, min_y: 0.0, max_x: app.params.stock_w, max_y: app.params.stock_h },
      app
        .outcome
        .as_ref()
        .map(|o| o.placed.iter().filter(|p| p.sheet == app.active_sheet).collect())
        .unwrap_or_default(),
    ),
  };
  if fit_box.is_empty() {
    return;
  }
  let map = Mapper::fit(fit_box, rect, 30.0, app.zoom, app.pan);

  // In Sheets view, clip parts to the stock board so nothing bleeds over the
  // sheet edge onto the empty canvas (placed parts always sit within the board).
  let part_painter = if app.view == View::Sheets {
    let tl = map.to_screen([0.0, app.params.stock_h]);
    let br = map.to_screen([app.params.stock_w, 0.0]);
    paint_board(app, &map, &painter);
    painter.with_clip_rect(Rect::from_two_pos(tl, br).intersect(rect))
  } else {
    painter.clone()
  };

  if app.view == View::Input {
    for (i, rings) in model.piece_rings.iter().enumerate() {
      paint_part(&part_painter, rings, model.outer_ring[i], &|p| map.to_screen(p), app.sel_id == Some(model.pieces[i].id));
    }
  } else {
    for pl in &placements {
      let rings = &model.piece_rings[pl.piece_index];
      let xf = pl.transform;
      let to_screen = |p: [f64; 2]| {
        let (wx, wy) = xf.apply(p[0], p[1]);
        map.to_screen([wx, wy])
      };
      paint_part(&part_painter, rings, model.outer_ring[pl.piece_index], &to_screen, app.sel_id == Some(model.pieces[pl.piece_index].id));
      if app.show_labels {
        paint_part_label(&part_painter, rings, &to_screen, pl.piece_index);
      }
      // A locked part is pinned; a future re-nest keeps it in place (P2-9).
      if pl.locked
        && let Some((min, max)) = screen_bbox(rings, &to_screen)
      {
        paint_lock_badge(&part_painter, Pos2::new(max.x - 3.0, min.y + 3.0));
      }
    }
  }

  // Click-pick selection by screen bbox (a click that wasn't a drag).
  if response.clicked()
    && let Some(pos) = response.interact_pointer_pos()
  {
    let pick = pick_part(app, model, &map, pos);
    app.intents.push(Intent::SelectPiece(pick));
  }
  // Right-click selects the part under the cursor, then opens its context menu.
  if response.secondary_clicked()
    && let Some(pos) = response.interact_pointer_pos()
  {
    let pick = pick_part(app, model, &map, pos);
    app.intents.push(Intent::SelectPiece(pick));
  }
  let sel_label = app.sel_id.and_then(|id| model.pieces.iter().find(|p| p.id == id).map(|p| p.label.clone()));
  let sheet_count = app.outcome.as_ref().map_or(0, |o| o.sheets);
  response.context_menu(|ui| part_context_menu(&mut app.intents, ui, sel_label.as_deref(), sheet_count));

  zoom_hud(app, ui, rect);
}

/// Right-click context menu for a placed part (S-5 actions).
fn part_context_menu(intents: &mut Vec<Intent>, ui: &mut egui::Ui, sel_label: Option<&str>, sheets: usize) {
  match sel_label {
    None => {
      ui.label(egui::RichText::new("Right-click a part to act on it").font(theme::sans(12.0)).color(theme::TEXT_MUTED));
    }
    Some(label) => {
      ui.label(egui::RichText::new(label.to_uppercase()).font(theme::sans(10.0)).color(theme::white_a(90)));
      ui.separator();
      if ui.button("Rotate 90° CW").clicked() {
        intents.push(Intent::RotateSelected(-1));
        ui.close();
      }
      if ui.button("Rotate 90° CCW").clicked() {
        intents.push(Intent::RotateSelected(1));
        ui.close();
      }
      if sheets > 1 {
        ui.menu_button("Move to Sheet", |ui| {
          for s in 0..sheets {
            if ui.button(format!("Sheet {}", s + 1)).clicked() {
              intents.push(Intent::MoveSelectedToSheet(s));
              ui.close();
            }
          }
        });
      }
      if ui.button("Lock Position").clicked() {
        intents.push(Intent::ToggleLockSelected);
        ui.close();
      }
      ui.separator();
      if ui.button("Flip Horizontal").clicked() {
        intents.push(Intent::FlipHSelected);
        ui.close();
      }
      if ui.button("Flip Vertical").clicked() {
        intents.push(Intent::FlipVSelected);
        ui.close();
      }
    }
  }
}

fn paint_board(app: &App, map: &Mapper, painter: &egui::Painter) {
  let w = app.params.stock_w;
  let h = app.params.stock_h;
  let tl = map.to_screen([0.0, h]);
  let br = map.to_screen([w, 0.0]);
  let board = Rect::from_two_pos(tl, br);
  painter.rect_filled(board.translate(Vec2::new(0.0, 10.0)), CornerRadius::same(2), Color32::from_black_alpha(120));
  painter.rect(board, CornerRadius::same(2), theme::BOARD, Stroke::new(1.2, theme::white_a(36)), StrokeKind::Inside);

  if app.show_grid {
    let step = 50.0;
    let mut g = step;
    while g < w {
      painter.line_segment([map.to_screen([g, 0.0]), map.to_screen([g, h])], Stroke::new(1.0, theme::GRID_LINE));
      g += step;
    }
    let mut g = step;
    while g < h {
      painter.line_segment([map.to_screen([0.0, g]), map.to_screen([w, g])], Stroke::new(1.0, theme::GRID_LINE));
      g += step;
    }
  }

  if app.show_margins {
    let m = app.params.sheet_margin_mm;
    if m > 0.0 && m * 2.0 < w.min(h) {
      let corners = [
        map.to_screen([m, m]),
        map.to_screen([w - m, m]),
        map.to_screen([w - m, h - m]),
        map.to_screen([m, h - m]),
      ];
      let stroke = Stroke::new(1.0, theme::accent_a(90));
      for i in 0..4 {
        dashed_segment(painter, corners[i], corners[(i + 1) % 4], stroke, 4.0, 4.0);
      }
    }
  }
}

/// Draw one part: outer contour filled + stroked in accent, holes stroked in
/// white-alpha. Strokes are constant px (non-scaling) by construction.
fn paint_part(
  painter: &egui::Painter,
  rings: &[Vec<[f64; 2]>],
  outer: usize,
  to_screen: &dyn Fn([f64; 2]) -> Pos2,
  selected: bool,
) {
  for (ri, ring) in rings.iter().enumerate() {
    if ring.len() < 2 {
      continue;
    }
    // Decimate to ≥~0.7px spacing before stroking. Flattened splines carry up to
    // 3001 sub-pixel-spaced points; egui's AA stroke tessellator throws huge
    // miter spikes at near-coincident vertices (the spokes across the canvas).
    let mut pts: Vec<Pos2> = Vec::with_capacity(ring.len());
    for &p in ring {
      let s = to_screen(p);
      if pts.last().is_none_or(|l: &Pos2| l.distance_sq(s) >= 0.5) {
        pts.push(s);
      }
    }
    if pts.len() < 2 {
      continue;
    }
    // Only close a ring whose endpoints coincide. Open curves (arcs, open
    // splines) must be drawn open — otherwise `closed_line` draws a stray chord
    // from end back to start.
    let (a, b) = (ring[0], ring[ring.len() - 1]);
    let closed = ring.len() >= 3 && (a[0] - b[0]).hypot(a[1] - b[1]) < 2.0;
    // No fill: part outlines are concave, and egui fan-fills a single shape
    // (garbage triangles across concavities). The accent stroke is the outline.
    let (width, color) = if ri == outer {
      (1.5, if selected { theme::ACCENT_HOVER } else { theme::ACCENT })
    } else {
      (1.15, theme::CUT_INNER)
    };
    stroke_ring(painter, pts, closed, Stroke::new(width, color));
  }
  if selected {
    paint_marquee(painter, rings, to_screen);
  }
}

/// Stroke a ring, splitting it at sharp direction reversals so egui's miter join
/// can't shoot a long sliver off an acute cusp. Flattened splines can contain
/// near-180° reversals (cusps / near-degenerate spikes); a single `closed_line`
/// then mitres them into straight lines that cross the whole canvas at large
/// zoom. Splitting there turns the cusp into two butt-capped endpoints (no
/// miter) while leaving the outline shape unchanged.
fn stroke_ring(painter: &egui::Painter, pts: Vec<Pos2>, closed: bool, stroke: Stroke) {
  let n = pts.len();
  if n < 2 {
    return;
  }
  let reversal = |a: Pos2, b: Pos2, c: Pos2| -> bool {
    let (d1, d2) = (b - a, c - b);
    let (l1, l2) = (d1.length(), d2.length());
    if l1 < 1e-3 || l2 < 1e-3 {
      return false;
    }
    (d1.x * d2.x + d1.y * d2.y) / (l1 * l2) < -0.7 // reversal sharper than ~134°
  };
  let any_rev = (1..n.saturating_sub(1)).any(|i| reversal(pts[i - 1], pts[i], pts[i + 1]))
    || (closed && n >= 3 && (reversal(pts[n - 1], pts[0], pts[1]) || reversal(pts[n - 2], pts[n - 1], pts[0])));
  if !any_rev {
    painter.add(if closed { Shape::closed_line(pts, stroke) } else { Shape::line(pts, stroke) });
    return;
  }
  // Split into open runs at each sharp reversal.
  let mut path = pts;
  if closed {
    path.push(path[0]);
  }
  let m = path.len();
  let mut bounds = vec![0usize];
  for i in 1..m - 1 {
    if reversal(path[i - 1], path[i], path[i + 1]) {
      bounds.push(i);
    }
  }
  bounds.push(m - 1);
  for w in bounds.windows(2) {
    if w[1] > w[0] {
      painter.add(Shape::line(path[w[0]..=w[1]].to_vec(), stroke));
    }
  }
}

fn paint_part_label(painter: &egui::Painter, rings: &[Vec<[f64; 2]>], to_screen: &dyn Fn([f64; 2]) -> Pos2, index: usize) {
  let Some((min, max)) = screen_bbox(rings, to_screen) else { return };
  let anchor = Pos2::new((min.x + max.x) * 0.5, min.y + 6.0);
  painter.text(anchor, Align2::CENTER_TOP, format!("{index}"), theme::mono(9.0), theme::white_a(107));
}

/// A small padlock marking a locked (pinned) placement, centred at `at`.
fn paint_lock_badge(painter: &egui::Painter, at: Pos2) {
  let body = Rect::from_center_size(at + Vec2::new(0.0, 2.0), Vec2::new(8.0, 6.0));
  // Shackle (open ring above the body), then the body, then a keyhole.
  painter.circle_stroke(at + Vec2::new(0.0, -1.5), 2.5, Stroke::new(1.3, theme::ACCENT));
  painter.rect_filled(body, CornerRadius::same(1), theme::ACCENT);
  painter.circle_filled(body.center(), 1.0, theme::BOARD);
}

fn paint_marquee(painter: &egui::Painter, rings: &[Vec<[f64; 2]>], to_screen: &dyn Fn([f64; 2]) -> Pos2) {
  let Some((min, max)) = screen_bbox(rings, to_screen) else { return };
  let r = Rect::from_min_max(min, max).expand(3.0);
  painter.rect_stroke(r, CornerRadius::ZERO, Stroke::new(1.5, theme::ACCENT), StrokeKind::Outside);
  for c in [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom()] {
    let h = Rect::from_center_size(c, Vec2::splat(7.0));
    painter.rect(h, CornerRadius::ZERO, theme::BOARD, Stroke::new(1.5, theme::ACCENT), StrokeKind::Middle);
  }
}

/// Screen-space bbox of a part's rings, or None if it has no points.
fn screen_bbox(rings: &[Vec<[f64; 2]>], to_screen: &dyn Fn([f64; 2]) -> Pos2) -> Option<(Pos2, Pos2)> {
  let mut min = Pos2::new(f32::MAX, f32::MAX);
  let mut max = Pos2::new(f32::MIN, f32::MIN);
  for ring in rings {
    for &p in ring {
      let s = to_screen(p);
      min = min.min(s);
      max = max.max(s);
    }
  }
  (min.x <= max.x).then_some((min, max))
}

fn pick_part(app: &App, model: &Model, map: &Mapper, pos: Pos2) -> Option<usize> {
  let candidates: Vec<(usize, crate::geom::Affine)> = match app.view {
    View::Input => (0..model.piece_rings.len()).map(|i| (i, IDENTITY)).collect(),
    View::Sheets => app
      .outcome
      .as_ref()
      .map(|o| {
        o.placed
          .iter()
          .filter(|p| p.sheet == app.active_sheet)
          .map(|p| (p.piece_index, p.transform))
          .collect()
      })
      .unwrap_or_default(),
  };
  let mut hit = None;
  for (i, xf) in candidates {
    let to_screen = |p: [f64; 2]| {
      let (wx, wy) = xf.apply(p[0], p[1]);
      map.to_screen([wx, wy])
    };
    if let Some((min, max)) = screen_bbox(&model.piece_rings[i], &to_screen)
      && Rect::from_min_max(min, max).contains(pos)
    {
      hit = Some(i); // later (top-most drawn) wins
    }
  }
  hit
}

const IDENTITY: crate::geom::Affine =
  crate::geom::Affine { m00: 1.0, m01: 0.0, m10: 0.0, m11: 1.0, tx: 0.0, ty: 0.0 };

fn zoom_hud(app: &mut App, ui: &mut egui::Ui, rect: Rect) {
  let box_rect = Rect::from_min_size(Pos2::new(rect.left() + 14.0, rect.bottom() - 40.0), Vec2::new(110.0, 26.0));
  ui.painter().rect(box_rect, CornerRadius::same(6), theme::MENU_BAR, Stroke::new(1.0, theme::BORDER_STRONG), StrokeKind::Inside);
  let minus = Rect::from_min_size(box_rect.min + Vec2::new(2.0, 2.0), Vec2::new(26.0, 22.0));
  let plus = Rect::from_min_size(Pos2::new(box_rect.right() - 28.0, box_rect.top() + 2.0), Vec2::new(26.0, 22.0));
  if ui.put(minus, egui::Button::new("−").fill(Color32::TRANSPARENT).stroke(Stroke::NONE)).clicked() {
    app.zoom = (app.zoom * 0.9).clamp(0.2, 8.0);
  }
  if ui.put(plus, egui::Button::new("+").fill(Color32::TRANSPARENT).stroke(Stroke::NONE)).clicked() {
    app.zoom = (app.zoom * 1.1).clamp(0.2, 8.0);
  }
  ui.painter().text(
    box_rect.center(),
    Align2::CENTER_CENTER,
    format!("{:.0}%", app.zoom * 100.0),
    theme::mono(11.0),
    theme::TEXT_SECONDARY,
  );
}

// ---------------------------------------------------------------------------
// New Nesting Job modal
// ---------------------------------------------------------------------------

fn new_job_dialog(app: &mut App, ctx: &egui::Context) {
  let mut open = true;
  egui::Window::new("New Nesting Job")
    .id(egui::Id::new("new_job_dialog"))
    .collapsible(false)
    .resizable(false)
    .fixed_size(Vec2::new(620.0, 0.0))
    .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
    .open(&mut open)
    .frame(
      egui::Frame::NONE
        .fill(theme::PANEL)
        .stroke(Stroke::new(1.0, theme::BORDER_STRONG))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(22)),
    )
    .show(ctx, |ui| {
      section_header_inline(ui, "Source DXF");
      ui.horizontal(|ui| {
        let name = app
          .model
          .as_ref()
          .and_then(|m| m.path.file_name().and_then(|s| s.to_str()))
          .unwrap_or("no file chosen")
          .to_string();
        inset_text(ui, &name, 460.0);
        if browse_pill(ui, "Browse…").clicked() {
          app.intents.push(Intent::OpenFileDialog);
        }
      });
      ui.add_space(14.0);
      ui.horizontal(|ui| {
        labeled_number(ui, "Width", &mut app.params.stock_w, "mm", 170.0);
        labeled_number(ui, "Height", &mut app.params.stock_h, "mm", 170.0);
        labeled_number(ui, "Spacing", &mut app.params.spacing_mm, "mm", 170.0);
      });
      ui.add_space(18.0);
      ui.separator();
      ui.add_space(6.0);
      ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let can = app.model.is_some();
        ui.add_enabled_ui(can, |ui| {
          if primary_button(ui, "Start nesting", 32.0).clicked() {
            app.intents.push(Intent::ShowNewJob(false));
            app.intents.push(Intent::StartNest(NestMode::Global));
          }
        });
        ui.add_space(8.0);
        if outline_button(ui, "Cancel", 32.0).clicked() {
          app.intents.push(Intent::ShowNewJob(false));
        }
      });
    });
  if !open {
    app.intents.push(Intent::ShowNewJob(false));
  }
}

// ---------------------------------------------------------------------------
// Reusable Slate widgets
// ---------------------------------------------------------------------------

fn section_header(ui: &mut egui::Ui, text: &str) {
  ui.add_space(9.0);
  ui.horizontal(|ui| {
    ui.add_space(14.0);
    section_header_inline(ui, text);
  });
  ui.add_space(2.0);
}

fn section_header_inline(ui: &mut egui::Ui, text: &str) {
  ui.label(egui::RichText::new(text.to_uppercase()).font(theme::sans(10.0)).strong().color(theme::white_a(107)));
}

fn field_label(ui: &mut egui::Ui, text: &str) {
  ui.label(egui::RichText::new(text).font(theme::sans(11.0)).color(theme::TEXT_SECONDARY));
}

fn divider(ui: &mut egui::Ui) {
  ui.add_space(12.0);
  let full = ui.available_rect_before_wrap();
  let y = full.top();
  ui.painter().line_segment(
    [Pos2::new(full.left() + 14.0, y), Pos2::new(full.right() - 14.0, y)],
    Stroke::new(1.0, theme::BORDER_HAIRLINE),
  );
  ui.add_space(2.0);
}

fn labeled_number(ui: &mut egui::Ui, label: &str, value: &mut f64, unit: &str, width: f32) {
  ui.vertical(|ui| {
    field_label(ui, label);
    ui.horizontal(|ui| {
      ui.add_sized(
        Vec2::new(width - 26.0, 28.0),
        egui::DragValue::new(value).range(0.0..=10000.0).speed(0.5).custom_formatter(|n, _| format!("{n:.0}")),
      );
      ui.label(egui::RichText::new(unit).font(theme::mono(10.0)).color(theme::TEXT_MUTED));
    });
  });
}

fn numeric_chip(ui: &mut egui::Ui, label: &str, value: &mut f64, range: std::ops::RangeInclusive<f64>, speed: f64) {
  ui.label(egui::RichText::new(label).font(theme::sans(12.0)).color(theme::TEXT_SECONDARY));
  ui.add_sized(Vec2::new(54.0, 24.0), egui::DragValue::new(value).range(range).speed(speed));
  ui.add_space(12.0);
}

fn inset_text(ui: &mut egui::Ui, text: &str, width: f32) {
  let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 28.0), Sense::hover());
  ui.painter().rect(rect, CornerRadius::same(4), theme::INPUT_INSET, Stroke::new(1.0, theme::BORDER_STRONG), StrokeKind::Inside);
  // Left-truncate long paths (keep the meaningful tail — filename/dir); the full
  // value is available on hover. ~6.15px per mono char at 11px.
  let font = theme::mono(11.0);
  let avail_chars = (((width - 16.0) / 6.15).floor() as usize).max(4);
  let truncated = text.chars().count() > avail_chars;
  let shown = if truncated {
    let tail: String = text.chars().rev().take(avail_chars - 1).collect::<Vec<_>>().into_iter().rev().collect();
    format!("…{tail}")
  } else {
    text.to_string()
  };
  ui.painter().text(Pos2::new(rect.left() + 8.0, rect.center().y), Align2::LEFT_CENTER, shown, font, theme::TEXT_SECONDARY);
  if truncated {
    resp.on_hover_text(text);
  }
}

fn browse_pill(ui: &mut egui::Ui, text: &str) -> egui::Response {
  ui.add_space(4.0);
  ui.add(
    egui::Button::new(egui::RichText::new(text).font(theme::sans(11.0)).color(theme::ACCENT))
      .fill(theme::accent_a(36))
      .stroke(Stroke::NONE)
      .corner_radius(CornerRadius::same(3))
      .min_size(Vec2::new(0.0, 24.0)),
  )
}

fn primary_button(ui: &mut egui::Ui, text: &str, h: f32) -> egui::Response {
  ui.add(
    egui::Button::new(egui::RichText::new(text).font(theme::sans(12.5)).strong().color(Color32::WHITE))
      .fill(theme::ACCENT)
      .stroke(Stroke::NONE)
      .corner_radius(CornerRadius::same(5))
      .min_size(Vec2::new(0.0, h)),
  )
}

fn outline_button(ui: &mut egui::Ui, text: &str, h: f32) -> egui::Response {
  ui.add(
    egui::Button::new(egui::RichText::new(text).font(theme::sans(12.5)).color(theme::ACCENT))
      .fill(Color32::TRANSPARENT)
      .stroke(Stroke::new(1.0, theme::accent_a(150)))
      .corner_radius(CornerRadius::same(5))
      .min_size(Vec2::new(0.0, h)),
  )
}

fn toggle_pill(ui: &mut egui::Ui, on: &mut bool) {
  let (rect, resp) = ui.allocate_exact_size(Vec2::new(32.0, 18.0), Sense::click());
  if resp.clicked() {
    *on = !*on;
  }
  let p = ui.painter();
  let track = if *on { theme::ACCENT } else { theme::BORDER_STRONG };
  p.rect_filled(rect, CornerRadius::same(9), track);
  let knob_x = if *on { rect.right() - 9.0 } else { rect.left() + 9.0 };
  p.circle_filled(Pos2::new(knob_x, rect.center().y), 7.0, Color32::WHITE);
  resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, *on, "rotation"));
  ui.add_space(12.0);
}

fn progress_bar(ui: &mut egui::Ui, frac: f32) {
  let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width().min(200.0), 7.0), Sense::hover());
  let p = ui.painter();
  p.rect_filled(rect, CornerRadius::same(4), theme::INPUT_INSET);
  let mut fill = rect;
  fill.set_width(rect.width() * frac.clamp(0.0, 1.0));
  p.rect_filled(fill, CornerRadius::same(4), theme::ACCENT);
}

fn stat_tile(ui: &mut egui::Ui, value: usize, label: &str) {
  egui::Frame::NONE
    .fill(theme::CARD)
    .stroke(Stroke::new(1.0, theme::BORDER_CARD))
    .corner_radius(CornerRadius::same(7))
    .inner_margin(egui::Margin::symmetric(12, 10))
    .show(ui, |ui| {
      // Two tiles must fit the ~228px panel content: 2×(78+24)+8 spacing = 212.
      ui.set_min_width(78.0);
      ui.vertical(|ui| {
        ui.label(egui::RichText::new(value.to_string()).font(theme::mono_bold(22.0)).color(theme::TEXT_PRIMARY));
        ui.label(egui::RichText::new(label).font(theme::sans(11.0)).color(theme::TEXT_MUTED));
      });
    });
}

fn stat_row(ui: &mut egui::Ui, label: &str, value: &str) {
  ui.horizontal(|ui| {
    ui.label(egui::RichText::new(label).font(theme::sans(12.0)).color(theme::TEXT_SECONDARY));
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
      ui.label(egui::RichText::new(value).font(theme::mono(12.0)).color(theme::TEXT_PRIMARY));
    });
  });
  ui.add_space(2.0);
}

// ---------------------------------------------------------------------------
// Painting helpers
// ---------------------------------------------------------------------------

fn paint_dot_grid(painter: &egui::Painter, rect: Rect) {
  let step = 24.0;
  let color = theme::white_a(8);
  let mut y = rect.top() + step;
  while y < rect.bottom() {
    let mut x = rect.left() + step;
    while x < rect.right() {
      painter.circle_filled(Pos2::new(x, y), 0.7, color);
      x += step;
    }
    y += step;
  }
}

fn dashed_segment(painter: &egui::Painter, a: Pos2, b: Pos2, stroke: Stroke, dash: f32, gap: f32) {
  let d = b - a;
  let len = d.length();
  if len < 1.0 {
    return;
  }
  let dir = d / len;
  let mut t = 0.0;
  while t < len {
    let e = (t + dash).min(len);
    painter.line_segment([a + dir * t, a + dir * e], stroke);
    t += dash + gap;
  }
}

/// Maps world (DXF, y-up) coordinates into a screen rect (y-down), fit to size
/// with a user zoom multiplier and pan offset.
struct Mapper {
  scale: f64,
  world_min_x: f64,
  world_max_y: f64,
  origin: Pos2,
}

impl Mapper {
  fn fit(world: Bbox, screen: Rect, pad: f32, zoom: f32, pan: Vec2) -> Mapper {
    let avail = screen.shrink(pad);
    let ww = world.width().max(1e-6);
    let wh = world.height().max(1e-6);
    let base = (avail.width() as f64 / ww).min(avail.height() as f64 / wh);
    let scale = base * zoom as f64;
    let draw_w = (ww * scale) as f32;
    let draw_h = (wh * scale) as f32;
    let origin = Pos2::new(
      avail.left() + (avail.width() - draw_w) * 0.5 + pan.x,
      avail.top() + (avail.height() - draw_h) * 0.5 + pan.y,
    );
    Mapper { scale, world_min_x: world.min_x, world_max_y: world.max_y, origin }
  }

  fn to_screen(&self, [x, y]: [f64; 2]) -> Pos2 {
    Pos2::new(
      self.origin.x + ((x - self.world_min_x) * self.scale) as f32,
      self.origin.y + ((self.world_max_y - y) * self.scale) as f32,
    )
  }
}

// ---------------------------------------------------------------------------
// Offscreen snapshot tests (egui_kittest + wgpu). Generate/refresh the design-
// comparison PNGs under tests/snapshots/ with `UPDATE_SNAPSHOTS=1 cargo test`.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod snapshots {
  use super::*;
  use egui_kittest::Harness;
  use egui_kittest::kittest::Queryable;
  use std::sync::Arc;
  use std::sync::atomic::{AtomicBool, AtomicUsize};

  /// Feasibility probe: confirm the wgpu offscreen renderer produces real,
  /// non-blank pixels in this environment before wiring the full app states.
  #[test]
  #[ignore = "needs a GPU (wgpu); run: cargo test -- --ignored  (UPDATE_SNAPSHOTS=1 to regenerate)"]
  fn kittest_can_render() {
    let mut harness = Harness::builder().with_size([240.0, 120.0]).wgpu().build_ui(|ui| {
      let r = ui.max_rect();
      ui.painter().rect_filled(r, 0.0, egui::Color32::from_rgb(0x49, 0x90, 0xe0));
      ui.painter().text(
        r.center(),
        egui::Align2::CENTER_CENTER,
        "twister",
        egui::FontId::proportional(22.0),
        egui::Color32::WHITE,
      );
    });
    harness.run();
    let img = harness.render().expect("wgpu offscreen render failed");
    let raw = img.as_raw();
    let first = raw.first().copied().unwrap_or(0);
    assert!(raw.iter().any(|&b| b != first), "rendered image is uniform (blank)");
    harness.snapshot("probe");
  }

  /// Load the fixture and nest it synchronously (no worker thread), reusing the
  /// real `run_pack`, so a test lands in the same packed state the GUI shows.
  /// Rect-packed (instant, deterministic) — the default for fast snapshots.
  fn populated_app() -> App {
    populated_app_with(Packer::Rect)
  }

  /// Load the fixture and nest it synchronously with `packer`. Rect is instant;
  /// Nest matches the live-app default (continuous rotation + dense packing).
  fn populated_app_with(packer: Packer) -> App {
    let mut app = App::new(theme::FontReport { sans_is_default: true });
    app.intents.clear();
    app.load(std::path::PathBuf::from("fixtures/gengar-stacked.dxf"));
    assert!(app.model.is_some(), "fixture failed to load");
    let (items, bboxes, labels) = {
      let m = app.model.as_ref().unwrap();
      (
        nest::build_items_with(&m.drawing, &m.pieces, app.params.kerf_mm),
        m.pieces.iter().map(|p| p.bbox).collect::<Vec<_>>(),
        m.pieces.iter().map(|p| p.label.clone()).collect::<Vec<_>>(),
      )
    };
    let ctx = egui::Context::default();
    let prog = AtomicUsize::new(0);
    let cancel = Arc::new(AtomicBool::new(false));
    let mut params = app.params.clone();
    params.packer = packer;
    params.time = 1.0; // short nest budget; ignored by the rect packer
    let outcome = run_pack(&params, &items, &bboxes, &labels, &[], &prog, cancel, &ctx);
    app.outcome = Some(outcome);
    app.recompute_stats();
    app.view = View::Sheets;
    app.active_sheet = 0;
    app
  }

  /// Index of the sheet carrying the most parts (the densest — most outlines to
  /// exercise a spike regression on).
  fn densest_sheet(app: &App) -> usize {
    let Some(o) = &app.outcome else { return 0 };
    let mut counts = std::collections::BTreeMap::<usize, usize>::new();
    for p in &o.placed {
      *counts.entry(p.sheet).or_default() += 1;
    }
    counts.into_iter().max_by_key(|&(_, c)| c).map_or(0, |(s, _)| s)
  }

  /// Build a 1280×820 wgpu harness that renders `app` through the shared
  /// [`layout`] fn. Frame 0 installs fonts/visuals (deferred to frame 1), so no
  /// frame paints the bold-mono family before it is registered.
  fn render(app: App) -> Harness<'static> {
    render_at(app, [1280.0, 820.0])
  }

  fn render_at(app: App, size: [f32; 2]) -> Harness<'static> {
    let mut app = app;
    let mut frame = 0u32;
    let mut harness = Harness::builder().with_size(size).wgpu().build_ui(move |ui| {
      if frame == 0 {
        theme::install_fonts(ui.ctx());
        ui.ctx().set_visuals(theme::visuals());
      } else {
        layout(&mut app, ui);
      }
      frame += 1;
    });
    harness.run();
    harness.run();
    harness
  }

  #[test]
  #[ignore = "needs a GPU (wgpu); run: cargo test -- --ignored  (UPDATE_SNAPSHOTS=1 to regenerate)"]
  fn snap_workspace() {
    render(populated_app()).snapshot("01_workspace");
  }

  #[test]
  #[ignore = "needs a GPU (wgpu); run: cargo test -- --ignored  (UPDATE_SNAPSHOTS=1 to regenerate)"]
  fn snap_modal() {
    let mut app = populated_app();
    app.show_new_job = true;
    render(app).snapshot("02_modal");
  }

  #[test]
  #[ignore = "needs a GPU (wgpu); run: cargo test -- --ignored  (UPDATE_SNAPSHOTS=1 to regenerate)"]
  fn snap_menu() {
    let mut h = render(populated_app());
    h.get_by_label("Nest").click();
    h.run();
    h.snapshot("03_menu");
  }

  #[test]
  #[ignore = "needs a GPU (wgpu); run: cargo test -- --ignored  (UPDATE_SNAPSHOTS=1 to regenerate)"]
  fn snap_context_menu() {
    let mut app = populated_app();
    // Pre-select a part on the active sheet so the context menu shows its header.
    if let (Some(o), Some(m)) = (&app.outcome, &app.model)
      && let Some(pl) = o.placed.iter().find(|p| p.sheet == 0)
    {
      app.sel_id = Some(m.pieces[pl.piece_index].id);
    }
    let mut h = render(app);
    h.get_by_label("nesting canvas").click_secondary();
    h.run();
    h.snapshot("04_context_menu");
  }

  /// Input view: pieces overlapped at the origin. Regression guard for the
  /// stroke-miter "spokes" bug — the decimation must keep this clean.
  #[test]
  #[ignore = "needs a GPU (wgpu); run: cargo test -- --ignored  (UPDATE_SNAPSHOTS=1 to regenerate)"]
  fn snap_input_view() {
    let mut app = populated_app();
    app.view = View::Input;
    render(app).snapshot("05_input_view");
  }

  /// Densely-packed Sheets view via the **nest** packer (rotation + many parts
  /// per sheet) — the live-app default. Regression guard for the "spikes across
  /// the sheet" bug that a 1-part rect sheet can't exercise.
  #[test]
  #[ignore = "needs a GPU (wgpu); run: cargo test -- --ignored  (UPDATE_SNAPSHOTS=1 to regenerate)"]
  fn snap_sheets_nested() {
    let mut app = populated_app_with(Packer::Nest);
    app.active_sheet = densest_sheet(&app);
    render(app).snapshot("07_sheets_nested");
  }

  /// A long absolute output path must truncate inside the OUTPUT field rather
  /// than overflow over the Browse pill.
  #[test]
  #[ignore = "needs a GPU (wgpu); run: cargo test -- --ignored  (UPDATE_SNAPSHOTS=1 to regenerate)"]
  fn snap_long_output_path() {
    let mut app = populated_app();
    if let Some(m) = app.model.as_mut() {
      m.path = std::path::PathBuf::from("/Users/bounce/Projects/twister-splitter/output/cut/gengar-stacked.dxf");
    }
    render(app).snapshot("06_long_path");
  }
}
