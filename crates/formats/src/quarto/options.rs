//! Chunk header (`{r, ...}`) and `#|` YAML option parsing for Quarto / R Markdown.

use std::collections::HashSet;

use wordcraft_doc::ChunkOptions;

use super::step_quoted;

/// Longest `#|` option line read, in bytes; longer lines stay in the code.
pub(super) const MAX_OPTION_LINE: usize = 8 * 1024;
/// Most options the document model keeps per chunk.
const MAX_CHUNK_OPTIONS: usize = wordcraft_doc::chunk::MAX_CHUNK_OPTIONS;
/// Longest option name or value the document model keeps, in characters.
const MAX_CHUNK_OPTION_CHARS: usize = wordcraft_doc::chunk::MAX_CHUNK_OPTION_CHARS;

/// Whether the first non-blank line in `lines` starts with `#|` (ignoring leading whitespace).
///
/// Export prepends one empty separator line whenever non-retained chunk code matches this
/// predicate, and import removes one leading empty line under the same predicate. Shifting a
/// leading run of `k >= 0` blank lines to `k + 1` on export and back to `k` on import distinguishes
/// the inserted separator from source blank lines and prevents blank-line accumulation.
pub(super) fn first_nonblank_is_hash_pipe<'a>(mut lines: impl Iterator<Item = &'a str>) -> bool {
    lines.find(|line| !line.trim().is_empty()).is_some_and(|line| line.trim_start().starts_with("#|"))
}

/// Top-level option entry seen inside a retained leading `#|` YAML block.
pub(super) enum RetainedYamlEntry {
    /// Supported single-line scalar option (`key: value`), kept in `ChunkOptions`.
    Scalar(String, String),
    /// Complex or oversized YAML option (`fig-cap: |`, list, nested mapping, etc.) whose key
    /// lives verbatim in `code` and supersedes any header option of the same name.
    Complex(String),
}

