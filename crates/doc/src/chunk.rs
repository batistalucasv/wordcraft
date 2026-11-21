//! Code chunks: executable R and Python code kept as Word content controls.
//!
//! A chunk is a block-level content control whose tag is `wordcraft:chunk:<language>` and whose
//! content is the code, one paragraph per line in the [`CHUNK_STYLE`] style (a shaded, bordered
//! code box). Word understands the control without knowing the tag, so a document with chunks
//! opens and saves like any other: the code stays ordinary, editable text.
//!
//! The control's options (`eval`, `echo`, `fig-cap`…) are Quarto chunk options. They travel with
//! the control in its `w:tag` (`wordcraft:chunk:<language>?…`, bounded to [`MAX_CONTROL_TEXT`]
//! characters with `eval` prioritized first) so copying a chunk inside Word preserves its options
//! even when Word assigns a fresh `w:id`. The full options JSON is also written to the custom
//! document property `wordcraft.chunk.<id>` so documents with many or long options keep full
//! fidelity on normal round trips.
//!
//! The kernel crate executes the code; this model only stores it, which keeps the document model
//! free of process handling and lets `.qmd` files be read and written without R or Python.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::control::{ContentControl, ControlKind, MAX_CONTROL_TEXT, walk_controls};
use crate::para::OBJ;
use crate::props::{Border, BorderStyle, Borders, CharProps, ParaProps, Rgb};
use crate::{Block, Blocks, DocError, Document, InlineObject, Paragraph, Pos, Result, StoryRef, Style, StyleKind, para_block};

/// Tag prefix of a code chunk's content control.
pub const CHUNK_TAG: &str = "wordcraft:chunk:";
/// Custom property prefix: `wordcraft.chunk.<id>` holds the chunk's options as JSON.
pub const CHUNK_PROP: &str = "wordcraft.chunk.";
/// Paragraph style of a chunk's code, and of the box around it.
pub const CHUNK_STYLE: &str = "CodeChunk";
/// Paragraph style of inserted console output blocks (editable code text, not a runnable chunk).
pub const CODE_OUTPUT_STYLE: &str = "CodeOutput";
/// Most options a chunk keeps.
pub const MAX_CHUNK_OPTIONS: usize = 256;
/// Longest option name or value a chunk keeps, in characters.
pub const MAX_CHUNK_OPTION_CHARS: usize = 4_096;
/// Most lines a chunk's code can span.
pub const MAX_CHUNK_LINES: usize = 100_000;
/// Longest code a chunk keeps, in bytes.
pub const MAX_CHUNK_CODE_BYTES: usize = 4 * 1024 * 1024;

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
        let raw = BTreeMap::<String, String>::deserialize(deserializer)?;
        let mut opts = ChunkOptions::new();
        for (k, v) in raw {
            opts.set(k, v);
        }
        Ok(opts)
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

    /// Set `key` to the raw `value`; past [`MAX_CHUNK_OPTIONS`] or a long name or value, nothing
    /// is stored (hostile input never grows the document without bound).
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        let value = value.into();
        if key.is_empty() || key.chars().count() > MAX_CHUNK_OPTION_CHARS || value.chars().count() > MAX_CHUNK_OPTION_CHARS {
            return;
        }
        if !self.values.contains_key(&key) && self.values.len() >= MAX_CHUNK_OPTIONS {
            return;
        }
        self.values.insert(key, value);
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
pub fn strip_yaml_comment(raw: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut escaped = false;
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
        } else if c == '#' {
            return raw[..i].trim_end();
        }
    }
    raw.trim_end()
}

/// A code line's value without the quotes around it, decoding only active quote escapes.
fn unquote(raw: &str) -> String {
    let t = raw.trim();
    let bytes = t.as_bytes();
    let Some(&first) = bytes.first() else { return String::new() };
    if (first == b'"' || first == b'\'') && bytes.last() == Some(&first) && bytes.len() >= 2 {
        let inner = t.get(1..t.len() - 1).unwrap_or("");
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some(next) => {
                        let unescaped = match (first, next) {
                            (b'"', '\\') => Some('\\'),
                            (b'"', '"') => Some('"'),
                            (b'\'', '\\') => Some('\\'),
                            (b'\'', '\'') => Some('\''),
                            _ => None,
                        };
                        if let Some(ch) = unescaped {
                            out.push(ch);
                        } else {
                            out.push('\\');
                            out.push(next);
                        }
                    }
                    None => out.push('\\'),
                }
            } else {
                out.push(c);
            }
        }
        out
    } else {
        t.to_string()
    }
}

/// One code chunk: its language, its source, and its Quarto options.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CodeChunk {
    /// The content control's `w:id`, when the chunk came from a document.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    /// `r`, `python` (lower-case, as Quarto writes it).
    pub language: String,
    /// The source: the lines joined with `\n` (a blank last line keeps its `\n`).
    pub code: String,
    pub options: ChunkOptions,
}

impl CodeChunk {
    pub fn new(language: impl Into<String>, code: impl Into<String>) -> CodeChunk {
        CodeChunk { id: None, language: normalize_language(&language.into()), code: code.into(), options: ChunkOptions::default() }
    }

    /// The content control tag: `wordcraft:chunk:<language>`.
    pub fn tag(&self) -> String {
        format!("{CHUNK_TAG}{}", if self.language.is_empty() { "text" } else { self.language.as_str() })
    }

    /// The `w:tag` value written onto `w:sdtPr`, carrying bounded options after `?`.
    pub fn control_tag(&self) -> String {
        encode_control_tag(&normalize_language(&self.language), &self.options)
    }

    /// The control's title, as Word's Tags pane shows it.
    pub fn alias(&self) -> String {
        let name = match self.language.as_str() {
            "r" => "R".to_string(),
            "python" => "Python".to_string(),
            "" => "Text".to_string(),
            other => {
                let mut c = other.chars();
                match c.next() {
                    Some(first) => first.to_uppercase().chain(c).collect(),
                    None => "Text".to_string(),
                }
            }
        };
        let alias = match self.options.label().filter(|l| !l.is_empty()) {
            Some(label) => format!("{name} code chunk ({label})"),
            None => format!("{name} code chunk"),
        };
        // The alias is plain text in the file: controls and absurd lengths from a hostile document
        // would make invalid or enormous markup.
        alias.chars().filter(|c| !c.is_control()).take(128).collect()
    }
}

/// A chunk found in a story, with where its content control starts and ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkRef {
    pub chunk: CodeChunk,
    pub start: Pos,
    pub end: Pos,
}

