//! Code chunks: executable R and Python code kept as Word content controls.
//!
//! A chunk is a block-level content control whose tag is `wordcraft:chunk:<language>` and whose
//! content is the code, one paragraph per line in the [`CHUNK_STYLE`] style (a shaded, bordered
//! code box). Word understands the control without knowing the tag, so a document with chunks
//! opens and saves like any other: the code stays ordinary, editable text.
//!
//! The control's options (`eval`, `echo`, `fig-cap`…) are Quarto chunk options. They travel with
//! the control in its `w:tag` (`wordcraft:chunk:<language>?…`, bounded to
//! [`crate::control::MAX_CONTROL_TEXT`] characters with `eval` prioritized first) so copying a
//! chunk inside Word preserves its options even when Word assigns a fresh `w:id`. The full
//! options JSON is also written to the custom document property `wordcraft.chunk.<id>` so
//! documents with many or long options keep full fidelity on normal round trips.
//!
//! The kernel crate executes the code; this model only stores it, which keeps the document model
//! free of process handling and lets `.qmd` files be read and written without R or Python.

mod codec;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::control::{ControlEvent, pair_events, walk_controls};
use crate::para::OBJ;
use crate::props::{Border, BorderStyle, Borders, CharProps, ParaProps, Rgb};
use crate::{
    Block, Blocks, ContentControl, ControlKind, CustomProp, DocError, Document, InlineObject, Paragraph, Path, Pos, Result, StoryRef, Style,
    StyleKind, para_block,
};

pub use codec::{ChunkOptions, chunk_prop_name, strip_yaml_comment, tag_language};
use codec::{
    TagOptions, custom_prop_options, decode_tag_options, encode_control_tag, encode_control_tag_with_truncated, normalize_language, options_of,
};

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

/// Deepest table nesting scanned (matches `control` and `walk` in the crate root).
const MAX_SCAN_DEPTH: usize = 16;

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

/// Lightweight location and identity of a chunk in a story without materializing its source code
/// or parsing its options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkBounds {
    pub id: Option<i64>,
    pub language: String,
    pub start: Pos,
    pub end: Pos,
}

/// Borrowed span produced by [`scan_chunk_spans`] before deciding whether to materialize code/options.
struct ChunkSpan<'a> {
    control: &'a ContentControl,
    blocks: &'a Blocks,
    language: String,
    first: usize,
    last: usize,
    start: Pos,
    end: Pos,
}

/// Tracks reserved content-control ids across a batch of chunk insertions so each
/// chunk allocates a collision-free id without rescanning stories or custom properties.
#[derive(Clone, Debug)]
pub struct ControlIdAllocator {
    used: HashSet<i64>,
    max_id: Option<i64>,
    next_free: i64,
    custom_props_len: usize,
}

impl ControlIdAllocator {
    fn new(custom_props_len: usize) -> Self {
        Self { used: HashSet::new(), max_id: None, next_free: 1, custom_props_len }
    }

    fn insert_existing(&mut self, id: i64) {
        self.used.insert(id);
        self.max_id = Some(self.max_id.map_or(id, |m| m.max(id)));
    }

    fn absorb_custom_props(&mut self, custom_props: &[CustomProp]) {
        for prop in custom_props {
            crate::work::steps(1);
            if let Some(prefix) = prop.name.get(..CHUNK_PROP.len())
                && prefix.eq_ignore_ascii_case(CHUNK_PROP)
                && let Some(suffix) = prop.name.get(CHUNK_PROP.len()..)
                && let Ok(id) = suffix.parse::<i64>()
            {
                self.insert_existing(id);
            }
        }
    }

    /// Reserve `preferred` when positive and unused; otherwise allocate one past the highest
    /// reserved id (or the lowest free positive id when [`i64::MAX`] is already in use).
    ///
    /// Returns `(id, inserted)` where `inserted` is `true` when `id` was freshly reserved.
    fn reserve(&mut self, preferred: Option<i64>) -> (i64, bool) {
        if let Some(id) = preferred
            && id > 0
            && self.used.insert(id)
        {
            self.max_id = Some(self.max_id.map_or(id, |m| m.max(id)));
            return (id, true);
        }
        if let Some(next) = self.max_id.and_then(|m| m.checked_add(1))
            && next > 0
            && self.used.insert(next)
        {
            self.max_id = Some(next);
            return (next, true);
        }
        while self.next_free < i64::MAX && self.used.contains(&self.next_free) {
            self.next_free = self.next_free.saturating_add(1);
        }
        let free = self.next_free;
        let inserted = self.used.insert(free);
        self.max_id = Some(self.max_id.map_or(free, |m| m.max(free)));
        self.next_free = self.next_free.saturating_add(1);
        (free, inserted)
    }
}

