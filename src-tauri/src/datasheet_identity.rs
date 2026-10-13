#![allow(dead_code)]
//! Datasheet identity: which parts a document is about, and who made it, by
//! hearing several witnesses and reconciling them.
//!
//! The parser's own guess (`document.parts`, `document.manufacturer`) is one
//! witness. The others are the document's title, the first pages of its own
//! text (the strongest witness: the company prints its name, its domain and
//! its part numbers on the first pages), and the file name (weak: real
//! downloads often carry no maker prefix). Each yields candidate parts and/or
//! a maker. The reconciled answer favours the document's own text and says how
//! it knows (`text`, `title`, `parser`, `filename`, or a `+` combination); a
//! disagreement is recorded as a diagnostic, never hidden. The parser's
//! envelope is never edited.
//!
//! Std only, so the rules can be exercised on their own. The same rules are
//! written a second time in `identity-score.mjs` (kept outside this repository)
//! (the corpus scorer); the tests here run the real corpus excerpts under
//! `datasheet_fixtures/identity/` through this code and pin the answers that
//! script prints, so the two cannot drift silently.

/// Pages of text the importer looks at, and the most bytes of them.
pub const WINDOW_PAGES: usize = 8;
pub const WINDOW_BYTES: usize = 48 * 1024;
/// The most bytes read from a text rendition before windowing.
pub const READ_BYTES: usize = 256 * 1024;

pub struct Evidence<'a> {
    /// `document.parts`, as the parser wrote them.
    pub parser_parts: &'a [String],
    pub parser_maker: Option<&'a str>,
    pub parser_maker_by: Option<&'a str>,
    pub title: Option<&'a str>,
    /// `document.original_name` and `document.file`.
    pub file_names: &'a [String],
    /// The text rendition (any length; windowed here).
    pub text: Option<&'a str>,
    /// Switch the weak file-name witness off (scoring, or when the name is a hash).
    pub use_filename: bool,
}

#[derive(Debug, Default, Clone)]
pub struct Identity {
    pub parts: Vec<String>,
    pub parts_by: String,
    pub maker: Option<String>,
    pub maker_by: Option<String>,
    /// A literature number printed on the first page or in the title (TI "SBAS444E").
    pub literature: Option<String>,
    pub diagnostics: Vec<String>,
    /// What each witness said, as JSON text.
    pub witnesses_json: String,
}

// ------------------------------------------------------------------ lexicon

struct Maker {
    name: &'static str,
    /// Lower-case phrases and domains that appear in the document's own text.
    text: &'static [&'static str],
    /// Lower-case first words of a file name.
    file: &'static [&'static str],
}

