//! How a number reads: durations, counts (tokens, wallet amounts), US dollars,
//! byte sizes and "how long ago".
//!
//! The Rust half of `client-shared/units.ts`. The TypeScript copy formats for
//! every client and card; this one is for the words Rust writes itself — the
//! cli's terminal and the cards it builds, the attributes it stamps on a
//! reply's result card, and the server's own sentences (the wallet bot, /sell,
//! the invite DM). The rules are written twice and the cases once: both test
//! suites read `client-shared/units.cases.json`, so change a rule, change a
//! row, and both must pass.
//!
//! ```text
//! duration  850ms · 59.9s · 1m 1s · 1h 1m 43s · 1d 1h 1m   (at most three units;
//!           trailing zero units dropped; rounded BEFORE the unit is picked)
//! count     999 · 1.2K · 999.9K · 1M · 25.5M · 2.5B        (one decimal)
//! money     $1,235 · $13.44 · $0.43 · $0.0042 · <$0.000001
//! bytes     500 B · 1.5 KB · 12.9 MB · 2.1 GB              (1000 to the KB)
//! ago       just now · 5m ago · 3h ago · 2d ago
//! ```
//!
//! The unit WORDS are the language pack's `units.*` keys. [`Units::with_pack`]
//! takes them from a pack (the server's `speak` lookup); otherwise the bundled
//! English and Chinese below, the same fallback the TypeScript copy carries.
//! Every rounding is done on integers, so the two copies agree digit for digit
//! (999,949 → "999.9K", 999,950 → "1M"). Pure std: no clock — "ago" takes the
//! seconds already elapsed — so the crate stays wasm-clean.

/// `(key, en, zh-Hans)` — the words `client-shared/units.ts` bundles, key for
/// key. A key the Chinese column leaves empty reads the same as English.
const WORDS: &[(&str, &str, &str)] = &[
    ("units.ms", "{n}ms", "{n} 毫秒"),
    ("units.s", "{n}s", "{n} 秒"),
    ("units.m", "{n}m", "{n} 分"),
    ("units.h", "{n}h", "{n} 小时"),
    ("units.d", "{n}d", "{n} 天"),
    ("units.K", "{n}K", ""),
    ("units.M", "{n}M", ""),
    ("units.B", "{n}B", ""),
    ("units.bytes.B", "{n} B", ""),
    ("units.bytes.KB", "{n} KB", ""),
    ("units.bytes.MB", "{n} MB", ""),
    ("units.bytes.GB", "{n} GB", ""),
    ("units.bytes.TB", "{n} TB", ""),
    ("units.ago.now", "just now", "刚刚"),
    ("units.ago.m", "{n}m ago", "{n} 分钟前"),
    ("units.ago.h", "{n}h ago", "{n} 小时前"),
    ("units.ago.d", "{n}d ago", "{n} 天前"),
    ("units.agoShort.now", "now", "刚刚"),
    ("units.agoShort.m", "{n}m", "{n} 分钟"),
    ("units.agoShort.h", "{n}h", "{n} 小时"),
    ("units.agoShort.d", "{n}d", "{n} 天"),
    ("units.of.token", "{name} tokens", "{name} token"),
    ("units.of.second", "{name} seconds", "{name} 秒"),
    ("units.of.image", "{name} images", "{name} 张"),
    ("units.of.other", "{name} {unit}", ""),
    ("units.one.token", "{name} token", "{name} token"),
    ("units.one.second", "{name} second", "{name} 秒"),
    ("units.one.image", "{name} image", "{name} 张"),
    ("units.tokens", "{n} tokens", "{n} token"),
];

/// The bundled word for `key` in `lang`, walking `zh-Hant` → `zh` → `en` the
/// way the TypeScript copy does. Only English and Chinese are bundled; any
/// other language reads English until a pack supplies its words.
fn bundled(key: &str, lang: &str) -> &'static str {
    let Some(&(_, en, zh)) = WORDS.iter().find(|(k, _, _)| *k == key) else {
        return "";
    };
    let base = lang.split('-').next().unwrap_or("");
    if (lang == "zh" || base == "zh") && !zh.is_empty() {
        zh
    } else {
        en
    }
}

/// A language pack's lookup: `key` → its string, when the pack has one.
pub type PackLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The formatter for one reader: their language, and optionally their pack.
#[derive(Clone, Copy)]
pub struct Units<'a> {
    lang: &'a str,
    pack: Option<PackLookup<'a>>,
}