/// The custom property that holds chunk `id`'s options.
pub fn chunk_prop_name(id: i64) -> String {
    format!("{CHUNK_PROP}{id}")
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

/// Priority order for encoding options into `w:tag` so execution-safety flags fit first.
const PRIORITY_TAG_OPTIONS: &[&str] = &["eval", "echo", "include", "label"];

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

fn percent_decode(s: &str) -> String {
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

fn encode_tag_pairs(ordered: &[(&str, &str)], max_body: usize, options: &ChunkOptions) -> (String, bool) {
    let mut body = String::new();
    let mut truncated = false;

    for &(key, val) in ordered {
        let sep = usize::from(!body.is_empty());
        let key_len = encoded_len(key);
        let val_len = encoded_len(val);
        let needed = sep.saturating_add(key_len).saturating_add(1).saturating_add(val_len);
        if body.len().saturating_add(needed) <= max_body {
            if sep == 1 {
                body.push('&');
            }
            push_percent_encoded(&mut body, key);
            body.push('=');
            push_percent_encoded(&mut body, val);
            continue;
        }

        truncated = true;
        // If `eval` had a long comment that didn't fit verbatim, keep its boolean flag so
        // `eval: false` still survives a copy in Word.
        if key == "eval"
            && let Some(flag) = options.flag("eval")
        {
            let compact = if flag { "true" } else { "false" };
            let compact_needed = sep.saturating_add(key_len).saturating_add(1).saturating_add(compact.len());
            if body.len().saturating_add(compact_needed) <= max_body {
                if sep == 1 {
                    body.push('&');
                }
                push_percent_encoded(&mut body, key);
                body.push('=');
                body.push_str(compact);
            }
        }
    }

    (body, truncated)
}

/// Encode `language` and `options` into a `w:tag` string within [`MAX_CONTROL_TEXT`] chars.
fn encode_control_tag(language: &str, options: &ChunkOptions) -> String {
    encode_control_tag_with_truncated(language, options, false)
}

/// Encode `language` and `options` into a `w:tag` string within [`MAX_CONTROL_TEXT`] chars,
/// optionally carrying forward the `!` truncated marker when full options are unavailable.
fn encode_control_tag_with_truncated(language: &str, options: &ChunkOptions, force_truncated: bool) -> String {
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

enum TagOptions {
    /// Legacy or hand-written `wordcraft:chunk:<lang>` tag with no `?` query: options are missing.
    Missing,
    /// Tag carried the complete option set (`wordcraft:chunk:<lang>?...`).
    Complete(ChunkOptions),
    /// Tag carried partial options (`wordcraft:chunk:<lang>?!...`) after hitting [`MAX_CONTROL_TEXT`].
    Truncated(ChunkOptions),
}

fn decode_tag_options(tag: &str) -> TagOptions {
    let Some(rest) = tag.strip_prefix(CHUNK_TAG) else {
        return TagOptions::Missing;
    };
    let Some((_, query)) = rest.split_once('?') else {
        return TagOptions::Missing;
    };
    let (truncated, pairs) = match query.strip_prefix('!') {
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
            opts.set(k, v);
        }
    }
    if truncated { TagOptions::Truncated(opts) } else { TagOptions::Complete(opts) }
}

/// A language name as a tag carries it: lower-case ASCII letters, digits, `-`, `_`, `+` and `.`,
/// cut to 32 characters (tags come from files, so they're treated as hostile).
fn normalize_language(language: &str) -> String {
    language.trim().to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '.')).take(32).collect()
}

/// The code a chunk's control holds: the text of the paragraphs it spans, one line each.
fn code_of(doc: &Document, start: &Pos, end: &Pos) -> Option<String> {
    if start.story != end.story || start.path.parent() != end.path.parent() {
        return None;
    }
    let first = start.path.last();
    let last = end.path.last();
    if last < first {
        return None;
    }
    let blocks = doc.container(start.story, &start.path)?;
    let mut lines = Vec::new();
    for block in blocks.get(first..=last)? {
        let Block::Para(p) = block.as_ref() else { return None };
        lines.push(p.text.chars().filter(|c| *c != OBJ).collect::<String>());
    }
    Some(lines.join("\n"))
}

/// Parse full options from `wordcraft.chunk.<id>` when present and valid JSON.
fn custom_prop_options(doc: &Document, control: &ContentControl) -> Option<ChunkOptions> {
    let json = control.id.and_then(|id| doc.custom_prop(&chunk_prop_name(id)))?;
    serde_json::from_str::<ChunkOptions>(json).ok()
}

