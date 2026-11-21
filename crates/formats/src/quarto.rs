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

use std::collections::{HashMap, HashSet};

use wordcraft_doc::{Blocks, ChunkOptions, CodeChunk, Document, StoryRef};

/// Custom property that holds a `.qmd`/`.Rmd` file's YAML front matter, delimiters included.
pub const FRONT_MATTER_PROP: &str = "wordcraft.quarto.frontMatter";

/// Chunks one file may declare; the rest stay ordinary code blocks.
const MAX_CHUNKS: usize = 4_096;
/// Longest chunk code kept as a chunk (the document model's own limit).
const MAX_CHUNK_BYTES: usize = wordcraft_doc::chunk::MAX_CHUNK_CODE_BYTES;
/// Longest `#|` option line read, in bytes; longer lines stay in the code.
const MAX_OPTION_LINE: usize = 8 * 1024;
/// Most options the document model keeps per chunk.
const MAX_CHUNK_OPTIONS: usize = wordcraft_doc::chunk::MAX_CHUNK_OPTIONS;
/// Longest option name or value the document model keeps, in characters.
const MAX_CHUNK_OPTION_CHARS: usize = wordcraft_doc::chunk::MAX_CHUNK_OPTION_CHARS;

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
pub fn export(doc: &Document) -> String {
    let mut out = String::with_capacity(doc_text_len(doc));
    if let Some(front_matter) = doc.custom_prop(FRONT_MATTER_PROP) {
        let front_matter = front_matter.trim();
        if !front_matter.is_empty() {
            out.push_str(front_matter);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
        }
    }
    let refs = doc.chunk_refs(StoryRef::Body);
    let starts: HashMap<usize, &wordcraft_doc::ChunkRef> =
        refs.iter().filter(|r| r.start.path.0.len() == 1 && r.end.path.0.len() == 1).map(|r| (r.start.path.last(), r)).collect();
    let mut shell = doc.clone();
    shell.body.clear();
    shell.custom_props.clear();
    let mut run: Blocks = Vec::new();
    let mut i = 0;
    while i < doc.body.len() {
        match starts.get(&i) {
            Some(r) if r.end.path.last() >= i => {
                flush(&mut out, &mut shell, &mut run);
                push_chunk(&mut out, &r.chunk);
                i = r.end.path.last().saturating_add(1);
            }
            _ => {
                if let Some(block) = doc.body.get(i) {
                    run.push(block.clone());
                }
                i += 1;
            }
        }
    }
    flush(&mut out, &mut shell, &mut run);
    out
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
    let first_line = chunk.code.lines().next().unwrap_or("");
    if first_line.trim_start().starts_with("#|") && retained.is_none() {
        out.push('\n');
    }
    out.push_str(&chunk.code);
    if !chunk.code.is_empty() {
        out.push('\n');
    }
    out.push_str(&fence);
    out.push_str("\n\n");
}

/// Top-level option entry seen inside a retained leading `#|` YAML block.
enum RetainedYamlEntry {
    /// Supported single-line scalar option (`key: value`), kept in `ChunkOptions`.
    Scalar(String, String),
    /// Complex or oversized YAML option (`fig-cap: |`, list, nested mapping, etc.) whose key
    /// lives verbatim in `code` and supersedes any header option of the same name.
    Complex(String),
}

/// Summary of a leading `#|` YAML block that contains complex YAML and therefore stays
/// verbatim in `chunk.code`.
struct RetainedYamlBlock {
    entries: Vec<RetainedYamlEntry>,
    top_level_keys: HashSet<String>,
}

enum YamlScanState {
    TopLevel,
    BlockScalar,
    NestedValue,
    DoubleQuoted,
    SingleQuoted,
    Flow { depth: usize, quote: Option<char> },
}

/// Strip the `#|` cell-option prefix and at most one delimiter space, returning the YAML line.
fn hash_pipe_yaml(line: &str) -> Option<&str> {
    let after_hash = line.trim_start().strip_prefix("#|")?;
    Some(after_hash.strip_prefix(' ').unwrap_or(after_hash))
}

/// Whether a YAML line starts a block sequence item (`-`, `- ...`, or `-\t...`).
fn is_yaml_sequence_item(yaml: &str) -> bool {
    yaml == "-" || yaml.starts_with("- ") || yaml.starts_with("-\t")
}

/// Return the YAML body of the next non-blank, non-comment `#|` line at or after `start`.
fn next_nonblank_noncomment_hash_pipe<'a>(lines: &[&'a str], start: usize) -> Option<&'a str> {
    for &line in lines.get(start..)? {
        let yaml = hash_pipe_yaml(line)?;
        let trimmed = yaml.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            return Some(yaml);
        }
    }
    None
}

/// Scan a double-quoted YAML segment starting inside `"..."`.
/// Returns `true` if an unescaped closing `"` is found on this line.
fn scan_double_quoted(s: &str) -> bool {
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            return true;
        }
    }
    false
}

/// Scan a single-quoted YAML segment starting inside `'...'`.
/// In YAML 1.2, `''` is an escaped single quote; a lone `'` closes the string.
fn scan_single_quoted(s: &str) -> bool {
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' {
            if chars.peek() == Some(&'\'') {
                chars.next();
            } else {
                return true;
            }
        }
    }
    false
}

/// Advance flow-collection nesting state across `s`, returning `(depth, quote)` at end of line
/// or `(0, None)` as soon as the flow collection closes.
fn scan_flow(s: &str, mut depth: usize, mut quote: Option<char>) -> (usize, Option<char>) {
    let mut escaped = false;
    let mut prev: Option<char> = None;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some('"') => {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    quote = None;
                }
            }
            Some('\'') => {
                if c == '\'' {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                    } else {
                        quote = None;
                    }
                }
            }
            _ => match c {
                '"' | '\'' => quote = Some(c),
                '[' | '{' => depth = depth.saturating_add(1),
                ']' | '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return (0, None);
                    }
                }
                '#' if prev.is_none_or(|p| matches!(p, ' ' | '\t')) => break,
                _ => {}
            },
        }
        prev = Some(c);
    }
    (depth, quote)
}

/// If `value` begins a multiline YAML construct (block scalar, nested value, unclosed quote,
/// unclosed flow collection, or a plain scalar followed by indented continuation lines), return
/// the scanner state needed to consume its remaining lines.
fn multiline_value_state(value: &str, lines: &[&str], next_idx: usize) -> Option<YamlScanState> {
    let scalar_val = wordcraft_doc::chunk::strip_yaml_comment(value);
    if scalar_val.starts_with('|') || scalar_val.starts_with('>') {
        return Some(YamlScanState::BlockScalar);
    }
    if scalar_val.is_empty() {
        let starts_nested = next_nonblank_noncomment_hash_pipe(lines, next_idx)
            .is_some_and(|next| next.starts_with(' ') || next.starts_with('\t') || is_yaml_sequence_item(next));
        return starts_nested.then_some(YamlScanState::NestedValue);
    }
    if let Some(rest) = value.strip_prefix('"') {
        if !scan_double_quoted(rest) {
            return Some(YamlScanState::DoubleQuoted);
        }
    } else if let Some(rest) = value.strip_prefix('\'') {
        if !scan_single_quoted(rest) {
            return Some(YamlScanState::SingleQuoted);
        }
    } else if value.starts_with('[') || value.starts_with('{') {
        let (depth, quote) = scan_flow(value, 0, None);
        if depth > 0 || quote.is_some() {
            return Some(YamlScanState::Flow { depth, quote });
        }
    }
    if next_nonblank_noncomment_hash_pipe(lines, next_idx).is_some_and(|next| next.starts_with(' ') || next.starts_with('\t')) {
        return Some(YamlScanState::BlockScalar);
    }
    None
}

