//! Convert BOOX (ONYX) `.note` files (infinite-note / `geo_layout` format) to
//! vector PDF (plus SVG/PNG).
//!
//! Pipeline: an internal `container` unpacks the ZIP, `proto`/`json_meta`/
//! `points` parse the metadata and strokes, [`model`] reassembles the tiled
//! infinite canvas into a single global coordinate space, and [`render`] emits
//! the output. Only [`model`], [`render`], and the [`Error`] type are public;
//! the parsing modules are crate-internal.
//!
//! ```no_run
//! use boox_notes_renderer::model::{Document, Layout};
//! use boox_notes_renderer::render::{self, FontOptions, PageSel};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let file = std::fs::File::open("input.note")?;
//! let doc = Document::load(file, Layout::PerPage)?;
//! let pdf: Vec<u8> = render::render_pdf(&doc, &FontOptions::default(), PageSel::All)?;
//! # Ok(())
//! # }
//! ```
//!
//! Recoverable problems (a skipped asset, a malformed optional field) never
//! abort a note; they are reported through the [`log`] facade — `log::warn!`
//! for skipped/malformed items, `log::info!` for font-download progress.
//! Install whatever logger your application uses to see them (the bundled CLI
//! installs a minimal stderr logger); without one they are dropped, per `log`'s
//! usual contract.

// Parsing internals — not part of the public API. The public surface is
// `model::{Document, Layout, Canvas, RenderItem, ...}` (with `Point` re-exported
// there), `render::{render_pdf, render_svg, render_png, FontOptions, PageSel}`,
// and `Error`/`Result`. Public signatures return `crate::Error`; internal
// per-entry failures (warned and skipped) use plain `io::Error`. `anyhow` is
// CLI-only.
pub(crate) mod container;
mod error;
pub(crate) mod ids;
pub(crate) mod json_meta;
pub mod model;
pub(crate) mod pen;
pub(crate) mod points;
pub(crate) mod proto;
pub mod render;

pub use error::{Error, Result};
