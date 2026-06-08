# AGENTS.md

Guidance for AI agents and developers working on `boox-notes-renderer`.

## What this is

A Rust lib + CLI that converts BOOX (ONYX) `.note` files to vector PDF (plus
SVG/PNG). Regular notes and infinite notes are both supported. The repository
is a flat cargo workspace:

- `crates/boox-notes-renderer` — the main lib + CLI (everything below).
- `crates/system-fonts` — a standalone, reusable crate that asks the OS font
  APIs for fonts (family name lookup + per-character system font fallback);
  see the Fonts section. No BOOX-specific code.

**In scope:** the modern `geo_layout` container format — handwriting strokes
(pressure-sensitive), highlighter, typed text boxes, embedded images, and
shape-tool geometry.

**Out of scope (by decision):** the legacy SQLite-based format (rejected with
an error); template/ruled backgrounds; pixel-exact reproduction of BOOX's
rendering.

## Commands

All commands run from the repository root (the workspace).

```sh
cargo build                  # debug build (all crates)
cargo test                   # all crates' unit tests + integration tests
cargo clippy --all-targets   # lint
cargo fmt                    # format

cargo run -- <in.note> [out.{pdf,svg,png}]    # the only bin, so no -p needed
  [--format pdf|svg|png]
  [--page N] [--scale F] [--single-canvas]
  [--flat-marker]
  [--font <path>] [--fonts-dir <dir>]... [--map-font "Name=Target"]...
  [--download-fonts] [--fonts-cache-dir <dir>]
```

The arguments and options are documented in [`README.md`](README.md).

Integration tests run against the committed fixture notes in
`crates/boox-notes-renderer/tests/examples/` (checked in, so the suite always
runs). `crates/system-fonts` carries its own host-font tests (macOS-gated
ones run here; Windows/Linux-gated ones need those hosts).

## Verifying output (important)

PDF output must be **looked at**, not just diffed. On macOS, rasterize a preview
and read it:

```sh
qlmanage -t -s 1400 -o out out/<name>.pdf   # writes out/<name>.pdf.png
```

Then open/inspect the PNG. `pdfinfo`/`pdftoppm` (poppler) are not installed here;
`qlmanage` (Quick Look) and `sips` are. `qlmanage` also renders `.svg` (via
WebKit) and `.png` directly, so the same command verifies every format.
Committed fixture notes live in `crates/boox-notes-renderer/tests/examples/`
(`Infinite Note.note` — multi-page/multi-tile; `Note.note` — every item kind),
each paired with **BOOX's own export** (`*.pdf`) for visual comparison.

## Architecture & data flow

```
container → proto / json_meta / points → model → render → PDF / SVG / PNG
```

| Module | Responsibility |
| --- | --- |
| `container` | Read the ZIP (and nested `shape/*.zip`) fully into memory; lookup by path. |
| `ids` | Normalize the simple (32-hex) / hyphenated (36) UUID forms BOOX mixes; decode space/NUL-padded binary UUIDs. |
| `proto` | Inline `#[derive(prost::Message)]` definitions — no `.proto`, no `protoc`, no `build.rs`. |
| `json_meta` | serde structs for JSON embedded inside protobuf; `sanitize_unquoted_keys` fixes BOOX's invalid `{0:..}` integer keys. |
| `points` | Parse the binary `#points` stroke files (big-endian). |
| `model` | Detect `geo_layout`; map tiles → global rects; resolve width/color; link shapes↔strokes; translate everything into one global space; compute the trimmed bounding box; emit an ordered `RenderItem` list per note (`Canvas`). |
| `pen` | Pure pen-geometry formulas shared by `model` (visual-bounds padding) and `render` (rasterization) — e.g. the charcoal stamp size — one definition so the two sides can't drift apart. |
| `render` | A `Backend` trait + shared per-item painters. `mod.rs` defines `Backend` (primitives in global coords) and `paint_canvas` (the item walk); `stroke`/`shape`/`text` compute geometry once and call the backend (images go straight to `Backend::draw_image`). Backends: `pdf` (`printpdf`, flips Y), `svg` (string + subset `@font-face`), `png` (`tiny-skia` raster, glyph outlines). `fonts` resolves each text box's requested family to a host font; `charcoal` rasterizes pen-22 grain; `emoji` pulls color-emoji bitmaps. |