/// Inspect leading `#|` lines for complex YAML (block scalars, sequences, nested mappings,
/// multiline quoted scalars/flow collections) or options that exceed the document model's limits
/// (`MAX_OPTION_LINE`, `MAX_CHUNK_OPTION_CHARS`, or `MAX_CHUNK_OPTIONS` combined with `existing`).
/// When any such entry is present, returns the top-level scalar options and top-level keys so the
/// block can stay verbatim in `code` while WordCraft still sees scalar options like `eval: false`
/// and export avoids duplicating them.
fn inspect_retained_yaml(text: &str, existing: &ChunkOptions) -> Option<RetainedYamlBlock> {
    let lines: Vec<&str> = text.lines().take_while(|l| l.trim_start().starts_with("#|")).collect();
    if lines.is_empty() {
        return None;
    }

    let mut entries = Vec::new();
    let mut top_level_keys = HashSet::new();
    let mut must_retain = false;
    let mut state = YamlScanState::TopLevel;

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let Some(yaml) = hash_pipe_yaml(line) else {
            break;
        };
        let is_blank = yaml.trim().is_empty();
        let is_indented = !is_blank && (yaml.starts_with(' ') || yaml.starts_with('\t'));

        match state {
            YamlScanState::BlockScalar => {
                if is_blank || is_indented {
                    i += 1;
                    continue;
                }
                state = YamlScanState::TopLevel;
                continue;
            }
            YamlScanState::NestedValue => {
                if is_blank || is_indented || yaml.starts_with('#') || is_yaml_sequence_item(yaml) {
                    i += 1;
                    continue;
                }
                state = YamlScanState::TopLevel;
                continue;
            }
            YamlScanState::DoubleQuoted => {
                if scan_double_quoted(yaml) {
                    state = YamlScanState::TopLevel;
                }
                i += 1;
                continue;
            }
            YamlScanState::SingleQuoted => {
                if scan_single_quoted(yaml) {
                    state = YamlScanState::TopLevel;
                }
                i += 1;
                continue;
            }
            YamlScanState::Flow { depth, quote } => {
                let (next_depth, next_quote) = scan_flow(yaml, depth, quote);
                state = if next_depth == 0 && next_quote.is_none() {
                    YamlScanState::TopLevel
                } else {
                    YamlScanState::Flow { depth: next_depth, quote: next_quote }
                };
                i += 1;
                continue;
            }
            YamlScanState::TopLevel => {}
        }

        if is_blank || yaml.starts_with('#') {
            i += 1;
            continue;
        }
        if is_indented || is_yaml_sequence_item(yaml) {
            must_retain = true;
            i += 1;
            continue;
        }

        let Some((raw_key, raw_value)) = yaml.split_once(':') else {
            break;
        };
        // Per YAML 1.2, a mapping `:` is followed by separation whitespace or end-of-line.
        if !raw_value.is_empty() && !raw_value.starts_with([' ', '\t']) {
            break;
        }
        let raw_key = raw_key.trim();
        let value = raw_value.trim();
        if !option_name_ok(raw_key) {
            if raw_key.starts_with('"') || raw_key.starts_with('\'') {
                must_retain = true;
                if let Some(next_state) = multiline_value_state(value, &lines, i + 1) {
                    state = next_state;
                }
                i += 1;
                continue;
            }
            break;
        }

        let key = option_key(raw_key);
        if key.is_empty() {
            break;
        }
        top_level_keys.insert(key.clone());

        if let Some(next_state) = multiline_value_state(value, &lines, i + 1) {
            must_retain = true;
            entries.push(RetainedYamlEntry::Complex(key));
            state = next_state;
        } else if line.len() > MAX_OPTION_LINE || key.chars().count() > MAX_CHUNK_OPTION_CHARS || value.chars().count() > MAX_CHUNK_OPTION_CHARS {
            must_retain = true;
            entries.push(RetainedYamlEntry::Complex(key));
        } else if wordcraft_doc::chunk::strip_yaml_comment(value).is_empty() {
            entries.push(RetainedYamlEntry::Complex(key));
        } else {
            entries.push(RetainedYamlEntry::Scalar(key, value.to_string()));
        }
        i += 1;
    }

    let extra_keys = top_level_keys.iter().filter(|k| !existing.contains_key(k)).count();
    if existing.len().saturating_add(extra_keys) > MAX_CHUNK_OPTIONS {
        must_retain = true;
    }

    must_retain.then_some(RetainedYamlBlock { entries, top_level_keys })
}

/// Whether an option name can be written as a `#|` line (names come from files).
fn option_name_ok(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) && !name.starts_with('-')
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

