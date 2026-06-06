# BOOX (ONYX) Notes Renderer

Convert BOOX (ONYX) `.note` files into PDF, SVG, or PNG.

`.note` is BOOX's undocumented handwriting format: a ZIP container holding
protobuf metadata, JSON, and a custom binary stroke format. Regular notes and
infinite notes are both supported — an infinite note stores its page as a
2-D grid of fixed-size tiles that together form one large canvas, and
`boox-notes-renderer` reassembles those tiles into a single global coordinate
space. Each note-page is rendered at its native page size to the format you
ask for (`--single-canvas` instead composites everything onto one sheet,
trimmed to content).

## Build

```sh
cargo build --release
# binary at target/release/boox-notes-renderer
```

## Usage

```sh
boox-notes-renderer <input.note> [output]
  [--format pdf|svg|png]
  [--page N] [--scale F] [--single-canvas]
  [--flat-marker]
  [--font <path>] [--fonts-path <dir>]... [--map-font "Name=Target"]...
  [--download-fonts]
```

### Arguments

- `<input.note>`
  The input `.note` file.

- `[output]`
  Output path. Defaults to the input name with the format's extension.
  SVG and PNG emit one file per note-page; with more than one page the files
  are named `stem-1.svg`, `stem-2.svg`, … PDF is always a single multi-page
  file.

### Options

- `--format pdf|svg|png`
  Output format. Inferred from the output extension when omitted; defaults to
  PDF.

- `--page N`
  Render only the Nth (1-based) page.

- `--scale F`
  PNG resolution in pixels per point (default `1.0`, i.e. 1860×2480 for a
  native page).

- `--single-canvas`
  Composite all of a note's pages onto one giant canvas (the infinite-canvas
  view, trimmed to content) instead of one output page per note-page.

- `--flat-marker`
  Draw each marker/highlighter stroke so it composites exactly once, avoiding
  the dark blotches some PDF viewers (e.g. macOS Preview) show where a
  translucent stroke crosses itself — an artifact BOOX's own exports share.
  Separate strokes still darken where they overlap.

- `--font <path>`
  Force this font file (TTF/OTF/TTC) for all text, bypassing name resolution.

- `--fonts-path <dir>`
  Add a directory to search for fonts. Repeatable.

- `--map-font "Name=Target"`
  Map a font family the note requests to a host family, e.g.
  `--map-font "Noto Sans CJK JP=Hiragino Sans"`. Repeatable.

- `--download-fonts`
  Download the note's fonts from Google Fonts (OFL/Apache) into a cache and
  use them, so text renders in the note's actual fonts.

### Font resolution

Fonts are resolved per text box from the family the note requests: a
`--map-font` override → exact match → a built-in map of the AOSP/Noto fonts
BOOX ships (`Noto Sans CJK JP`→Hiragino, `Noto Serif`→Times,
`Roboto`→Helvetica/Arial, …) → a CJK fallback. PDF embeds a glyph subset per
font, SVG embeds a subset `@font-face` per font, and PNG rasterizes glyph
outlines. Color emoji are drawn from the platform emoji font (Apple Color
Emoji, or a downloaded Noto Color Emoji).

### Examples

```sh
boox-notes-renderer Note.note # Produces `Note.pdf`
boox-notes-renderer Note.note out/Note.svg
boox-notes-renderer Note.note Page.png --page 3 --scale 2
boox-notes-renderer Note.note Canvas.svg --single-canvas
```

### Page layout

A `.note` is a sequence of pages (`pageNameList` order). By default each
note-page becomes one output page at its native size (1860×2480) — matching
BOOX's per-page export, and reproducing single-page notes exactly.
`--single-canvas` instead stitches the pages into one sheet via their tile
placement rects.

## Use as a library

The CLI is a thin wrapper over the library crate. The public surface is
`model` (parse a `.note` into render-ready data), `render` (emit PDF/SVG/PNG
bytes), and the typed `Error`:

```rust
use boox_notes_renderer::model::{Document, Layout};
use boox_notes_renderer::render::{self, FontOptions, PageSel};

let file = std::fs::File::open("input.note")?;
let doc = Document::load(file, Layout::PerPage)?;
let pdf: Vec<u8> = render::render_pdf(&doc, &FontOptions::default(), PageSel::All)?;
```

`Document::load` accepts any `Read + Seek` (e.g. an in-memory `Cursor`), and
the render functions return bytes/strings — file I/O stays with the caller.
`FontOptions` is built with `Default` plus the chainable `with_explicit` /
`with_dirs` / `with_map` / `with_download` setters.

Recoverable problems (a skipped asset, a malformed optional field, download
progress) are reported through the [`log`](https://docs.rs/log) facade
(`warn`/`info`); install your application's logger to see them. The library is
silent by default.

The default `cli` feature exists only for the binary (clap/anyhow); library
consumers can skip building those:

```toml
[dependencies]
boox-notes-renderer = { version = "0.1", default-features = false }
```

## What is rendered

- **Handwriting strokes** with pressure-varied width and per-stroke color.
- **Highlighter** (pen type 15) at reduced opacity, drawn beneath ink.
- **Text boxes** (typed text) with an embedded font; lines break at the
  note's own line breaks (no width wrapping is invented).
- **Embedded images** (JPEG/PNG) placed at their bounding rect.
- **Shape-tool geometry** (pen type 40): lines and polygons from the GeoJSON
  payload, with stroke color/width, optional fill, and dashed line styles.

Layer order is honored; within a layer, translucent strokes are drawn before
opaque ones so highlighter sits behind ink.

## Limitations

- Only the modern `geo_layout` container format is supported. Notes saved
  in the older SQLite-based format are rejected with an error.
- Template backgrounds (ruled lines, dot grids) are not drawn — pages have a
  plain white background. (BOOX fetches these from an external SVG CDN.)
- A single giant page may exceed a PDF viewer's maximum page size
  (~200 inch / 14400 pt), or a very large PNG may be skipped, for extremely large
  notes (raise/lower `--scale` for PNG).

## References

The format details were worked out with reference to
[`boox-note-parser`](https://github.com/hhornbacher/boox-note-parser) (stroke
binary and protobuf schemas) and
[`boox-note-optimizer`](https://github.com/nrontsis/boox-note-optimizer)
(pen/shape rendering semantics).