Design rule: **`model` produces global coordinates and resolved styles;
`render` is "dumb"** (transform to output space, emit primitives via the
`Backend` trait). Keep format/semantic decisions in `model`; keep per-format
quirks in the backend. Painters (`stroke`/`shape`/`text`) must stay
backend-agnostic — they may not reference `printpdf`/`tiny-skia` or pre-flip Y.

## Format

A `.note` is a ZIP. Single-note: `note/pb/note_info` at root; multi-note:
`note_tree` + one `<note_id>/` dir per note. We locate notes by scanning for
`*/note/pb/note_info`. UUIDs appear in both simple and hyphenated forms — always
normalize before keying.

Coordinates: note space is top-left origin, y-down, 1 unit = 1 PDF pt. PDF is
bottom-left origin, y-up. `to_page` in the PDF backend (`render/pdf.rs`) does
the Y flip and subtracts the canvas origin.

The rest of this document describes each entity of the format — pages/tiles,
stroke points, shapes, pens, text boxes, images, and shape-tool geometry — and
how we render it.

## Pages & tiles (`geo_layout`)

- A note is a **sequence of pages** in `note_info` field 20 `{"pageNameList":[…]}`
  (`note_info` wraps the metadata in field 1 — decode the wrapper first).
  Default rendering is **one PDF page per note-page** at the native size
  (1860×2480), placing strokes in page-local coords — this matches BOOX's
  export. See `Layout::PerPage` / `assemble_per_page`.
- `Layout::SingleCanvas` (`--single-canvas`) instead composites every tile onto
  one sheet via the field-6 placement rects, trimmed to content.
- The tile/page is **1860×2480**. `virtual/doc/pb/*` content JSON has
  `contentType: "geo_layout"` (our format gate).
- `virtual/page/pb/<file>` is a **repeated** container; each entry has
  `page_uuid` (field 1) and a **global placement rect** in `dimensions_json`
  (field 6). Tile-local stroke coords are translated by `(rect.left, rect.top)`.
- **Handwriting strokes can carry an affine matrix** (field 8) — content the
  user moved/resized. Apply the matrix to the points and scale the pen width by
  the matrix's linear scale, or those strokes land in the wrong place at the
  wrong size.
- Placement (`stroke_matrix` + `bbox_fit`): apply the matrix, then map the
  points' **center** onto the field-7 bbox center, then add the tile origin.
  The bbox is tile-local, so the tile origin is still required.
  - center alignment (not corner) matters — the bbox pads the points
    symmetrically by half the stroke width, so corner-snapping shifts wide
    highlighters by ~half their width.
  - a **boundary-crossing stroke is duplicated into each tile it touches**,
    and the duplicate's points keep the *source* tile's coordinate frame while
    its bbox is in the *referencing* tile's frame. The bbox-center fit snaps
    these duplicates back onto their true spot. Don't fix this by clipping —
    that chops real cross-tile strokes.
- some boundary-crossing copies (notably **pen_type 2000** history) carry
  a bbox entirely *outside* the tile that owns them. BOOX clips each page
  to its tile on export so they never render; a composited single canvas has no
  clip, so they would leak into neighbouring pages. We cull any stroke whose
  field-7 bbox is **entirely disjoint** from its tile rect (`bbox_outside_tile`)
  — membership culling, not pixel clipping, so legitimately overflowing strokes
  survive and draw in full.
- single-canvas output differs from BOOX's export only in scale
  (BOOX downscales large multi-tile exports; we emit 1:1) and page region
  (BOOX draws template backgrounds and pads to a full tile block; we crop
  tight to content, intentionally). The element layout itself is identical.