/// The options for `control`, along with whether the options are missing/incomplete.
///
/// Prefers the custom property `wordcraft.chunk.<id>` when present and valid JSON (since it has
/// no 1024-char tag limit), and falls back to options encoded on `control.tag`.
fn options_of(doc: &Document, control: &ContentControl) -> (ChunkOptions, bool) {
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

fn chunk_from_range(doc: &Document, range: &crate::ControlRange) -> Option<CodeChunk> {
    if !range.control.block {
        return None;
    }
    let language = tag_language(&range.control.tag)?;
    let code = code_of(doc, &range.start, &range.end)?;
    let (options, _) = options_of(doc, &range.control);
    Some(CodeChunk { id: range.control.id, language, code, options })
}

impl Document {
    /// Every chunk in every story, in document order (the body first).
    pub fn chunks(&self) -> Vec<CodeChunk> {
        let mut out: Vec<CodeChunk> = self.chunk_refs(StoryRef::Body).into_iter().map(|r| r.chunk).collect();
        for id in self.parts.keys() {
            out.extend(self.chunk_refs(StoryRef::Part(*id)).into_iter().map(|r| r.chunk));
        }
        out
    }

    /// The chunks of one story, in document order.
    pub fn chunk_refs(&self, story: StoryRef) -> Vec<ChunkRef> {
        let mut out: Vec<ChunkRef> = self
            .control_ranges(story)
            .into_iter()
            .filter_map(|r| chunk_from_range(self, &r).map(|chunk| ChunkRef { chunk, start: r.start.clone(), end: r.end.clone() }))
            .collect();
        out.sort_by(|a, b| (&a.start.path, a.start.off).cmp(&(&b.start.path, b.start.off)));
        out
    }

    /// Whether `range`'s chunk options could not be recovered (no valid `wordcraft.chunk.<id>`
    /// property and the tag carried neither complete options nor an `eval` flag).
    pub fn chunk_options_missing(&self, range: &ChunkRef) -> bool {
        let Some(para) = self.para(range.start.story, &range.start.path) else {
            return true;
        };
        let Some(InlineObject::ControlStart { control }) = para.object_at(range.start.off) else {
            return true;
        };
        options_of(self, control).1
    }

    /// `w:id`s in use anywhere in the document (including reserved chunk properties).
    fn control_ids(&self) -> BTreeSet<i64> {
        let mut ids = BTreeSet::new();
        let stories = std::iter::once(StoryRef::Body).chain(self.parts.keys().map(|k| StoryRef::Part(*k)));
        for story in stories {
            if let Some(blocks) = self.story(story) {
                walk_controls(blocks, &mut |c| {
                    if let Some(id) = c.id {
                        ids.insert(id);
                    }
                });
            }
        }
        for prop in &self.custom_props {
            if let Some(prefix) = prop.name.get(..CHUNK_PROP.len())
                && prefix.eq_ignore_ascii_case(CHUNK_PROP)
                && let Some(suffix) = prop.name.get(CHUNK_PROP.len()..)
                && let Ok(id) = suffix.parse::<i64>()
            {
                ids.insert(id);
            }
        }
        ids
    }

    /// The next free content control id (one past the highest used, never [`i64::MAX`] twice).
    pub fn next_control_id(&self) -> i64 {
        let ids = self.control_ids();
        let candidate = ids.last().copied().and_then(|id| id.checked_add(1));
        match candidate {
            Some(next) if next > 0 && !ids.contains(&next) => next,
            _ => {
                let mut free = 1i64;
                for &id in &ids {
                    if id == free {
                        free = free.saturating_add(1);
                    } else if id > free {
                        break;
                    }
                }
                free
            }
        }
    }

    /// The blocks that hold `chunk` as a block-level content control, ready to put in a story.
    ///
    /// The chunk gets a fresh control id unless its own is unused, and its options go into the
    /// custom property beside that id. The [`CHUNK_STYLE`] style is added when missing.
    pub fn chunk_blocks(&mut self, chunk: &CodeChunk) -> Result<Blocks> {
        if chunk.code.len() > MAX_CHUNK_CODE_BYTES {
            return Err(DocError::Invalid(format!("code chunk is larger than {MAX_CHUNK_CODE_BYTES} bytes")));
        }
        let line_count = chunk.code.bytes().filter(|b| *b == b'\n').count() + 1;
        if line_count > MAX_CHUNK_LINES {
            return Err(DocError::Invalid(format!("code chunk has more than {MAX_CHUNK_LINES} lines")));
        }
        if chunk.code.contains(OBJ) {
            return Err(DocError::Invalid("code chunk contains U+FFFC, which is reserved for inline objects".to_string()));
        }
        let id = match chunk.id {
            Some(id) if id > 0 && !self.control_ids().contains(&id) => id,
            _ => self.next_control_id(),
        };
        let language = normalize_language(&chunk.language);
        let mut control = ContentControl::new(ControlKind::RichText);
        control.id = Some(id);
        control.tag = encode_control_tag(&language, &chunk.options);
        control.title = chunk.alias();
        control.block = true;
        if self.styles.get(CHUNK_STYLE).is_none() {
            Arc::make_mut(&mut self.styles).upsert(chunk_style());
        }

        let prop = chunk_prop_name(id);
        let json = serde_json::to_string(&chunk.options).unwrap_or_else(|_| "{}".to_string());
        self.set_custom_prop(&prop, &json);

        let lines: Vec<&str> = chunk.code.split('\n').collect();
        let last = lines.len().saturating_sub(1);
        let mut blocks = Vec::with_capacity(lines.len());
        for (i, line) in lines.iter().enumerate() {
            let mut p = Paragraph::with_text(line, CharProps::default()).styled(CHUNK_STYLE);
            let mark = CharProps::default();
            if i == 0 {
                p.insert_object(0, InlineObject::ControlStart { control: Box::new(control.clone()) }, &mark)?;
            }
            if i == last {
                p.insert_object(p.len(), InlineObject::ControlEnd, &mark)?;
            }
            blocks.push(para_block(p));
        }
        Ok(blocks)
    }

    /// Append `chunk` at the end of the body. Returns the control id it got.
    pub fn push_chunk(&mut self, chunk: &CodeChunk) -> Result<i64> {
        let before = self.next_control_id();
        let blocks = self.chunk_blocks(chunk)?;
        let id = blocks.first().and_then(|b| b.as_para()).and_then(|p| p.objects.first()).and_then(|o| match o {
            InlineObject::ControlStart { control } => control.id,
            _ => None,
        });
        self.body.extend(blocks);
        Ok(id.unwrap_or(before))
    }

    /// Replace the language and code of chunk `id` in the body, keeping its control properties,
    /// custom options, existing `CodeChunk` style, enclosing content control markers, and any
    /// section break on its last paragraph.
    /// Returns `Ok(true)` if the chunk changed, `Ok(false)` if unchanged or not found.
    pub fn update_chunk(&mut self, id: i64, language: &str, code: &str) -> Result<bool> {
        if code.len() > MAX_CHUNK_CODE_BYTES {
            return Err(DocError::Invalid(format!("code chunk is larger than {MAX_CHUNK_CODE_BYTES} bytes")));
        }
        let line_count = code.bytes().filter(|b| *b == b'\n').count() + 1;
        if line_count > MAX_CHUNK_LINES {
            return Err(DocError::Invalid(format!("code chunk has more than {MAX_CHUNK_LINES} lines")));
        }
        if code.contains(OBJ) {
            return Err(DocError::Invalid("code chunk contains U+FFFC, which is reserved for inline objects".to_string()));
        }

        let Some(found) = self.chunk_refs(StoryRef::Body).into_iter().find(|r| r.chunk.id == Some(id)) else {
            return Ok(false);
        };
        let norm_lang = normalize_language(language);
        let effective_lang = if norm_lang.is_empty() { "text" } else { norm_lang.as_str() };
        if found.chunk.language == effective_lang && found.chunk.code == code {
            return Ok(false);
        }

        if found.start.story != found.end.story || found.start.path.parent() != found.end.path.parent() {
            return Ok(false);
        }
        let Some(container) = self.container(found.start.story, &found.start.path) else {
            return Ok(false);
        };
        let first = found.start.path.last();
        let last = found.end.path.last();
        if last < first || last >= container.len() {
            return Ok(false);
        }

        let Some(first_para) = container.get(first).and_then(|b| b.as_para()) else {
            return Ok(false);
        };
        let Some(last_para) = container.get(last).and_then(|b| b.as_para()) else {
            return Ok(false);
        };
        let Some(InlineObject::ControlStart { control }) = first_para.object_at(found.start.off) else {
            return Ok(false);
        };
        if !matches!(last_para.object_at(found.end.off), Some(InlineObject::ControlEnd)) {
            return Ok(false);
        }
        let mut control = (**control).clone();

        let leading_controls: Vec<InlineObject> = first_para
            .objects_with_offsets()
            .filter(|(off, obj)| *off < found.start.off && matches!(obj, InlineObject::ControlStart { .. } | InlineObject::ControlEnd))
            .map(|(_, obj)| obj.clone())
            .collect();
        let trailing_controls: Vec<InlineObject> = last_para
            .objects_with_offsets()
            .filter(|(off, obj)| *off > found.end.off && matches!(obj, InlineObject::ControlStart { .. } | InlineObject::ControlEnd))
            .map(|(_, obj)| obj.clone())
            .collect();

        let has_full_options_prop = custom_prop_options(self, &control).is_some();
        let new_tag = match decode_tag_options(&control.tag) {
            TagOptions::Missing => format!("{CHUNK_TAG}{effective_lang}"),
            TagOptions::Truncated(_) if !has_full_options_prop => encode_control_tag_with_truncated(effective_lang, &found.chunk.options, true),
            TagOptions::Complete(_) | TagOptions::Truncated(_) => encode_control_tag(effective_lang, &found.chunk.options),
        };
        control.tag = new_tag;
        let mut updated = CodeChunk::new(effective_lang, code);
        updated.options = found.chunk.options.clone();
        control.title = updated.alias();

        let trailing_section = last_para.section.clone();
        let first_ppr = first_para.props.clone();

        if self.styles.get(CHUNK_STYLE).is_none() {
            Arc::make_mut(&mut self.styles).upsert(chunk_style());
        }

        let lines: Vec<&str> = code.split('\n').collect();
        let last_line = lines.len().saturating_sub(1);
        let mut blocks = Vec::with_capacity(lines.len());
        let mark = CharProps::default();
        for (i, line) in lines.iter().enumerate() {
            let mut p = Paragraph::with_text(line, CharProps::default()).styled(CHUNK_STYLE);
            if i == 0 {
                p.props.page_break_before = first_ppr.page_break_before;
                p.props.keep_next = first_ppr.keep_next;
                p.props.keep_lines = first_ppr.keep_lines;
                let mut insert_off = 0;
                for marker in &leading_controls {
                    p.insert_object(insert_off, marker.clone(), &mark)?;
                    insert_off += OBJ.len_utf8();
                }
                p.insert_object(insert_off, InlineObject::ControlStart { control: Box::new(control.clone()) }, &mark)?;
            }
            if i == last_line {
                if let Some(sect) = &trailing_section {
                    p.section = Some(sect.clone());
                }
                p.insert_object(p.len(), InlineObject::ControlEnd, &mark)?;
                for marker in &trailing_controls {
                    p.insert_object(p.len(), marker.clone(), &mark)?;
                }
            }
            blocks.push(para_block(p));
        }

        let container = self.container_mut(found.start.story, &found.start.path)?;
        container.splice(first..=last, blocks);
        Ok(true)
    }

    /// Rename (or clear the label of) chunk `id`, updating its `label` option, control tag/alias,
    /// and `wordcraft.chunk.<id>` custom property. Returns `Ok(true)` if the label changed.
    pub fn set_chunk_label(&mut self, id: i64, label: Option<&str>) -> Result<bool> {
        let cleaned = label
            .map(|raw| raw.chars().filter(|c| !c.is_control()).collect::<String>())
            .map(|s| s.trim().chars().take(128).collect::<String>())
            .filter(|s| !s.is_empty());

        let Some(found) = self.chunk_refs(StoryRef::Body).into_iter().find(|r| r.chunk.id == Some(id)) else {
            return Ok(false);
        };
        let existing = found.chunk.options.label().map(str::to_owned);
        if existing == cleaned {
            return Ok(false);
        }

        let mut options = found.chunk.options.clone();
        match cleaned {
            Some(ref value) => options.set("label", value.clone()),
            None => {
                let _ = options.remove("label");
                let _ = options.remove("id");
            }
        }

        let norm_lang = normalize_language(&found.chunk.language);
        let effective_lang = if norm_lang.is_empty() { "text" } else { norm_lang.as_str() };
        let prop = chunk_prop_name(id);
        let json = serde_json::to_string(&options).unwrap_or_else(|_| "{}".to_string());
        self.set_custom_prop(&prop, &json);

        let mut updated = CodeChunk::new(effective_lang, &found.chunk.code);
        updated.options = options.clone();
        let new_tag = encode_control_tag(effective_lang, &options);
        let new_title = updated.alias();
        if let Some(control) = self.control_mut(&found.start) {
            control.tag = new_tag;
            control.title = new_title;
        }
        Ok(true)
    }

    /// Ensure the [`CODE_OUTPUT_STYLE`] paragraph style exists on the document.
    pub fn ensure_code_output_style(&mut self) {
        if self.styles.get(CODE_OUTPUT_STYLE).is_none() {
            Arc::make_mut(&mut self.styles).upsert(code_output_style());
        }
    }
}

/// The style a chunk's code is set in: a light code box, readable in Word without anything else.
fn chunk_style() -> Style {
    let border = Border { style: BorderStyle::Single, width: 2.25, color: Some(Rgb(0x8C, 0x8C, 0x8C)), space: 6.0 };
    Style {
        id: CHUNK_STYLE.into(),
        name: "Code Chunk".into(),
        kind: StyleKind::Paragraph,
        based_on: Some("Normal".into()),
        para: ParaProps {
            shading: Some(Rgb(0xF4, 0xF4, 0xF4)),
            borders: Some(Borders { left: Some(border), ..Default::default() }),
            indent_left: Some(8.0),
            indent_right: Some(8.0),
            space_before: Some(0.0),
            space_after: Some(0.0),
            ..Default::default()
        },
        chr: CharProps { font: Some("Courier New".into()), size: Some(9.5), no_proof: Some(true), ..Default::default() },
        ..Default::default()
    }
}

fn code_output_style() -> Style {
    let border = Border { style: BorderStyle::Single, width: 1.5, color: Some(Rgb(0xCB, 0xD5, 0xE1)), space: 6.0 };
    Style {
        id: CODE_OUTPUT_STYLE.into(),
        name: "Code Output".into(),
        kind: StyleKind::Paragraph,
        based_on: Some("Normal".into()),
        para: ParaProps {
            shading: Some(Rgb(0xF8, 0xFA, 0xFC)),
            borders: Some(Borders { left: Some(border), ..Default::default() }),
            indent_left: Some(8.0),
            indent_right: Some(8.0),
            space_before: Some(0.0),
            space_after: Some(0.0),
            ..Default::default()
        },
        chr: CharProps { font: Some("Courier New".into()), size: Some(9.5), no_proof: Some(true), ..Default::default() },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk() -> CodeChunk {
        let mut c = CodeChunk::new("r", "x <- 1\nplot(x)");
        c.options.set("label", "fig-plot");
        c.options.set("echo", "FALSE");
        c.options.set("fig-cap", "\"A plot\"");
        c
    }

    #[test]
    fn a_chunk_is_a_tagged_block_control_and_reads_back() {
        let mut doc = Document::new();
        let id = doc.push_chunk(&chunk()).unwrap();
        let found = doc.chunks();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].language, "r");
        assert_eq!(found[0].code, "x <- 1\nplot(x)");
        assert_eq!(found[0].id, Some(id));
        assert_eq!(found[0].options.get("echo"), Some("FALSE"));
        assert!(!found[0].options.echo());
        assert_eq!(found[0].options.fig_caption().as_deref(), Some("A plot"));
        let range = &doc.control_ranges(StoryRef::Body)[0];
        assert!(range.control.tag.starts_with("wordcraft:chunk:r?"), "{}", range.control.tag);
        assert!(range.control.block);
        assert_eq!(range.control.title, "R code chunk (fig-plot)");
    }

    #[test]
    fn a_chunk_keeps_every_line_including_empty_ones() {
        let mut doc = Document::new();
        let code = "a\n\nb\n";
        doc.push_chunk(&CodeChunk::new("python", code)).unwrap();
        assert_eq!(doc.chunks()[0].code, code);
    }

    #[test]
    fn two_chunks_get_distinct_control_ids() {
        let mut doc = Document::new();
        let a = doc.push_chunk(&CodeChunk::new("r", "1")).unwrap();
        let b = doc.push_chunk(&CodeChunk::new("python", "2")).unwrap();
        assert_ne!(a, b);
        assert_eq!(doc.chunks().len(), 2);
    }

    #[test]
    fn a_hostile_language_makes_a_safe_tag() {
        let chunk = CodeChunk::new("R\n\" onload=\"x", "1");
        assert!(!chunk.tag().contains('"'));
        assert!(!chunk.tag().contains('\n'));
        assert!(chunk.tag().starts_with(CHUNK_TAG));
    }

    #[test]
    fn options_read_flags_and_quoted_text() {
        let mut o = ChunkOptions::new();
        o.set("eval", "true");
        o.set("echo", "'FALSE'");
        o.set("fig.cap", "\"A \\\"quoted\\\" caption\"");
        assert!(o.eval());
        assert!(!o.echo());
        assert_eq!(o.fig_caption().as_deref(), Some("A \"quoted\" caption"));
    }

    #[test]
    fn oversized_code_is_refused_not_truncated() {
        let mut doc = Document::new();
        let code = "x".repeat(MAX_CHUNK_CODE_BYTES + 1);
        assert!(doc.chunk_blocks(&CodeChunk::new("r", code)).is_err());
    }

    #[test]
    fn chunk_options_are_bounded() {
        let mut o = ChunkOptions::new();
        for i in 0..(MAX_CHUNK_OPTIONS + 10) {
            o.set(format!("k{i}"), "1");
        }
        assert_eq!(o.len(), MAX_CHUNK_OPTIONS);
        o.set("k0", "x".repeat(MAX_CHUNK_OPTION_CHARS + 1));
        assert_eq!(o.get("k0"), Some("1"));
    }

    #[test]
    fn unquote_preserves_unsupported_escapes() {
        assert_eq!(unquote(r#""C:\temp""#), r#"C:\temp"#);
        assert_eq!(unquote(r#"'C:\temp'"#), r#"C:\temp"#);
        assert_eq!(unquote(r#""a\nb""#), r#"a\nb"#);
        assert_eq!(unquote(r#"'a\nb'"#), r#"a\nb"#);
        assert_eq!(unquote(r#""a\"b""#), "a\"b");
        assert_eq!(unquote(r#"'a\'b'"#), "a'b");
        assert_eq!(unquote(r#""path\\""#), r#"path\"#);
    }

    #[test]
    fn chunk_options_deserialization_is_bounded() {
        let mut map = std::collections::BTreeMap::new();
        for i in 0..(MAX_CHUNK_OPTIONS + 50) {
            map.insert(format!("opt{i}"), "val".to_string());
        }
        map.insert("huge".to_string(), "x".repeat(MAX_CHUNK_OPTION_CHARS + 100));
        let json = serde_json::to_string(&map).unwrap();
        let opts: ChunkOptions = serde_json::from_str(&json).unwrap();
        assert!(opts.len() <= MAX_CHUNK_OPTIONS);
        assert_eq!(opts.get("huge"), None);
    }

    #[test]
    fn chunk_blocks_reserves_id_before_insertion() {
        let mut doc = Document::new();
        let b1 = doc.chunk_blocks(&CodeChunk::new("r", "1")).unwrap();
        let b2 = doc.chunk_blocks(&CodeChunk::new("r", "2")).unwrap();
        let id1 = b1.first().and_then(|b| b.as_para()).and_then(|p| p.objects.first()).and_then(|o| match o {
            InlineObject::ControlStart { control } => control.id,
            _ => None,
        });
        let id2 = b2.first().and_then(|b| b.as_para()).and_then(|p| p.objects.first()).and_then(|o| match o {
            InlineObject::ControlStart { control } => control.id,
            _ => None,
        });
        assert!(id1.is_some() && id2.is_some());
        assert_ne!(id1, id2);
    }

    #[test]
    fn chunk_blocks_rejects_object_replacement_character() {
        let mut doc = Document::new();
        let custom_props_before = doc.custom_props.clone();
        let chunk = CodeChunk::new("r", "print('\u{FFFC}')");
        let result = doc.chunk_blocks(&chunk);
        assert!(matches!(result, Err(DocError::Invalid(_))));
        assert_eq!(doc.custom_props, custom_props_before);
    }

    #[test]
    fn label_strips_paired_single_or_double_quotes_and_preserves_raw_value() {
        let mut o = ChunkOptions::new();
        o.set("label", "'fig-single'");
        assert_eq!(o.label(), Some("fig-single"));
        assert_eq!(o.get("label"), Some("'fig-single'"));

        o.set("label", "\"fig-double\"");
        assert_eq!(o.label(), Some("fig-double"));
        assert_eq!(o.get("label"), Some("\"fig-double\""));

        o.set("label", "'unmatched");
        assert_eq!(o.label(), Some("'unmatched"));

        o.set("label", "'fig-comment' # trailing comment");
        assert_eq!(o.label(), Some("fig-comment"));
        assert_eq!(o.get("label"), Some("'fig-comment' # trailing comment"));
    }

    #[test]
    fn options_decode_yaml_comments_and_booleans() {
        let mut o = ChunkOptions::new();
        o.set("eval", "false # example only");
        assert_eq!(o.flag("eval"), Some(false));
        assert!(!o.eval());
        assert_eq!(o.get("eval"), Some("false # example only"));

        o.set("echo", "'true' # comment");
        assert_eq!(o.flag("echo"), Some(true));
        assert!(o.echo());

        o.set("fig.cap", "\"Plot #1\" # description");
        assert_eq!(o.fig_caption().as_deref(), Some("Plot #1"));
        assert_eq!(o.get("fig.cap"), Some("\"Plot #1\" # description"));
    }

    #[test]
    fn chunk_stored_in_table_cell_is_returned_by_chunks() {
        use crate::table::Table;
        let mut doc = Document::new();
        let blocks = doc.chunk_blocks(&CodeChunk::new("r", "val <- 42")).unwrap();
        let mut table = Table::new(1, 1, 100.0);
        table.rows[0].cells[0].blocks = blocks;
        doc.body = vec![Arc::new(Block::Table(table))];
        let chunks = doc.chunks();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].language, "r");
        assert_eq!(chunks[0].code, "val <- 42");
    }

    #[test]
    fn inline_control_with_chunk_tag_is_not_read_as_chunk() {
        let mut doc = Document::new();
        let mut control = ContentControl::new(ControlKind::RichText);
        control.block = false;
        control.tag = format!("{CHUNK_TAG}r");
        control.id = Some(101);
        let mut p = Paragraph::with_text("inline text", CharProps::default());
        let mark = CharProps::default();
        p.insert_object(0, InlineObject::ControlStart { control: Box::new(control) }, &mark).unwrap();
        p.insert_object(p.len(), InlineObject::ControlEnd, &mark).unwrap();
        doc.body = vec![para_block(p)];
        assert!(doc.chunks().is_empty());
    }

    #[test]
    fn case_insensitive_chunk_property_reservation() {
        let mut doc = Document::new();
        doc.set_custom_prop("WORDCRAFT.CHUNK.1", "{\"eval\":\"false\"}");
        assert_eq!(doc.next_control_id(), 2);
        let id = doc.push_chunk(&CodeChunk::new("r", "1")).unwrap();
        assert_eq!(id, 2);
        assert_eq!(doc.custom_prop("WORDCRAFT.CHUNK.1"), Some("{\"eval\":\"false\"}"));
    }

    #[test]
    fn multibyte_custom_property_crossing_chunk_prefix_boundary_does_not_panic() {
        let mut doc = Document::new();
        // Byte 16 (`CHUNK_PROP.len()`) falls inside multi-byte UTF-8 characters here.
        doc.set_custom_prop("Document title é", "Draft");
        doc.set_custom_prop("123456789012345😀", "Emoji");
        doc.set_custom_prop("wordcraft.chunk.é", "Not an id");
        doc.set_custom_prop("WORDCRAFT.CHUNK.1", "{\"eval\":\"false\"}");

        assert_eq!(doc.next_control_id(), 2);
        let id = doc.push_chunk(&CodeChunk::new("r", "1 + 1")).unwrap();
        assert_eq!(id, 2);
        assert_eq!(doc.custom_prop("Document title é"), Some("Draft"));
    }

    #[test]
    fn next_control_id_finds_lowest_positive_when_overflowing() {
        let mut doc = Document::new();
        doc.set_custom_prop(&chunk_prop_name(1), "{}");
        doc.set_custom_prop(&chunk_prop_name(i64::MAX), "{}");
        assert_eq!(doc.next_control_id(), 2);
    }

    #[test]
    fn chunk_blocks_rejects_excessive_lines() {
        let mut doc = Document::new();
        let code = "\n".repeat(MAX_CHUNK_LINES + 1);
        let result = doc.chunk_blocks(&CodeChunk::new("r", code));
        assert!(matches!(result, Err(DocError::Invalid(_))));
    }

    #[test]
    fn chunk_blocks_preserves_existing_code_chunk_style() {
        let mut doc = Document::new();
        let custom_style = Style { id: CHUNK_STYLE.into(), name: "Custom Box Style".into(), kind: StyleKind::Paragraph, ..Default::default() };
        Arc::make_mut(&mut doc.styles).upsert(custom_style);
        let _ = doc.chunk_blocks(&CodeChunk::new("r", "1 + 1")).unwrap();
        assert_eq!(doc.styles.get(CHUNK_STYLE).map(|s| s.name.as_str()), Some("Custom Box Style"));
    }

    #[test]
    fn update_chunk_replaces_code_and_language_without_losing_options() {
        let mut doc = Document::new();
        let mut opts = ChunkOptions::new();
        opts.set("eval", "false");
        let mut chunk = CodeChunk::new("r", "a <- 1\nb <- 2");
        chunk.options = opts.clone();
        let id = doc.push_chunk(&chunk).unwrap();
        assert!(!doc.update_chunk(id, "r", "a <- 1\nb <- 2").unwrap());
        assert!(doc.update_chunk(id, "python", "x = 42").unwrap());
        let chunks = doc.chunks();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].id, Some(id));
        assert_eq!(chunks[0].language, "python");
        assert_eq!(chunks[0].code, "x = 42");
        assert_eq!(chunks[0].options, opts);
    }

    #[test]
    fn update_chunk_rejects_invalid_input_without_mutating() {
        let mut doc = Document::new();
        let id = doc.push_chunk(&CodeChunk::new("python", "x = 1")).unwrap();
        let err = doc.update_chunk(id, "python", &format!("a{OBJ}b"));
        assert!(matches!(err, Err(DocError::Invalid(_))));
        let too_many_lines = "\n".repeat(MAX_CHUNK_LINES + 1);
        assert!(matches!(doc.update_chunk(id, "python", &too_many_lines), Err(DocError::Invalid(_))));
        assert_eq!(doc.chunks()[0].code, "x = 1");
    }

    #[test]
    fn update_chunk_updates_nested_table_cell_chunk_without_touching_body_block_zero() {
        use crate::{Block, Path, Table};

        let mut doc = Document::from_text("body intro");
        let cell_blocks = doc.chunk_blocks(&CodeChunk::new("r", "a <- 1")).unwrap();
        let id = doc.chunk_refs(StoryRef::Body).first().and_then(|r| r.chunk.id).unwrap_or_else(|| match &*cell_blocks[0] {
            Block::Para(p) => match &p.objects[0] {
                InlineObject::ControlStart { control } => control.id.unwrap(),
                _ => panic!("expected control start"),
            },
            _ => panic!("expected para"),
        });
        let mut table = Table::new(1, 1, 200.0);
        table.rows[0].cells[0].blocks = cell_blocks;
        doc.body.push(Arc::new(Block::Table(table)));

        assert!(doc.update_chunk(id, "python", "x = 99").unwrap());
        assert_eq!(doc.para(StoryRef::Body, &Path::top(0)).unwrap().plain_text(), "body intro");
        let updated = doc.chunk_refs(StoryRef::Body).into_iter().find(|r| r.chunk.id == Some(id)).unwrap();
        assert_eq!(updated.chunk.language, "python");
        assert_eq!(updated.chunk.code, "x = 99");
        assert_eq!(updated.start.path.0, vec![1, 0, 0, 0]);
    }

    #[test]
    fn update_chunk_treats_empty_language_on_text_chunk_as_noop() {
        let mut doc = Document::new();
        let id = doc.push_chunk(&CodeChunk::new("", "plain")).unwrap();
        assert!(!doc.update_chunk(id, "", "plain").unwrap());
    }

    #[test]
    fn copied_chunk_with_fresh_id_and_no_property_keeps_options_from_tag() {
        let mut doc = Document::new();
        let mut c = CodeChunk::new("r", "unlink('important')");
        c.options.set("eval", "false");
        c.options.set("echo", "FALSE");
        c.options.set("fig-cap", "\"A & B = 100%\"");
        c.options.set("label", "fig-safe");
        let old_id = doc.push_chunk(&c).unwrap();

        // Simulate Word copy/paste: fresh control id and no matching `wordcraft.chunk.<new_id>`.
        let start = doc.control_ranges(StoryRef::Body)[0].start.clone();
        doc.control_mut(&start).unwrap().id = Some(old_id + 99);
        doc.remove_custom_prop(&chunk_prop_name(old_id));

        let refs = doc.chunk_refs(StoryRef::Body);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].chunk.id, Some(old_id + 99));
        assert_eq!(refs[0].chunk.language, "r");
        assert!(!refs[0].chunk.options.eval());
        assert!(!refs[0].chunk.options.echo());
        assert_eq!(refs[0].chunk.options.fig_caption().as_deref(), Some("A & B = 100%"));
        assert_eq!(refs[0].chunk.options.label(), Some("fig-safe"));
        assert!(!doc.chunk_options_missing(&refs[0]));
    }

    #[test]
    fn empty_options_chunk_with_fresh_id_and_no_property_is_not_missing() {
        let mut doc = Document::new();
        let old_id = doc.push_chunk(&CodeChunk::new("python", "print(1)")).unwrap();
        let start = doc.control_ranges(StoryRef::Body)[0].start.clone();
        doc.control_mut(&start).unwrap().id = Some(old_id + 50);
        doc.remove_custom_prop(&chunk_prop_name(old_id));

        let refs = doc.chunk_refs(StoryRef::Body);
        assert_eq!(refs.len(), 1);
        assert!(refs[0].chunk.options.is_empty());
        assert!(refs[0].chunk.options.eval());
        assert!(!doc.chunk_options_missing(&refs[0]));
    }

    #[test]
    fn legacy_tag_without_query_and_no_property_reports_missing_options() {
        let mut doc = Document::new();
        let old_id = doc.push_chunk(&CodeChunk::new("r", "1 + 1")).unwrap();
        let start = doc.control_ranges(StoryRef::Body)[0].start.clone();
        let control = doc.control_mut(&start).unwrap();
        control.tag = "wordcraft:chunk:r".to_string();
        control.id = Some(old_id + 10);
        doc.remove_custom_prop(&chunk_prop_name(old_id));

        let refs = doc.chunk_refs(StoryRef::Body);
        assert_eq!(refs.len(), 1);
        assert!(doc.chunk_options_missing(&refs[0]));
    }

    #[test]
    fn huge_options_stay_bounded_in_tag_and_preserve_eval_false_on_copy() {
        let mut doc = Document::new();
        let mut c = CodeChunk::new("r", "stop('boom')");
        c.options.set("fig-cap", format!("\"{}\"", "a".repeat(2_000)));
        c.options.set("eval", "false # very long trailing explanation that would exceed tag limit ".to_owned() + &"x".repeat(1_200));
        c.options.set("warning", "false");
        let old_id = doc.push_chunk(&c).unwrap();

        let range = &doc.control_ranges(StoryRef::Body)[0];
        assert!(range.control.tag.chars().count() <= MAX_CONTROL_TEXT);
        assert!(range.control.tag.starts_with("wordcraft:chunk:r?!"), "{}", range.control.tag);

        // With the custom property intact, the full 2000-char caption is preserved.
        assert_eq!(doc.chunks()[0].options.fig_caption().as_deref().map(str::len), Some(2_000));

        // After copying in Word (fresh id, no property), eval: false and warning: false survive.
        let start = range.start.clone();
        doc.control_mut(&start).unwrap().id = Some(old_id + 100);
        doc.remove_custom_prop(&chunk_prop_name(old_id));

        let refs = doc.chunk_refs(StoryRef::Body);
        assert!(!refs[0].chunk.options.eval());
        assert_eq!(refs[0].chunk.options.flag("warning"), Some(false));
        assert!(!doc.chunk_options_missing(&refs[0]));
    }

    #[test]
    fn exact_budget_eval_with_overflow_option_keeps_eval_false_on_copy() {
        let mut doc = Document::new();
        let mut c = CodeChunk::new("r", "stop('boom')");
        // `wordcraft:chunk:r?` (18) + `eval=` (5) + `false%20%23` (11) + 990 `'x'` = 1024 == MAX_CONTROL_TEXT.
        c.options.set("eval", format!("false #{}", "x".repeat(990)));
        c.options.set("echo", "false");
        let old_id = doc.push_chunk(&c).unwrap();

        let range = &doc.control_ranges(StoryRef::Body)[0];
        assert!(range.control.tag.chars().count() <= MAX_CONTROL_TEXT);
        assert!(range.control.tag.starts_with("wordcraft:chunk:r?!"), "{}", range.control.tag);

        let start = range.start.clone();
        doc.control_mut(&start).unwrap().id = Some(old_id + 200);
        doc.remove_custom_prop(&chunk_prop_name(old_id));

        let refs = doc.chunk_refs(StoryRef::Body);
        assert!(!refs[0].chunk.options.eval());
        assert!(!refs[0].chunk.options.echo());
        assert!(!doc.chunk_options_missing(&refs[0]));
    }

    #[test]
    fn tag_language_and_malformed_percent_escapes_never_panic() {
        assert_eq!(tag_language("wordcraft:chunk:r?eval=false"), Some("r".into()));
        assert_eq!(tag_language("wordcraft:chunk:python?"), Some("python".into()));
        assert_eq!(tag_language("wordcraft:chunk:r?!eval=false"), Some("r".into()));
        assert_eq!(tag_language("wordcraft:chunk:?eval=false"), None);
        assert_eq!(tag_language("wordcraft:chunk:???"), None);

        assert_eq!(percent_decode("abc%ZZ%2%FF%20def"), "abc%ZZ%2\u{FFFD} def");
        let TagOptions::Complete(opts) = decode_tag_options("wordcraft:chunk:r?badpair&k=%FF%20v") else {
            panic!("expected Complete");
        };
        assert_eq!(opts.get("k"), Some("\u{FFFD} v"));
    }

    #[test]
    fn update_chunk_preserves_missing_options_on_legacy_chunk() {
        let mut doc = Document::new();
        let chunk = CodeChunk::new("python", "x = 1");
        let id = doc.push_chunk(&chunk).unwrap();
        // Turn into a legacy chunk: strip tag query and custom property.
        let range = &doc.control_ranges(StoryRef::Body)[0];
        let start = range.start.clone();
        doc.control_mut(&start).unwrap().tag = "wordcraft:chunk:python".to_string();
        doc.remove_custom_prop(&chunk_prop_name(id));

        let refs = doc.chunk_refs(StoryRef::Body);
        assert!(doc.chunk_options_missing(&refs[0]));

        let updated = doc.update_chunk(id, "r", "y = 2").unwrap();
        assert!(updated);

        let after_refs = doc.chunk_refs(StoryRef::Body);
        assert_eq!(after_refs[0].chunk.language, "r");
        assert_eq!(after_refs[0].chunk.code, "y = 2");
        assert!(doc.chunk_options_missing(&after_refs[0]), "legacy chunk without options query or property must retain missing options status");

        // Updating a queried chunk re-encodes tag within MAX_CONTROL_TEXT.
        let mut doc2 = Document::new();
        let mut chunk2 = CodeChunk::new("python", "a = 1");
        chunk2.options.set("eval", "false");
        let id2 = doc2.push_chunk(&chunk2).unwrap();
        let updated2 = doc2.update_chunk(id2, "r", "b = 2").unwrap();
        assert!(updated2);
        let refs2 = doc2.chunk_refs(StoryRef::Body);
        assert_eq!(refs2[0].chunk.language, "r");
        assert_eq!(refs2[0].chunk.options.get("eval"), Some("false"));
        let range2 = &doc2.control_ranges(StoryRef::Body)[0];
        assert!(range2.control.tag.starts_with("wordcraft:chunk:r?"));
        assert!(range2.control.tag.chars().count() <= MAX_CONTROL_TEXT);
    }

    #[test]
    fn update_chunk_preserves_incomplete_options_on_copied_truncated_chunk() {
        let mut doc = Document::new();
        let id = doc.push_chunk(&CodeChunk::new("r", "x <- 1")).unwrap();
        let start = doc.control_ranges(StoryRef::Body)[0].start.clone();
        doc.control_mut(&start).unwrap().tag = "wordcraft:chunk:r?!echo=false".to_string();
        doc.remove_custom_prop(&chunk_prop_name(id));

        let before = doc.chunk_refs(StoryRef::Body);
        assert!(doc.chunk_options_missing(&before[0]));
        assert!(!before[0].chunk.options.echo());

        // Editing source only must keep the `!` truncated marker and missing-options status.
        assert!(doc.update_chunk(id, "r", "x <- 2").unwrap());
        let after = doc.chunk_refs(StoryRef::Body);
        assert_eq!(after[0].chunk.code, "x <- 2");
        assert!(!after[0].chunk.options.echo());
        assert!(
            doc.chunk_options_missing(&after[0]),
            "copied chunk with truncated tag and no custom property must keep incomplete options after edit"
        );
        let range = &doc.control_ranges(StoryRef::Body)[0];
        assert!(range.control.tag.starts_with("wordcraft:chunk:r?!"), "{}", range.control.tag);
        assert!(range.control.tag.chars().count() <= MAX_CONTROL_TEXT);

        // Changing language also keeps `!` and recovered options bounded.
        assert!(doc.update_chunk(id, "python", "x = 3").unwrap());
        let after_lang = doc.chunk_refs(StoryRef::Body);
        assert_eq!(after_lang[0].chunk.language, "python");
        assert!(!after_lang[0].chunk.options.echo());
        assert!(doc.chunk_options_missing(&after_lang[0]));
        let range_lang = &doc.control_ranges(StoryRef::Body)[0];
        assert!(range_lang.control.tag.starts_with("wordcraft:chunk:python?!"), "{}", range_lang.control.tag);
        assert!(range_lang.control.tag.chars().count() <= MAX_CONTROL_TEXT);

        // Empty truncated tag (`?!`) with no custom property also remains incomplete.
        let start_empty = range_lang.start.clone();
        doc.control_mut(&start_empty).unwrap().tag = "wordcraft:chunk:r?!".to_string();
        assert!(doc.update_chunk(id, "r", "x <- 4").unwrap());
        let after_empty = doc.chunk_refs(StoryRef::Body);
        assert!(doc.chunk_options_missing(&after_empty[0]));
        assert_eq!(doc.control_ranges(StoryRef::Body)[0].control.tag, "wordcraft:chunk:r?!");

        // Complete options in a copied chunk tag (no `!`, no custom property) stay complete.
        let mut complete_doc = Document::new();
        let complete_id = complete_doc.push_chunk(&CodeChunk::new("r", "a <- 1")).unwrap();
        let complete_start = complete_doc.control_ranges(StoryRef::Body)[0].start.clone();
        complete_doc.control_mut(&complete_start).unwrap().tag = "wordcraft:chunk:r?echo=false".to_string();
        complete_doc.remove_custom_prop(&chunk_prop_name(complete_id));
        assert!(complete_doc.update_chunk(complete_id, "r", "a <- 2").unwrap());
        let complete_after = complete_doc.chunk_refs(StoryRef::Body);
        assert!(!complete_doc.chunk_options_missing(&complete_after[0]));
        assert!(!complete_after[0].chunk.options.echo());
        assert_eq!(complete_doc.control_ranges(StoryRef::Body)[0].control.tag, "wordcraft:chunk:r?echo=false");

        // When a valid full custom property exists, re-encoding uses the full options.
        let mut prop_doc = Document::new();
        let mut prop_chunk = CodeChunk::new("r", "b <- 1");
        prop_chunk.options.set("echo", "false");
        let prop_id = prop_doc.push_chunk(&prop_chunk).unwrap();
        let prop_start = prop_doc.control_ranges(StoryRef::Body)[0].start.clone();
        prop_doc.control_mut(&prop_start).unwrap().tag = "wordcraft:chunk:r?!echo=false".to_string();
        assert!(prop_doc.update_chunk(prop_id, "r", "b <- 2").unwrap());
        let prop_after = prop_doc.chunk_refs(StoryRef::Body);
        assert!(!prop_doc.chunk_options_missing(&prop_after[0]));
        assert_eq!(prop_doc.control_ranges(StoryRef::Body)[0].control.tag, "wordcraft:chunk:r?echo=false");

        // When the custom property is malformed JSON, treat options as unavailable and keep `!`.
        let mut bad_prop_doc = Document::new();
        let bad_id = bad_prop_doc.push_chunk(&CodeChunk::new("r", "c <- 1")).unwrap();
        let bad_start = bad_prop_doc.control_ranges(StoryRef::Body)[0].start.clone();
        bad_prop_doc.control_mut(&bad_start).unwrap().tag = "wordcraft:chunk:r?!echo=false".to_string();
        bad_prop_doc.set_custom_prop(&chunk_prop_name(bad_id), "{not valid json");
        assert!(bad_prop_doc.update_chunk(bad_id, "r", "c <- 2").unwrap());
        let bad_after = bad_prop_doc.chunk_refs(StoryRef::Body);
        assert!(bad_prop_doc.chunk_options_missing(&bad_after[0]));
        assert_eq!(bad_prop_doc.control_ranges(StoryRef::Body)[0].control.tag, "wordcraft:chunk:r?!echo=false");
    }

    fn assert_nested_controls(doc: &Document, chunk_id: i64, outer: &ContentControl, repeating: &ContentControl, last_block: usize) {
        assert!(!doc.has_unbalanced_controls());
        let ranges = doc.control_ranges(StoryRef::Body);
        assert_eq!(ranges.len(), 3, "enclosing block controls must survive chunk edit");
        let chunk_r = ranges.iter().find(|r| r.control.id == Some(chunk_id)).expect("chunk control present");
        let rep_r = ranges.iter().find(|r| r.control.id == repeating.id).expect("repeating control preserved");
        let out_r = ranges.iter().find(|r| r.control.id == outer.id).expect("outer control preserved");
        assert_eq!(out_r.depth, 0);
        assert_eq!(rep_r.depth, 1);
        assert_eq!(chunk_r.depth, 2);
        assert_eq!(out_r.start.path, crate::Path::top(0));
        assert_eq!(out_r.end.path, crate::Path::top(last_block));
        assert_eq!(rep_r.start.path, crate::Path::top(0));
        assert_eq!(rep_r.end.path, crate::Path::top(last_block));
        assert_eq!(chunk_r.start.path, crate::Path::top(0));
        assert_eq!(chunk_r.end.path, crate::Path::top(last_block));
        assert!(out_r.start.off < rep_r.start.off);
        assert!(rep_r.start.off < chunk_r.start.off);
        assert!(chunk_r.end.off < rep_r.end.off);
        assert!(rep_r.end.off < out_r.end.off);
        assert_eq!(&out_r.control, outer);
        assert_eq!(&rep_r.control, repeating);
    }

    fn locked_outer_control() -> ContentControl {
        use crate::control::ControlLock;

        let mut outer = ContentControl::new(ControlKind::RichText);
        outer.id = Some(9001);
        outer.tag = "section:locked".to_string();
        outer.title = "Locked Section".to_string();
        outer.lock = ControlLock::SdtLocked;
        outer.block = true;
        outer
    }

    #[test]
    fn update_chunk_preserves_enclosing_block_controls_on_single_paragraph_chunk() {
        use crate::control::ControlLock;

        let mut doc = Document::new();
        doc.body.clear();
        let chunk_id = doc.push_chunk(&CodeChunk::new("r", "x <- 1")).unwrap();
        let chunk_path = doc.chunk_refs(StoryRef::Body)[0].start.path.clone();
        assert_eq!(chunk_path, crate::Path::top(0));
        let mark = CharProps::default();

        let outer = locked_outer_control();
        let mut repeating = ContentControl::new(ControlKind::RepeatingSection { title: "Item".to_string(), no_insert_delete: true });
        repeating.id = Some(9002);
        repeating.tag = "section:repeat".to_string();
        repeating.title = "Repeating Section".to_string();
        repeating.lock = ControlLock::SdtContentLocked;
        repeating.block = true;

        {
            let p = doc.para_mut(StoryRef::Body, &chunk_path).unwrap();
            p.insert_object(0, InlineObject::ControlStart { control: Box::new(outer.clone()) }, &mark).unwrap();
            p.insert_object(OBJ.len_utf8(), InlineObject::ControlStart { control: Box::new(repeating.clone()) }, &mark).unwrap();
            p.insert_object(p.len(), InlineObject::ControlEnd, &mark).unwrap();
            p.insert_object(p.len(), InlineObject::ControlEnd, &mark).unwrap();
        }
        assert_nested_controls(&doc, chunk_id, &outer, &repeating, 0);

        // Single-paragraph -> single-paragraph edit.
        assert!(doc.update_chunk(chunk_id, "python", "x = 2").unwrap());
        assert_nested_controls(&doc, chunk_id, &outer, &repeating, 0);
        assert_eq!(doc.chunks()[0].language, "python");
        assert_eq!(doc.chunks()[0].code, "x = 2");

        // Single-paragraph -> multi-paragraph edit keeps outer starts on first para and ends on last para.
        assert!(doc.update_chunk(chunk_id, "python", "x = 2\ny = 3\nz = 4").unwrap());
        assert_nested_controls(&doc, chunk_id, &outer, &repeating, 2);
        assert_eq!(doc.chunks()[0].code, "x = 2\ny = 3\nz = 4");

        // Multi-paragraph -> single-paragraph edit preserves nesting and exact marker order.
        assert!(doc.update_chunk(chunk_id, "r", "done <- TRUE").unwrap());
        assert_nested_controls(&doc, chunk_id, &outer, &repeating, 0);
        assert_eq!(doc.chunks()[0].code, "done <- TRUE");
    }

    #[test]
    fn update_chunk_preserves_outer_control_spanning_adjacent_paragraphs() {
        let mark = CharProps::default();
        let outer = locked_outer_control();

        // Outer control starting on a preceding paragraph and closing on the chunk's last paragraph.
        let mut doc2 = Document::from_text("intro");
        let cid2 = doc2.push_chunk(&CodeChunk::new("r", "a <- 1")).unwrap();
        assert_eq!(doc2.chunk_refs(StoryRef::Body)[0].start.path, crate::Path::top(1));
        {
            let p0 = doc2.para_mut(StoryRef::Body, &crate::Path::top(0)).unwrap();
            p0.insert_object(0, InlineObject::ControlStart { control: Box::new(outer.clone()) }, &mark).unwrap();
            let p1 = doc2.para_mut(StoryRef::Body, &crate::Path::top(1)).unwrap();
            p1.insert_object(p1.len(), InlineObject::ControlEnd, &mark).unwrap();
        }
        assert!(!doc2.has_unbalanced_controls());
        assert!(doc2.update_chunk(cid2, "r", "a <- 1\nb <- 2").unwrap());
        assert!(!doc2.has_unbalanced_controls());
        let r2 = doc2.control_ranges(StoryRef::Body);
        let out2 = r2.iter().find(|r| r.control.id == Some(9001)).expect("outer control preserved");
        assert_eq!(out2.start.path, crate::Path::top(0));
        assert_eq!(out2.end.path, crate::Path::top(2));
        assert_eq!(out2.control, outer);

        // Outer control starting on the chunk's first paragraph and closing on a following paragraph.
        let mut doc3 = Document::new();
        doc3.body.clear();
        let cid3 = doc3.push_chunk(&CodeChunk::new("r", "c <- 1")).unwrap();
        assert_eq!(doc3.chunk_refs(StoryRef::Body)[0].start.path, crate::Path::top(0));
        doc3.body.push(para_block(Paragraph::with_text("outro", CharProps::default())));
        {
            let p0 = doc3.para_mut(StoryRef::Body, &crate::Path::top(0)).unwrap();
            p0.insert_object(0, InlineObject::ControlStart { control: Box::new(outer.clone()) }, &mark).unwrap();
            let p1 = doc3.para_mut(StoryRef::Body, &crate::Path::top(1)).unwrap();
            p1.insert_object(p1.len(), InlineObject::ControlEnd, &mark).unwrap();
        }
        assert!(!doc3.has_unbalanced_controls());
        assert!(doc3.update_chunk(cid3, "r", "c <- 1\nd <- 2").unwrap());
        assert!(!doc3.has_unbalanced_controls());
        let r3 = doc3.control_ranges(StoryRef::Body);
        let out3 = r3.iter().find(|r| r.control.id == Some(9001)).expect("outer control preserved");
        assert_eq!(out3.start.path, crate::Path::top(0));
        assert_eq!(out3.end.path, crate::Path::top(2));
        assert_eq!(out3.control, outer);
    }
}