const LEXICON: &[Maker] = &[
    Maker { name: "Texas Instruments", text: &["texas instruments", "ti.com"], file: &["ti", "texas"] },
    Maker { name: "Analog Devices", text: &["analog devices", "analog.com"], file: &["adi", "analog"] },
    Maker { name: "Infineon", text: &["infineon", "international rectifier", "irf.com"], file: &["infineon", "ir"] },
    Maker { name: "Microchip Technology", text: &["microchip", "atmel"], file: &["microchip", "atmel"] },
    Maker { name: "Espressif", text: &["espressif"], file: &["espressif"] },
    Maker { name: "Bosch Sensortec", text: &["bosch sensortec", "bosch"], file: &["bosch"] },
    Maker { name: "Vishay", text: &["vishay", "siliconix"], file: &["vishay"] },
    Maker { name: "Murata", text: &["murata"], file: &["murata"] },
    Maker { name: "Zilog", text: &["zilog"], file: &["zilog"] },
    Maker { name: "JST", text: &["jst", "jst-mfg"], file: &["jst"] },
    Maker { name: "STMicroelectronics", text: &["stmicroelectronics", "st.com"], file: &["st", "stm"] },
    Maker { name: "NXP", text: &["nxp"], file: &["nxp"] },
    Maker { name: "onsemi", text: &["onsemi", "on semiconductor", "fairchild"], file: &["onsemi", "fairchild"] },
    Maker { name: "Renesas", text: &["renesas", "intersil"], file: &["renesas", "intersil"] },
    Maker { name: "Nexperia", text: &["nexperia"], file: &["nexperia"] },
    Maker { name: "Diodes Incorporated", text: &["diodes incorporated", "diodes.com"], file: &["diodes"] },
    Maker { name: "Maxim Integrated", text: &["maxim integrated", "maximintegrated"], file: &["maxim"] },
    Maker { name: "Linear Technology", text: &["linear technology"], file: &["linear", "ltc"] },
    Maker { name: "Nordic Semiconductor", text: &["nordic semiconductor", "nordicsemi"], file: &["nordic"] },
    Maker { name: "Silicon Labs", text: &["silicon labs", "silabs"], file: &["silabs"] },
    Maker { name: "ROHM", text: &["rohm"], file: &["rohm"] },
    Maker { name: "Toshiba", text: &["toshiba"], file: &["toshiba"] },
    Maker { name: "Cypress", text: &["cypress semiconductor"], file: &["cypress"] },
    Maker { name: "Allegro MicroSystems", text: &["allegro microsystems"], file: &["allegro"] },
    Maker { name: "Monolithic Power Systems", text: &["monolithic power"], file: &["mps"] },
    Maker { name: "Semtech", text: &["semtech"], file: &["semtech"] },
    Maker { name: "Lattice Semiconductor", text: &["lattice semiconductor"], file: &["lattice"] },
    Maker { name: "Littelfuse", text: &["littelfuse"], file: &["littelfuse"] },
    Maker { name: "TDK", text: &["tdk"], file: &["tdk"] },
    Maker { name: "Kemet", text: &["kemet"], file: &["kemet"] },
    Maker { name: "Panasonic", text: &["panasonic"], file: &["panasonic"] },
    Maker { name: "arXiv", text: &["arxiv"], file: &["arxiv"] },
];

// ------------------------------------------------------------------ helpers

pub fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
}

fn esc(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if (c as u32) < 0x20 => o.push(' '),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn json_list(items: &[String]) -> String {
    format!("[{}]", items.iter().map(|s| esc(s)).collect::<Vec<_>>().join(","))
}

/// The text the importer reads: from page 1 to `WINDOW_PAGES`, at most `WINDOW_BYTES`.
pub fn window(text: &str) -> &str {
    let start = text.find("=== PAGE 1 ===").unwrap_or(0);
    let body = &text[start..];
    let end = body.find(&format!("=== PAGE {} ===", WINDOW_PAGES + 1)).unwrap_or(body.len());
    let mut body = &body[..end];
    if body.len() > WINDOW_BYTES {
        let mut cut = WINDOW_BYTES;
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body = &body[..cut];
    }
    body
}

fn first_page(win: &str) -> &str {
    let end = win.find("=== PAGE 2 ===").unwrap_or(win.len());
    &win[..end]
}

/// Words of a line: ASCII letters, digits and `- + _ . /` kept, everything
/// else (including markdown escapes) a separator.
fn words(line: &str) -> Vec<String> {
    let cleaned: String = line
        .chars()
        .filter(|c| *c != '\\')
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '+' | '_' | '.' | '/') { c } else { ' ' })
        .collect();
    cleaned
        .split_whitespace()
        .map(|w| w.trim_matches(|c| c == '.' || c == '-' || c == '/' || c == '_').to_string())
        .filter(|w| !w.is_empty())
        .collect()
}

#[derive(PartialEq)]
enum Kind {
    Part,
    Wildcard,
    No,
}