/// Replace each Quarto chunk with a sentinel fenced block and collect the chunks.
fn prepare(text: &str) -> (String, Vec<Pending>) {
    let mut out = String::with_capacity(text.len());
    let mut chunks: Vec<Pending> = Vec::new();
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut i = 0;
    let mut in_fence: Option<(char, usize)> = None;
    let tag = format!("{:x}", (text.as_ptr() as usize) ^ text.len());

    while i < lines.len() {
        let line = lines[i];
        if let Some((f_ch, f_len)) = in_fence {
            out.push_str(line);
            if closes(line, f_ch, f_len) {
                in_fence = None;
            }
            i += 1;
            continue;
        }

        let Some((ch, len, info)) = chunk_open(line) else {
            if let Some((f_ch, f_len)) = fence_open(line) {
                in_fence = Some((f_ch, f_len));
            }
            out.push_str(line);
            i += 1;
            continue;
        };

        let mut j = i + 1;
        let mut body = String::new();
        while let Some(line) = lines.get(j) {
            if closes(line, ch, len) {
                break;
            }
            body.push_str(line);
            j += 1;
        }
        if j >= lines.len() {
            // An unclosed fence stays as it was written: append the entire remaining suffix unchanged.
            for line in &lines[i..] {
                out.push_str(line);
            }
            break;
        }
        let body = body.strip_suffix('\n').unwrap_or(&body);
        if chunks.len() >= MAX_CHUNKS || body.len() > MAX_CHUNK_BYTES {
            out.push_str(lines[i]);
            for line in lines.get(i + 1..=j).into_iter().flatten() {
                out.push_str(line);
            }
            i = j + 1;
            continue;
        }
        let (language, options) = parse_info(info);
        // Only the languages the kernel runs are chunks; a `{mermaid}` or `{julia}` block stays an
        // ordinary code block, keepable and readable, not a control that cannot run.
        if !matches!(language.as_str(), "r" | "python") {
            out.push_str(lines[i]);
            for line in lines.get(i + 1..=j).into_iter().flatten() {
                out.push_str(line);
            }
            i = j + 1;
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
        i = j + 1;
    }
    (out, chunks)
}

/// Swap each sentinel paragraph for its chunk's content control.
fn apply_chunks(doc: &mut Document, mut chunks: Vec<Pending>) {
    if chunks.is_empty() {
        return;
    }
    let mut by_sentinel: HashMap<String, CodeChunk> = HashMap::new();
    for pending in chunks.drain(..) {
        by_sentinel.insert(pending.sentinel, pending.chunk);
    }
    let mut ids = doc.control_id_allocator();
    let old = std::mem::take(&mut doc.body);
    doc.body = Vec::with_capacity(old.len());
    let mut i = 0;
    while i < old.len() {
        let sentinel_key = old.get(i).and_then(|b| b.as_para()).map(|p| p.text.clone());
        let (matches, too_large) = if let Some(chunk) = sentinel_key.as_deref().and_then(|s| by_sentinel.get(s)) {
            let line_count = chunk.code.bytes().filter(|b| *b == b'\n').count() + 1;
            if chunk.code.len() > MAX_CHUNK_BYTES || line_count > wordcraft_doc::chunk::MAX_CHUNK_LINES {
                (None, true)
            } else {
                let end = i.saturating_add(1).saturating_add(line_count);
                if end <= old.len()
                    && old
                        .get(i + 1..end)
                        .into_iter()
                        .flatten()
                        .zip(chunk.code.split('\n'))
                        .all(|(b, line)| b.as_para().is_some_and(|p| p.text == line))
                {
                    (Some(end), false)
                } else {
                    (None, false)
                }
            }
        } else {
            (None, false)
        };
        if too_large {
            if let Some(s) = sentinel_key.as_deref() {
                by_sentinel.remove(s);
            }
            // Code the model refuses (larger than a chunk may be): drop the sentinel only, so
            // the code stays as ordinary paragraphs.
            i += 1;
            continue;
        }
        if let Some(end) = matches {
            let Some(chunk) = sentinel_key.as_deref().and_then(|s| by_sentinel.remove(s)) else {
                i += 1;
                continue;
            };
            match doc.chunk_blocks_with_ids(&chunk, &mut ids) {
                Ok(blocks) => {
                    doc.body.extend(blocks);
                    i = end;
                    continue;
                }
                // Code the model refuses (larger than a chunk may be): drop the sentinel only, so
                // the code stays as ordinary paragraphs.
                Err(_) => {
                    i += 1;
                    continue;
                }
            }
        }
        if let Some(block) = old.get(i) {
            doc.body.push(block.clone());
        }
        i += 1;
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

/// Whether `line` opens an executable chunk: an unindented fence with `{language …}`.
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
    let inner = rest.strip_prefix('{')?;
    let end = find_chunk_header_end(inner)?;
    let info = inner.get(..end)?;
    let info = info.trim();
    if info.is_empty() || info.starts_with('.') || info.chars().next().is_some_and(|c| c.is_ascii_whitespace()) {
        return None;
    }
    // A closing fence has nothing after its run; an info string with braces is a chunk header.
    Some((ch, len, info))
}

/// Find the matching `}` for `{` at the start of a chunk header, respecting quotes and nested delimiters.
fn find_chunk_header_end(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut brace_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;

    for (idx, ch) in s.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                quote = None;
            }
        } else {
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
    }
    None
}

/// Whether `line` opens an ordinary Markdown code fence (at least 3 backticks or tildes).
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

/// The language and header options of a chunk header such as `r, echo=FALSE, fig.cap="A plot"`.
fn parse_info(info: &str) -> (String, ChunkOptions) {
    let mut options = ChunkOptions::new();
    let mut parts = split_commas(info);
    let first = parts.next().unwrap_or("").trim();
    let mut language = String::new();
    // `{r}` or `{r label, echo=FALSE}`: knitr allows the label, separated by a space.
    let (head, rest) = match first.split_once(char::is_whitespace) {
        Some((head, rest)) => (head, Some(rest.trim())),
        None => (first, None),
    };
    if head.starts_with('.') {
        return (String::new(), options);
    }
    if !head.contains('=') {
        language = normalize(head);
        if let Some(rest) = rest.filter(|r| !r.is_empty()) {
            if rest.contains('=') {
                parse_option(rest, &mut options);
            } else {
                let label = option_key(rest);
                if !label.is_empty() {
                    options.set("label", rest.chars().filter(|c| !c.is_control()).take(128).collect::<String>());
                }
            }
        }
    } else {
        parse_option(first, &mut options);
    }
    for part in parts {
        parse_option(part, &mut options);
    }
    (language, options)
}

/// Split at commas that are not inside a quoted value or balanced parentheses/brackets.
fn split_commas(info: &str) -> impl Iterator<Item = &str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut depth = 0usize;
    for (at, c) in info.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
        } else {
            match c {
                '\'' | '"' => quote = Some(c),
                '(' | '[' | '{' => depth = depth.saturating_add(1),
                ')' | ']' | '}' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    parts.push(info.get(start..at).unwrap_or(""));
                    start = at + 1;
                }
                _ => {}
            }
        }
    }
    parts.push(info.get(start..).unwrap_or(""));
    parts.into_iter()
}

/// Whether an option value in an R chunk header is an R expression (not a literal scalar).
fn is_r_expression(key: &str, val: &str) -> bool {
    if matches!(key, "label" | "id") {
        return false;
    }
    let v = val.trim();
    if v.is_empty() || v.starts_with("!expr") {
        return false;
    }
    if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2) || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2) {
        return false;
    }
    if matches!(v.to_ascii_lowercase().as_str(), "true" | "false" | "t" | "f" | "yes" | "no" | "null" | "na") {
        return false;
    }
    if v.parse::<f64>().is_ok() {
        return false;
    }
    true
}

/// Read `key=value` (or a bare language name) from a header part.
fn parse_option(part: &str, options: &mut ChunkOptions) {
    let Some((key, value)) = part.split_once('=') else { return };
    let key = option_key(key);
    let value = value.trim();
    if !key.is_empty() && !value.is_empty() {
        if is_r_expression(&key, value) {
            options.set(key, format!("!expr {value}"));
        } else {
            options.set(key, value);
        }
    }
}