/// Summary of a leading `#|` YAML block that contains complex YAML and therefore stays
/// verbatim in `chunk.code`.
pub(super) struct RetainedYamlBlock {
    pub(super) entries: Vec<RetainedYamlEntry>,
    pub(super) top_level_keys: HashSet<String>,
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
        if quote == Some('\'') {
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                } else {
                    quote = None;
                }
            }
        } else if !step_quoted(&mut quote, &mut escaped, c) {
            match c {
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
            }
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

fn advance_multiline_state(state: YamlScanState, yaml: &str) -> (YamlScanState, bool) {
    let is_blank = yaml.trim().is_empty();
    let is_indented = !is_blank && (yaml.starts_with(' ') || yaml.starts_with('\t'));
    match state {
        YamlScanState::BlockScalar => {
            if is_blank || is_indented {
                (YamlScanState::BlockScalar, true)
            } else {
                (YamlScanState::TopLevel, false)
            }
        }
        YamlScanState::NestedValue => {
            if is_blank || is_indented || yaml.starts_with('#') || is_yaml_sequence_item(yaml) {
                (YamlScanState::NestedValue, true)
            } else {
                (YamlScanState::TopLevel, false)
            }
        }
        YamlScanState::DoubleQuoted => {
            let next = if scan_double_quoted(yaml) { YamlScanState::TopLevel } else { YamlScanState::DoubleQuoted };
            (next, true)
        }
        YamlScanState::SingleQuoted => {
            let next = if scan_single_quoted(yaml) { YamlScanState::TopLevel } else { YamlScanState::SingleQuoted };
            (next, true)
        }
        YamlScanState::Flow { depth, quote } => {
            let (next_depth, next_quote) = scan_flow(yaml, depth, quote);
            let next = if next_depth == 0 && next_quote.is_none() {
                YamlScanState::TopLevel
            } else {
                YamlScanState::Flow { depth: next_depth, quote: next_quote }
            };
            (next, true)
        }
        YamlScanState::TopLevel => (YamlScanState::TopLevel, false),
    }
}

enum TopLevelAction {
    Skip,
    Continue { retain: bool, next_state: Option<YamlScanState>, entry: Option<RetainedYamlEntry> },
    Stop,
}

fn parse_top_level_yaml_line(line: &str, yaml: &str, lines: &[&str], idx: usize, top_level_keys: &mut HashSet<String>) -> TopLevelAction {
    let is_blank = yaml.trim().is_empty();
    if is_blank || yaml.starts_with('#') {
        return TopLevelAction::Skip;
    }
    let is_indented = yaml.starts_with(' ') || yaml.starts_with('\t');
    if is_indented || is_yaml_sequence_item(yaml) {
        return TopLevelAction::Continue { retain: true, next_state: None, entry: None };
    }
    let Some((raw_key, raw_value)) = yaml.split_once(':') else {
        return TopLevelAction::Stop;
    };
    // Per YAML 1.2, a mapping `:` is followed by separation whitespace or end-of-line.
    if !raw_value.is_empty() && !raw_value.starts_with([' ', '\t']) {
        return TopLevelAction::Stop;
    }
    let raw_key = raw_key.trim();
    let value = raw_value.trim();
    if !option_name_ok(raw_key) {
        return if raw_key.starts_with('"') || raw_key.starts_with('\'') {
            TopLevelAction::Continue { retain: true, next_state: multiline_value_state(value, lines, idx + 1), entry: None }
        } else {
            TopLevelAction::Stop
        };
    }

    let key = option_key(raw_key);
    if key.is_empty() {
        return TopLevelAction::Stop;
    }
    top_level_keys.insert(key.clone());

    if let Some(next_state) = multiline_value_state(value, lines, idx + 1) {
        TopLevelAction::Continue { retain: true, next_state: Some(next_state), entry: Some(RetainedYamlEntry::Complex(key)) }
    } else if line.len() > MAX_OPTION_LINE || key.chars().count() > MAX_CHUNK_OPTION_CHARS || value.chars().count() > MAX_CHUNK_OPTION_CHARS {
        TopLevelAction::Continue { retain: true, next_state: None, entry: Some(RetainedYamlEntry::Complex(key)) }
    } else if wordcraft_doc::chunk::strip_yaml_comment(value).is_empty() {
        TopLevelAction::Continue { retain: false, next_state: None, entry: Some(RetainedYamlEntry::Complex(key)) }
    } else {
        TopLevelAction::Continue { retain: false, next_state: None, entry: Some(RetainedYamlEntry::Scalar(key, value.to_string())) }
    }
}

/// Inspect leading `#|` lines for complex YAML (block scalars, sequences, nested mappings,
/// multiline quoted scalars/flow collections) or options that exceed the document model's limits
/// (`MAX_OPTION_LINE`, `MAX_CHUNK_OPTION_CHARS`, or `MAX_CHUNK_OPTIONS` combined with `existing`).
/// When any such entry is present, returns the top-level scalar options and top-level keys so the
/// block can stay verbatim in `code` while WordCraft still sees scalar options like `eval: false`
/// and export avoids duplicating them.
pub(super) fn inspect_retained_yaml(text: &str, existing: &ChunkOptions) -> Option<RetainedYamlBlock> {
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
        let Some(yaml) = hash_pipe_yaml(line) else { break };
        if !matches!(state, YamlScanState::TopLevel) {
            let (next_state, consumed) = advance_multiline_state(state, yaml);
            state = next_state;
            if consumed {
                i += 1;
            }
            continue;
        }

        match parse_top_level_yaml_line(line, yaml, &lines, i, &mut top_level_keys) {
            TopLevelAction::Skip => {}
            TopLevelAction::Stop => break,
            TopLevelAction::Continue { retain, next_state, entry } => {
                must_retain |= retain;
                if let Some(next) = next_state {
                    state = next;
                }
                if let Some(e) = entry {
                    entries.push(e);
                }
            }
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
pub(super) fn option_name_ok(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) && !name.starts_with('-')
}

/// The language and header options of a chunk header such as `r, echo=FALSE, fig.cap="A plot"`.
pub(super) fn parse_info(info: &str) -> (String, ChunkOptions) {
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
            } else if !option_key(rest).is_empty() {
                options.set("label", rest.chars().filter(|c| !c.is_control()).take(128).collect::<String>());
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
        if step_quoted(&mut quote, &mut escaped, c) {
            continue;
        }
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
    v.parse::<f64>().is_err()
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

fn apply_retained_options(retained: RetainedYamlBlock, options: &mut ChunkOptions) {
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
}

fn parse_simple_hash_pipe_line<'a>(line: &'a str, next_line: Option<&str>) -> Option<(String, &'a str)> {
    if line.len() > MAX_OPTION_LINE {
        return None;
    }
    let after_hash = line.trim_start().strip_prefix("#|")?;
    let trimmed_content = after_hash.trim();
    if trimmed_content.starts_with('-') {
        return None;
    }
    let (key, value) = trimmed_content.split_once(':')?;
    let key = option_key(key);
    let value = value.trim();
    if key.is_empty() || value.is_empty() || value.starts_with('|') || value.starts_with('>') {
        return None;
    }
    if let Some(next_after) = next_line.and_then(|l| l.trim_start().strip_prefix("#|"))
        && (next_after.starts_with("  ") || next_after.starts_with('\t'))
    {
        return None;
    }
    Some((key, value))
}

/// Strip the leading `#| key: value` lines, returning the code and the options they set.
pub(super) fn strip_option_lines(body: &str, mut options: ChunkOptions) -> (String, ChunkOptions) {
    // Keep complex or unstoreable YAML blocks verbatim in `code` for export, while still reading
    // top-level scalar options from that same block so WordCraft honours flags such as `eval: false`.
    if let Some(retained) = inspect_retained_yaml(body, &options) {
        apply_retained_options(retained, &mut options);
        return (body.to_string(), options);
    }

    let lines: Vec<&str> = body.split('\n').collect();
    let mut start = 0;
    while let Some(line) = lines.get(start).copied() {
        let Some((key, value)) = parse_simple_hash_pipe_line(line, lines.get(start + 1).copied()) else {
            break;
        };
        if options.try_set(key, value).is_err() {
            break;
        }
        start += 1;
    }
    if let Some(sep) = lines.get(start)
        && sep.is_empty()
        && lines.get(start + 1..).is_some_and(|rest| first_nonblank_is_hash_pipe(rest.iter().copied()))
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
pub(super) fn option_key(key: &str) -> String {
    let cleaned: String = key.trim().chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')).take(64).collect();
    if let Some(alias) = normalize_option_alias(&cleaned) { alias.to_string() } else { cleaned }
}

/// A language name as a chunk tag carries it.
fn normalize(language: &str) -> String {
    language.trim().to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '.')).take(32).collect()
}
