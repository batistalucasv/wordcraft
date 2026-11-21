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

use std::collections::HashMap;

use wordcraft_doc::{Blocks, ChunkOptions, CodeChunk, Document, StoryRef};

/// Custom property that holds a `.qmd`/`.Rmd` file's YAML front matter, delimiters included.
pub const FRONT_MATTER_PROP: &str = "wordcraft.quarto.frontMatter";

/// Chunks one file may declare; the rest stay ordinary code blocks.
const MAX_CHUNKS: usize = 4_096;
/// Longest chunk code kept as a chunk (the document model's own limit).
const MAX_CHUNK_BYTES: usize = wordcraft_doc::chunk::MAX_CHUNK_CODE_BYTES;
/// Longest `#|` option line read, in bytes; longer lines stay in the code.
const MAX_OPTION_LINE: usize = 8 * 1024;

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
    for (key, value) in chunk.options.iter() {
        if !option_name_ok(key) {
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
    if first_line.trim_start().starts_with("#|") && !starts_with_retained_yaml(&chunk.code) {
        out.push('\n');
    }
    out.push_str(&chunk.code);
    if !chunk.code.is_empty() {
        out.push('\n');
    }
    out.push_str(&fence);
    out.push_str("\n\n");
}

/// Whether `code` begins with YAML cell options that were retained in the code verbatim.
fn starts_with_retained_yaml(code: &str) -> bool {
    for line in code.lines() {
        let t = line.trim_start();
        if !t.starts_with("#|") {
            break;
        }
        let after_hash = t.strip_prefix("#|").unwrap_or("");
        let trimmed_content = after_hash.trim();
        if trimmed_content.starts_with('-') {
            return true;
        }
        if let Some((_, value)) = trimmed_content.split_once(':') {
            let v = value.trim();
            if v.starts_with('|') || v.starts_with('>') {
                return true;
            }
        }
        if after_hash.starts_with("  ") || after_hash.starts_with('\t') {
            return true;
        }
    }
    false
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
            match doc.chunk_blocks(&chunk) {
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
    let lines: Vec<&str> = body.split('\n').collect();
    // Leave the cell options block in `code` unchanged when it contains unsupported YAML.
    let mut i = 0;
    let mut has_unsupported = false;
    while i < lines.len() {
        let t = lines[i].trim_start();
        if !t.starts_with("#|") {
            break;
        }
        let after_hash = t.strip_prefix("#|").unwrap_or("");
        let trimmed_content = after_hash.trim();
        if trimmed_content.starts_with('-') {
            has_unsupported = true;
            break;
        }
        if let Some((_, value)) = trimmed_content.split_once(':') {
            let v = value.trim();
            if v.starts_with('|') || v.starts_with('>') {
                has_unsupported = true;
                break;
            }
        }
        if after_hash.starts_with("  ") || after_hash.starts_with('\t') {
            has_unsupported = true;
            break;
        }
        i += 1;
    }
    if has_unsupported {
        return (body.to_string(), options);
    }

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
        options.set(key, value);
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
}