fn is_noise(n: &str) -> bool {
    // Interfaces and standards that carry digits but name no part.
    const NOISE: &[&str] = &[
        "usb", "ddr", "i2c", "i2s", "spi", "can", "rs", "ieee", "jedec", "iso", "iec", "ul", "rohs", "wifi", "lte",
        "bt", "ble", "uart", "pcie", "sata", "ip", "mil", "eia", "aec", "qfn", "sot", "soic", "tssop", "vssop",
        "wson", "bga", "lqfp", "tqfp", "dip",
    ];
    let trimmed = n.trim_end_matches(|c: char| c.is_ascii_digit());
    let stripped = trimmed.strip_suffix(|c: char| c.is_ascii_lowercase()).unwrap_or(trimmed);
    [n, trimmed, stripped].iter().any(|c| NOISE.contains(c))
}

fn classify(w: &str) -> Kind {
    let b = w.as_bytes();
    if w.len() < 3 || w.len() > 32 || !b[0].is_ascii_alphabetic() {
        return Kind::No;
    }
    if !w.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '+' | '.' | '_')) {
        return Kind::No;
    }
    let letters = w.chars().filter(|c| c.is_ascii_alphabetic()).count();
    let digits = w.chars().filter(|c| c.is_ascii_digit()).count();
    if letters < 1 || digits < 1 {
        return Kind::No;
    }
    let chars: Vec<char> = w.chars().collect();
    for i in 1..chars.len() {
        if chars[i] == 'x' && (chars[i - 1].is_ascii_uppercase() || chars[i - 1].is_ascii_digit()) {
            return Kind::Wildcard;
        }
    }
    if is_noise(&norm(w)) {
        return Kind::No;
    }
    Kind::Part
}

/// TI-style literature numbers: SBAS444E, SLOS068AB, SLVU720A, SNOSD82D.
pub fn is_literature(w: &str) -> bool {
    let c: Vec<char> = w.chars().collect();
    if c.len() < 7 || c.len() > 10 || c[0] != 'S' || !c.iter().all(|x| x.is_ascii_uppercase() || x.is_ascii_digit()) {
        return false;
    }
    let mut i = 1;
    let mut letters = 0;
    while i < c.len() && c[i].is_ascii_uppercase() {
        letters += 1;
        i += 1;
    }
    let mut digits = 0;
    while i < c.len() && c[i].is_ascii_digit() {
        digits += 1;
        i += 1;
    }
    let mut tail = 0;
    while i < c.len() && c[i].is_ascii_uppercase() {
        tail += 1;
        i += 1;
    }
    i == c.len() && (3..=4).contains(&letters) && (2..=3).contains(&digits) && tail <= 2
}

fn find_literature(s: &str) -> Option<String> {
    for w in words(s) {
        for piece in w.split('-') {
            if is_literature(piece) {
                return Some(piece.to_string());
            }
        }
    }
    None
}

/// Tidy one part token: cut a `_` suffix, drop the lead-free marker `PbF`,
/// trim punctuation. `None` when no digit is left.
fn finish(tok: &str) -> Option<String> {
    let core = tok.split('_').next().unwrap_or(tok);
    let mut t = core.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '+').to_string();
    let lower = t.to_ascii_lowercase();
    if lower.ends_with("pbf") && t.len() >= 7 {
        t.truncate(t.len() - 3);
        t = t.trim_end_matches('-').to_string();
    }
    if t.chars().any(|c| c.is_ascii_digit()) && t.len() >= 3 {
        Some(t)
    } else {
        None
    }
}

fn push_unique(out: &mut Vec<String>, t: String) {
    let n = norm(&t);
    if !n.is_empty() && !out.iter().any(|o| norm(o) == n) {
        out.push(t);
    }
}

