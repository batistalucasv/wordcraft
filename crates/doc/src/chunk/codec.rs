//! Option storage, tag encoding/decoding, and YAML/quoting helpers for code chunks.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Serialize};

use crate::control::{ContentControl, MAX_CONTROL_TEXT};
use crate::{DocError, Document, Result};

use super::{CHUNK_PROP, CHUNK_TAG, MAX_CHUNK_OPTION_CHARS, MAX_CHUNK_OPTIONS};

/// Priority order for encoding options into `w:tag` so execution-safety flags fit first.
const PRIORITY_TAG_OPTIONS: &[&str] = &["eval", "echo", "include", "label"];

/// The options of a code chunk: Quarto's chunk options, by name, with the value as written.
///
/// Values keep their source spelling, quotes included (`"A plot"`, `FALSE`, `1`), so writing
/// them back needs no guessing about types; [`ChunkOptions::text`] and [`ChunkOptions::flag`]
/// read them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ChunkOptions {
    values: BTreeMap<String, String>,
}

impl<'de> Deserialize<'de> for ChunkOptions {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct OptionsVisitor;

        impl<'de> Visitor<'de> for OptionsVisitor {
            type Value = ChunkOptions;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "a map of at most {MAX_CHUNK_OPTIONS} chunk options")
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut opts = ChunkOptions::new();
                while let Some((key, value)) = map.next_entry::<String, String>()? {
                    opts.try_set(key, value).map_err(serde::de::Error::custom)?;
                }
                Ok(opts)
            }
        }

        deserializer.deserialize_map(OptionsVisitor)
    }
}

impl ChunkOptions {
    pub fn new() -> ChunkOptions {
        ChunkOptions::default()
    }

    /// The raw value written for `key`, quotes included.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// Validate and store `key = value`, returning [`DocError::Invalid`] if `key` is empty,
    /// either `key` or `value` exceeds [`MAX_CHUNK_OPTION_CHARS`], or inserting a new key would
    /// exceed [`MAX_CHUNK_OPTIONS`].
    pub fn try_set(&mut self, key: impl Into<String>, value: impl Into<String>) -> Result<()> {
        let key = key.into();
        let value = value.into();
        if key.is_empty() {
            return Err(DocError::Invalid("chunk option name cannot be empty".to_string()));
        }
        if key.chars().count() > MAX_CHUNK_OPTION_CHARS {
            return Err(DocError::Invalid(format!("chunk option name exceeds {MAX_CHUNK_OPTION_CHARS} characters")));
        }
        if value.chars().count() > MAX_CHUNK_OPTION_CHARS {
            return Err(DocError::Invalid(format!("chunk option value exceeds {MAX_CHUNK_OPTION_CHARS} characters")));
        }
        if !self.values.contains_key(&key) && self.values.len() >= MAX_CHUNK_OPTIONS {
            return Err(DocError::Invalid(format!("chunk already has the maximum of {MAX_CHUNK_OPTIONS} options")));
        }
        self.values.insert(key, value);
        Ok(())
    }

    /// Set `key` to the raw `value`; past [`MAX_CHUNK_OPTIONS`] or an empty/oversized name or
    /// value, nothing is stored (hostile input never grows the document without bound).
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let _ = self.try_set(key, value);
    }

    /// Remove `key`; its raw value if it had one.
    pub fn remove(&mut self, key: &str) -> Option<String> {
        self.values.remove(key)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Every option, by name, in sorted order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// The text of `key` without its quoting quotes.
    pub fn text(&self, key: &str) -> Option<String> {
        self.get(key).map(strip_yaml_comment).map(unquote)
    }

    /// The boolean an option reads as (`true`, `TRUE`, `yes`, `1`…), when it reads as one.
    pub fn flag(&self, key: &str) -> Option<bool> {
        let raw = self.get(key)?;
        let scalar = strip_yaml_comment(raw);
        let value = unquote(scalar);
        match value.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" | "t" => Some(true),
            "false" | "no" | "0" | "f" => Some(false),
            _ => None,
        }
    }

    /// The chunk's label (`label`), which Quarto uses to name its figures and tables.
    pub fn label(&self) -> Option<&str> {
        let raw = self.get("label").or_else(|| self.get("id"))?;
        let s = strip_yaml_comment(raw);
        let bytes = s.as_bytes();
        if bytes.len() >= 2 && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"') || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'')) {
            Some(&s[1..s.len() - 1])
        } else {
            Some(s)
        }
    }

    /// The figure caption (`fig-cap`, or knitr's `fig.cap` / `fig_caption`).
    pub fn fig_caption(&self) -> Option<String> {
        ["fig-cap", "fig_caption", "fig.cap"].iter().find_map(|k| self.text(k))
    }

    /// `eval`: whether the code runs (Quarto's default is `true`).
    pub fn eval(&self) -> bool {
        self.flag("eval").unwrap_or(true)
    }

    /// `echo`: whether the code shows in the output (Quarto's default is `true`).
    pub fn echo(&self) -> bool {
        self.flag("echo").unwrap_or(true)
    }
}