## Stroke points (`#points` binary)

`point/<page_id>/<page_id>#<points_id>#points`, all integers big-endian:

- Header 76B: `version:u32`, `page_id:[u8;36]`, `points_id:[u8;36]`.
- Tail 4B: u32 offset of the stroke table.
- Table entry 44B: `stroke_id:[u8;36]`, `start_addr:u32`, `packed:u32`
  (`count = packed>>4`).
- Point 16B: `timestamp_rel:u32`, `x:f32`, `y:f32`, `tilt_x:i8`, `tilt_y:i8`,
  `pressure:u16` (0..=4095). `timestamp_rel` is cumulative ms from the stroke
  start (kept as `Point::t`).

## Shapes (protobuf inside nested `shape/*.zip`)

Every drawable item — stroke, text box, image, geometry — is a `Shape` record.
Fields used: 1 `stroke_uuid`, 2 `created`, 4 `color` (ARGB as signed int),
5 `stroke_width:f32`, 6 `layer_id`, 7 `bbox_json`, 8 `matrix_json` (3×3 affine
`[a,b,tx,c,d,ty,0,0,1]`), 9 `text_style_json`, 10 `text_plain`, 11
`render_scale_json` (createArgs; carries `penAttrs.texture` for charcoal), 12
`pen_type`, 14 `image_info_json` (has `relativePath` → `resource/data/<file>`),
16 `points_uuid`, 17 `line_style_json`, 20 `extra_json` (GeoJSON
`featureCollection`), 22 `rich_text`.

ARGB: `a=(c>>24)&0xff, r=(c>>16)&0xff, g=(c>>8)&0xff, b=c&0xff` (alpha 0 ⇒ opaque).

`pen_type` (field 12) selects the entity kind: handwriting pens (below), `6/16`
text box, `19` image, `40` shape-tool geometry.

## Pens (`pen_type`)

Stroke rendering splits by pen kind (`render/stroke.rs`). The pen models
reproduce BOOX's pen behavior (verified against BOOX's exports); the formulas
below are the spec — see the code for details. Throughout, `p = pressure/4095`
and `t = nominal width`.

The pressure-width pens (`5/21`) draw as short **round-capped segments**, each
stroked at the mean width of its endpoints — the same geometry BOOX's export
uses. `SetOutlineThickness` is coalesced (emitted only when the width changes).

### `2` ballpoint (and `2000…` history strokes)

Constant width `t` — as is any pen type not listed here. Drawn as a single
**stroked polyline** (round cap/join): compact, gap-free at corners and
self-crossings.

### `5` fountain

Pressure-curve width `width = max(2.0, 0.964·(t+3)·p^0.599)`, then a zero-phase
EMA (α≈0.2, forward+backward so peaks stay aligned) smooths the per-point
widths so they never step abruptly.

### `15` highlighter/marker

BOOX's export draws **one polyline of every raw point, stroked once at the
constant nominal width** (round cap/join), at **alpha 127/255** with **blend
`Multiply`**. We emit the same single stroke op per stroke (over the raw,
undecimated points), so any viewer renders our output and BOOX's identically.

Some viewers (Quartz/Preview) chunk a long translucent stroked path into
separately-composited passes, darkening self-crossings; BOOX's own files show
the same artifact. `--flat-marker` (`Document::flatten_markers`) opts out: each
marker stroke becomes **one nonzero-winding fill** of its constant-width
capsule union, which composites once in every viewer; per-stroke 127/255 alpha
+ Multiply stay, so separate strokes still darken where they cross.

### `21` nBrush

A two-phase width model (`neo_brush_widths`). A pen-down
**dwell** phase (while the pen stays within 3 px of the stroke start) seeds a
velocity accumulator from the pressure ramp; the **move** phase computes
`width = velAccum + D·√p` (`D = 2·t`, clamped to `[0.5, D+3]`), stepping
`velAccum` by an inter-sample distance ratio — deceleration thickens,
acceleration thins. This produces the pen's signature look: thin travelling
shafts, fat blobs at dwell/turn/stop.