fn validate_chunk_source(code: &str) -> Result<()> {
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
    Ok(())
}

fn block_list_may_have_chunks(bl: &Blocks) -> bool {
    bl.iter().any(|b| {
        matches!(&**b, Block::Para(p) if p.objects.iter().any(|o| {
            matches!(o, InlineObject::ControlStart { control } if control.block && control.tag.starts_with(CHUNK_TAG))
        }))
    })
}

fn span_is_para_range(bl: &Blocks, first: usize, last: usize) -> bool {
    last >= first && bl.get(first..=last).is_some_and(|slice| slice.iter().all(|b| matches!(&**b, Block::Para(_))))
}

fn make_pos(story: StoryRef, prefix: &[u32], block: usize, off: usize) -> Pos {
    let mut path = Vec::with_capacity(prefix.len() + 1);
    path.extend_from_slice(prefix);
    path.push(u32::try_from(block).unwrap_or(u32::MAX));
    Pos::new(story, Path(path), off)
}

struct MatchedChunkPair<'a> {
    control: &'a ContentControl,
    language: String,
    first: usize,
    last: usize,
    start_off: usize,
    end_off: usize,
}

fn matched_chunk_pair<'a>(bl: &'a Blocks, events: &[ControlEvent<'a>], open: usize, close: usize) -> Option<MatchedChunkPair<'a>> {
    let (Some(ControlEvent::Open { block: ob, off: Some(oo), control }), Some(ControlEvent::Close { block: cb, off: Some(co) })) =
        (events.get(open), events.get(close))
    else {
        return None;
    };
    if !control.block || !span_is_para_range(bl, *ob, *cb) {
        return None;
    }
    let language = tag_language(&control.tag)?;
    Some(MatchedChunkPair { control, language, first: *ob, last: *cb, start_off: *oo, end_off: *co })
}

fn scan_block_list_spans<'a>(story: StoryRef, bl: &'a Blocks, prefix: &mut Vec<u32>, depth: usize, out: &mut Vec<ChunkSpan<'a>>) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    if block_list_may_have_chunks(bl) {
        let events = crate::control::control_events(bl);
        let (pairs, _) = pair_events(&events);
        for (o, c, _) in pairs {
            if let Some(m) = matched_chunk_pair(bl, &events, o, c) {
                out.push(ChunkSpan {
                    control: m.control,
                    blocks: bl,
                    language: m.language,
                    first: m.first,
                    last: m.last,
                    start: make_pos(story, prefix, m.first, m.start_off),
                    end: make_pos(story, prefix, m.last, m.end_off),
                });
            }
        }
    }
    for (i, b) in bl.iter().enumerate() {
        if let Block::Table(t) = &**b {
            prefix.push(u32::try_from(i).unwrap_or(u32::MAX));
            for (r, row) in t.rows.iter().enumerate() {
                for (c, cell) in row.cells.iter().enumerate() {
                    prefix.push(u32::try_from(r).unwrap_or(u32::MAX));
                    prefix.push(u32::try_from(c).unwrap_or(u32::MAX));
                    scan_block_list_spans(story, &cell.blocks, prefix, depth + 1, out);
                    prefix.pop();
                    prefix.pop();
                }
            }
            prefix.pop();
        }
    }
}

fn scan_chunk_spans(doc: &Document, story: StoryRef) -> Vec<ChunkSpan<'_>> {
    let Some(bl) = doc.story(story) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    scan_block_list_spans(story, bl, &mut Vec::new(), 0, &mut out);
    out.sort_by(|a, b| (&a.start.path, a.start.off).cmp(&(&b.start.path, b.start.off)));
    out
}

fn block_list_has_chunk(bl: &Blocks, depth: usize) -> bool {
    if depth > MAX_SCAN_DEPTH {
        return false;
    }
    if block_list_may_have_chunks(bl) {
        let events = crate::control::control_events(bl);
        let (pairs, _) = pair_events(&events);
        if pairs.into_iter().any(|(o, c, _)| matched_chunk_pair(bl, &events, o, c).is_some()) {
            return true;
        }
    }
    bl.iter().any(
        |b| matches!(&**b, Block::Table(t) if t.rows.iter().any(|row| row.cells.iter().any(|cell| block_list_has_chunk(&cell.blocks, depth + 1)))),
    )
}