/// Strip the leading `#| key: value` lines, returning the code and the options they set.
fn strip_option_lines(body: &str, mut options: ChunkOptions) -> (String, ChunkOptions) {
    // Keep complex or unstoreable YAML blocks verbatim in `code` for export, while still reading
    // top-level scalar options from that same block so WordCraft honours flags such as `eval: false`.
    if let Some(retained) = inspect_retained_yaml(body, &options) {
        const PRIORITY_KEYS: &[&str] = &["eval", "echo", "include", "label"];
        for entry in &retained.entries {
            match entry {
                RetainedYamlEntry::Scalar(key, value) if PRIORITY_KEYS.contains(&key.as_str()) => {
                    options.set(key.clone(), value.clone());
                }
                RetainedYamlEntry::Complex(key) if PRIORITY_KEYS.contains(&key.as_str()) => {
                    options.remove(key);
                }
                _ => {}
            }
        }
        for entry in retained.entries {
            match entry {
                RetainedYamlEntry::Scalar(key, value) if !PRIORITY_KEYS.contains(&key.as_str()) => {
                    options.set(key, value);
                }
                RetainedYamlEntry::Complex(key) if !PRIORITY_KEYS.contains(&key.as_str()) => {
                    options.remove(&key);
                }
                _ => {}
            }
        }
        return (body.to_string(), options);
    }

    let lines: Vec<&str> = body.split('\n').collect();
    let mut start = 0;
    while start < lines.len() {
        let line = lines[start];
        let t = line.trim_start();
        if !t.starts_with("#|") {
            break;
        }
        if line.len() > MAX_OPTION_LINE {
            break;
        }
        let after_hash = t.strip_prefix("#|").unwrap_or("");
        let trimmed_content = after_hash.trim();
        if trimmed_content.starts_with('-') {
            break;
        }
        let Some((key, value)) = trimmed_content.split_once(':') else {
            break;
        };
        let key = option_key(key);
        let value = value.trim();
        if key.is_empty() || value.is_empty() || value.starts_with('|') || value.starts_with('>') {
            break;
        }
        if let Some(next_line) = lines.get(start + 1) {
            let next_t = next_line.trim_start();
            if let Some(next_after) = next_t.strip_prefix("#|")
                && (next_after.starts_with("  ") || next_after.starts_with('\t'))
            {
                break;
            }
        }
        options.set(key.clone(), value);
        if options.get(&key) != Some(value) {
            break;
        }
        start += 1;
    }
    if let Some(sep) = lines.get(start)
        && sep.trim().is_empty()
        && lines.get(start + 1).is_some_and(|l| l.trim_start().starts_with("#|"))
    {
        start += 1;
    }
    (lines.get(start..).unwrap_or(&[]).join("\n"), options)
}

/// Normalize known knitr option aliases to Quarto's hyphenated spelling or documented case.
fn normalize_option_alias(key: &str) -> Option<&'static str> {
    match key.to_ascii_lowercase().as_str() {
        "r.options" => Some("R.options"),
        "fig.cap" => Some("fig-cap"),
        "fig.subcap" => Some("fig-subcap"),
        "fig.width" => Some("fig-width"),
        "fig.height" => Some("fig-height"),
        "fig.dim" => Some("fig-dim"),
        "fig.asp" => Some("fig-asp"),
        "fig.align" => Some("fig-align"),
        "fig.link" => Some("fig-link"),
        "fig.alt" => Some("fig-alt"),
        "fig.env" => Some("fig-env"),
        "fig.pos" => Some("fig-pos"),
        "fig.scap" => Some("fig-scap"),
        "fig.show" => Some("fig-show"),
        "fig.retina" => Some("fig-retina"),
        "out.width" => Some("out-width"),
        "out.height" => Some("out-height"),
        "out.extra" => Some("out-extra"),
        "tbl.cap" => Some("tbl-cap"),
        "tbl.subcap" => Some("tbl-subcap"),
        "tbl.colwidths" => Some("tbl-colwidths"),
        "code.fold" => Some("code-fold"),
        "code.summary" => Some("code-summary"),
        "code.overflow" => Some("code-overflow"),
        "code.line.numbers" => Some("code-line-numbers"),
        "code.tools" => Some("code-tools"),
        "strip.white" => Some("strip-white"),
        "dev.args" => Some("dev-args"),
        "cache.path" => Some("cache-path"),
        "cache.vars" => Some("cache-vars"),
        "cache.lazy" => Some("cache-lazy"),
        "cache.comments" => Some("cache-comments"),
        "cache.rebuild" => Some("cache-rebuild"),
        _ => None,
    }
}

/// A language or option name as Quarto writes it: normalizes known aliases (like `fig.cap` to `fig-cap`)
/// while preserving documented native names (such as `R.options`).
fn option_key(key: &str) -> String {
    let cleaned: String = key.trim().chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')).take(64).collect();
    if let Some(alias) = normalize_option_alias(&cleaned) { alias.to_string() } else { cleaned }
}