/// English — the cli's terminal, and anything written into a message every
/// reader shares (a card re-formats it in each reader's language).
pub const EN: Units<'static> = Units { lang: "en", pack: None };

impl<'a> Units<'a> {
    /// The bundled words for `lang` (`"en"`, `"zh-Hans"`, …).
    pub fn new(lang: &'a str) -> Self {
        Units { lang, pack: None }
    }

    /// The pack's words first — the same strings a client paints — then the
    /// bundled ones for `lang`.
    pub fn with_pack(lang: &'a str, pack: PackLookup<'a>) -> Self {
        Units { lang, pack: Some(pack) }
    }

    fn word(&self, key: &str, n: &str) -> String {
        self.words(key, &[("n", n)])
    }

    /// [`Units::word`], for a key with more than `{n}` in it.
    fn words(&self, key: &str, vars: &[(&str, &str)]) -> String {
        // A pack's empty string is a word nobody wrote yet: the bundled one
        // reads better than a gap.
        let from_pack = self.pack.and_then(|p| p(key)).filter(|w| !w.is_empty());
        let raw = from_pack.as_deref().unwrap_or_else(|| bundled(key, self.lang));
        let mut out = if raw.is_empty() { key.to_string() } else { raw.to_string() };
        for (k, v) in vars {
            out = out.replace(&format!("{{{k}}}"), v);
        }
        out
    }

    /// What one of a wallet currency is: "Opus 5.5 tokens" / 「Opus 5.5 token」
    /// / "Seedance 2 seconds". `unit` is the price row's; `None` (or empty)
    /// means token. A unit the words don't know reads as its own name.
    ///
    /// A currency IS a model and one of it is one output-equivalent unit of
    /// that model, so a bare "500K Opus 5.5" leaves the reader asking 500K of
    /// what — the amount always says its unit (owner, 10-22).
    pub fn unit_of(&self, name: &str, unit: Option<&str>) -> String {
        self.unit_of_count(name, unit, None)
    }

    /// [`Units::unit_of`] for `count` of them: 1 reads singular in English
    /// ("1 Opus 5.5 token").
    pub fn unit_of_count(&self, name: &str, unit: Option<&str>, count: Option<u128>) -> String {
        let u = unit.filter(|u| !u.is_empty()).unwrap_or("token");
        let key = match (u, count) {
            ("token" | "second" | "image", Some(1)) => format!("units.one.{u}"),
            ("token" | "second" | "image", _) => format!("units.of.{u}"),
            _ => "units.of.other".to_string(),
        };
        self.words(&key, &[("name", name), ("unit", u)])
    }

    /// An amount with its unit: "500K Opus 5.5 tokens"; a debit is "−500K …".
    /// `grouped` writes every digit ("1,000,000 GPT-5.6 Terra tokens").
    pub fn amount(&self, n: f64, name: &str, unit: Option<&str>, grouped: bool) -> String {
        let digits = if grouped { self::grouped(n.abs()) } else { self.compact(n.abs()) };
        let r = if n.is_finite() { n.abs().round() as u128 } else { 0 };
        let sign = if n < 0.0 && r != 0 { "−" } else { "" };
        format!("{sign}{digits} {}", self.unit_of_count(name, unit, Some(r)))
    }

    /// A count of tokens no single currency owns — a grant's monthly cap,
    /// what a listing sold: "2M tokens" / 「2M token」.
    pub fn tokens(&self, n: f64) -> String {
        self.word("units.tokens", &self.compact(n))
    }

