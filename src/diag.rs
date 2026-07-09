//! Structured diagnostics surfaced by the pipeline.
//!
//! The library never prints: `extract` and the nesting helpers *return* their
//! warnings as [`Diagnostic`]s so the CLI can print them and the GUI can render
//! them in its status/diagnostics area. This keeps the library UI-free.

/// How loud a diagnostic is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
  /// Informational: something was adjusted but the result is still correct.
  Info,
  /// Warning: the output may not match the user's intent (wrong footprint,
  /// skipped geometry, an approximation).
  Warning,
}

/// One user-relevant message about the pipeline's handling of the input.
#[derive(Clone, Debug)]
pub struct Diagnostic {
  pub severity: Severity,
  /// The piece this is about, when it can be tied to a list row.
  pub piece_label: Option<String>,
  pub message: String,
}

impl Diagnostic {
  pub fn info(message: impl Into<String>) -> Self {
    Diagnostic { severity: Severity::Info, piece_label: None, message: message.into() }
  }

  pub fn warning(message: impl Into<String>) -> Self {
    Diagnostic { severity: Severity::Warning, piece_label: None, message: message.into() }
  }

  /// Attach the label of the piece this diagnostic is about.
  pub fn for_piece(mut self, label: impl Into<String>) -> Self {
    self.piece_label = Some(label.into());
    self
  }
}
