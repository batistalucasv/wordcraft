//! Quarto / R Markdown (`.qmd`, `.Rmd`): Markdown whose fenced blocks may be executable code
//! chunks (` ```{r} `, ` ```{python} `) with Quarto chunk options.
//!
//! The text around the chunks is ordinary Markdown, so it is read by `craft-markdown` as it is.
//! Each chunk is replaced, for that read only, by a fenced block whose first line is a sentinel;
//! the sentinel's paragraph is then swapped for the chunk's content control ([`CodeChunk`],
//! keeping the source line for line). Writing goes the other way: Markdown for the blocks around
//! each chunk and a fence for the chunk itself.
//!
//! Options are kept as written (`echo: false`, `fig-cap: "A plot"`). Both of Quarto's ways of
//! spelling them are read: the chunk header (`{r, echo=FALSE, fig.cap="A plot"}`) and `#|` lines
//! at the top of the code; writing uses `#|`, which Quarto and current knitr both read.
//!
//! The file's YAML front matter (title, `format:`, bibliography…) has no place in a Word
//! document, so it is kept verbatim in the custom property [`FRONT_MATTER_PROP`] and written
//! back at the top.
//!
//! # Edge cases
//!
//! - **List-indented chunks**: A fenced chunk indented under a list item (`in_list && indent > 0`)
//!   belongs to that item. Replacing it with a top-level sentinel fence would end the list, so
//!   list-indented chunks are left as ordinary Markdown code blocks.
//! - **Non-body, table-cell, and overlapping chunks**: [`export`] rejects chunks stored in
//!   table cells (`path.0.len() != 1`), chunks in non-body stories (`StoryRef::Part(*)` such as
//!   headers/footers/comments), and overlapping or nested chunks with an explicit error rather
//!   than silently dropping their executable metadata.

mod options;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use wordcraft_doc::{Block, Blocks, ChunkBounds, CodeChunk, Document, StoryRef};

use options::{first_nonblank_is_hash_pipe, inspect_retained_yaml, option_key, option_name_ok, parse_info, strip_option_lines};

/// Custom property that holds a `.qmd`/`.Rmd` file's YAML front matter, delimiters included.
pub const FRONT_MATTER_PROP: &str = "wordcraft.quarto.frontMatter";

/// Chunks one file may declare; the rest stay ordinary code blocks.
const MAX_CHUNKS: usize = 4_096;
/// Longest chunk code kept as a chunk (the document model's own limit).
const MAX_CHUNK_BYTES: usize = wordcraft_doc::chunk::MAX_CHUNK_CODE_BYTES;

/// Import a Quarto or R Markdown file onto the default page.
pub fn import(text: &str) -> Document {
    import_on(text, &wordcraft_doc::SectionProps::default())
}

/// Import a Quarto or R Markdown file onto `page`.
pub fn import_on(text: &str, page: &wordcraft_doc::SectionProps) -> Document {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let text = text.replace("\r\n", "\n");
    let (front_matter, rest) = split_front_matter(&text);
    let (markdown, chunks) = prepare(rest);
    let mut doc = crate::markdown::import_on(&markdown, page);
    apply_chunks(&mut doc, chunks);
    if let Some(front_matter) = front_matter {
        doc.set_custom_prop(FRONT_MATTER_PROP, front_matter);
    }
    doc.ensure_nonempty();
    doc
}