/// `ATmega48A/PA/88A/PA/328/P` -> ATmega48A, ATmega48PA, ATmega88A, ...
fn expand(w: &str) -> Vec<String> {
    if !w.contains('/') {
        return finish(w).into_iter().collect();
    }
    let mut segs = w.split('/').filter(|s| !s.is_empty());
    let base = match segs.next() {
        Some(b) if classify(b) == Kind::Part => b,
        _ => return Vec::new(),
    };
    let prefix: String = base.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    let mut digits: String = base
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let mut out: Vec<String> = finish(base).into_iter().collect();
    for seg in segs {
        let cand = if seg.starts_with(|c: char| c.is_ascii_digit()) {
            digits = seg.chars().take_while(|c| c.is_ascii_digit()).collect();
            format!("{prefix}{seg}")
        } else if seg.len() <= 3 && seg.chars().all(|c| c.is_ascii_alphabetic()) {
            format!("{prefix}{digits}{seg}")
        } else if classify(seg) == Kind::Part {
            seg.to_string()
        } else {
            continue;
        };
        out.extend(finish(&cand));
    }
    out
}

/// The part numbers that open a line (a heading or a title): skip up to four
/// short junk words, then take the run of part-like words, stopping at the
/// first word that is not one. `OPAx134`/`ADS111x` family wildcards name no part.
pub fn lead_parts(line: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let (mut started, mut skipped) = (false, 0);
    for w in words(line) {
        let lw = w.to_ascii_lowercase();
        if lw == "and" || lw == "or" {
            if !started {
                skipped += 1;
                if skipped > 4 {
                    break;
                }
            }
            continue;
        }
        if is_literature(&w) {
            if started {
                break;
            }
            skipped += 1;
            continue;
        }
        let kind = if w.contains('/') {
            if expand(&w).is_empty() { Kind::No } else { Kind::Part }
        } else {
            classify(&w)
        };
        match kind {
            Kind::Part => {
                for t in expand(&w) {
                    push_unique(&mut out, t);
                }
                started = true;
            }
            Kind::Wildcard => started = true,
            Kind::No => {
                if started {
                    break;
                }
                skipped += 1;
                if w.len() > 4 || skipped > 4 {
                    break;
                }
            }
        }
    }
    out
}

/// The parser's own part strings: split on `, / ;` and whitespace, a token
/// needs a digit. A token that is the file name itself is not a part.
fn parser_tokens(raws: &[String], stems: &[String], diags: &mut Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in raws {
        if !stems.is_empty() && stems.contains(&norm(raw)) {
            diags.push(format!("parser part {raw:?} is the file name; treated as the filename witness"));
            continue;
        }
        for tok in raw.split(|c: char| c == ',' || c == '/' || c == ';' || c.is_whitespace()) {
            let tok = tok.trim_matches(|c: char| !c.is_alphanumeric() && c != '+');
            if tok.is_empty() || tok.len() > 40 || !tok.chars().any(|c| c.is_ascii_digit()) {
                continue;
            }
            if !stems.is_empty() && stems.contains(&norm(tok)) {
                continue;
            }
            if let Some(t) = finish(tok) {
                push_unique(&mut out, t);
            }
        }
    }
    out
}

fn file_stems(names: &[String]) -> Vec<String> {
    let mut v = Vec::new();
    for n in names {
        let base = n.rsplit('/').next().unwrap_or(n);
        let stem = base.rsplit_once('.').map(|(a, _)| a).unwrap_or(base);
        let s = norm(stem);
        if s.len() >= 4 && !v.contains(&s) {
            v.push(s);
        }
    }
    v
}

const STOP_WORDS: &[&str] = &[
    "ds", "datasheet", "datasheets", "data", "sheet", "appnote", "app", "note", "spec", "specs", "productspec",
    "userguide", "guide", "manual", "trm", "errata", "master", "local", "older", "family", "series", "mlcc",
    "pdf", "reference", "rev", "revision", "final", "preliminary", "sidecar", "connector",
];

