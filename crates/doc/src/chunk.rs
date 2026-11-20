//! Code chunks: executable R and Python code kept as Word content controls.
//!
//! A chunk is a block-level content control whose tag is `wordcraft:chunk:<language>` and whose
//! content is the code, one paragraph per line in the [`CHUNK_STYLE`] style (a shaded, bordered
//! code box). Word understands the control without knowing the tag, so a document with chunks
//! opens and saves like any other: the code stays ordinary, editable text.
//!
//! The control's options (`eval`, `echo`, `fig-cap`…) are Quarto chunk options, kept in a custom
//! document property named `wordcraft.chunk.<id>`, where `<id>` is the control's `w:id`. Custom
//! properties are part of the package Word itself writes and preserves, so the metadata survives
//! a round trip through Word without putting anything non-standard in the markup.
//!
//! The kernel crate executes the code; this model only stores it, which keeps the document model
//! free of process handling and lets `.qmd` files be read and written without R or Python.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::control::{ContentControl, ControlKind, walk_controls};
use crate::para::OBJ;
use crate::props::{Border, BorderStyle, Borders, CharProps, ParaProps, Rgb};
use crate::{Block, Blocks, DocError, Document, InlineObject, Paragraph, Pos, Result, StoryRef, Style, StyleKind, para_block};

/// Tag prefix of a code chunk's content control.
pub const CHUNK_TAG: &str = "wordcraft:chunk:";
/// Custom property prefix: `wordcraft.chunk.<id>` holds the chunk's options as JSON.
pub const CHUNK_PROP: &str = "wordcraft.chunk.";
/// Paragraph style of a chunk's code, and of the box around it.
pub const CHUNK_STYLE: &str = "CodeChunk";
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
    let rest = tag.strip_prefix(CHUNK_TAG)?.trim();
    if rest.is_empty() {
        return None;
    }
    Some(normalize_language(rest))
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

/// The options stored beside control `id`, or none when the property is missing or unreadable.
fn options_of(doc: &Document, id: Option<i64>) -> ChunkOptions {
    let Some(json) = id.and_then(|id| doc.custom_prop(&chunk_prop_name(id))) else { return ChunkOptions::default() };
    serde_json::from_str(json).unwrap_or_default()
}

fn chunk_from_range(doc: &Document, range: &crate::ControlRange) -> Option<CodeChunk> {
    if !range.control.block {
        return None;
    }
    let language = tag_language(&range.control.tag)?;
    let code = code_of(doc, &range.start, &range.end)?;
    Some(CodeChunk { id: range.control.id, language, code, options: options_of(doc, range.control.id) })
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
            if prop.name.len() > CHUNK_PROP.len()
                && prop.name[..CHUNK_PROP.len()].eq_ignore_ascii_case(CHUNK_PROP)
                && let Ok(id) = prop.name[CHUNK_PROP.len()..].parse::<i64>()
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
        control.tag = format!("{CHUNK_TAG}{}", if language.is_empty() { "text" } else { language.as_str() });
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
    /// custom options, existing `CodeChunk` style, and any section break on its last paragraph.
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
        let new_tag = format!("{CHUNK_TAG}{effective_lang}");

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
        let Some(mut control) = first_para.objects.iter().find_map(|o| match o {
            InlineObject::ControlStart { control } if control.id == Some(id) => Some((**control).clone()),
            _ => None,
        }) else {
            return Ok(false);
        };

        control.tag = new_tag;
        let mut updated = CodeChunk::new(effective_lang, code);
        updated.options = found.chunk.options.clone();
        control.title = updated.alias();

        let trailing_section = container.get(last).and_then(|b| b.as_para()).and_then(|p| p.section.clone());
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
                p.insert_object(0, InlineObject::ControlStart { control: Box::new(control.clone()) }, &mark)?;
            }
            if i == last_line {
                if let Some(sect) = &trailing_section {
                    p.section = Some(sect.clone());
                }
                p.insert_object(p.len(), InlineObject::ControlEnd, &mark)?;
            }
            blocks.push(para_block(p));
        }

        let container = self.container_mut(found.start.story, &found.start.path)?;
        container.splice(first..=last, blocks);
        Ok(true)
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
        assert_eq!(range.control.tag, "wordcraft:chunk:r");
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
}
