//! The public error type.
//!
//! Everything fallible in the public API ([`Document::load`] and the
//! `render_*` functions) returns [`Error`]. Third-party error types (`zip`,
//! `prost`) are deliberately not exposed — they are folded into
//! [`std::io::Error`] or message strings — so their version bumps stay
//! private to this crate.
//!
//! [`Document::load`]: crate::model::Document::load

/// Errors returned by [`Document::load`](crate::model::Document::load) and the
/// [`render`](crate::render) functions.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The input could not be read as a ZIP archive (I/O error, or not a ZIP).
    #[error("reading .note archive: {0}")]
    Archive(#[source] std::io::Error),
    /// The archive contains no `note/pb/note_info` entry — not a `.note` file
    /// this crate recognizes.
    #[error("no note_info found; not a recognized .note archive")]
    NotANote,
    /// The note uses a container format other than `geo_layout` (e.g. the
    /// legacy SQLite-based format), which is out of scope by decision.
    #[error(
        "unsupported note format '{content_type}' (only the infinite-note 'geo_layout' format is supported)"
    )]
    UnsupportedFormat {
        /// The `contentType` the note declared.
        content_type: String,
    },
    /// No format marker was found, so the note's format could not be
    /// determined.
    #[error("could not determine note format (no virtual/doc geo_layout marker found)")]
    UnknownFormat,
    /// A required metadata structure failed to decode. (A malformed *optional*
    /// field never aborts a note — those are reported via `log::warn!` and
    /// skipped.)
    #[error("decoding {what}: {detail}")]
    Malformed {
        /// What was being decoded (an archive entry path or structure name).
        what: String,
        /// The underlying decode failure, as text.
        detail: String,
    },
    /// The document has no pages to render.
    #[error("document has no pages")]
    NoPages,
    /// The requested page index is outside the document.
    #[error("page {page} out of range (valid: 1..={pages})")]
    PageOutOfRange {
        /// The requested 1-based page index.
        page: usize,
        /// How many pages the document has.
        pages: usize,
    },
    /// A page's pixel dimensions exceed what the PNG rasterizer can allocate.
    #[error("page {page} too large to rasterize ({width}x{height} px)")]
    RasterTooLarge {
        /// The 1-based page index that could not be rasterized.
        page: usize,
        /// The requested raster width in pixels.
        width: u32,
        /// The requested raster height in pixels.
        height: u32,
    },
    /// Encoding a rendered page to PNG failed.
    #[error("encoding page {page} to PNG: {detail}")]
    Encode {
        /// The 1-based page index that failed to encode.
        page: usize,
        /// The underlying encode failure, as text.
        detail: String,
    },
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;