fn is_stop(w: &str) -> bool {
    let l = w.to_ascii_lowercase();
    if STOP_WORDS.contains(&l.as_str()) {
        return true;
    }
    // revA, v2.6, mar78, 2008
    let b = l.as_bytes();
    (l.starts_with("rev") && l.len() <= 5)
        || (l.starts_with('v') && l.len() >= 2 && b[1].is_ascii_digit())
        || (l.len() == 5
            && matches!(&l[..3], "jan" | "feb" | "mar" | "apr" | "may" | "jun" | "jul" | "aug" | "sep" | "oct" | "nov" | "dec")
            && l[3..].chars().all(|c| c.is_ascii_digit()))
        || (l.len() == 4 && l.chars().all(|c| c.is_ascii_digit()))
}

/// Maker and part read off a file name: `ti-lm358-family-datasheet.pdf`.
fn filename_witness(names: &[String]) -> (Option<&'static str>, Vec<String>, Option<String>) {
    for n in names {
        let base = n.rsplit('/').next().unwrap_or(n);
        let stem = base.rsplit_once('.').map(|(a, _)| a).unwrap_or(base);
        let ws: Vec<&str> = stem.split(|c: char| c == '-' || c == '_' || c == ' ').filter(|w| !w.is_empty()).collect();
        if ws.is_empty() {
            continue;
        }
        let first = ws[0].to_ascii_lowercase();
        let maker = LEXICON.iter().find(|m| m.file.contains(&first.as_str())).map(|m| m.name);
        let rest = if maker.is_some() { &ws[1..] } else { &ws[..] };
        let mut run: Vec<&str> = Vec::new();
        for w in rest {
            if is_stop(w) {
                break;
            }
            if run.is_empty() {
                if classify(w) == Kind::Part {
                    run.push(w);
                } else if !w.chars().all(|c| c.is_ascii_digit() || c == '.') {
                    continue; // a leading word that is no part: keep looking
                } else {
                    break;
                }
            } else if w.len() <= 5 && w.chars().all(|c| c.is_ascii_alphanumeric()) {
                run.push(w);
            } else {
                break;
            }
        }
        let mut part = None;
        let mut lit = None;
        if let Some(first) = run.first() {
            if is_literature(&first.to_ascii_uppercase()) {
                lit = Some(first.to_ascii_uppercase());
            } else {
                // trailing short words that carry nothing (a lone digit run) stay; text is stronger anyway
                part = finish(&run.join("-").to_ascii_uppercase());
            }
        }
        if maker.is_some() || part.is_some() || lit.is_some() {
            return (maker, part.into_iter().collect(), lit);
        }
    }
    (None, Vec::new(), None)
}

// -------------------------------------------------------------------- maker

/// Whole-word, case-insensitive count of `needle` (lower-case) in `hay` (lower-case).
fn count_word(hay: &str, needle: &str) -> (usize, usize) {
    let hb = hay.as_bytes();
    let (mut count, mut first) = (0, usize::MAX);
    let mut from = 0;
    while let Some(i) = hay[from..].find(needle) {
        let at = from + i;
        let end = at + needle.len();
        let before_ok = at == 0 || !hb[at - 1].is_ascii_alphanumeric();
        let after_ok = end >= hb.len() || !hb[end].is_ascii_alphanumeric();
        if before_ok && after_ok {
            count += 1;
            first = first.min(at);
        }
        from = end;
    }
    (count, first)
}