/// Strip a trailing YAML comment (`# ...`) outside quotes.
///
/// Per YAML 1.2, `#` starts a comment only at the start of the input or when
/// preceded by separation whitespace; a `#` inside a plain scalar such as
/// `C# tutorial` is kept literally.
pub fn strip_yaml_comment(raw: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut prev: Option<char> = None;
    for (i, c) in raw.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
        } else if c == '#' && prev.is_none_or(|p| matches!(p, ' ' | '\t' | '\n' | '\r')) {
            return raw[..i].trim_end();
        }
        prev = Some(c);
    }
    raw.trim_end()
}

/// A code line's value without the quotes around it, decoding only active quote escapes.
pub(super) fn unquote(raw: &str) -> String {
    let t = raw.trim();
    let bytes = t.as_bytes();
    let Some(&first) = bytes.first() else { return String::new() };
    if (first == b'"' || first == b'\'') && bytes.last() == Some(&first) && bytes.len() >= 2 {
        let inner = t.get(1..t.len() - 1).unwrap_or("");
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                out.push_str(&decode_quote_escape(first, chars.next()));
            } else {
                out.push(c);
            }
        }
        out
    } else {
        t.to_string()
    }
}

fn decode_quote_escape(quote: u8, next: Option<char>) -> String {
    match (quote, next) {
        (b'"', Some('\\')) | (b'\'', Some('\\')) => "\\".to_string(),
        (b'"', Some('"')) => "\"".to_string(),
        (b'\'', Some('\'')) => "'".to_string(),
        (_, Some(ch)) => format!("\\{ch}"),
        (_, None) => "\\".to_string(),
    }
}

/// The custom property that holds chunk `id`'s options.
pub fn chunk_prop_name(id: i64) -> String {
    format!("{CHUNK_PROP}{id}")
}

/// A language name as a tag carries it: lower-case ASCII letters, digits, `-`, `_`, `+` and `.`,
/// cut to 32 characters (tags come from files, so they're treated as hostile).
pub(super) fn normalize_language(language: &str) -> String {
    language.trim().to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '.')).take(32).collect()
}

/// The language a chunk tag names, `None` when the tag isn't a chunk's.
pub fn tag_language(tag: &str) -> Option<String> {
    let rest = tag.strip_prefix(CHUNK_TAG)?;
    let lang = rest.split_once('?').map_or(rest, |(l, _)| l).trim();
    if lang.is_empty() {
        return None;
    }
    let normalized = normalize_language(lang);
    if normalized.is_empty() {
        return None;
    }
    Some(normalized)
}

fn is_tag_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~')
}

fn encoded_len(s: &str) -> usize {
    s.bytes().map(|b| if is_tag_unreserved(b) { 1 } else { 3 }).sum()
}

fn push_percent_encoded(out: &mut String, s: &str) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for b in s.bytes() {
        if is_tag_unreserved(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0F) as usize] as char);
        }
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub(super) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn append_encoded_pair(body: &mut String, key: &str, val: &str, max_body: usize) -> bool {
    let sep = usize::from(!body.is_empty());
    let needed = sep.saturating_add(encoded_len(key)).saturating_add(1).saturating_add(encoded_len(val));
    if body.len().saturating_add(needed) > max_body {
        return false;
    }
    if sep == 1 {
        body.push('&');
    }
    push_percent_encoded(body, key);
    body.push('=');
    push_percent_encoded(body, val);
    true
}

fn encode_tag_pairs(ordered: &[(&str, &str)], max_body: usize, options: &ChunkOptions) -> (String, bool) {
    let mut body = String::new();
    let mut truncated = false;

    for &(key, val) in ordered {
        if append_encoded_pair(&mut body, key, val, max_body) {
            continue;
        }
        truncated = true;
        // If `eval` had a long comment that didn't fit verbatim, keep its boolean flag so
        // `eval: false` still survives a copy in Word.
        if key == "eval"
            && let Some(flag) = options.flag("eval")
        {
            let compact = if flag { "true" } else { "false" };
            let _ = append_encoded_pair(&mut body, key, compact, max_body);
        }
    }

    (body, truncated)
}