/// Export `doc` as a Quarto document. A `.Rmd` file is the same text.
///
/// Returns an error if `doc` contains code chunks that cannot be written as top-level Quarto
/// fenced chunks without losing their executable form and options (such as chunks inside table
/// cells, overlapping chunks, or chunks outside the document body).
pub fn export(doc: &Document) -> Result<String, String> {
    let has_body_chunks = doc.has_chunks(StoryRef::Body);
    let bounds = if has_body_chunks {
        let b = doc.chunk_bounds(StoryRef::Body);
        let table_cell_chunks = b.iter().filter(|r| r.start.path.0.len() != 1 || r.end.path.0.len() != 1).count();
        if table_cell_chunks > 0 {
            return Err(table_cell_chunk_error(table_cell_chunks));
        }
        b
    } else {
        Vec::new()
    };

    let non_body_chunks: usize = doc.parts.keys().map(|id| doc.chunk_count(StoryRef::Part(*id))).sum();
    if non_body_chunks > 0 {
        return Err(non_body_chunk_error(non_body_chunks));
    }

    let mut out = String::with_capacity(doc_text_len(doc));
    write_front_matter(&mut out, doc);

    if !has_body_chunks {
        if !doc.body.is_empty() {
            let text = crate::markdown::export(doc);
            let text = text.trim_end_matches('\n');
            if !text.is_empty() {
                out.push_str(text);
                out.push_str("\n\n");
            }
        }
        return Ok(out);
    }

    validate_top_level_bounds(&bounds, doc.body.len())?;
    let refs = doc.chunk_refs(StoryRef::Body);
    let mut starts: HashMap<usize, &wordcraft_doc::ChunkRef> = HashMap::with_capacity(refs.len());
    for r in &refs {
        starts.insert(r.start.path.last(), r);
    }

    let mut shell = doc.clone();
    shell.body.clear();
    shell.custom_props.clear();
    let mut run: Blocks = Vec::new();
    let mut i = 0;
    while i < doc.body.len() {
        match starts.get(&i) {
            Some(r) => {
                flush(&mut out, &mut shell, &mut run);
                push_chunk(&mut out, &r.chunk);
                i = r.end.path.last().saturating_add(1);
            }
            None => {
                if let Some(block) = doc.body.get(i) {
                    run.push(block.clone());
                }
                i += 1;
            }
        }
    }
    flush(&mut out, &mut shell, &mut run);
    Ok(out)
}

fn table_cell_chunk_error(count: usize) -> String {
    if count == 1 {
        "Quarto export cannot preserve 1 code chunk inside a table cell: move it to the document body or save as .docx".to_string()
    } else {
        format!("Quarto export cannot preserve {count} code chunks inside table cells: move them to the document body or save as .docx")
    }
}

fn non_body_chunk_error(count: usize) -> String {
    if count == 1 {
        "Quarto export cannot preserve 1 code chunk outside the document body: move it to the document body or save as .docx".to_string()
    } else {
        format!("Quarto export cannot preserve {count} code chunks outside the document body: move them to the document body or save as .docx")
    }
}

fn validate_top_level_bounds(bounds: &[ChunkBounds], body_len: usize) -> Result<(), String> {
    let mut next_block = 0usize;
    for b in bounds {
        let start = b.start.path.last();
        let end = b.end.path.last();
        if start < next_block || end < start || end >= body_len {
            return Err(
                "Quarto export cannot preserve overlapping or nested code chunks: separate them into distinct top-level blocks or save as .docx"
                    .to_string(),
            );
        }
        next_block = end.saturating_add(1);
    }
    Ok(())
}

fn write_front_matter(out: &mut String, doc: &Document) {
    let Some(front_matter) = doc.custom_prop(FRONT_MATTER_PROP) else { return };
    let front_matter = front_matter.trim();
    if front_matter.is_empty() {
        return;
    }
    out.push_str(front_matter);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
}

/// A rough size for the output, so it is not reallocated as it grows.
fn doc_text_len(doc: &Document) -> usize {
    doc.body.iter().fold(0usize, |n, b| n.saturating_add(b.as_para().map_or(0, |p| p.text.len().saturating_add(2))))
}

/// Write `run` (the blocks before a chunk, or after the last) as Markdown.
fn flush(out: &mut String, shell: &mut Document, run: &mut Blocks) {
    if run.is_empty() {
        return;
    }
    shell.body = std::mem::take(run);
    let text = crate::markdown::export(shell);
    shell.body.clear();
    let text = text.trim_end_matches('\n');
    if text.is_empty() {
        return;
    }
    out.push_str(text);
    out.push_str("\n\n");
}

/// Write one chunk as a Quarto fenced block.
fn push_chunk(out: &mut String, chunk: &CodeChunk) {
    let fence = fence_marker(&chunk.code);
    out.push_str(&fence);
    out.push('{');
    out.push_str(if chunk.language.is_empty() { "text" } else { chunk.language.as_str() });
    out.push('}');
    out.push('\n');
    let retained = inspect_retained_yaml(&chunk.code, &chunk.options);
    for (key, value) in chunk.options.iter() {
        if !option_name_ok(key) {
            continue;
        }
        if retained.as_ref().is_some_and(|r| r.top_level_keys.contains(&option_key(key))) {
            continue;
        }
        out.push_str("#| ");
        out.push_str(key);
        out.push(':');
        let value: String = value.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
        let value = value.trim();
        if !value.is_empty() {
            out.push(' ');
            out.push_str(value);
        }
        out.push('\n');
    }
    if retained.is_none() && first_nonblank_is_hash_pipe(chunk.code.split('\n')) {
        out.push('\n');
    }
    out.push_str(&chunk.code);
    if !chunk.code.is_empty() {
        out.push('\n');
    }
    out.push_str(&fence);
    out.push_str("\n\n");
}

