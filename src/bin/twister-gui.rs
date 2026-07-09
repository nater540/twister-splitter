//! Desktop front-end for twister-splitter.
//!
//! Thin entry point: all UI lives in the library's `gui` module so it can be
//! exercised by tests/harnesses without going through `main`.
//!
//! Run with: `cargo run --features gui --bin twister-gui`

fn main() -> eframe::Result<()> {
  twister_splitter::gui::run()
}