    /// Milliseconds → "850ms" / "12.3s" / "1m 1s" / "1h 1m 43s" / "1d 1h 1m".
    /// Under a minute: one decimal of a second. From a minute up: whole
    /// seconds, at most three units from the largest; a day and over drops the
    /// seconds.
    pub fn duration(&self, ms: f64) -> String {
        let ms = if ms.is_finite() && ms > 0.0 { ms } else { 0.0 };
        let whole = ms.round() as u64;
        if whole < 1000 {
            return self.word("units.ms", &whole.to_string());
        }
        let t = (ms / 100.0).round() as u64;
        if t < 600 {
            return self.word("units.s", &tenths(t as u128));
        }
        let s = (ms / 1000.0).round() as u64;
        let mut parts: Vec<(u64, &str)> = if s < 3600 {
            vec![(s / 60, "m"), (s % 60, "s")]
        } else if s < 86_400 {
            vec![(s / 3600, "h"), (s % 3600 / 60, "m"), (s % 60, "s")]
        } else {
            let m = (ms / 60_000.0).round() as u64;
            vec![(m / 1440, "d"), (m % 1440 / 60, "h"), (m % 60, "m")]
        };
        while parts.len() > 1 && parts.last().is_some_and(|(n, _)| *n == 0) {
            parts.pop();
        }
        parts
            .iter()
            .map(|(n, k)| self.word(&format!("units.{k}"), &n.to_string()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Seconds → the same as [`Units::duration`].
    pub fn seconds(&self, secs: f64) -> String {
        self.duration(secs * 1000.0)
    }

    /// A clock that ticks — a job still running, a cooldown counting down:
    /// whole seconds ("0s" · "12s" · "1m 1s" · "1h 1m 42s"), never "12.4s".
    pub fn elapsed(&self, ms: f64) -> String {
        let s = if ms.is_finite() && ms > 0.0 { (ms / 1000.0).floor() } else { 0.0 };
        if s == 0.0 {
            self.word("units.s", "0")
        } else {
            self.duration(s * 1000.0)
        }
    }

    /// A count → "999" / "1.2K" / "999.9K" / "1M" / "25.5M" / "2.5B": one
    /// decimal, ".0" dropped, rounded half-up before the tier is picked.
    /// Tokens, wallet amounts — any quantity read at a glance.
    pub fn compact(&self, n: f64) -> String {
        if !n.is_finite() {
            return js_number(n);
        }
        let r = n.abs().round() as u128;
        let sign = if n < 0.0 && r != 0 { "-" } else { "" };
        if r < 1000 {
            return format!("{sign}{r}");
        }
        const TIERS: [(u128, &str); 3] = [(1_000, "K"), (1_000_000, "M"), (1_000_000_000, "B")];
        for (i, (unit, k)) in TIERS.iter().enumerate() {
            let t = (r.saturating_mul(10) + unit / 2) / unit;
            if t < 10_000 || i == TIERS.len() - 1 {
                return format!("{sign}{}", self.word(&format!("units.{k}"), &tenths(t)));
            }
        }
        format!("{sign}{r}")
    }

    /// Bytes → "500 B" / "1.5 KB" / "12.9 MB" / "2.1 GB": one decimal, 1000 to
    /// the KB — the way the reader's own file manager counts.
    pub fn bytes(&self, n: f64) -> String {
        let r = if n.is_finite() && n > 0.0 { n.round() as u128 } else { 0 };
        if r < 1000 {
            return self.word("units.bytes.B", &r.to_string());
        }
        const TIERS: [(u128, &str); 4] =
            [(1_000, "KB"), (1_000_000, "MB"), (1_000_000_000, "GB"), (1_000_000_000_000, "TB")];
        for (i, (unit, k)) in TIERS.iter().enumerate() {
            let t = (r.saturating_mul(10) + unit / 2) / unit;
            // Rounded first: 999.96 KB is "1 MB", never "1000 KB".
            if t < 10_000 || i == TIERS.len() - 1 {
                return self.word(&format!("units.bytes.{k}"), &tenths(t));
            }
        }
        self.word("units.bytes.B", &r.to_string())
    }

    /// Seconds since → "just now" / "5m ago" / "3h ago" / "2d ago".
    pub fn ago(&self, secs: f64) -> String {
        let (unit, n) = ago_bucket(secs);
        self.word(&format!("units.ago.{unit}"), &n.to_string())
    }

    /// The same where "ago" is already understood → "now" / "5m" / "3h" / "2d".
    pub fn ago_short(&self, secs: f64) -> String {
        let (unit, n) = ago_bucket(secs);
        self.word(&format!("units.agoShort.{unit}"), &n.to_string())
    }
}

/// How long ago, in whole units: under a minute is "now".
fn ago_bucket(secs: f64) -> (&'static str, u64) {
    let s = if secs.is_finite() && secs > 0.0 { secs.floor() as u64 } else { 0 };
    if s < 60 {
        ("now", 0)
    } else if s < 3600 {
        ("m", s / 60)
    } else if s < 86_400 {
        ("h", s / 3600)
    } else {
        ("d", s / 86_400)
    }
}

/// Integer tenths → "12" / "12.5".
fn tenths(t: u128) -> String {
    if t.is_multiple_of(10) {
        (t / 10).to_string()
    } else {
        format!("{}.{}", t / 10, t % 10)
    }
}

/// JavaScript's `String(n)` for the values the formatters can't place.
fn js_number(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n > 0.0 {
        "Infinity".into()
    } else {
        "-Infinity".into()
    }
}

/// 1234567 → "1,234,567" (rounded to a whole number; the sign is kept).
pub fn grouped(n: f64) -> String {
    if !n.is_finite() {
        return js_number(n);
    }
    let r = n.abs().round() as u128;
    let digits = r.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if n < 0.0 && r != 0 {
        out.push('-');
    }
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// An integer scaled by 10^d → "12.34" (d ≥ 1).
fn fixed(v: u128, d: usize) -> String {
    let s = format!("{v:0>width$}", width = d + 1);
    let (whole, frac) = s.split_at(s.len() - d);
    format!("{whole}.{frac}")
}

/// US dollars → "$1,235" / "$13.44" / "$0.43" / "$0.015" / "$0.0001" /
/// "<$0.000001". A dollar and up: cents (whole dollars from $1,000). Under a
/// dollar: two significant digits, at least two decimals — a wallet unit is a
/// fraction of a cent, and "$0.00" reads as "this has no price". The same in
/// every language.
pub fn usd(v: f64) -> String {
    if !v.is_finite() {
        return js_number(v);
    }
    if v == 0.0 {
        return "$0".into();
    }
    let a = v.abs();
    let sign = if v < 0.0 { "-" } else { "" };
    let cents = (a * 100.0).round() as u128;
    if cents >= 100 {
        if cents >= 100_000 {
            return format!("{sign}${}", grouped(a));
        }
        return format!("{sign}${}", fixed(cents, 2));
    }
    // The first scale with two whole digits BEFORE rounding — rounding first
    // would read $0.0099 as "0.010".
    let mut d: i32 = 2;
    while a * 10f64.powi(d) < 10.0 && d < 6 {
        d += 1;
    }
    let s = (a * 10f64.powi(d)).round() as u128;
    if s == 0 {
        return format!("{sign}<$0.000001");
    }
    let mut out = fixed(s, d as usize);
    // Two significant digits may leave a trailing zero ("0.0010") — drop it,
    // but never below cents ("0.50" stays).
    while out.ends_with('0') && out.len() - out.find('.').map_or(0, |i| i + 1) > 2 {
        out.pop();
    }
    format!("{sign}${out}")
}

/// [`usd`] without its "$", for a sentence that writes its own:
/// 「out ${out}/M」 → "out $15.00/M".
pub fn usd_digits(v: f64) -> String {
    usd(v).replacen('$', "", 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// The table both copies are tested against.
    const CASES: &str = include_str!("../../client-shared/units.cases.json");

    fn rows(name: &str) -> Vec<(f64, String, String)> {
        let table: Value = serde_json::from_str(CASES).expect("units.cases.json parses");
        let rows = table[name].as_array().unwrap_or_else(|| panic!("{name}: no such section"));
        assert!(rows.len() >= 7, "{name}: the table has rows");
        rows.iter()
            .map(|r| {
                let r = r.as_array().expect("a row is an array");
                let input = r[0].as_f64().expect("a numeric input");
                let en = r[1].as_str().expect("an English output").to_string();
                let zh = r.get(2).and_then(Value::as_str).map_or_else(|| en.clone(), str::to_string);
                (input, en, zh)
            })
            .collect()
    }

    fn both(name: &str, f: impl Fn(&Units, f64) -> String) {
        for (v, en, zh) in rows(name) {
            assert_eq!(f(&Units::new("en"), v), en, "{name}({v}) en");
            assert_eq!(f(&Units::new("zh-Hans"), v), zh, "{name}({v}) zh-Hans");
        }
    }

    #[test]
    fn every_row_of_the_shared_table() {
        both("duration_ms", |u, v| u.duration(v));
        both("elapsed_ms", |u, v| u.elapsed(v));
        both("compact", |u, v| u.compact(v));
        both("usd", |_, v| usd(v));
        both("grouped", |_, v| grouped(v));
        both("bytes", |u, v| u.bytes(v));
        both("ago_s", |u, v| u.ago(v));
        both("ago_short_s", |u, v| u.ago_short(v));
        both("tokens", |u, v| u.tokens(v));
    }

    #[test]
    fn an_amount_says_its_unit() {
        let table: Value = serde_json::from_str(CASES).expect("units.cases.json parses");
        let (en, zh) = (Units::new("en"), Units::new("zh-Hans"));
        for r in table["unit_of"].as_array().expect("unit_of rows") {
            let (name, unit) = (r[0].as_str().unwrap(), r[1].as_str());
            assert_eq!(en.unit_of(name, unit), r[2].as_str().unwrap(), "unit_of({name}, {unit:?}) en");
            assert_eq!(zh.unit_of(name, unit), r[3].as_str().unwrap(), "unit_of({name}, {unit:?}) zh-Hans");
        }
        for r in table["unit_of_one"].as_array().expect("unit_of_one rows") {
            let (name, unit) = (r[0].as_str().unwrap(), r[1].as_str());
            assert_eq!(en.unit_of_count(name, unit, Some(1)), r[2].as_str().unwrap(), "unit_of({name}, {unit:?}, 1) en");
            assert_eq!(zh.unit_of_count(name, unit, Some(1)), r[3].as_str().unwrap(), "unit_of({name}, {unit:?}, 1) zh-Hans");
        }
        for r in table["amount"].as_array().expect("amount rows") {
            let (n, name, unit, grouped) =
                (r[0].as_f64().unwrap(), r[1].as_str().unwrap(), r[2].as_str(), r[3].as_bool().unwrap());
            assert_eq!(en.amount(n, name, unit, grouped), r[4].as_str().unwrap(), "amount({n}, {name}) en");
            assert_eq!(zh.amount(n, name, unit, grouped), r[5].as_str().unwrap(), "amount({n}, {name}) zh-Hans");
        }
    }

    #[test]
    fn the_screenshot_that_started_it() {
        // 2026-10-22: a reply's result bar read "3702.9s · 25488.3k tok · $13.4366".
        assert_eq!(EN.duration(3_702_900.0), "1h 1m 43s");
        assert_eq!(Units::new("zh-Hans").duration(3_702_900.0), "1 小时 1 分 43 秒");
        assert_eq!(EN.compact(25_488_300.0), "25.5M");
        assert_eq!(usd(13.4366), "$13.44");
    }

    #[test]
    fn languages_fall_back_like_the_typescript_copy() {
        assert_eq!(Units::new("zh-Hant").duration(61_000.0), "1 分 1 秒", "zh-Hant falls back through zh");
        assert_eq!(Units::new("zh").ago(300.0), "5 分钟前");
        assert_eq!(Units::new("fr").duration(3_702_900.0), "1h 1m 43s", "an unknown language reads English");
        let ja = |k: &str| match k {
            "units.h" => Some("{n}時間".to_string()),
            "units.m" => Some("{n}分".to_string()),
            "units.s" => Some("{n}秒".to_string()),
            _ => None,
        };
        assert_eq!(Units::with_pack("ja", &ja).duration(3_702_900.0), "1時間 1分 43秒", "the pack's words win");
        assert_eq!(Units::with_pack("ja", &ja).compact(2_500_000.0), "2.5M", "a key the pack lacks reads bundled");
        let blank = |_: &str| Some(String::new());
        assert_eq!(Units::with_pack("ja", &blank).duration(3_702_900.0), "1h 1m 43s", "an empty word reads bundled");
    }

    #[test]
    fn the_words_are_the_typescript_copys_words() {
        // client-shared/units.ts bundles exactly these keys; a key added there
        // and not here would read as the raw key in Rust.
        let ts = include_str!("../../client-shared/units.ts");
        let mut in_ts: Vec<&str> = ts
            .lines()
            .filter_map(|l| l.trim().strip_prefix("\"units."))
            .filter_map(|l| l.split('"').next())
            .collect();
        in_ts.sort_unstable();
        in_ts.dedup();
        let mut here: Vec<&str> = WORDS.iter().map(|(k, _, _)| k.strip_prefix("units.").unwrap()).collect();
        here.sort_unstable();
        assert_eq!(here, in_ts);
        for (k, en, zh) in WORDS {
            assert!(ts.contains(&format!("\"{k}\": {}", serde_json::to_string(en).unwrap())), "{k}: English matches");
            if !zh.is_empty() {
                assert!(ts.contains(&format!("\"{k}\": {}", serde_json::to_string(zh).unwrap())), "{k}: Chinese matches");
            }
        }
    }

    #[test]
    fn usd_digits_drops_only_the_sign() {
        assert_eq!(usd_digits(15.0), "15.00");
        assert_eq!(usd_digits(0.075), "0.075");
        assert_eq!(usd_digits(0.0), "0");
    }
}