/// A fence longer than any run of the character used, so code cannot close it early.
fn fence_marker(code: &str) -> String {
    let backticks = longest_run(code, '`');
    let tildes = longest_run(code, '~');
    let (ch, longest) = if backticks <= tildes { ('`', backticks) } else { ('~', tildes) };
    ch.to_string().repeat(longest.saturating_add(1).max(3))
}

/// The longest run of `ch` in `code`.
fn longest_run(code: &str, ch: char) -> usize {
    let mut longest = 0usize;
    let mut run = 0usize;
    for c in code.chars() {
        if c == ch {
            run = run.saturating_add(1);
            if run > longest {
                longest = run;
            }
        } else {
            run = 0;
        }
    }
    longest
}

/// The YAML front matter (delimiters included) and the rest of the file.
fn split_front_matter(text: &str) -> (Option<&str>, &str) {
    let mut lines = text.split_inclusive('\n');
    let Some(first) = lines.next() else { return (None, text) };
    if first.trim_end_matches(['\r', '\n']) != "---" {
        return (None, text);
    }
    let mut pos = first.len();
    for line in lines {
        pos += line.len();
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" || trimmed == "..." {
            return (Some(&text[..pos]), &text[pos..]);
        }
    }
    (None, text)
}

/// A chunk ready to be put back into the parsed document, by its sentinel paragraph.
struct Pending {
    sentinel: String,
    chunk: CodeChunk,
}

fn push_lines(out: &mut String, lines: &[&str]) {
    for line in lines {
        out.push_str(line);
    }
}

fn update_list_state(in_list: &mut bool, prev_blank: &mut bool, line: &str, indent: usize, rest: &str) {
    let blank = line.trim().is_empty();
    if !blank {
        if indent < 4 && is_list_marker(rest) {
            *in_list = true;
        } else if indent == 0 && (*prev_blank || fence_open(line).is_some()) {
            // An unindented line ends the list after a blank line; a fence ends it at once,
            // since a fence is never a lazy continuation line.
            *in_list = false;
        }
    }
    *prev_blank = blank;
}

fn read_chunk_body(lines: &[&str], start: usize, indent: usize, ch: char, len: usize) -> Option<(usize, String)> {
    let mut j = start;
    let mut body = String::new();
    while let Some(line) = lines.get(j) {
        if closes(line, ch, len) {
            return Some((j, body));
        }
        // CommonMark removes up to the opening fence's indentation from each content line.
        body.push_str(strip_spaces(line, indent));
        j += 1;
    }
    None
}

/// Replace each Quarto chunk with a sentinel fenced block and collect the chunks.
fn prepare(text: &str) -> (String, Vec<Pending>) {
    let mut out = String::with_capacity(text.len());
    let mut chunks: Vec<Pending> = Vec::new();
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut i = 0;
    let mut in_fence: Option<(char, usize)> = None;
    // Whether a list item may still be open. A chunk fence indented under a list item belongs to
    // that item; replacing it with a sentinel fence at column zero would end the list, so such a
    // fence is left as written. Unsure cases count as open: the fence then stays ordinary code.
    let mut in_list = false;
    let mut prev_blank = true;
    let tag = format!("{:x}", (text.as_ptr() as usize) ^ text.len());

    while i < lines.len() {
        let line = lines[i];
        if let Some((f_ch, f_len)) = in_fence {
            out.push_str(line);
            if closes(line, f_ch, f_len) {
                in_fence = None;
            }
            prev_blank = false;
            i += 1;
            continue;
        }

        let (indent, rest) = leading_indent_cols(line);
        update_list_state(&mut in_list, &mut prev_blank, line, indent, rest);
        let open = if in_list && indent > 0 { None } else { chunk_open(line) };
        let Some((ch, len, info)) = open else {
            if let Some(fence) = fence_open(line) {
                in_fence = Some(fence);
            }
            out.push_str(line);
            i += 1;
            continue;
        };

        let Some((close_idx, body)) = read_chunk_body(&lines, i + 1, indent, ch, len) else {
            // An unclosed fence stays as it was written: append the entire remaining suffix unchanged.
            push_lines(&mut out, &lines[i..]);
            break;
        };
        let body = body.strip_suffix('\n').unwrap_or(&body);
        if chunks.len() >= MAX_CHUNKS || body.len() > MAX_CHUNK_BYTES {
            push_lines(&mut out, &lines[i..=close_idx]);
            i = close_idx + 1;
            continue;
        }
        let (language, options) = parse_info(info);
        // Only the languages the kernel runs are chunks; a `{mermaid}` or `{julia}` block stays an
        // ordinary code block, keepable and readable, not a control that cannot run.
        if !matches!(language.as_str(), "r" | "python") {
            push_lines(&mut out, &lines[i..=close_idx]);
            i = close_idx + 1;
            continue;
        }
        let (code, options) = strip_option_lines(body, options);
        let sentinel = format!("wordcraft-chunk-sentinel-{tag}-{}", chunks.len());
        let fence = fence_marker(&code);
        out.push_str(&fence);
        out.push('\n');
        out.push_str(&sentinel);
        out.push('\n');
        out.push_str(&code);
        out.push('\n');
        out.push_str(&fence);
        out.push('\n');
        let mut chunk = CodeChunk::new(language, code);
        chunk.options = options;
        chunks.push(Pending { sentinel, chunk });
        i = close_idx + 1;
    }
    (out, chunks)
}

