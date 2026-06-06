//! End-to-end conversion tests against the committed fixture notes in
//! `tests/examples/` (a multi-page "Infinite Note" and a single-page "Note" that
//! exercises every item kind). These are checked in, so the tests always run.
//! The matching `*.pdf` files there are BOOX's own export, kept for manual
//! visual comparison (`qlmanage`), not asserted against here.

use std::fs::File;
use std::path::PathBuf;

use boox_notes_renderer::model::{Document, Layout, RenderItem};
use boox_notes_renderer::render::{self, FontOptions, PageSel};

fn load(name: &str, layout: Layout) -> Document {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/examples")
        .join(name);
    let file = File::open(&path).unwrap_or_else(|e| panic!("opening {}: {e}", path.display()));
    Document::load(file, layout).expect("note should parse")
}

#[test]
fn load_rejects_non_note_input_with_typed_error() {
    // Not a ZIP at all → Archive.
    let err = Document::load(std::io::Cursor::new(b"not a zip".to_vec()), Layout::PerPage)
        .expect_err("garbage input should not parse");
    assert!(matches!(err, boox_notes_renderer::Error::Archive(_)));

    // A valid but empty ZIP (just the 22-byte end-of-central-directory record):
    // parses as an archive, but contains no note_info → NotANote.
    let mut empty_zip = vec![0x50, 0x4b, 0x05, 0x06]; // "PK\x05\x06"
    empty_zip.extend([0u8; 18]);
    let err = Document::load(std::io::Cursor::new(empty_zip), Layout::PerPage)
        .expect_err("an empty archive is not a note");
    assert!(matches!(err, boox_notes_renderer::Error::NotANote));
}

#[test]
fn per_page_yields_one_canvas_per_page_with_strokes() {
    // "Infinite Note" is a multi-page infinite note → several per-page canvases.
    let doc = load("Infinite Note.note", Layout::PerPage);
    assert!(
        doc.canvases.len() > 1,
        "multi-page note yields several pages"
    );
    assert!(
        doc.canvases
            .iter()
            .flat_map(|c| &c.items)
            .any(|i| matches!(i, RenderItem::Stroke(_)))
    );
    let pdf = render::render_pdf(&doc, &FontOptions::default(), PageSel::All).unwrap();
    assert!(pdf.starts_with(b"%PDF"));
    assert!(pdf.len() > 10_000);
}

#[test]
fn pdf_page_selection_matches_svg_png() {
    // render_pdf now takes a PageSel like the other backends: One(p) emits just
    // that page, and an out-of-range page errors instead of panicking.
    let doc = load("Infinite Note.note", Layout::PerPage);
    assert!(doc.canvases.len() > 1, "need a multi-page note");
    let one = render::render_pdf(&doc, &FontOptions::default(), PageSel::One(1)).unwrap();
    assert!(one.starts_with(b"%PDF"));
    let n = doc.canvases.len();
    // The error is typed (not stringly): out-of-range reports the valid range.
    assert!(matches!(
        render::render_pdf(&doc, &FontOptions::default(), PageSel::One(n + 1)),
        Err(boox_notes_renderer::Error::PageOutOfRange { page, pages }) if page == n + 1 && pages == n
    ));
}

#[test]
fn single_canvas_is_one_large_page() {
    // SingleCanvas composites every page onto one sheet; for this multi-tile note
    // the result is wider than a single 1860-wide tile.
    let doc = load("Infinite Note.note", Layout::SingleCanvas);
    assert_eq!(doc.canvases.len(), 1);
    assert!(doc.canvases[0].width > 1860.0);
}

#[test]
fn note_has_image_text_and_geometry() {
    let doc = load("Note.note", Layout::PerPage);
    // The note exercises image, text and geometry items.
    let all: Vec<&RenderItem> = doc.canvases.iter().flat_map(|c| &c.items).collect();
    assert!(
        all.iter().any(|i| matches!(i, RenderItem::Image(_))),
        "image"
    );
    assert!(all.iter().any(|i| matches!(i, RenderItem::Text(_))), "text");
    assert!(
        all.iter().any(|i| matches!(i, RenderItem::Geometry(_))),
        "geometry"
    );

    let pdf = render::render_pdf(&doc, &FontOptions::default(), PageSel::All).unwrap();
    assert!(pdf.starts_with(b"%PDF"));
}

#[test]
fn svg_renders_one_file_per_page_with_embedded_font() {
    let doc = load("Note.note", Layout::PerPage);
    let pages =
        render::render_svg(&doc, &FontOptions::default(), PageSel::All).expect("svg should render");
    assert_eq!(pages.len(), doc.canvases.len());
    let (_, svg) = &pages[0];
    assert!(svg.starts_with("<svg"));
    assert!(svg.ends_with("</svg>"));
    // The note has text, so a subset font should be embedded.
    assert!(svg.contains("@font-face"), "subset font embedded");
}

#[test]
fn png_renders_expected_pixel_size() {
    let doc = load("Note.note", Layout::PerPage);
    let scale = 2.0;
    let pages = render::render_png(&doc, &FontOptions::default(), PageSel::One(1), scale)
        .expect("png should render");
    assert_eq!(pages.len(), 1);
    let (_, bytes) = &pages[0];
    assert!(bytes.starts_with(b"\x89PNG"));
    // Decode the IHDR width/height (big-endian u32 at offsets 16/20).
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let expected = (doc.canvases[0].width * scale).ceil() as u32;
    assert_eq!(w, expected, "png width matches scaled canvas");
}

#[test]
fn flat_marker_marks_highlighter_strokes() {
    let mut doc = load("Note.note", Layout::PerPage);
    doc.flatten_markers();
    let mut seen = 0;
    for canvas in &doc.canvases {
        for item in &canvas.items {
            if let RenderItem::Stroke(s) = item
                && s.pen_type == 15
            {
                seen += 1;
                assert!(s.flat_marker, "marker stroke marked flat");
                // Translucency + Multiply compositing are kept.
                assert!(s.color.a < 1.0, "marker stays translucent");
            }
        }
    }
    assert!(seen > 0, "note contains pen-15 strokes");
}