/// A language name as a chunk tag carries it.
fn normalize(language: &str) -> String {
    language.trim().to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '.')).take(32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wordcraft_doc::Paragraph;

    fn text_of(doc: &Document) -> String {
        doc.body.iter().filter_map(|b| b.as_para()).map(|p: &Paragraph| p.text.replace('\u{FFFC}', "")).collect::<Vec<_>>().join("\n")
    }

    const SOURCE: &str = r#"---
title: "Field methods"
format: docx
---

# Survey

Plan the season before going out.

```{r, label=fig-abundance, echo=FALSE, fig.cap="Species abundance"}
abundance <- c(10, 25, 5)
barplot(abundance)
```

Notes on the method.

```{python}
#| label: tbl-sites
#| eval: false
sites = ["A", "B"]
print(sites)
```
"#;

    #[test]
    fn import_keeps_chunks_and_options() {
        let doc = import(SOURCE);
        let chunks = doc.chunks();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].language, "r");
        assert_eq!(chunks[0].code, "abundance <- c(10, 25, 5)\nbarplot(abundance)");
        assert_eq!(chunks[0].options.get("label"), Some("fig-abundance"));
        assert_eq!(chunks[0].options.get("echo"), Some("FALSE"));
        assert_eq!(chunks[0].options.fig_caption().as_deref(), Some("Species abundance"));
        assert_eq!(chunks[1].language, "python");
        assert_eq!(chunks[1].options.label(), Some("tbl-sites"));
        assert!(!chunks[1].options.eval());
        let text = text_of(&doc);
        assert!(text.contains("Plan the season before going out."));
        assert!(text.contains("Notes on the method."));
        assert!(!text.contains("title:"));
        assert_eq!(doc.custom_prop(FRONT_MATTER_PROP).map(str::trim), Some("---\ntitle: \"Field methods\"\nformat: docx\n---"));
    }

    #[test]
    fn export_writes_the_chunks_back_with_their_options() {
        let doc = import(SOURCE);
        let text = export(&doc);
        assert!(text.starts_with("---\ntitle: \"Field methods\"\nformat: docx\n---\n"));
        assert!(text.contains(
            "```{r}\n#| echo: FALSE\n#| fig-cap: \"Species abundance\"\n#| label: fig-abundance\nabundance <- c(10, 25, 5)\nbarplot(abundance)\n```"
        ));
        assert!(text.contains("```{python}\n#| eval: false\n#| label: tbl-sites\nsites = [\"A\", \"B\"]\nprint(sites)\n```"));
        assert!(text.contains("Plan the season before going out."));
    }

    #[test]
    fn export_import_is_stable() {
        let once = export(&import(SOURCE));
        let twice = export(&import(&once));
        assert_eq!(once, twice);
        let doc = import(&once);
        assert_eq!(doc.chunks().len(), 2);
        assert_eq!(doc.chunks()[0].options.fig_caption().as_deref(), Some("Species abundance"));
    }

    #[test]
    fn a_chunk_with_an_empty_line_round_trips() {
        let source = "```{r}\na\n\nb\n```\n";
        let doc = import(source);
        assert_eq!(doc.chunks()[0].code, "a\n\nb");
        let again = import(&export(&doc));
        assert_eq!(again.chunks()[0].code, "a\n\nb");
    }

    #[test]
    fn an_empty_chunk_round_trips() {
        let doc = import("```{r}\n```\n");
        assert_eq!(doc.chunks().len(), 1);
        assert_eq!(doc.chunks()[0].code, "");
        let again = import(&export(&doc));
        assert_eq!(again.chunks().len(), 1);
        assert_eq!(again.chunks()[0].code, "");
    }

    #[test]
    fn an_ordinary_code_fence_is_not_a_chunk() {
        let doc = import("```r\nx <- 1\n```\n");
        assert!(doc.chunks().is_empty());
        assert!(text_of(&doc).contains("x <- 1"));
    }

    #[test]
    fn a_brace_fence_of_another_language_stays_code() {
        for source in ["```{julia}\n1 + 1\n```\n", "```{mermaid}\ngraph TD; A-->B\n```\n"] {
            let doc = import(source);
            assert!(doc.chunks().is_empty(), "{source}");
            assert!(text_of(&doc).contains("1 + 1") || text_of(&doc).contains("A-->B"), "{source}");
        }
    }

    #[test]
    fn an_unclosed_chunk_fence_does_not_panic_or_lose_text() {
        let doc = import("```{r}\nx <- 1\n");
        assert!(doc.chunks().is_empty());
        assert!(text_of(&doc).contains("x <- 1"));
    }

    #[test]
    fn a_long_fence_in_the_code_is_escaped() {
        let code = "`\u{0060}\u{0060}\u{0060}";
        let mut chunk = CodeChunk::new("r", code);
        chunk.options.set("label", "x");
        let mut doc = Document::new();
        doc.push_chunk(&chunk).unwrap();
        let out = export(&doc);
        assert!(out.starts_with("~~~{r}") || out.starts_with("````{r}"), "{out}");
        assert_eq!(import(&out).chunks()[0].code, code);
    }

    #[test]
    fn option_values_never_break_the_option_line() {
        let mut chunk = CodeChunk::new("r", "1");
        chunk.options.set("label", "a\nb");
        let mut doc = Document::new();
        doc.push_chunk(&chunk).unwrap();
        let out = export(&doc);
        assert!(out.contains("#| label: a b\n"), "{out}");
    }

    #[test]
    fn the_extension_dispatch_reads_and_writes_both_names() {
        for ext in ["qmd", "rmd"] {
            let doc = crate::import(ext, SOURCE.as_bytes()).expect("our format").expect("read");
            assert_eq!(doc.chunks().len(), 2, "{ext}");
            let bytes = crate::export(ext, &doc).expect("our format").expect("write");
            let text = String::from_utf8(bytes).expect("utf-8");
            assert!(text.contains("```{r}") && text.contains("```{python}"), "{ext}: {text}");
        }
    }

    #[test]
    fn front_matter_is_not_written_twice() {
        let mut doc = Document::new();
        doc.set_custom_prop(FRONT_MATTER_PROP, "---\ntitle: T\n---");
        assert_eq!(export(&doc).matches("title: T").count(), 1);
    }

    #[test]
    fn header_options_preserve_nested_commas_and_escapes() {
        let source = "```{r, fig.cap = paste(\"Air, Space\", \"Quality\"), fig.dim = c(6, 4), label = plot1}\nplot(1)\n```\n";
        let doc = import(source);
        let chunks = doc.chunks();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].options.get("label"), Some("plot1"));
        assert_eq!(chunks[0].options.get("fig-dim"), Some("!expr c(6, 4)"));
        assert_eq!(chunks[0].options.get("fig-cap"), Some("!expr paste(\"Air, Space\", \"Quality\")"));
    }

    #[test]
    fn block_scalar_and_list_options_preserved_verbatim_in_code() {
        let source = "```{r}\n#| label: test\n#| fig-cap: |\n#|   A multi-line\n#|   caption\nplot(1)\n```\n";
        let doc = import(source);
        let chunks = doc.chunks();
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].code.contains("#| label: test"));
        assert!(chunks[0].code.contains("#| fig-cap: |"));
        assert!(chunks[0].code.contains("#|   A multi-line"));
        assert!(chunks[0].code.contains("plot(1)"));
        let exported = export(&doc);
        assert!(exported.contains("```{r}\n#| label: test\n#| fig-cap: |"));
    }

    #[test]
    fn ordinary_fence_containing_chunk_header_remains_unparsed() {
        let source = "````markdown\nHere is an example:\n```{r}\nx <- 1\n```\n````\n";
        let doc = import(source);
        assert!(doc.chunks().is_empty());
        let text = text_of(&doc);
        assert!(text.contains("```{r}"));
        assert!(text.contains("x <- 1"));
    }

    #[test]
    fn source_comments_starting_with_hash_pipe_round_trip_losslessly() {
        let mut chunk = CodeChunk::new("r", "#| label: literal\n1");
        chunk.options.set("echo", "false");
        let mut doc = Document::new();
        doc.push_chunk(&chunk).unwrap();
        let exported = export(&doc);
        let imported = import(&exported);
        let imported_chunks = imported.chunks();
        assert_eq!(imported_chunks.len(), 1);
        assert_eq!(imported_chunks[0].options.get("echo"), Some("false"));
        assert_eq!(imported_chunks[0].code, "#| label: literal\n1");
    }

    #[test]
    fn source_comments_without_options_round_trip_losslessly() {
        let chunk = CodeChunk::new("r", "#| not an option\nx <- 1");
        let mut doc = Document::new();
        doc.push_chunk(&chunk).unwrap();
        let exported = export(&doc);
        let imported = import(&exported);
        let imported_chunks = imported.chunks();
        assert_eq!(imported_chunks.len(), 1);
        assert!(imported_chunks[0].options.is_empty());
        assert_eq!(imported_chunks[0].code, "#| not an option\nx <- 1");
    }

    #[test]
    fn pandoc_class_fence_is_not_parsed_as_executable_chunk() {
        let source = "```{.python}\nx = 1\n```\n";
        let doc = import(source);
        assert!(doc.chunks().is_empty());
        let text = text_of(&doc);
        assert!(text.contains("x = 1"));
        let exported = export(&doc);
        assert!(!exported.contains("```{python}"));
    }

    #[test]
    fn tab_indented_line_does_not_close_fence() {
        let source = "````markdown\n\t````\n```{r}\nx <- 1\n```\n````\n";
        let doc = import(source);
        assert!(doc.chunks().is_empty());
    }

    #[test]
    fn front_matter_closing_delimiter_must_be_unindented() {
        let source = "---\ntitle: \"Test\"\nabstract: |\n  line 1\n  ---\n  line 2\n---\nBody text\n";
        let doc = import(source);
        let fm = doc.custom_prop(FRONT_MATTER_PROP).unwrap();
        assert!(fm.contains("line 2"));
        assert!(text_of(&doc).contains("Body text"));
    }

    #[test]
    fn fence_longer_than_256_backticks_does_not_close_early() {
        let code = "`".repeat(300);
        let mut doc = Document::new();
        doc.push_chunk(&CodeChunk::new("python", &code)).unwrap();
        let exported = export(&doc);
        let imported = import(&exported);
        assert_eq!(imported.chunks().len(), 1);
        assert_eq!(imported.chunks()[0].code, code);
    }

    #[test]
    fn source_comments_with_existing_blank_line_do_not_accumulate_blank_lines() {
        let mut chunk = CodeChunk::new("r", "\n#| note\n1");
        chunk.options.set("echo", "false");
        let mut doc = Document::new();
        doc.push_chunk(&chunk).unwrap();
        let exported1 = export(&doc);
        assert!(!exported1.contains("\n\n\n#| note"), "does not add a redundant separator");
        let imported1 = import(&exported1);
        let exported2 = export(&imported1);
        assert_eq!(exported1, exported2, "round trips do not accumulate blank lines");
    }

    #[test]
    fn retained_yaml_cell_options_export_without_preceding_blank_line() {
        let source = "```{python}\n#| fig-cap: |\n#|   Caption\n#| eval: false\nprint(1)\n```\n";
        let doc = import(source);
        let exported = export(&doc);
        assert!(exported.contains("```{python}\n#| fig-cap: |"));
        assert!(!exported.contains("```{python}\n\n#| fig-cap: |"));
    }

    #[test]
    fn source_starting_with_hash_pipe_without_options_round_trips_losslessly() {
        let chunk = CodeChunk::new("r", "#| label: literal\n1");
        let mut doc = Document::new();
        doc.push_chunk(&chunk).unwrap();
        let exported = export(&doc);
        assert!(exported.contains("```{r}\n\n#| label: literal\n1\n```"));
        let imported = import(&exported);
        let chunks = imported.chunks();
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].options.is_empty(), "options should remain empty");
        assert_eq!(chunks[0].code, "#| label: literal\n1", "source code should be preserved losslessly");
    }

    #[test]
    fn chunk_header_outer_brace_handles_quoted_braces_and_nested_delimiters() {
        let source = "```{r, fig.cap = \"Plot with } brace\", echo = FALSE}\nplot(1)\n```\n";
        let doc = import(source);
        let chunks = doc.chunks();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].options.get("fig-cap"), Some("\"Plot with } brace\""));
        assert_eq!(chunks[0].options.get("echo"), Some("FALSE"));
    }

    #[test]
    fn option_key_preserves_native_r_options_and_normalizes_known_aliases() {
        let source = "```{r, R.options = list(digits = 3), fig.cap = \"Title\"}\n#| out.width: 80%\n#| custom.param: value\nplot(1)\n```\n";
        let doc = import(source);
        let chunks = doc.chunks();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].options.get("R.options"), Some("!expr list(digits = 3)"));
        assert_eq!(chunks[0].options.get("fig-cap"), Some("\"Title\""));
        assert_eq!(chunks[0].options.get("out-width"), Some("80%"));
        assert_eq!(chunks[0].options.get("custom.param"), Some("value"));
    }

    #[test]
    fn chunk_exceeding_line_limit_keeps_fallback_text_without_vector_explosion() {
        let code = "x <- 1\n".repeat(wordcraft_doc::chunk::MAX_CHUNK_LINES + 5);
        let source = format!("```{{r}}\n{code}```\n");
        let doc = import(&source);
        assert!(doc.chunks().is_empty(), "oversized chunk should not be kept as a CodeChunk control");
        assert!(!doc.body.is_empty(), "fallback paragraphs should remain in doc.body");
    }

    #[test]
    fn importing_many_chunks_scales_linearly() {
        let make_source = |n: usize| {
            let mut s = String::with_capacity(n * 24);
            for i in 0..n {
                s.push_str(&format!("```{{r}}\nx <- {i}\n```\n\n"));
            }
            s
        };
        let small = make_source(64);
        let big = make_source(512);
        let mut small_doc = Document::new();
        let mut big_doc = Document::new();
        let s = wordcraft_doc::work::count(|| small_doc = import(&small));
        let b = wordcraft_doc::work::count(|| big_doc = import(&big));

        let small_ids: std::collections::HashSet<i64> = small_doc.control_ranges(StoryRef::Body).iter().filter_map(|r| r.control.id).collect();
        let big_ids: std::collections::HashSet<i64> = big_doc.control_ranges(StoryRef::Body).iter().filter_map(|r| r.control.id).collect();
        assert_eq!(small_ids.len(), 64);
        assert_eq!(big_ids.len(), 512);
        assert!(big_ids.iter().all(|&id| id > 0));
        assert_eq!(big_doc.custom_props.len(), 512);

        let ratio = b as f64 / s.max(1) as f64;
        assert!(s > 0, "no steps counted for chunk import");
        assert!(ratio <= 12.0, "8x the chunks cost {ratio:.1}x the steps ({s} -> {b})");

        // Full MAX_CHUNKS import produces distinct positive IDs and matching properties.
        let full = make_source(MAX_CHUNKS);
        let full_doc = import(&full);
        let full_ids: std::collections::HashSet<i64> = full_doc.control_ranges(StoryRef::Body).iter().filter_map(|r| r.control.id).collect();
        assert_eq!(full_ids.len(), MAX_CHUNKS);
        assert!(full_ids.iter().all(|&id| id > 0));
        assert_eq!(full_doc.custom_props.len(), MAX_CHUNKS);
    }

    #[test]
    fn apply_chunks_preserves_existing_control_and_property_ids() {
        use wordcraft_doc::{CharProps, ContentControl, ControlKind, InlineObject, Part, PartKind, para_block};

        let mut doc = Document::new();
        let mark = CharProps::default();

        // Pre-existing control id = 1 in doc.body.
        let mut body_ctrl = ContentControl::new(ControlKind::RichText);
        body_ctrl.id = Some(1);
        let mut body_para = Paragraph::with_text("intro", CharProps::default());
        body_para.insert_object(0, InlineObject::ControlStart { control: Box::new(body_ctrl) }, &mark).unwrap();
        body_para.insert_object(body_para.len(), InlineObject::ControlEnd, &mark).unwrap();

        // Pre-existing control id = 2 in a header part.
        let mut part_ctrl = ContentControl::new(ControlKind::RichText);
        part_ctrl.id = Some(2);
        let mut part_para = Paragraph::with_text("header", CharProps::default());
        part_para.insert_object(0, InlineObject::ControlStart { control: Box::new(part_ctrl) }, &mark).unwrap();
        part_para.insert_object(part_para.len(), InlineObject::ControlEnd, &mark).unwrap();
        doc.parts.insert(1, Part { kind: PartKind::Header, blocks: vec![para_block(part_para)], ..Default::default() });

        // Pre-existing chunk properties (mixed and lowercase) reserving ids 3 and 4.
        doc.set_custom_prop("WORDCRAFT.CHUNK.3", "{\"eval\":\"false\"}");
        doc.set_custom_prop("wordcraft.chunk.4", "{\"label\":\"keep\"}");

        let s1 = "wordcraft-chunk-sentinel-test-0".to_string();
        let s2 = "wordcraft-chunk-sentinel-test-1".to_string();
        doc.body = vec![
            para_block(body_para),
            para_block(Paragraph::with_text(&s1, CharProps::default())),
            para_block(Paragraph::with_text("x <- 1", CharProps::default())),
            para_block(Paragraph::with_text(&s2, CharProps::default())),
            para_block(Paragraph::with_text("y = 2", CharProps::default())),
        ];

        let mut c1 = CodeChunk::new("r", "x <- 1");
        c1.options.set("label", "first");
        let mut c2 = CodeChunk::new("python", "y = 2");
        c2.id = Some(2); // collides with header part control
        c2.options.set("label", "second");

        apply_chunks(&mut doc, vec![Pending { sentinel: s1, chunk: c1 }, Pending { sentinel: s2, chunk: c2 }]);

        let chunks = doc.chunks();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].id, Some(5));
        assert_eq!(chunks[0].options.label(), Some("first"));
        assert_eq!(chunks[1].id, Some(6));
        assert_eq!(chunks[1].options.label(), Some("second"));
        assert_eq!(doc.custom_prop("WORDCRAFT.CHUNK.3"), Some("{\"eval\":\"false\"}"));
        assert_eq!(doc.custom_prop("wordcraft.chunk.4"), Some("{\"label\":\"keep\"}"));
    }

    #[test]
    fn retained_yaml_cell_options_parse_scalar_options_without_duplicating_on_export() {
        let source = "```{python}\n#| fig-cap: |\n#|   Caption\n#| eval: false\nprint(1)\n```\n";
        let doc = import(source);
        let chunks = doc.chunks();
        assert_eq!(chunks.len(), 1);
        assert!(!chunks[0].options.eval(), "eval: false in a retained YAML block must disable evaluation");
        assert_eq!(chunks[0].options.get("eval"), Some("false"));
        assert_eq!(chunks[0].code, "#| fig-cap: |\n#|   Caption\n#| eval: false\nprint(1)");

        let exported = export(&doc);
        assert!(exported.contains("```{python}\n#| fig-cap: |\n#|   Caption\n#| eval: false\nprint(1)\n```"), "{exported}");
        assert_eq!(exported.matches("#| eval:").count(), 1, "eval must not be emitted twice: {exported}");
        assert_eq!(export(&import(&exported)), exported, "round trip must be idempotent");
    }

    #[test]
    fn retained_yaml_ignores_nested_and_block_scalar_lines_while_reading_top_level_scalars() {
        let block_scalar =
            "```{python}\n#| fig-cap: |\n#|   Example line:\n#|   eval: false\n#|\n#|   second paragraph\n#| echo: false\nprint(1)\n```\n";
        let doc = import(block_scalar);
        let chunks = doc.chunks();
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].options.eval(), "indented eval: false inside fig-cap block scalar must not disable eval");
        assert!(!chunks[0].options.echo(), "top-level echo: false after block scalar must be parsed");
        let exported = export(&doc);
        assert_eq!(exported.matches("#| echo:").count(), 1, "{exported}");
        assert_eq!(export(&import(&exported)), exported);

        let nested_and_list = concat!(
            "```{r}\n",
            "#| label: fig-panels\n",
            "#| fig.width: 6\n",
            "#| fig-subcap: # panel captions\n",
            "#|   - \"First\"\n",
            "#|   - \"Second\"\n",
            "#| params:\n",
            "#|   eval: true\n",
            "#| layout: [\n",
            "#|   [1, 1]\n",
            "#| ]\n",
            "#| eval: false\n",
            "plot(1)\n",
            "```\n",
        );
        let doc = import(nested_and_list);
        let chunks = doc.chunks();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].options.label(), Some("fig-panels"));
        assert_eq!(chunks[0].options.get("fig-width"), Some("6"));
        assert!(!chunks[0].options.eval());
        let exported = export(&doc);
        assert_eq!(exported.matches("#| label:").count(), 1, "{exported}");
        assert_eq!(exported.matches("fig.width:").count(), 1, "{exported}");
        assert_eq!(exported.matches("fig-width:").count(), 0, "alias must not duplicate as canonical key: {exported}");
        assert_eq!(exported.matches("#| eval:").count(), 1, "{exported}");
        assert_eq!(export(&import(&exported)), exported);

        let multiline_quoted = "```{python}\n#| fig-cap: \"First line\n#| eval: false\"\n#| echo: false\nprint(1)\n```\n";
        let doc = import(multiline_quoted);
        let chunks = doc.chunks();
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].options.eval(), "eval: false inside multiline double-quoted caption must not disable eval");
        assert!(!chunks[0].options.echo());
        let exported = export(&doc);
        assert_eq!(exported.matches("#| echo:").count(), 1, "{exported}");
        assert_eq!(export(&import(&exported)), exported);
    }

    #[test]
    fn header_options_merge_with_retained_yaml_without_duplicating_keys() {
        let source = "```{r, echo=FALSE, eval=TRUE, fig.cap=\"Header\"}\n#| fig-cap: |\n#|   Multiline\n#|   caption\n#| eval: false\nplot(1)\n```\n";
        let doc = import(source);
        let chunks = doc.chunks();
        assert_eq!(chunks.len(), 1);
        assert!(!chunks[0].options.echo(), "header echo=FALSE must be preserved");
        assert!(!chunks[0].options.eval(), "#| eval: false must override header eval=TRUE");
        assert_eq!(chunks[0].options.fig_caption(), None, "block scalar fig-cap must supersede header fig.cap");

        let exported = export(&doc);
        assert!(exported.contains("```{r}\n#| echo: FALSE\n#| fig-cap: |\n#|   Multiline\n#|   caption\n#| eval: false\nplot(1)\n```"), "{exported}");
        assert_eq!(exported.matches("#| echo:").count(), 1, "{exported}");
        assert_eq!(exported.matches("#| eval:").count(), 1, "{exported}");
        assert_eq!(exported.matches("#| fig-cap:").count(), 1, "{exported}");
        assert_eq!(export(&import(&exported)), exported, "second round trip must be idempotent");
    }

    #[test]
    fn overlong_caption_option_is_retained_instead_of_consumed_while_short_captions_stay_options() {
        let overlong_inner = "a".repeat(wordcraft_doc::chunk::MAX_CHUNK_OPTION_CHARS - 1);
        let overlong_value = format!("\"{overlong_inner}\"");
        assert_eq!(overlong_value.chars().count(), wordcraft_doc::chunk::MAX_CHUNK_OPTION_CHARS + 1);
        let overlong_line = format!("#| fig-cap: {overlong_value}");
        assert!(overlong_line.len() <= MAX_OPTION_LINE, "test input must fit the 8 KiB line limit");

        let source = format!("Intro paragraph.\n\n```{{r, echo=FALSE, fig.cap=\"Header\"}}\n#| eval: false\n{overlong_line}\nplot(1)\n```\n");
        let mut doc = import(&source);
        let chunks = doc.chunks();
        assert_eq!(chunks.len(), 1);
        assert!(!chunks[0].options.echo(), "header echo=FALSE must be preserved");
        assert!(!chunks[0].options.eval(), "eval: false alongside an unstoreable caption must still parse");
        assert_eq!(chunks[0].options.fig_caption(), None, "overlong #| fig-cap must supersede header fig.cap");
        assert!(chunks[0].code.contains(&overlong_line), "rejected option line must stay in chunk code instead of being consumed");

        // Simulate a formatted edit on surrounding prose before exporting.
        if let Some(wordcraft_doc::Block::Para(p)) = doc.body.first_mut().map(std::sync::Arc::make_mut)
            && let Some(run) = p.runs.first_mut()
        {
            run.props.bold = Some(true);
        }
        let exported = export(&doc);
        assert!(exported.contains(&overlong_line), "caption must survive export after edit");
        assert!(exported.contains("```{r}\n#| echo: FALSE\n#| eval: false\n#| fig-cap:"), "retained block must stay attached to options");
        assert_eq!(exported.matches("#| eval:").count(), 1, "eval must not be duplicated");
        assert_eq!(exported.matches("#| fig-cap:").count(), 1, "fig-cap must not be duplicated");
        assert_eq!(export(&import(&exported)), exported, "round trip must be idempotent");

        // Short captions and exact-limit captions still become normal ChunkOptions.
        let exact_inner = "b".repeat(wordcraft_doc::chunk::MAX_CHUNK_OPTION_CHARS - 2);
        for caption in ["Short caption", exact_inner.as_str()] {
            let normal = format!("```{{r}}\n#| fig-cap: \"{caption}\"\nplot(1)\n```\n");
            let normal_doc = import(&normal);
            let normal_chunks = normal_doc.chunks();
            assert_eq!(normal_chunks.len(), 1);
            assert_eq!(normal_chunks[0].options.fig_caption().as_deref(), Some(caption));
            assert_eq!(normal_chunks[0].code, "plot(1)");
        }
    }

    #[test]
    fn option_count_overflow_retains_raw_options_without_dropping_lines() {
        let mut body_opts = String::new();
        for i in 0..(wordcraft_doc::chunk::MAX_CHUNK_OPTIONS + 4) {
            body_opts.push_str(&format!("#| opt-{i:03}: {i}\n"));
        }
        body_opts.push_str("#| eval: false\n");
        let source = format!("```{{r}}\n{body_opts}x <- 1\n```\n");
        let doc = import(&source);
        let chunks = doc.chunks();
        assert_eq!(chunks.len(), 1);
        assert!(!chunks[0].options.eval(), "eval: false must still take effect past MAX_CHUNK_OPTIONS");
        assert!(chunks[0].code.contains("#| opt-259: 259"));

        let exported = export(&doc);
        assert!(exported.contains("#| opt-000: 0"));
        assert!(exported.contains("#| opt-259: 259"));
        assert_eq!(exported.matches("#| eval:").count(), 1);
        assert!(!exported.contains("```{r}\n\n#| opt-000:"));
        assert_eq!(export(&import(&exported)), exported);

        // Header options + #| options exceeding MAX_CHUNK_OPTIONS combined.
        let header = (0..250).map(|i| format!("h{i:03}={i}")).collect::<Vec<_>>().join(", ");
        let mut cell_opts = String::new();
        for i in 0..10 {
            cell_opts.push_str(&format!("#| c{i:03}: {i}\n"));
        }
        cell_opts.push_str("#| eval: false\n");
        let combined = format!("```{{r, {header}}}\n{cell_opts}x <- 1\n```\n");
        let combined_doc = import(&combined);
        let combined_chunks = combined_doc.chunks();
        assert_eq!(combined_chunks.len(), 1);
        assert!(!combined_chunks[0].options.eval(), "eval: false must still parse when header + body exceed MAX_CHUNK_OPTIONS");
        assert!(combined_chunks[0].code.contains("#| c009: 9"));

        let combined_exported = export(&combined_doc);
        assert!(combined_exported.contains("#| h000: 0"));
        assert!(combined_exported.contains("#| h249: 249"));
        assert!(combined_exported.contains("#| c000: 0"));
        assert!(combined_exported.contains("#| c009: 9"));
        assert_eq!(combined_exported.matches("#| eval:").count(), 1);
        assert!(!combined_exported.contains("\n\n#| c000:"));
        assert_eq!(export(&import(&combined_exported)), combined_exported);
    }

    #[test]
    fn oversized_option_line_over_8kib_and_non_yaml_rejected_line_are_not_consumed() {
        let huge_inner = "x".repeat(MAX_OPTION_LINE + 64);
        let huge_line = format!("#| fig-cap: \"{huge_inner}\"");
        let huge_source = format!("```{{r}}\n{huge_line}\n#| eval: false\nplot(1)\n```\n");
        let doc = import(&huge_source);
        let chunks = doc.chunks();
        assert_eq!(chunks.len(), 1);
        assert!(!chunks[0].options.eval());
        assert!(chunks[0].code.contains(&huge_line));
        let exported = export(&doc);
        assert!(exported.contains(&huge_line));
        assert_eq!(exported.matches("#| eval:").count(), 1);
        assert!(!exported.contains("```{r}\n\n#| fig-cap:"));
        assert_eq!(export(&import(&exported)), exported);

        // Non-YAML syntax (missing space after `:`) with an unstoreable value must still not be consumed.
        let compact_inner = "z".repeat(wordcraft_doc::chunk::MAX_CHUNK_OPTION_CHARS + 1);
        let compact_line = format!("#| fig-cap:\"{compact_inner}\"");
        let compact_source = format!("```{{r}}\n#| eval: false\n{compact_line}\nplot(1)\n```\n");
        let compact_doc = import(&compact_source);
        let compact_chunks = compact_doc.chunks();
        assert_eq!(compact_chunks.len(), 1);
        assert!(!compact_chunks[0].options.eval());
        assert!(compact_chunks[0].code.contains(&compact_line));
        assert!(export(&compact_doc).contains(&compact_line));
    }
}