enum SentinelMatch {
    None,
    TooLarge,
    Matched(usize),
}

fn classify_sentinel(old: &VecDeque<Arc<Block>>, by_sentinel: &HashMap<String, CodeChunk>) -> SentinelMatch {
    let Some(text) = old.front().and_then(|b| b.as_para()).map(|p| p.text.as_str()) else {
        return SentinelMatch::None;
    };
    let Some(chunk) = by_sentinel.get(text) else {
        return SentinelMatch::None;
    };
    let line_count = chunk.code.bytes().filter(|b| *b == b'\n').count() + 1;
    if chunk.code.len() > MAX_CHUNK_BYTES || line_count > wordcraft_doc::chunk::MAX_CHUNK_LINES {
        return SentinelMatch::TooLarge;
    }
    if 1 + line_count > old.len() {
        return SentinelMatch::None;
    }
    let matches_lines =
        (1..=line_count).zip(chunk.code.split('\n')).all(|(idx, line)| old.get(idx).and_then(|b| b.as_para()).is_some_and(|p| p.text == line));
    if matches_lines { SentinelMatch::Matched(line_count) } else { SentinelMatch::None }
}

/// Swap each sentinel paragraph for its chunk's content control, moving non-chunk blocks directly.
fn apply_chunks(doc: &mut Document, mut chunks: Vec<Pending>) {
    if chunks.is_empty() {
        return;
    }
    let mut by_sentinel: HashMap<String, CodeChunk> = HashMap::with_capacity(chunks.len());
    for pending in chunks.drain(..) {
        by_sentinel.insert(pending.sentinel, pending.chunk);
    }
    let mut ids = doc.control_id_allocator();
    let mut old = VecDeque::from(std::mem::take(&mut doc.body));
    doc.body = Vec::with_capacity(old.len());

    while !old.is_empty() {
        match classify_sentinel(&old, &by_sentinel) {
            SentinelMatch::TooLarge => {
                // Code the model refuses (larger than a chunk may be): drop the sentinel only, so
                // the code stays as ordinary paragraphs.
                if let Some(sentinel_block) = old.pop_front()
                    && let Some(p) = sentinel_block.as_para()
                {
                    by_sentinel.remove(&p.text);
                }
            }
            SentinelMatch::Matched(line_count) => {
                let Some(sentinel_block) = old.pop_front() else { break };
                let Some(chunk) = sentinel_block.as_para().and_then(|p| by_sentinel.remove(&p.text)) else {
                    doc.body.push(sentinel_block);
                    continue;
                };
                if let Ok(blocks) = doc.chunk_blocks_with_ids(&chunk, &mut ids) {
                    old.drain(..line_count);
                    doc.body.extend(blocks);
                }
                // On Err(_), sentinel_block has already been popped and dropped, leaving the code as
                // ordinary paragraphs.
            }
            SentinelMatch::None => {
                if let Some(block) = old.pop_front() {
                    doc.body.push(block);
                }
            }
        }
    }
}

