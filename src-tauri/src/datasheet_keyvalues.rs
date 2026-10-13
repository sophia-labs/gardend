//! Key values for every part type, derived from the spec rows the importer
//! already maps (datasheets slice 1b).
//!
//! Tom's `params[]` only knows vin/vout/iout/fsw, so an op-amp, MOSFET, sensor,
//! MCU or ADC card would show nothing. Per product type this module picks, from
//! the rows, the one row that states each key value:
//!
//! * match on the row's symbol first, then the parameter name (case- and
//!   space-insensitive), then prefer a section (`Recommended Operating` for a
//!   supply, `Absolute Maximum` for VDS/ID, `Electrical Characteristics` for
//!   the rest), then the better-read row, then the earlier row;
//! * a row with no number in any slot is never picked (`OFFSET VOLTAGE`
//!   header rows, a VOS row whose cells did not parse): the value stays
//!   absent, never invented;
//! * `onResistance` takes the row at the lowest VGS stated; `sampleRate` the
//!   highest rate. Every other matching row stays reachable as its own row
//!   assertion.
//!
//! The product type is Tom's `document.product_type` when it names one of the
//! kinds below, else inferred from the part number, title and file name (the
//! rules of his `classify.py` KEYWORDS, plus part-number patterns).

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    OpAmp,
    Mosfet,
    Ldo,
    Sensor,
    Mcu,
    Adc,
}

impl Kind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Kind::OpAmp => "op-amp",
            Kind::Mosfet => "mosfet",
            Kind::Ldo => "ldo",
            Kind::Sensor => "sensor",
            Kind::Mcu => "mcu",
            Kind::Adc => "adc",
        }
    }
    fn keys(self) -> &'static [&'static str] {
        match self {
            Kind::OpAmp => &["gainBandwidth", "inputOffsetVoltage", "supplyVoltage"],
            Kind::Mosfet => &["drainSourceVoltage", "onResistance", "drainCurrent"],
            Kind::Ldo => &["inputVoltage", "outputCurrent", "dropout"],
            Kind::Sensor => &["range", "accuracy", "supplyVoltage"],
            Kind::Mcu => &["core", "flash", "ram"],
            Kind::Adc => &["resolution", "sampleRate", "supplyVoltage"],
        }
    }
}

fn alnum(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn from_tom(product_type: &str) -> Option<Kind> {
    Some(match product_type {
        "amplifier" => Kind::OpAmp,
        "discrete" => Kind::Mosfet,
        "ldo" => Kind::Ldo,
        "sensor" => Kind::Sensor,
        "mcu" | "soc" => Kind::Mcu,
        _ => return None,
    })
}

/// `(kind, inferred)`: `inferred` is false when Tom's own `product_type` said so.
pub(crate) fn classify(doc: &Value) -> Option<(Kind, bool)> {
    let text = |k: &str| doc.get(k).and_then(Value::as_str).unwrap_or("");
    if let Some(k) = from_tom(text("product_type")) {
        return Some((k, false));
    }
    if !matches!(text("product_type"), "" | "unknown") {
        return None; // converter, charger, app-note, ...: params[] covers it or nothing applies
    }
    let parts: Vec<String> = doc
        .get("parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        // `parts` may hold several names in one string: "OPA134, OPA2134, OPA4134"
        .flat_map(|p| p.split(|c| c == ',' || c == '/').map(alnum).collect::<Vec<_>>())
        .collect();
    let title = text("title").to_lowercase();
    let file = alnum(text("file"));
    let names: Vec<&String> = parts.iter().chain(std::iter::once(&file)).collect();
    let any_part = |toks: &[&str]| names.iter().any(|p| toks.iter().any(|t| p.contains(t)));
    let title_has = |words: &[&str]| words.iter().any(|w| title.contains(w));
    // `Si2302CDS`: a part name that starts with "si" and a digit.
    let si_part = parts.iter().any(|p| p.strip_prefix("si").is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit())));
    if title_has(&["mosfet"]) || si_part || any_part(&["irlb", "irlr", "irf", "bsc0", "bsz0", "ipp0", "ipb0", "csd1", "csd8"]) {
        return Some((Kind::Mosfet, true));
    }
    if title_has(&["operational amplifier", "op amp", "op-amp"]) || any_part(&["opa1", "opa2", "opa4", "lm158", "lm258", "lm358", "lm2904", "tl07", "tl08", "ne5532"]) {
        return Some((Kind::OpAmp, true));
    }
    if title_has(&["adc", "analog-to-digital"]) || any_part(&["ads1", "ads8", "mcp3"]) {
        return Some((Kind::Adc, true));
    }
    if title_has(&["microcontroller"]) || any_part(&["esp32", "atmega", "attiny", "stm32", "rp2040", "nrf5"]) {
        return Some((Kind::Mcu, true));
    }
    if title_has(&["sensor"]) || any_part(&["bme2", "bmp2", "tmp1", "sht3", "sht4", "hdc"]) {
        return Some((Kind::Sensor, true));
    }
    None
}

