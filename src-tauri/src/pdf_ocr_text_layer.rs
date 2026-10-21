//! A scan's text layer: the words Tesseract reads on each page of a PDF that has no text, with where each word sits in
//! the page's own coordinates (PDF user space, points, y up), so a reader can lay them over the page as selectable,
//! searchable text and the document made from the scan can have its words.
//!
//! The scan itself is never rewritten: this produces a sidecar (`sophia.ocr-text-layer/1`) beside the original.
//!
//! Engines: pdfium (BSD/Apache, loaded at run time) renders a page; the `tesseract` program reads it (TSV: one row per
//! word with its pixel box and confidence). A page that already has text is left alone ("has-text"). Where to find them:
//! `GARDEN_PDFIUM_LIB` (a library file), else `libpdfium` beside the gardend executable, else the system's; and
//! `GARDEN_TESSERACT` (a program), else `tesseract` on PATH.

use pdfium_render::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

pub(crate) const OCR_TEXT_LAYER_SCHEMA: &str = "sophia.ocr-text-layer/1";

/// A page with at least this many characters of text is read as text, not scanned.
const TEXT_PAGE_MIN_CHARS: usize = 20;
/// Pixels per inch Tesseract reads a page at; the long side is capped so a poster does not become a gigapixel.
const OCR_DPI: f32 = 300.0;
const OCR_MAX_SIDE_PX: f32 = 5000.0;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OcrTextLayer {
    pub schema: String,
    /// "tesseract 5.5.2", as the program reports itself.
    pub engine: String,
    pub lang: String,
    pub dpi: u32,
    pub page_count: u32,
    pub pages: Vec<OcrPage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OcrPage {
    /// 1-based.
    pub page: u32,
    /// "ocr": read by Tesseract; "has-text": the PDF already has text here; "failed": Tesseract failed on this page
    /// (the reason is in `note`).
    pub status: String,
    /// The page's /Rotate, clockwise degrees (0, 90, 180, 270): the page is read as it is shown, upright, and each
    /// word placed back in the page's own coordinates; a reader turns its text the same way it turns the page.
    #[serde(default)]
    pub rotation: u32,
    /// The page box the words are placed in (the crop box, else the media box), PDF user space.
    pub left: f32,
    pub bottom: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<OcrWord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OcrWord {
    pub text: String,
    /// The word's box in PDF user space: x0 < x1, y0 (bottom) < y1 (top).
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    /// Tesseract's confidence, 0-100.
    pub conf: f32,
    /// Tesseract's block, paragraph and line numbers on the page: words with the same three are one line.
    pub block: u32,
    pub par: u32,
    pub line: u32,
}

impl OcrPage {
    /// The page's words as text: a line per line, a blank line between paragraphs.
    pub(crate) fn text(&self) -> String {
        let mut out = String::new();
        let mut last: Option<(u32, u32, u32)> = None;
        for word in &self.words {
            let key = (word.block, word.par, word.line);
            match last {
                None => {}
                Some((block, par, _)) if (block, par) != (key.0, key.1) => out.push_str("\n\n"),
                Some(previous) if previous != key => out.push('\n'),
                Some(_) => out.push(' '),
            }
            out.push_str(&word.text);
            last = Some(key);
        }
        out
    }
}

impl OcrTextLayer {
    /// The scan's words as paragraphs, page by page (pages without OCR words are skipped).
    pub(crate) fn paragraphs(&self) -> Vec<(u32, String)> {
        let mut paragraphs = Vec::new();
        for page in &self.pages {
            for paragraph in page.text().split("\n\n") {
                let joined = join_hyphenated_lines(paragraph);
                if !joined.trim().is_empty() {
                    paragraphs.push((page.page, joined));
                }
            }
        }
        paragraphs
    }
}

/// One paragraph's lines as running text: a word broken at a line end with a hyphen is joined again.
fn join_hyphenated_lines(paragraph: &str) -> String {
    let mut out = String::new();
    for line in paragraph.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if out.ends_with('-') && line.chars().next().is_some_and(char::is_lowercase) {
            out.pop();
        } else if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(line);
    }
    out
}

static PDFIUM: OnceLock<Result<Pdfium, String>> = OnceLock::new();

fn pdfium_library_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("GARDEN_PDFIUM_LIB").filter(|value| !value.is_empty()) {
        candidates.push(PathBuf::from(path));
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        candidates.push(Pdfium::pdfium_platform_library_name_at_path(&dir));
        candidates.push(Pdfium::pdfium_platform_library_name_at_path(
            &dir.join("lib"),
        ));
    }
    candidates
}