/// Count leading indentation in columns: spaces add 1, tabs advance to the next 4-space tab stop.
fn leading_indent_cols(line: &str) -> (usize, &str) {
    let mut cols = 0usize;
    let mut rest_idx = 0;
    for (i, c) in line.char_indices() {
        match c {
            ' ' => {
                cols = cols.saturating_add(1);
                rest_idx = i + 1;
            }
            '\t' => {
                cols = (cols + 4) & !3;
                rest_idx = i + 1;
            }
            _ => break,
        }
    }
    (cols, line.get(rest_idx..).unwrap_or(""))
}

/// `line` without up to `n` leading spaces.
fn strip_spaces(line: &str, n: usize) -> &str {
    let spaces = line.bytes().take(n).take_while(|b| *b == b' ').count();
    line.get(spaces..).unwrap_or(line)
}

/// Whether `rest` (a line after its indentation) starts with a Markdown list item marker:
/// `-`, `*` or `+`, or one to nine digits and `.` or `)`, followed by a space, a tab or the end.
fn is_list_marker(rest: &str) -> bool {
    let rest = rest.trim_end_matches(['\r', '\n']);
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let after = match rest.as_bytes().first() {
        Some(b'-' | b'*' | b'+') => 1,
        Some(_) if (1..=9).contains(&digits) && matches!(rest.as_bytes().get(digits), Some(b'.' | b')')) => digits + 1,
        _ => return false,
    };
    matches!(rest.as_bytes().get(after), None | Some(b' ' | b'\t'))
}

/// Whether `line` opens an executable chunk: a fence indented less than four columns with
/// `{language …}`.
fn chunk_open(line: &str) -> Option<(char, usize, &str)> {
    let (indent, t) = leading_indent_cols(line);
    if indent >= 4 {
        return None;
    }
    let ch = t.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let len = t.chars().take_while(|c| *c == ch).count();
    if len < 3 {
        return None;
    }
    let rest = t.get(len..)?;
    if ch == '`' && rest.contains('`') {
        return None;
    }
    let inner = rest.strip_prefix('{')?;
    let end = find_chunk_header_end(inner)?;
    let info = inner.get(..end)?.trim();
    if info.is_empty() || info.starts_with('.') || info.chars().next().is_some_and(|c| c.is_ascii_whitespace()) {
        return None;
    }
    // A closing fence has nothing after its run; an info string with braces is a chunk header.
    Some((ch, len, info))
}

/// Advance quote/backslash-escape state for header scanning; returns `true` when `ch` was consumed inside quotes.
pub(super) fn step_quoted(quote: &mut Option<char>, escaped: &mut bool, ch: char) -> bool {
    let Some(q) = *quote else { return false };
    if *escaped {
        *escaped = false;
    } else if ch == '\\' {
        *escaped = true;
    } else if ch == q {
        *quote = None;
    }
    true
}

/// Find the matching `}` for `{` at the start of a chunk header, respecting quotes and nested delimiters.
fn find_chunk_header_end(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut brace_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;

    for (idx, ch) in s.char_indices() {
        if step_quoted(&mut quote, &mut escaped, ch) {
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '{' => brace_depth = brace_depth.saturating_add(1),
            '}' => {
                if brace_depth > 0 {
                    brace_depth -= 1;
                } else if paren_depth == 0 && bracket_depth == 0 {
                    return Some(idx);
                }
            }
            '(' => paren_depth = paren_depth.saturating_add(1),
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '[' => bracket_depth = bracket_depth.saturating_add(1),
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            _ => {}
        }
    }
    None
}

/// Whether `line` opens an ordinary Markdown code fence (at least 3 backticks or tildes).
///
/// Per CommonMark, a backtick fence's info string cannot contain backticks; otherwise an inline
/// code span at the start of a line (such as `` ```code``` ``) would be mistaken for a fence.
fn fence_open(line: &str) -> Option<(char, usize)> {
    let (indent, t) = leading_indent_cols(line);
    if indent >= 4 {
        return None;
    }
    let ch = t.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let len = t.chars().take_while(|c| *c == ch).count();
    if len < 3 {
        return None;
    }
    if ch == '`' && t.get(len..).is_some_and(|info| info.contains('`')) {
        return None;
    }
    Some((ch, len))
}

/// Whether `line` closes a fence opened with `ch` repeated at least `len` times.
fn closes(line: &str, ch: char, len: usize) -> bool {
    let (indent, t) = leading_indent_cols(line);
    if indent >= 4 {
        return false;
    }
    let n = t.chars().take_while(|c| *c == ch).count();
    n >= len && t.chars().skip(n).all(|c| c.is_whitespace())
}