fn count_block_list_chunks(bl: &Blocks, depth: usize) -> usize {
    if depth > MAX_SCAN_DEPTH {
        return 0;
    }
    let mut total = 0usize;
    if block_list_may_have_chunks(bl) {
        let events = crate::control::control_events(bl);
        let (pairs, _) = pair_events(&events);
        total += pairs.into_iter().filter(|&(o, c, _)| matched_chunk_pair(bl, &events, o, c).is_some()).count();
    }
    for b in bl {
        if let Block::Table(t) = &**b {
            for row in &t.rows {
                for cell in &row.cells {
                    total += count_block_list_chunks(&cell.blocks, depth + 1);
                }
            }
        }
    }
    total
}

/// Join the paragraph lines of a chunk span into its source text with one allocation.
fn code_of(blocks: &Blocks, first: usize, last: usize) -> Option<String> {
    let slice = blocks.get(first..=last)?;
    let mut cap = slice.len().saturating_sub(1);
    for block in slice {
        let Block::Para(p) = block.as_ref() else { return None };
        cap = cap.saturating_add(p.text.len());
    }
    let mut out = String::with_capacity(cap);
    for (idx, block) in slice.iter().enumerate() {
        let Block::Para(p) = block.as_ref() else { return None };
        if idx > 0 {
            out.push('\n');
        }
        if p.text.contains(OBJ) {
            for c in p.text.chars().filter(|c| *c != OBJ) {
                out.push(c);
            }
        } else {
            out.push_str(&p.text);
        }
    }
    Some(out)
}

fn materialize_chunk_ref(doc: &Document, span: ChunkSpan<'_>) -> Option<ChunkRef> {
    let code = code_of(span.blocks, span.first, span.last)?;
    let (options, _) = options_of(doc, span.control);
    Some(ChunkRef { chunk: CodeChunk { id: span.control.id, language: span.language, code, options }, start: span.start, end: span.end })
}

fn surrounding_control_markers(para: &Paragraph, keep: impl Fn(usize) -> bool) -> Vec<InlineObject> {
    para.objects_with_offsets()
        .filter(|(off, obj)| keep(*off) && matches!(obj, InlineObject::ControlStart { .. } | InlineObject::ControlEnd))
        .map(|(_, obj)| obj.clone())
        .collect()
}

fn build_updated_chunk_blocks(
    code: &str,
    first_para: &Paragraph,
    last_para: &Paragraph,
    start_off: usize,
    end_off: usize,
    control: ContentControl,
) -> Result<Blocks> {
    let leading_controls = surrounding_control_markers(first_para, |off| off < start_off);
    let trailing_controls = surrounding_control_markers(last_para, |off| off > end_off);
    let trailing_section = last_para.section.clone();
    let first_ppr = &first_para.props;

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
    Ok(blocks)
}

impl Document {
    /// Whether `story` contains at least one valid block-level code chunk.
    ///
    /// Performs zero heap allocations when the story contains no chunk controls.
    pub fn has_chunks(&self, story: StoryRef) -> bool {
        self.story(story).is_some_and(|bl| block_list_has_chunk(bl, 0))
    }

    /// Whether any story in the document contains at least one valid block-level code chunk.
    pub fn has_any_chunks(&self) -> bool {
        self.has_chunks(StoryRef::Body) || self.parts.keys().any(|id| self.has_chunks(StoryRef::Part(*id)))
    }

    /// Number of valid block-level code chunks in `story`, without reading code or options.
    pub fn chunk_count(&self, story: StoryRef) -> usize {
        self.story(story).map_or(0, |bl| count_block_list_chunks(bl, 0))
    }