/// (maker, mentions, runner-up) the document's text names most.
fn text_maker(win: &str) -> Option<(&'static str, usize, Option<(&'static str, usize)>)> {
    let hay = win.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_lowercase();
    let mut hits: Vec<(&'static str, usize, usize)> = Vec::new();
    for m in LEXICON {
        let (mut c, mut f) = (0, usize::MAX);
        for p in m.text {
            let (pc, pf) = count_word(&hay, p);
            c += pc;
            f = f.min(pf);
        }
        if c > 0 {
            hits.push((m.name, c, f));
        }
    }
    // most mentions; the earlier first mention breaks a tie
    hits.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
    let top = hits.first()?;
    let second = hits.get(1).map(|h| (h.0, h.1));
    Some((top.0, top.1, second))
}

fn same_maker(a: &str, b: &str) -> bool {
    let (na, nb) = (norm(a), norm(b));
    !na.is_empty() && !nb.is_empty() && (na.contains(&nb) || nb.contains(&na))
}

// ---------------------------------------------------------------- reconcile

pub fn reconcile(e: &Evidence<'_>) -> Identity {
    let mut diags: Vec<String> = Vec::new();
    let names: &[String] = if e.use_filename { e.file_names } else { &[] };
    // The parser's guess is demoted when it only echoes the file name, even if
    // the file-name witness is switched off (it was never a reading of the document).
    let stems = file_stems(e.file_names);
    let parser = parser_tokens(e.parser_parts, &stems, &mut diags);
    let title_parts: Vec<String> = e.title.map(lead_parts).unwrap_or_default();
    let win = e.text.map(window);
    let mut text_parts: Vec<String> = Vec::new();
    let mut text_lit: Option<String> = None;
    let mut text_maker_hit = None;
    if let Some(w) = win {
        let page1 = first_page(w);
        for line in page1.lines().filter(|l| l.starts_with("## ")).take(4) {
            for t in lead_parts(line) {
                push_unique(&mut text_parts, t);
            }
        }
        text_lit = find_literature(page1);
        text_maker_hit = text_maker(w);
    }
    let (file_maker, file_parts, file_lit) = filename_witness(names);
    let literature = e.title.and_then(find_literature).or(text_lit.clone()).or(file_lit);

    // ---- parts: text, title, parser, filename
    let mut parts: Vec<String> = Vec::new();
    for t in &text_parts {
        push_unique(&mut parts, t.clone());
    }
    for t in &title_parts {
        push_unique(&mut parts, t.clone());
    }
    let mut parser_kept: Vec<String> = Vec::new();
    for t in &parser {
        let n = norm(t);
        // a short tail of a longer part number ("A12" for GRM188R71C105KA12) is a fragment
        let fragment = n.len() <= 4 && parts.iter().any(|p| norm(p).len() > n.len() && norm(p).ends_with(&n));
        if fragment {
            continue;
        }
        parser_kept.push(t.clone());
        push_unique(&mut parts, t.clone());
    }
    let mut by: Vec<&str> = Vec::new();
    let in_parts = |list: &[String]| list.iter().any(|t| parts.iter().any(|p| norm(p) == norm(t)));
    if in_parts(&text_parts) {
        by.push("text");
    }
    if in_parts(&title_parts) {
        by.push("title");
    }
    if in_parts(&parser_kept) {
        by.push("parser");
    }
    if parts.is_empty() {
        for t in &file_parts {
            push_unique(&mut parts, t.clone());
        }
        if !parts.is_empty() {
            by.push("filename");
        }
    }
    let parts_by = by.join("+");
    if !parser.is_empty() {
        let pset: Vec<String> = parser.iter().map(|t| norm(t)).collect();
        let fset: Vec<String> = parts.iter().map(|t| norm(t)).collect();
        if !(pset.iter().all(|p| fset.contains(p)) && fset.iter().all(|p| pset.contains(p))) {
            let others: Vec<&str> = by.iter().copied().filter(|b| *b != "parser").collect();
            diags.push(format!(
                "parts contested: parser={} vs {}={}",
                json_list(&parser),
                if others.is_empty() { "reconciled".to_string() } else { others.join("+") },
                json_list(&parts)
            ));
        }
    }

    // ---- maker: text, then a parser reading that is not a bare part-prefix guess, then the file name
    let parser_maker = e.parser_maker.filter(|m| !m.trim().is_empty());
    let parser_by = e.parser_maker_by.filter(|m| !m.trim().is_empty() && *m != "none");
    let parser_label = |m: &str| match parser_by {
        Some(b) => format!("{m} ({b})"),
        None => m.to_string(),
    };
    let (mut maker, mut maker_by): (Option<String>, Option<String>) = (None, None);
    if let Some((tm, _n, second)) = text_maker_hit {
        let mut by = vec!["text"];
        if let Some(pm) = parser_maker {
            if same_maker(pm, tm) {
                by.push("parser");
            } else {
                diags.push(format!("maker contested: parser={} vs text={tm}", parser_label(pm)));
            }
        }
        if let Some(fm) = file_maker {
            if same_maker(fm, tm) {
                by.push("filename");
            } else {
                diags.push(format!("maker contested: filename={fm} vs text={tm}"));
            }
        }
        if let Some((sm, sn)) = second {
            if sn >= 2 {
                diags.push(format!("maker also named in the text: {sm} ({sn} mentions)"));
            }
        }
        maker = Some(tm.to_string());
        maker_by = Some(by.join("+"));
    } else if let (Some(pm), true) = (parser_maker, parser_by.is_some_and(|b| b != "part-prefix")) {
        maker = Some(pm.to_string());
        maker_by = Some("parser".into());
        if let Some(fm) = file_maker {
            if !same_maker(fm, pm) {
                diags.push(format!("maker contested: parser={} vs filename={fm}", parser_label(pm)));
            }
        }
    } else if let Some(fm) = file_maker {
        let mut by = vec!["filename"];
        if let Some(pm) = parser_maker {
            if same_maker(pm, fm) {
                by.push("parser");
            } else {
                diags.push(format!("maker contested: parser={} vs filename={fm}", parser_label(pm)));
            }
        }
        maker = Some(fm.to_string());
        maker_by = Some(by.join("+"));
    } else if let Some(pm) = parser_maker {
        maker = Some(pm.to_string());
        maker_by = Some("parser".into());
    }

    // ---- what each witness said
    let mut w: Vec<String> = Vec::new();
    {
        let mut p: Vec<String> = vec![format!("\"parts\":{}", json_list(&parser))];
        if let Some(m) = parser_maker {
            p.push(format!("\"maker\":{}", esc(m)));
        }
        if let Some(b) = parser_by {
            p.push(format!("\"makerBy\":{}", esc(b)));
        }
        w.push(format!("\"parser\":{{{}}}", p.join(",")));
    }
    if !title_parts.is_empty() {
        w.push(format!("\"title\":{{\"parts\":{}}}", json_list(&title_parts)));
    }
    if win.is_some() {
        let mut p: Vec<String> = vec![format!("\"parts\":{}", json_list(&text_parts))];
        if let Some((tm, n, _)) = text_maker_hit {
            p.push(format!("\"maker\":{}", esc(tm)));
            p.push(format!("\"mentions\":{n}"));
        }
        if let Some(l) = &text_lit {
            p.push(format!("\"literature\":{}", esc(l)));
        }
        w.push(format!("\"text\":{{{}}}", p.join(",")));
    }
    if file_maker.is_some() || !file_parts.is_empty() {
        let mut p: Vec<String> = vec![format!("\"parts\":{}", json_list(&file_parts))];
        if let Some(m) = file_maker {
            p.push(format!("\"maker\":{}", esc(m)));
        }
        w.push(format!("\"filename\":{{{}}}", p.join(",")));
    }

    Identity {
        parts,
        parts_by,
        maker,
        maker_by,
        literature,
        diagnostics: diags,
        witnesses_json: format!("{{{}}}", w.join(",")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev<'a>(parts: &'a [String], title: Option<&'a str>, names: &'a [String], text: Option<&'a str>) -> Evidence<'a> {
        Evidence {
            parser_parts: parts,
            parser_maker: None,
            parser_maker_by: None,
            title,
            file_names: names,
            text,
            use_filename: true,
        }
    }

    #[test]
    fn slash_shorthand_expands_to_the_family() {
        assert_eq!(
            lead_parts("ATmega48A/PA/88A/PA/168A/PA/328/P"),
            ["ATmega48A", "ATmega48PA", "ATmega88A", "ATmega88PA", "ATmega168A", "ATmega168PA", "ATmega328", "ATmega328P"]
        );
        assert_eq!(lead_parts("OPA2134/OPA134/OPA4134 SoundPlus"), ["OPA2134", "OPA134", "OPA4134"]);
    }

    #[test]
    fn lead_parts_stop_at_prose_and_skip_wildcards() {
        assert_eq!(lead_parts("TMP117 High-Accuracy, Low-Power Sensor"), ["TMP117"]);
        assert_eq!(lead_parts("Publ i c BSC016N06NS Final datasheet"), ["BSC016N06NS"]);
        assert!(lead_parts("ADS111x Ultra-Small, Low-Power 16-Bit ADCs").is_empty());
        assert!(lead_parts("OPAx134 High-Performance Audio").is_empty());
        assert!(lead_parts("Ultra-low power 10-bit 50-90MSps SAR ADCs").is_empty());
        assert_eq!(lead_parts("Z80':CPU Z80A-CPU"), ["Z80"]);
        assert_eq!(lead_parts("ADS1113, ADS1114, ADS1115 SBAS444E - MAY 2009"), ["ADS1113", "ADS1114", "ADS1115"]);
    }

    #[test]
    fn literature_numbers_are_not_parts() {
        for l in ["SBAS444E", "SLOS068AB", "SLVU720A", "SNOSD82D", "SLVA477B"] {
            assert!(is_literature(l), "{l}");
        }
        assert!(!is_literature("STM32F407") && !is_literature("SOT23") && !is_literature("TPS54331"));
    }

    #[test]
    fn the_file_name_reads_a_maker_and_a_part_only_when_it_has_one() {
        let n = |s: &str| vec![s.to_string()];
        let (m, p, l) = filename_witness(&n("infineon-bsc016n06ns_DS_V2.6.pdf"));
        assert_eq!((m, p, l), (Some("Infineon"), vec!["BSC016N06NS".to_string()], None));
        let (m, p, l) = filename_witness(&n("ti-slva477b-appnote.pdf"));
        assert_eq!((m, p, l), (Some("Texas Instruments"), vec![], Some("SLVA477B".to_string())));
        let (m, p, _) = filename_witness(&n("arxiv-2312.14592-sar-adc-65nm.pdf"));
        assert_eq!((m, p), (Some("arXiv"), vec![]));
        let (m, p, _) = filename_witness(&n("zilog-z80-ctc-productspec-feb80.pdf"));
        assert_eq!((m, p), (Some("Zilog"), vec!["Z80-CTC".to_string()]));
        assert_eq!(filename_witness(&n("a1b2c3.pdf")).0, None);
    }

    #[test]
    fn without_text_the_parser_still_speaks_and_a_filename_part_is_the_last_resort() {
        let parts = vec!["ti-slva477b-appnote".to_string()];
        let names = vec!["ti-slva477b-appnote.pdf".to_string()];
        let id = reconcile(&ev(&parts, None, &names, None));
        assert!(id.parts.is_empty());
        assert_eq!(id.literature.as_deref(), Some("SLVA477B"));
        assert_eq!(id.maker.as_deref(), Some("Texas Instruments"));
        assert_eq!(id.maker_by.as_deref(), Some("filename"));
        assert!(id.diagnostics.iter().any(|d| d.contains("is the file name")), "{:?}", id.diagnostics);

        let none: Vec<String> = Vec::new();
        let names = vec!["infineon-bsc016n06ns_DS_V2.6.pdf".to_string()];
        let id = reconcile(&ev(&none, None, &names, None));
        assert_eq!((id.parts.clone(), id.parts_by.as_str()), (vec!["BSC016N06NS".to_string()], "filename"));
        // switched off, nothing is invented
        let mut off = ev(&none, None, &names, None);
        off.use_filename = false;
        let id = reconcile(&off);
        assert!(id.parts.is_empty() && id.maker.is_none());
    }
}
