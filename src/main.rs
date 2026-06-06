//! `boox-notes-renderer` — convert an BOOX (ONYX) infinite-note `.note` file to
//! PDF, SVG, or PNG.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};

use boox_notes_renderer::model::{Document, Layout};
use boox_notes_renderer::render::{self, FontOptions, PageSel};

#[derive(Parser)]
#[command(
    name = "boox-notes-renderer",
    about = "Convert BOOX (ONYX) .note files to PDF, SVG, or PNG"
)]
struct Args {
    /// Input `.note` file.
    input: PathBuf,
    /// Output path (defaults to the input name with the format's extension).
    output: Option<PathBuf>,
    /// Force this font file (TTF/OTF/TTC) for all text, bypassing name
    /// resolution. By default the note's font names are resolved to host fonts.
    #[arg(long)]
    font: Option<PathBuf>,
    /// Extra directory to search for fonts (repeatable).
    #[arg(long = "fonts-path")]
    fonts_path: Vec<PathBuf>,
    /// Map a requested font family to a host family, e.g.
    /// `--map-font "Noto Sans CJK JP=Hiragino Sans"` (repeatable).
    #[arg(long = "map-font")]
    map_font: Vec<String>,
    /// Download the note's fonts from Google Fonts (OFL/Apache) into a cache and
    /// use them, so text renders in the note's actual fonts. Any catalog family
    /// works; the BOOX Noto CJK names and color emoji are always fetched.
    #[arg(long = "download-fonts")]
    download_fonts: bool,
    /// Composite all pages onto one giant canvas (infinite-canvas mode) instead
    /// of the default one-page-per-note-page output.
    #[arg(long)]
    single_canvas: bool,
    /// Draw each translucent marker/highlighter stroke so it composites exactly
    /// once: no dark blotches where a stroke crosses itself (an artifact some
    /// PDF viewers add to BOOX's single-stroked-path construct).
    /// Per-stroke translucency and Multiply blending are kept, so separate
    /// strokes still darken where they overlap.
    #[arg(long)]
    flat_marker: bool,
    /// Output format. Inferred from the output extension if omitted (default PDF).
    #[arg(long, value_enum)]
    format: Option<Format>,
    /// Render only this single (1-based) page.
    #[arg(long)]
    page: Option<usize>,
    /// PNG scale in pixels per point (default 1.0).
    #[arg(long, default_value_t = 1.0)]
    scale: f32,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Pdf,
    Svg,
    Png,
}

impl Format {
    fn ext(self) -> &'static str {
        match self {
            Format::Pdf => "pdf",
            Format::Svg => "svg",
            Format::Png => "png",
        }
    }
}

/// Print the library's diagnostics (emitted via the `log` facade) to stderr:
/// `warn` with a `warning:` prefix, `info` (font-download progress) as-is.
/// Restricted to this crate's records so dependencies' logging stays out of
/// the CLI output.
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info && metadata.target().starts_with("boox_notes_renderer")
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        match record.level() {
            log::Level::Error => eprintln!("error: {}", record.args()),
            log::Level::Warn => eprintln!("warning: {}", record.args()),
            _ => eprintln!("{}", record.args()),
        }
    }

    fn flush(&self) {}
}

fn main() -> Result<()> {
    let _ = log::set_logger(&StderrLogger).map(|()| log::set_max_level(log::LevelFilter::Info));
    let args = Args::parse();

    let layout = if args.single_canvas {
        Layout::SingleCanvas
    } else {
        Layout::PerPage
    };

    // Resolve format: explicit flag > output extension > PDF.
    let format = args.format.unwrap_or_else(|| {
        args.output
            .as_deref()
            .and_then(Path::extension)
            .and_then(|e| e.to_str())
            .and_then(|e| match e.to_ascii_lowercase().as_str() {
                "svg" => Some(Format::Svg),
                "png" => Some(Format::Png),
                "pdf" => Some(Format::Pdf),
                _ => None,
            })
            .unwrap_or(Format::Pdf)
    });

    let file =
        fs::File::open(&args.input).with_context(|| format!("opening {}", args.input.display()))?;
    let mut doc = Document::load(file, layout).context("parsing .note")?;
    if args.flat_marker {
        doc.flatten_markers();
    }

    // Page-range validation happens in the render functions (typed
    // Error::PageOutOfRange), before any font setup/download.
    let sel = match args.page {
        Some(p) => PageSel::One(p),
        None => PageSel::All,
    };

    // Output stem (without extension) used to build per-file names.
    let stem = args
        .output
        .clone()
        .unwrap_or_else(|| args.input.clone())
        .with_extension("");

    // Parse "Requested=Target" font mappings.
    let map = args
        .map_font
        .iter()
        .filter_map(|m| {
            let (k, v) = m.split_once('=')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    let font_opts = FontOptions::default()
        .with_explicit(args.font)
        .with_dirs(args.fonts_path)
        .with_map(map)
        .with_download(args.download_fonts);

    let files: Vec<(String, Vec<u8>)> = match format {
        // PDF is one (multi-page) file; --page selects a single page inside it.
        Format::Pdf => vec![(String::new(), render::render_pdf(&doc, &font_opts, sel)?)],
        Format::Svg => render::render_svg(&doc, &font_opts, sel)?
            .into_iter()
            .map(|(s, svg)| (s, svg.into_bytes()))
            .collect(),
        Format::Png => render::render_png(&doc, &font_opts, sel, args.scale)?,
    };

    let ext = format.ext();
    for (suffix, bytes) in &files {
        let path = PathBuf::from(format!("{}{}.{}", stem.display(), suffix, ext));
        fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    }
    eprintln!(
        "wrote {} file(s) ({}), {} total bytes",
        files.len(),
        ext,
        files.iter().map(|(_, b)| b.len()).sum::<usize>()
    );
    Ok(())
}