    /// Lightweight identity and position bounds for every chunk in `story`, in document order,
    /// without joining chunk source strings or parsing chunk options.
    pub fn chunk_bounds(&self, story: StoryRef) -> Vec<ChunkBounds> {
        scan_chunk_spans(self, story)
            .into_iter()
            .map(|span| ChunkBounds { id: span.control.id, language: span.language, start: span.start, end: span.end })
            .collect()
    }

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
        scan_chunk_spans(self, story).into_iter().filter_map(|span| materialize_chunk_ref(self, span)).collect()
    }

    /// Find and materialize only the chunk with control id `id` in `story`, avoiding code/option
    /// allocations for every other chunk in the story.
    pub fn chunk_ref_by_id(&self, story: StoryRef, id: i64) -> Option<ChunkRef> {
        let span = scan_chunk_spans(self, story).into_iter().find(|s| s.control.id == Some(id))?;
        materialize_chunk_ref(self, span)
    }

    /// Whether `range`'s chunk options could not be recovered (no valid `wordcraft.chunk.<id>`
    /// property and the tag carried neither complete options nor an `eval` flag).
    pub fn chunk_options_missing(&self, range: &ChunkRef) -> bool {
        self.chunk_options_missing_at(&range.start)
    }

    /// Whether the chunk control starting at `start` is missing complete options.
    pub fn chunk_options_missing_at(&self, start: &Pos) -> bool {
        let Some(para) = self.para(start.story, &start.path) else {
            return true;
        };
        let Some(InlineObject::ControlStart { control }) = para.object_at(start.off) else {
            return true;
        };
        options_of(self, control).1
    }

    /// Collect every content-control id currently in use across all stories and reserved
    /// `wordcraft.chunk.<id>` custom properties into an allocator for batch chunk insertion.
    ///
    /// Pass the allocator to [`Document::chunk_blocks_with_ids`] to avoid rescanning the
    /// document and its custom properties for every chunk. Do not add new controls or
    /// `wordcraft.chunk.*` custom properties to the document outside the allocator while
    /// reusing it.
    pub fn control_id_allocator(&self) -> ControlIdAllocator {
        let mut allocator = ControlIdAllocator::new(self.custom_props.len());
        let stories = std::iter::once(StoryRef::Body).chain(self.parts.keys().map(|k| StoryRef::Part(*k)));
        for story in stories {
            if let Some(blocks) = self.story(story) {
                walk_controls(blocks, &mut |c| {
                    if let Some(id) = c.id {
                        allocator.insert_existing(id);
                    }
                });
            }
        }
        allocator.absorb_custom_props(&self.custom_props);
        allocator
    }

    /// The next free content control id (one past the highest used, never [`i64::MAX`] twice).
    pub fn next_control_id(&self) -> i64 {
        self.control_id_allocator().reserve(None).0
    }

    /// The blocks that hold `chunk` as a block-level content control, ready to put in a story.
    ///
    /// The chunk gets a fresh control id unless its own is unused, and its options go into the
    /// custom property beside that id. The [`CHUNK_STYLE`] style is added when missing.
    pub fn chunk_blocks(&mut self, chunk: &CodeChunk) -> Result<Blocks> {
        let mut ids = self.control_id_allocator();
        self.chunk_blocks_with_ids(chunk, &mut ids)
    }

    /// Like [`Document::chunk_blocks`], reserving the chunk's control id from a pre-collected
    /// [`ControlIdAllocator`] so bulk imports do not rescan the document or custom properties
    /// per chunk.
    pub fn chunk_blocks_with_ids(&mut self, chunk: &CodeChunk, ids: &mut ControlIdAllocator) -> Result<Blocks> {
        validate_chunk_source(&chunk.code)?;
        let props_unchanged = self.custom_props.len() == ids.custom_props_len;
        if !props_unchanged {
            ids.absorb_custom_props(&self.custom_props);
        }
        let (id, inserted) = ids.reserve(chunk.id);
        let language = normalize_language(&chunk.language);
        let mut control = ContentControl::new(ControlKind::RichText);
        control.id = Some(id);
        control.tag = encode_control_tag(&language, &chunk.options);
        control.title = chunk.alias();
        control.block = true;

        let lines: Vec<&str> = chunk.code.split('\n').collect();
        let last = lines.len().saturating_sub(1);
        let mut blocks = Vec::with_capacity(lines.len());
        let mark = CharProps::default();
        for (i, line) in lines.iter().enumerate() {
            let mut p = Paragraph::with_text(line, CharProps::default()).styled(CHUNK_STYLE);
            if i == 0 {
                p.insert_object(0, InlineObject::ControlStart { control: Box::new(control.clone()) }, &mark)?;
            }
            if i == last {
                p.insert_object(p.len(), InlineObject::ControlEnd, &mark)?;
            }
            blocks.push(para_block(p));
        }

        if self.styles.get(CHUNK_STYLE).is_none() {
            Arc::make_mut(&mut self.styles).upsert(chunk_style());
        }
        let prop = chunk_prop_name(id);
        let json = serde_json::to_string(&chunk.options).unwrap_or_else(|_| "{}".to_string());
        if props_unchanged && inserted {
            self.custom_props.push(CustomProp { name: prop, kind: "lpwstr".into(), value: json, ..Default::default() });
            ids.custom_props_len = self.custom_props.len();
        } else {
            self.set_custom_prop(&prop, &json);
            ids.custom_props_len = self.custom_props.len();
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
        validate_chunk_source(code)?;

        let Some(found) = self.chunk_ref_by_id(StoryRef::Body, id) else {
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
        let has_full_options_prop = custom_prop_options(self, &control).is_some();
        control.tag = match decode_tag_options(&control.tag) {
            TagOptions::Missing => format!("{CHUNK_TAG}{effective_lang}"),
            TagOptions::Truncated(_) if !has_full_options_prop => encode_control_tag_with_truncated(effective_lang, &found.chunk.options, true),
            TagOptions::Complete(_) | TagOptions::Truncated(_) => encode_control_tag(effective_lang, &found.chunk.options),
        };
        let mut updated = CodeChunk::new(effective_lang, code);
        updated.options = found.chunk.options.clone();
        control.title = updated.alias();

        let blocks = build_updated_chunk_blocks(code, first_para, last_para, found.start.off, found.end.off, control)?;
        if self.styles.get(CHUNK_STYLE).is_none() {
            Arc::make_mut(&mut self.styles).upsert(chunk_style());
        }
        let container = self.container_mut(found.start.story, &found.start.path)?;
        container.splice(first..=last, blocks);
        Ok(true)
    }

    /// Rename (or clear the label of) chunk `id`, updating its `label` option and control
    /// tag/alias, and its `wordcraft.chunk.<id>` custom property when full options are known.
    /// Chunks with missing or truncated options stay marked incomplete (`!`) without writing a
    /// full custom property. Returns `Ok(true)` if the label changed, or `Err(DocError::Invalid)`
    /// if the chunk is at [`MAX_CHUNK_OPTIONS`] without an existing `label` or the label cannot
    /// fit in an incomplete control tag.
    pub fn set_chunk_label(&mut self, id: i64, label: Option<&str>) -> Result<bool> {
        let cleaned = label
            .map(|raw| raw.chars().filter(|c| !c.is_control()).collect::<String>())
            .map(|s| s.trim().chars().take(128).collect::<String>())
            .filter(|s| !s.is_empty());

        let Some(bounds) = self.chunk_bounds(StoryRef::Body).into_iter().find(|b| b.id == Some(id)) else {
            return Ok(false);
        };
        let Some(InlineObject::ControlStart { control }) =
            self.para(bounds.start.story, &bounds.start.path).and_then(|p| p.object_at(bounds.start.off))
        else {
            return Ok(false);
        };
        let (mut options, _) = options_of(self, control);
        if options.label().map(str::to_owned) == cleaned {
            return Ok(false);
        }

        let options_complete = custom_prop_options(self, control).is_some() || matches!(decode_tag_options(&control.tag), TagOptions::Complete(_));
        match cleaned {
            Some(ref value) => {
                if !options.contains_key("label") && options.len() >= MAX_CHUNK_OPTIONS {
                    return Err(DocError::Invalid(format!("chunk already has the maximum of {MAX_CHUNK_OPTIONS} options; cannot store label")));
                }
                options.set("label", value.clone());
            }
            None => {
                let _ = options.remove("label");
                let _ = options.remove("id");
            }
        }

        let norm_lang = normalize_language(&bounds.language);
        let effective_lang = if norm_lang.is_empty() { "text" } else { norm_lang.as_str() };
        let mut updated = CodeChunk::new(effective_lang, "");
        updated.options = options.clone();
        let new_tag = if options_complete {
            encode_control_tag(effective_lang, &options)
        } else {
            let tag = encode_control_tag_with_truncated(effective_lang, &options, true);
            if cleaned.is_some() && !matches!(decode_tag_options(&tag), TagOptions::Truncated(ref opts) if opts.get("label") == cleaned.as_deref()) {
                return Err(DocError::Invalid("chunk label does not fit in the control tag while options are incomplete".to_string()));
            }
            tag
        };
        let new_title = updated.alias();
        if options_complete {
            let prop = chunk_prop_name(id);
            let json = serde_json::to_string(&options).unwrap_or_else(|_| "{}".to_string());
            self.set_custom_prop(&prop, &json);
        }
        if let Some(control) = self.control_mut(&bounds.start) {
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