/// Encode `language` and `options` into a `w:tag` string within [`MAX_CONTROL_TEXT`] chars.
pub(super) fn encode_control_tag(language: &str, options: &ChunkOptions) -> String {
    encode_control_tag_with_truncated(language, options, false)
}

/// Encode `language` and `options` into a `w:tag` string within [`MAX_CONTROL_TEXT`] chars,
/// optionally carrying forward the `!` truncated marker when full options are unavailable.
pub(super) fn encode_control_tag_with_truncated(language: &str, options: &ChunkOptions, force_truncated: bool) -> String {
    let lang = if language.is_empty() { "text" } else { language };
    let mut prefix = String::with_capacity(CHUNK_TAG.len() + lang.len() + 2);
    prefix.push_str(CHUNK_TAG);
    prefix.push_str(lang);
    prefix.push('?');
    if options.is_empty() {
        if force_truncated {
            prefix.push('!');
        }
        return prefix;
    }

    let max_body = MAX_CONTROL_TEXT.saturating_sub(prefix.len());
    let mut ordered: Vec<(&str, &str)> = Vec::with_capacity(options.len());
    for &key in PRIORITY_TAG_OPTIONS {
        if let Some(val) = options.get(key) {
            ordered.push((key, val));
        }
    }
    for (key, val) in options.iter() {
        if !PRIORITY_TAG_OPTIONS.contains(&key) {
            ordered.push((key, val));
        }
    }

    if force_truncated {
        let (truncated_body, _) = encode_tag_pairs(&ordered, max_body.saturating_sub(1), options);
        prefix.push('!');
        prefix.push_str(&truncated_body);
        return prefix;
    }

    let (body, truncated) = encode_tag_pairs(&ordered, max_body, options);
    if truncated {
        // Reserve 1 byte for `!` before encoding so an exact-budget `eval` compacts rather than
        // getting popped after filling `max_body`.
        let (truncated_body, _) = encode_tag_pairs(&ordered, max_body.saturating_sub(1), options);
        prefix.push('!');
        prefix.push_str(&truncated_body);
    } else {
        prefix.push_str(&body);
    }
    prefix
}

pub(super) enum TagOptions {
    /// Legacy or hand-written `wordcraft:chunk:<lang>` tag with no `?` query: options are missing.
    Missing,
    /// Tag carried the complete option set (`wordcraft:chunk:<lang>?...`).
    Complete(ChunkOptions),
    /// Tag carried partial options (`wordcraft:chunk:<lang>?!...`) after hitting [`MAX_CONTROL_TEXT`]
    /// or rejecting invalid/out-of-bounds tag entries.
    Truncated(ChunkOptions),
}

pub(super) fn decode_tag_options(tag: &str) -> TagOptions {
    let Some(rest) = tag.strip_prefix(CHUNK_TAG) else {
        return TagOptions::Missing;
    };
    let Some((_, query)) = rest.split_once('?') else {
        return TagOptions::Missing;
    };
    let (mut truncated, pairs) = match query.strip_prefix('!') {
        Some(tail) => (true, tail),
        None => (false, query),
    };
    let mut opts = ChunkOptions::new();
    if !pairs.is_empty() {
        for pair in pairs.split('&') {
            let Some((raw_k, raw_v)) = pair.split_once('=') else {
                continue;
            };
            let k = percent_decode(raw_k);
            let v = percent_decode(raw_v);
            if opts.try_set(k, v).is_err() {
                truncated = true;
            }
        }
    }
    if truncated { TagOptions::Truncated(opts) } else { TagOptions::Complete(opts) }
}

/// Parse full options from `wordcraft.chunk.<id>` when present and valid JSON.
pub(super) fn custom_prop_options(doc: &Document, control: &ContentControl) -> Option<ChunkOptions> {
    let json = control.id.and_then(|id| doc.custom_prop(&chunk_prop_name(id)))?;
    serde_json::from_str::<ChunkOptions>(json).ok()
}

/// The options for `control`, along with whether the options are missing/incomplete.
///
/// Prefers the custom property `wordcraft.chunk.<id>` when present and valid JSON (since it has
/// no 1024-char tag limit), and falls back to options encoded on `control.tag`.
pub(super) fn options_of(doc: &Document, control: &ContentControl) -> (ChunkOptions, bool) {
    if let Some(opts) = custom_prop_options(doc, control) {
        return (opts, false);
    }
    match decode_tag_options(&control.tag) {
        TagOptions::Complete(opts) => (opts, false),
        TagOptions::Truncated(opts) => {
            let missing_eval = opts.get("eval").is_none();
            (opts, missing_eval)
        }
        TagOptions::Missing => (ChunkOptions::default(), true),
    }
}