The integrator must run over the **raw** points (decimation corrupts it) and is
**timestamp-independent** (the model uses a fixed dt; the recorded timestamps
are jittery and unusable).

### `22` charcoal

Width is **tilt-driven**; pressure drives the grain **density/darkness**
instead (per-pixel keep probability keyed on `pscale = sqrt(p/3) + 0.3`). The
stroke is baked into a grain raster (`render/charcoal.rs`) and placed as an
inline image, like BOOX's export.

`penAttrs.texture` (proto field 11) selects the grain variant: texture 1 is
the dense V1 grain; texture 2 is the V2 grain that breaks up at light pressure.
See `render/charcoal.rs` for the model.

### `60/61` calligraphy

A fixed **rotated-rectangle (chisel) nib** stamped along the path —
`width(dir) = L·|cos(dir−α)| + w·|sin(dir−α)|`, with `L = nominal`,
`w = L/clamp(nominal,1,10)`, `α = +45°` (60) / `−45°` (61). Pressure- and
tilt-independent. Drawn by filling the swept nib quads (one nonzero-winding
fill), so stroke ends are the chisel's flat angled edge.

### `37` scanline fill

The stroke points come in pairs, each pair being opposite corners of a filled
rectangle. (Not exercised by the bundled examples.)

## Text boxes (pen `6/16`)

- Content is the plain text (field 10) or, when present, the rich-text HTML
  (field 22): `<b>/<strong>`, `<i>/<em>`, `<u>`, `<br>`, block ends, and
  entities are honored; `<font face>` names the requested family.
- Style comes from `text_style_json` (field 9): font size, alignment
  (0/1/2 = left/center/right), line spacing, bold/italic/underline flags, and
  the BOOX device font path (a resolution hint).
- The box is placed by its field-7 bbox. Lines break only at explicit `\n` —
  no width wrapping is invented. BOOX draws no border, so neither do we.
- Fonts are resolved per text box; see the Fonts section below.

## Fonts

`render/fonts.rs` maps the font a note asks for (a family name from the
rich-text `<font face>`, plus the BOOX device font path's file stem as an
extra name hint — the path itself is never used) to an actual font file on
the host. Resolution has **two axes**: name candidates (outer), and for each
candidate every *place* a font can come from (inner, `FontDb::find_one`).

**Places** (`find_one`, exact family name, first hit wins):