/// The process's one pdfium (its bindings can be made once per process).
pub(crate) fn pdfium() -> Result<&'static Pdfium, String> {
    PDFIUM
        .get_or_init(|| {
            for candidate in pdfium_library_candidates() {
                if candidate.is_file() {
                    return Pdfium::bind_to_library(&candidate)
                        .map(Pdfium::new)
                        .map_err(|error| format!("pdfium at {} could not be loaded: {error}", candidate.display()));
                }
            }
            Pdfium::bind_to_system_library().map(Pdfium::new).map_err(|error| {
                format!(
                    "pdfium is not available: set GARDEN_PDFIUM_LIB to a libpdfium file or put it beside gardend ({error})"
                )
            })
        })
        .as_ref()
        .map_err(Clone::clone)
}

fn tesseract_program() -> String {
    std::env::var("GARDEN_TESSERACT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "tesseract".to_string())
}

/// "tesseract 5.5.2", or an error naming what to install.
pub(crate) fn tesseract_version() -> Result<String, String> {
    let program = tesseract_program();
    let output = Command::new(&program).arg("--version").output().map_err(|error| {
        format!("Tesseract is not available ({program}: {error}); install it (macOS: brew install tesseract; Debian: apt install tesseract-ocr) or set GARDEN_TESSERACT")
    })?;
    let text = String::from_utf8_lossy(&output.stdout).to_string()
        + &String::from_utf8_lossy(&output.stderr);
    Ok(text
        .lines()
        .next()
        .unwrap_or("tesseract")
        .trim()
        .to_string())
}

#[derive(Debug, Clone)]
pub(crate) struct OcrOptions {
    /// Tesseract language(s), e.g. "eng" or "eng+fra".
    pub lang: String,
    /// Read every page, even one that already has text.
    pub force: bool,
}

impl Default for OcrOptions {
    fn default() -> Self {
        Self {
            lang: "eng".to_string(),
            force: false,
        }
    }
}

/// Read the text of every page that has none. `progress(done, total)` after each page; `cancelled()` is asked between
/// pages and stops the run with an error.
pub(crate) fn ocr_pdf_text_layer(
    bytes: &[u8],
    options: &OcrOptions,
    work_dir: &Path,
    mut progress: impl FnMut(u32, u32),
    cancelled: impl Fn() -> bool,
) -> Result<OcrTextLayer, String> {
    let engine = tesseract_version()?;
    let pdfium = pdfium()?;
    let document = pdfium
        .load_pdf_from_byte_slice(bytes, None)
        .map_err(|error| format!("the PDF could not be opened: {error}"))?;
    let total = document.pages().len() as u32;
    std::fs::create_dir_all(work_dir)
        .map_err(|error| format!("could not make {}: {error}", work_dir.display()))?;
    let mut pages = Vec::with_capacity(total as usize);
    for (index, page) in document.pages().iter().enumerate() {
        if cancelled() {
            return Err("cancelled".to_string());
        }
        let number = index as u32 + 1;
        let frame = page
            .boundaries()
            .crop()
            .or_else(|_| page.boundaries().media())
            .map(|boundary| boundary.bounds)
            .unwrap_or_else(|_| {
                PdfRect::new_from_values(0.0, 0.0, page.height().value, page.width().value)
            });
        let rotation = match page.rotation().unwrap_or(PdfPageRenderRotation::None) {
            PdfPageRenderRotation::None => 0,
            PdfPageRenderRotation::Degrees90 => 90,
            PdfPageRenderRotation::Degrees180 => 180,
            PdfPageRenderRotation::Degrees270 => 270,
        };
        let mut out = OcrPage {
            page: number,
            status: "ocr".to_string(),
            rotation,
            left: frame.left().value,
            bottom: frame.bottom().value,
            width: frame.width().value,
            height: frame.height().value,
            words: Vec::new(),
            note: None,
        };
        let chars = page
            .text()
            .map(|text| text.all().chars().filter(|c| !c.is_whitespace()).count())
            .unwrap_or(0);
        if chars >= TEXT_PAGE_MIN_CHARS && !options.force {
            out.status = "has-text".to_string();
        } else {
            match read_page(&page, &out, number, options, work_dir) {
                Ok(words) => out.words = words,
                Err(error) => {
                    out.status = "failed".to_string();
                    out.note = Some(error);
                }
            }
        }
        pages.push(out);
        progress(number, total);
    }
    Ok(OcrTextLayer {
        schema: OCR_TEXT_LAYER_SCHEMA.to_string(),
        engine,
        lang: options.lang.clone(),
        dpi: OCR_DPI as u32,
        page_count: total,
        pages,
    })
}

fn read_page(
    page: &PdfPage<'_>,
    frame: &OcrPage,
    number: u32,
    options: &OcrOptions,
    work_dir: &Path,
) -> Result<Vec<OcrWord>, String> {
    let long_side_pt = frame.width.max(frame.height).max(1.0);
    let scale = (OCR_DPI / 72.0).min(OCR_MAX_SIDE_PX / long_side_pt);
    // Rendered as shown: a page turned a quarter is as wide as the box is tall.
    let shown_width = if frame.rotation % 180 == 90 {
        frame.height
    } else {
        frame.width
    };
    let target_width = (shown_width * scale).round().max(1.0) as i32;
    let config = PdfRenderConfig::new()
        .set_target_width(target_width)
        .render_form_data(false);
    let bitmap = page
        .render_with_config(&config)
        .map_err(|error| format!("page {number} could not be rendered: {error}"))?;
    let image = bitmap
        .as_image()
        .map_err(|error| format!("page {number} could not be read as an image: {error}"))?;
    let (px_width, px_height) = (image.width() as f32, image.height() as f32);
    let png = work_dir.join(format!("page-{number:05}.png"));
    image
        .to_luma8()
        .save(&png)
        .map_err(|error| format!("page {number} could not be written: {error}"))?;
    let dpi = (scale * 72.0).round() as u32;
    let output = Command::new(tesseract_program())
        .arg(&png)
        .arg("stdout")
        .args(["-l", &options.lang, "--dpi", &dpi.to_string(), "tsv"])
        .output()
        .map_err(|error| format!("Tesseract could not run on page {number}: {error}"));
    let _ = std::fs::remove_file(&png);
    let output = output?;
    if !output.status.success() {
        return Err(format!(
            "Tesseract failed on page {number}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(
        parse_tesseract_tsv(&String::from_utf8_lossy(&output.stdout))
            .into_iter()
            .map(|word| {
                let (x0, y0, x1, y1) = shown_box_to_page(
                    frame,
                    (
                        word.left / px_width,
                        word.top / px_height,
                        (word.left + word.width) / px_width,
                        (word.top + word.height) / px_height,
                    ),
                );
                OcrWord {
                    x0,
                    y0,
                    x1,
                    y1,
                    text: word.text,
                    conf: word.conf,
                    block: word.block,
                    par: word.par,
                    line: word.line,
                }
            })
            .collect(),
    )
}

/// A box on the page as shown (fractions of its width and height, from the top left) in the page's own coordinates
/// (PDF user space, y up), undoing the page's /Rotate: (x0, y0, x1, y1) with x0 < x1 and y0 < y1.
fn shown_box_to_page(
    frame: &OcrPage,
    (u0, v0, u1, v1): (f32, f32, f32, f32),
) -> (f32, f32, f32, f32) {
    let (l, b, w, h) = (frame.left, frame.bottom, frame.width, frame.height);
    let point = |u: f32, v: f32| -> (f32, f32) {
        match frame.rotation {
            90 => (l + v * w, b + u * h),
            180 => (l + w - u * w, b + v * h),
            270 => (l + w - v * w, b + h - u * h),
            _ => (l + u * w, b + h - v * h),
        }
    };
    let (ax, ay) = point(u0, v0);
    let (bx, by) = point(u1, v1);
    (ax.min(bx), ay.min(by), ax.max(bx), ay.max(by))
}

#[derive(Debug, Clone, PartialEq)]
struct TsvWord {
    block: u32,
    par: u32,
    line: u32,
    left: f32,
    top: f32,
    width: f32,
    height: f32,
    conf: f32,
    text: String,
}

/// Tesseract's TSV: `level page_num block_num par_num line_num word_num left top width height conf text`; level 5 rows
/// are words. Empty words and the header are skipped.
fn parse_tesseract_tsv(tsv: &str) -> Vec<TsvWord> {
    tsv.lines()
        .filter_map(|line| {
            let cols: Vec<&str> = line.split('\t').collect();
            if cols.len() < 12 || cols[0] != "5" {
                return None;
            }
            let text = cols[11..].join("\t").trim().to_string();
            if text.is_empty() {
                return None;
            }
            let number = |i: usize| cols[i].trim().parse::<f32>().ok();
            Some(TsvWord {
                block: number(2)? as u32,
                par: number(3)? as u32,
                line: number(4)? as u32,
                left: number(6)?,
                top: number(7)?,
                width: number(8)?,
                height: number(9)?,
                conf: number(10).unwrap_or(-1.0),
                text,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_tesseract_tsv_words_and_skips_the_rest() {
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
                   1\t1\t0\t0\t0\t0\t0\t0\t2480\t3508\t-1\t\n\
                   5\t1\t1\t1\t1\t1\t300\t400\t220\t60\t96.5\tThe\n\
                   5\t1\t1\t1\t1\t2\t540\t400\t300\t60\t91.0\tquick\n\
                   5\t1\t1\t1\t1\t3\t860\t400\t10\t60\t95.0\t \n";
        let words = parse_tesseract_tsv(tsv);
        assert_eq!(words.len(), 2);
        assert_eq!(
            words[1],
            TsvWord {
                block: 1,
                par: 1,
                line: 1,
                left: 540.0,
                top: 400.0,
                width: 300.0,
                height: 60.0,
                conf: 91.0,
                text: "quick".into()
            }
        );
    }

    #[test]
    fn a_page_reads_as_lines_and_paragraphs_and_joins_a_hyphenated_line_end() {
        let word = |text: &str, block, par, line| OcrWord {
            text: text.into(),
            x0: 0.0,
            y0: 0.0,
            x1: 1.0,
            y1: 1.0,
            conf: 90.0,
            block,
            par,
            line,
        };
        let page = OcrPage {
            page: 3,
            status: "ocr".into(),
            rotation: 0,
            left: 0.0,
            bottom: 0.0,
            width: 612.0,
            height: 792.0,
            note: None,
            words: vec![
                word("Knowledge", 1, 1, 1),
                word("is", 1, 1, 1),
                word("an", 1, 1, 2),
                word("ef-", 1, 1, 2),
                word("fective", 1, 1, 3),
                word("action.", 1, 1, 3),
                word("Next", 1, 2, 1),
            ],
        };
        assert_eq!(page.text(), "Knowledge is\nan ef-\nfective action.\n\nNext");
        let layer = OcrTextLayer {
            schema: OCR_TEXT_LAYER_SCHEMA.into(),
            engine: "t".into(),
            lang: "eng".into(),
            dpi: 300,
            page_count: 3,
            pages: vec![page],
        };
        assert_eq!(
            layer.paragraphs(),
            vec![
                (3, "Knowledge is an effective action.".to_string()),
                (3, "Next".to_string())
            ]
        );
    }

    #[test]
    fn a_box_on_the_page_as_shown_goes_back_through_the_pages_rotation() {
        // A 400 x 600 crop box whose bottom sits 72 pt up (as the Maturana scan's pages do).
        let frame = |rotation| OcrPage {
            page: 1,
            status: "ocr".into(),
            rotation,
            left: 0.0,
            bottom: 72.0,
            width: 400.0,
            height: 600.0,
            words: vec![],
            note: None,
        };
        // The top-left tenth of the page as shown.
        let shown = (0.0, 0.0, 0.1, 0.1);
        assert_eq!(
            shown_box_to_page(&frame(0), shown),
            (0.0, 612.0, 40.0, 672.0)
        );
        // Turned a quarter clockwise: what shows at the top left is the box's bottom-left corner.
        assert_eq!(
            shown_box_to_page(&frame(90), shown),
            (0.0, 72.0, 40.0, 132.0)
        );
        // Upside down: the box's bottom-right corner.
        assert_eq!(
            shown_box_to_page(&frame(180), shown),
            (360.0, 72.0, 400.0, 132.0)
        );
        // A quarter the other way: its top-right corner.
        assert_eq!(
            shown_box_to_page(&frame(270), shown),
            (360.0, 612.0, 400.0, 672.0)
        );
    }

    /// The real engines, end to end: write a page of text with pdfium, photograph it into a new PDF that holds only the
    /// picture (a scan), and read it back: the words come back, in reading order, where they were printed.
    #[test]
    fn reads_a_scanned_page_back_into_words_where_they_were_printed() {
        if tesseract_version().is_err() || pdfium().is_err() {
            eprintln!(
                "SKIPPED: needs tesseract and pdfium (GARDEN_PDFIUM_LIB): {:?} {:?}",
                tesseract_version().err(),
                pdfium().err()
            );
            return;
        }
        let pdfium = pdfium().unwrap();
        let mut printed = pdfium.create_new_pdf().unwrap();
        let size = PdfPagePaperSize::a4();
        let mut page = printed.pages_mut().create_page_at_end(size).unwrap();
        let font = printed.fonts_mut().helvetica();
        page.objects_mut()
            .create_text_object(
                PdfPoints::new(72.0),
                PdfPoints::new(700.0),
                "The quick brown fox jumps over the lazy dog",
                font,
                PdfPoints::new(22.0),
            )
            .unwrap();
        page.objects_mut()
            .create_text_object(
                PdfPoints::new(72.0),
                PdfPoints::new(400.0),
                "Knowledge is effective action",
                font,
                PdfPoints::new(22.0),
            )
            .unwrap();
        let photo = page
            .render_with_config(&PdfRenderConfig::new().set_target_width(2480))
            .unwrap()
            .as_image()
            .unwrap();
        let mut scan = pdfium.create_new_pdf().unwrap();
        let mut scan_page = scan.pages_mut().create_page_at_end(size).unwrap();
        scan_page
            .objects_mut()
            .create_image_object(
                PdfPoints::ZERO,
                PdfPoints::ZERO,
                &photo,
                Some(size.width()),
                Some(size.height()),
            )
            .unwrap();
        let bytes = scan.save_to_bytes().unwrap();

        let dir = std::env::temp_dir().join(format!("gardend-ocr-test-{}", std::process::id()));
        let mut seen = Vec::new();
        let layer = ocr_pdf_text_layer(
            &bytes,
            &OcrOptions::default(),
            &dir,
            |done, total| seen.push((done, total)),
            || false,
        )
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(seen, vec![(1, 1)]);
        assert_eq!(layer.pages[0].status, "ocr");
        let text = layer.pages[0].text();
        assert!(
            text.contains("The quick brown fox jumps over the lazy dog"),
            "read: {text:?}"
        );
        assert!(
            text.contains("Knowledge is effective action"),
            "read: {text:?}"
        );
        let fox = layer.pages[0]
            .words
            .iter()
            .find(|word| word.text == "fox")
            .expect("fox");
        // Printed on a baseline at y = 700 pt, 22 pt type: the word's box straddles that line, from about 72 pt in.
        assert!(
            fox.y0 > 690.0 && fox.y0 < 705.0 && fox.y1 > 710.0 && fox.y1 < 725.0,
            "fox at {fox:?}"
        );
        assert!(fox.x0 > 72.0 && fox.x1 < 300.0, "fox at {fox:?}");

        // The same photograph stored upside down on a page turned 180 degrees (it shows upright, as a scanned book's
        // pages often are): the words are read upright and placed back where they sit in the page's coordinates.
        let mut turned = pdfium.create_new_pdf().unwrap();
        let mut turned_page = turned.pages_mut().create_page_at_end(size).unwrap();
        turned_page
            .objects_mut()
            .create_image_object(
                PdfPoints::ZERO,
                PdfPoints::ZERO,
                &photo.rotate180(),
                Some(size.width()),
                Some(size.height()),
            )
            .unwrap();
        turned_page.set_rotation(PdfPageRenderRotation::Degrees180);
        let read = ocr_pdf_text_layer(
            &turned.save_to_bytes().unwrap(),
            &OcrOptions::default(),
            &dir,
            |_, _| {},
            || false,
        )
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            (read.pages[0].status.as_str(), read.pages[0].rotation),
            ("ocr", 180)
        );
        assert!(
            read.pages[0]
                .text()
                .contains("The quick brown fox jumps over the lazy dog"),
            "read: {:?}",
            read.pages[0].text()
        );
        let fox = read.pages[0]
            .words
            .iter()
            .find(|word| word.text == "fox")
            .expect("fox");
        // Shown 72 pt in from the left at a baseline 700 pt up; stored turned, so in the page's own coordinates it is
        // as far in from the right and as far down from the top (A4: 595 x 842).
        let (w, h) = (size.width().value, size.height().value);
        assert!(fox.x1 < w - 72.0 && fox.x0 > w - 300.0, "fox at {fox:?}");
        assert!(
            fox.y1 > h - 705.0 && fox.y1 < h - 690.0 && fox.y0 > h - 725.0,
            "fox at {fox:?}"
        );

        // The same PDF with its text still in it is left alone.
        let has_text = ocr_pdf_text_layer(
            &printed.save_to_bytes().unwrap(),
            &OcrOptions::default(),
            &dir,
            |_, _| {},
            || false,
        )
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(has_text.pages[0].status, "has-text");
        assert!(has_text.pages[0].words.is_empty());
    }
}