struct RowView<'a> {
    idx: usize,
    row: &'a Value,
    sym: String,     // letters and digits only
    raw_sym: String, // lower-case, no whitespace, symbols kept
    par: String,   // parameter and symbol columns together (Docling often swaps them)
    pexact: String, // the parameter name alone
    sec: String,
    unit: String,
}

fn numbers(row: &Value) -> [Option<f64>; 3] {
    ["min", "typ", "max"].map(|k| row.get(k).and_then(Value::as_f64))
}

fn conf_rank(row: &Value) -> u8 {
    match row.get("confidence").and_then(Value::as_str) {
        Some("high") => 0,
        Some("medium") => 1,
        Some("low") => 2,
        _ => 3,
    }
}

fn vgs(row: &Value) -> f64 {
    // `VGS = 4.5V, ID = 25A`: the first gate voltage stated.
    let c = row.get("conditions").and_then(Value::as_str).unwrap_or("");
    let lower = c.to_lowercase().replace([' ', '_'], "");
    let Some(at) = lower.find("vgs=") else { return 1e9 };
    let rest = &lower[at + 4..];
    let end = rest
        .find(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .unwrap_or(rest.len());
    rest[..end].parse().unwrap_or(1e9)
}

/// `(strength, extra)` when the row states `key`; lower is better.
fn matches(key: &str, v: &RowView<'_>) -> Option<(u8, f64)> {
    let (sym, raw, par) = (v.sym.as_str(), v.raw_sym.as_str(), v.par.as_str());
    let has = |s: &str| par.contains(s);
    let none = |ss: &[&str]| ss.iter().all(|s| !par.contains(s));
    let m = match key {
        "gainBandwidth" => {
            if ["gbw", "gbp", "gbwp"].contains(&sym) { Some(0) } else if has("gainbandwidth") { Some(1) } else { None }
        }
        "inputOffsetVoltage" => {
            if !none(&["drift"]) || sym.starts_with("dvos") {
                None
            } else if ["vos", "vio"].contains(&sym) { Some(0) } else if has("inputoffsetvoltage") { Some(1) } else { None }
        }
        "supplyVoltage" => {
            if !none(&["rejection", "current", "ratio"]) {
                None
            } else if ["vs", "vcc", "vdd", "v+"].contains(&raw) { Some(0) } else if has("supplyvoltage") || has("powersupply") || (par.starts_with("vs") && has("supply")) { Some(1) } else { None }
        }
        "drainSourceVoltage" => {
            if sym == "vds" { Some(0) } else if ["vdss"].contains(&sym) { Some(1) } else if ["bvdss", "vbrdss"].contains(&sym) { Some(2) }
            else if (has("drainsourcevoltage") || has("draintosourcevoltage")) && none(&["breakdown"]) { Some(1) } else { None }
        }
        "onResistance" => {
            if sym.starts_with("rdson") { Some(0) } else if has("onresistance") && none(&["temperature", "coefficient"]) { Some(1) } else { None }
        }
        "drainCurrent" => {
            if !none(&["pulse"]) {
                None
            } else if raw == "id" || (raw.starts_with("id") && raw[2..].chars().next().is_some_and(|c| !c.is_alphabetic())) { Some(0) }
            else if has("continuousdraincurrent") { Some(1) } else { None }
        }
        "dropout" => {
            if ["vdo", "vdropout"].contains(&sym) { Some(0) } else if has("dropout") { Some(1) } else { None }
        }
        "inputVoltage" => {
            if sym == "vin" { Some(0) } else if has("inputvoltage") && none(&["ripple", "rejection", "drift"]) { Some(1) } else { None }
        }
        "outputCurrent" => {
            if ["iout", "io"].contains(&sym) { Some(0) } else if has("outputcurrent") && none(&["limit", "short"]) { Some(1) } else { None }
        }
        "range" => {
            if has("temperature") || v.unit.contains("°c") {
                None
            } else if has("measurementrange") { Some(0) } else if has("range") { Some(1) } else { None }
        }
        "accuracy" => {
            if !none(&["standby", "clock", "time"]) { None }
            else if has("temperatureaccuracy") || has("absoluteaccuracy") { Some(0) } else if has("accuracy") { Some(1) } else { None }
        }
        "resolution" => {
            if !has("resolution") { None } else if v.unit.starts_with("bit") { Some(0) } else { Some(1) }
        }
        "sampleRate" => {
            if ["sps", "ksps", "msps"].contains(&v.unit.as_str()) || has("datarate") || has("samplerate") { Some(0) } else { None }
        }
        "flash" => ["flash", "flashmemory", "flashsize", "embeddedflash"].contains(&v.pexact.as_str()).then_some(0),
        "ram" => ["ram", "sram", "sramsize", "internalsram", "datamemory"].contains(&v.pexact.as_str()).then_some(0),
        _ => None,
    };
    let extra = match key {
        "onResistance" => vgs(v.row),
        "sampleRate" => -numbers(v.row).iter().flatten().fold(f64::MIN, |a, b| a.max(*b)),
        _ => 0.0,
    };
    m.map(|s| (s, extra))
}

/// A key value must be in the right kind of unit; an empty unit (a cell that
/// did not parse) is allowed, a wrong one (`dB` for a supply) is not.
fn unit_ok(key: &str, unit: &str) -> bool {
    let u = unit.trim();
    if u.is_empty() {
        return true;
    }
    match key {
        "supplyVoltage" | "drainSourceVoltage" | "inputVoltage" | "inputOffsetVoltage" | "dropout" => u.ends_with('v'),
        "outputCurrent" | "drainCurrent" => u.ends_with('a'),
        "gainBandwidth" => u.ends_with("hz"),
        "onResistance" => u.contains('ω') || u.contains("ohm"),
        _ => true,
    }
}

fn section_rank(key: &str, sec: &str) -> u8 {
    let order: &[&str] = match key {
        "supplyVoltage" | "inputVoltage" | "outputCurrent" => &["recommended operating", "electrical", "absolute"],
        "drainSourceVoltage" | "drainCurrent" => &["absolute maximum", "key performance"],
        _ => &["electrical characteristics", "key performance", "electrical"],
    };
    order.iter().position(|o| sec.contains(o)).map_or(order.len() as u8, |p| p as u8)
}

pub(crate) struct Pick {
    pub(crate) key: &'static str,
    /// Index into `spec_rows`, when a row states the value.
    pub(crate) row: Option<usize>,
    /// The value, for a non-numeric key value (an MCU core name).
    pub(crate) text: Option<String>,
}

/// Select the key values of `kind` from `rows` (`(spec_rows index, row)`).
/// `have` lists keys a Tom param already states; those are not repeated.
pub(crate) fn select(kind: Kind, doc: &Value, rows: &[(usize, &Value)], have: &[&str]) -> Vec<Pick> {
    let views: Vec<RowView<'_>> = rows
        .iter()
        .map(|(idx, row)| {
            let s = |k: &str| row.get(k).and_then(Value::as_str).unwrap_or("");
            // Key-performance tables put the symbol in the parameter column.
            let symbol = if s("symbol").is_empty() { s("parameter") } else { s("symbol") };
            let pexact = if s("parameter").is_empty() { s("symbol") } else { s("parameter") };
            RowView {
                idx: *idx,
                row,
                sym: alnum(symbol),
                raw_sym: symbol.to_lowercase().split_whitespace().collect(),
                par: alnum(&format!("{} {}", s("parameter"), s("symbol"))),
                pexact: alnum(pexact),
                sec: s("section").to_lowercase(),
                unit: s("unit").to_lowercase(),
            }
        })
        .collect();
    let mut out = Vec::new();
    for &key in kind.keys() {
        if have.contains(&key) {
            continue;
        }
        if key == "core" {
            let title = doc.get("title").and_then(Value::as_str).unwrap_or("");
            let parts = doc.get("parts").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")).unwrap_or_default();
            let blob = format!("{title} {parts}");
            let core = ["RISC-V", "Xtensa", "Cortex-M0+", "Cortex-M0", "Cortex-M3", "Cortex-M4", "Cortex-M7", "AVR"]
                .into_iter()
                .find(|c| blob.contains(c));
            if let Some(c) = core {
                out.push(Pick { key, row: None, text: Some(c.to_string()) });
            }
            continue;
        }
        let best = views
            .iter()
            .filter(|v| numbers(v.row).iter().any(Option::is_some) && unit_ok(key, &v.unit))
            .filter_map(|v| matches(key, v).map(|(s, x)| (s, section_rank(key, &v.sec), conf_rank(v.row), x, v.idx)))
            .min_by(|a, b| {
                (a.0, a.1, a.2)
                    .cmp(&(b.0, b.1, b.2))
                    .then(a.3.partial_cmp(&b.3).unwrap_or(std::cmp::Ordering::Equal))
                    .then(a.4.cmp(&b.4))
            });
        if let Some(b) = best {
            out.push(Pick { key, row: Some(b.4), text: None });
        }
    }
    out
}