1. The local index: the `--fonts-dir` dirs, then (with `--download-fonts`)
   the download cache dir — scanned **non-recursively**, reading each file's
   name table via mmap (so huge `.ttc`s aren't fully read). Per family the
   index keeps the face closest to upright Regular (weight distance from 400,
   italic/oblique penalized), so a family spanning weights resolves to
   Regular. `.ttc` face indices are kept (`ResolvedFont = (path, index)`).
2. With `--download-fonts`: a Google Fonts download (exact catalog family,
   trailing style words like "Bold" stripped progressively; the catalog check
   is a local map, so misses cost nothing).
3. With the `system-fonts` cargo feature (in the default set): the OS font
   APIs (`render/system_fonts.rs`) — **the only source of system fonts**; no
   fixed system path is ever scanned.

**Name candidates** (`FontDb::resolve`, each goes through `find_one`):

1. `--font <path>` — forces one font for everything (emoji excepted).
2. A `--map-font` target (an override: applied before the requested name
   itself, so the mapping wins even when the requested family exists).
3. The requested family itself (and the device-path stem).
4. The built-in substitutes (`DEFAULT_MAP`) for the AOSP/Noto families BOOX
   ships: the canonical name first, then the Google Fonts name of the same
   design ("Noto Sans CJK JP" is "Noto Sans JP" there — so a cached download
   beats a system substitute), then system substitutes (Hiragino, Times,
   Helvetica/Arial, …).
5. A loose substring match (local index only — the OS can't substring-match).
6. The CJK-capable fallback (`FALLBACK_FAMILIES`: Hiragino Sans / PingFang SC
   / Yu Gothic / … through `find_one`, first found).

**OS lookup** (`system-fonts`, in the default feature set; `default-features =
false` gives a pure-Rust build where fonts come only from
`--fonts-dir`/downloads): the `crates/system-fonts` crate talks to the
platform APIs directly — Core Text on macOS/iOS (descriptor matching; misses
cleanly, unlike `CTFontCreateWithName`), DirectWrite on Windows (`dwrote`),
fontconfig on Linux/FreeBSD (via dlopen, so no build dep and graceful
absence). Families resolve to a file path + `.ttc` face index, landing on the
Regular face even when the OS ships one file per weight (macOS Hiragino), and
reach fonts outside any public dir (PingFang lives in a private framework on
modern macOS/iOS).

**Per-character fallback.** Each character the box's font lacks is re-resolved
by `font_for_char`: first — with `system-fonts` — the OS fallback **asked with
the actual character** (`fallback_for_char`: Core Text's
`CTFontCreateForString`, DirectWrite's `MapCharacters`, fontconfig charset
matching), so the OS returns a font capable of drawing it; then the single CJK
fallback (`FALLBACK_FAMILIES`, the only per-char fallback with the feature
off). For Han the OS follows the user's language settings (ja → Hiragino, zh →
PingFang) unless the requested family carries a language (`lang_hint`: "Noto
Sans CJK JP" → `ja`, "…SC" → `zh-Hans`), passed through so a Japanese note
stays Japanese-shaped on a Chinese host and vice versa. The glyph is verified
against the cmap (`advance_em`); answers are memoized per (char, lang). Line
layout (`render/text.rs`) measures advances from the font that will actually
draw each character, and `used_chars` mirrors the same choice.

**Embedding.** PDF embeds a glyph subset per font (subsetting is ours, via
`allsorts` — printpdf 0.9's own subsetting is disabled upstream); SVG embeds a
subset `@font-face` as a base64 data URI (with a Unicode cmap — browsers reject
Mac-Roman-only); PNG rasterizes glyph outlines with `ttf-parser`.

**Downloads** (`render/download.rs`, `--download-fonts`): fonts land in the
platform cache dir under `boox-notes-renderer/fonts` (or `--fonts-cache-dir` /
`FontOptions::with_cache_dir`), which is then indexed like a `--fonts-dir`
(after the user dirs) so they resolve by real family name. Two mechanisms: the
curated Noto list (fetched eagerly, includes Noto Color Emoji) and the full
Google Fonts catalog (family list cached on disk; per-family files on demand,
static Regular preferred, variable fonts at their default instance). Bytes are
validated as sfnt (`looks_like_font`) before caching and reuse (`cached_font`),
so a poisoned entry — error HTML, or a WOFF the sfnt-only embed path can't
parse — is treated as missing and re-fetched.

**Emoji** (`render/emoji.rs`): single-codepoint emoji are drawn as inline
images from a color-emoji font's embedded PNG bitmaps (so they render in color
instead of tofu). The emoji font resolves like any other font — Apple Color
Emoji, then Noto Color Emoji from the host index, then the `--download-fonts`
caches — but `--font` deliberately does not hijack it (it forces *text* only).
ZWJ sequences fall back to their components; skin-tone modifiers and variation
selectors are skipped. Bitmaps are cached per (font, char), and the font file
is mmapped so the ~190 MB Apple Color Emoji `.ttc` is never fully read.

## Images (pen `19`)

`image_info_json` (field 14) carries `relativePath` (or `localPath`) whose file
name resolves to `resource/data/<file>` in the ZIP. The decoded image (JPEG/PNG)
is placed at the shape's field-7 bbox rect.

## Shape-tool geometry (pen `40`)

`extra_json.featureCollection` (a JSON string). Each feature's `geometry.type` +
`properties.subType` selects the primitive (`collect_geo_feature`):

- `LineString` = open polyline; `LineString`+`WaveLine` = sine wave (`waveAttr`).
- `Polygon` = **edge-pair** list (`[[start,end],…]`, non-standard) → closed ring.
- `MultiPoint`+`Oval` (or bare MultiPoint) = bbox-corner ellipse; `+Curve` =
  quadratic Bézier (3 pts); `+Arc` = elliptical arc (`[min,max,[startDeg,sweepDeg]]`);
  `+Bracket` = curly brace (tip + two ends).
- `DirectionLine` / `BidirectionalLine` = line + filled arrowhead(s);
  `MultiLineString` = separate segments.

`properties.strokeAttr` (color/width), `properties.fillAttr`. Coords are
transformed by the field-8 matrix; stroke width is scaled by its linear scale.

- grouped shapes nest a whole `FeatureCollection` under a feature's
  `geometry`, so the real leaves live one or more levels down. `build_geo`
  recurses into any node carrying a `features` array; only nodes with
  `coordinates` are leaves. Reading one level deep silently drops the shape.
- `properties.lineStyle` (`{type, dashLineIntervals:[on,off], phase}`) marks
  dashed edges (`type != 0`); the on/off lengths (matrix-scaled) are carried on
  `GeoPath.dash` and emitted as a dash pattern around the stroke.

## printpdf 0.9 gotchas (learned the hard way)

- **Keep printpdf's default features.** With `default-features = false`, text
  glyph encoding silently breaks (`ShowText` emits an empty `[] TJ`). Cargo.toml
  uses `features = ["jpeg", "png"]` and leaves the default (`html`) on.
- Image decoding needs an image feature → `jpeg`/`png` enabled.
- `Op::SetTextCursor` maps to PDF `Td` (**relative** move). For multi-line text,
  emit the delta between consecutive line origins (`render/text.rs`).
- `ParsedFont::from_bytes(bytes, index, &mut warnings)` supports `.ttc` via the
  index. macOS Japanese fonts are Hiragino `.ttc` only.
- Image placement: `Op::UseXobject` with `XObjectTransform { dpi: Some(72.0),
  scale_x: target_w/px_w, scale_y: target_h/px_h, translate_x/y: bottom-left }`.

## Conventions

- Don't add `protoc`/`build.rs` — keep prost messages inline in `proto.rs`.
- A single malformed JSON/field should never abort a whole note: parsers return
  `Option`/warn (`log::warn!`) and continue. Hard failures (not a ZIP, no
  note_info, wrong format, a *required* structure undecodable, page out of
  range, a page too large to rasterize, a PNG encode failure) are the typed
  `crate::Error` (`error.rs`, thiserror).
- **Public API hygiene** (the crate is also a library): public signatures
  return `crate::Error` — never `anyhow` (CLI-only) and never third-party
  error types (`zip`/`prost` errors are folded into `io::Error`/strings so
  their version bumps stay private). Internal per-entry failures that are
  warned-and-skipped use plain `io::Error`. CLI-only dependencies
  (clap/anyhow) sit behind the default `cli` feature (`required-features` on
  the bin), so `default-features = false` library consumers don't build them. Public enums/option structs that will
  grow (`Error`, `RenderItem`, `Layout`, `PageSel`, `FontOptions`) are
  `#[non_exhaustive]`; `FontOptions` is built via `Default` + `with_*` setters.
  Library diagnostics go through the `log` facade — `log::warn!` for
  skipped/malformed items, `log::info!` for download progress — never bare
  `eprintln!`. The library installs no logger (silent by default, per `log`'s
  contract); the CLI's `StderrLogger` in `main.rs` prints them.
- When adding a new shape/pen kind: parse + resolve in `model` (new `RenderItem`
  variant, update the pen-type match and the bbox accumulation), then add a dumb
  draw path in `render`. Verify with a real note via `qlmanage`.
