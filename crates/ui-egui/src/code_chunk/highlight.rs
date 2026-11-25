//! Lightweight syntax highlighting for R and Python code chunks.

use craft::ui::style::{UiTokens, rgba_color};
use egui::Color32;
use wordcraft_engine::chunks::CodeLanguage;

/// Theme-derived palette for syntax-highlighted tokens.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CodeColours {
    text: Color32,
    keyword: Color32,
    builtin: Color32,
    function: Color32,
    string: Color32,
    number: Color32,
    comment: Color32,
}

impl CodeColours {
    pub(crate) fn new(tokens: &UiTokens) -> Self {
        let color = &tokens.color;
        Self {
            text: rgba_color(color.text.normal),
            keyword: rgba_color(color.accent.text),
            builtin: rgba_color(color.accent.text),
            function: rgba_color(color.status.info.fg),
            string: rgba_color(color.status.good.fg),
            number: rgba_color(color.status.warn.fg),
            comment: rgba_color(color.text.muted),
        }
    }

    pub(crate) fn of(self, kind: TokenKind) -> Color32 {
        match kind {
            TokenKind::Plain => self.text,
            TokenKind::Keyword => self.keyword,
            TokenKind::Builtin => self.builtin,
            TokenKind::Function => self.function,
            TokenKind::String => self.string,
            TokenKind::Number => self.number,
            TokenKind::Comment => self.comment,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenKind {
    Plain,
    Keyword,
    Builtin,
    Function,
    String,
    Number,
    Comment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Token<'a> {
    pub(crate) text: &'a str,
    pub(crate) kind: TokenKind,
}

/// Split code into coloured spans that together are the whole input. Single-line comments,
/// strings (including Python's triple-quoted ones), numbers, keywords, common builtins and calls
/// are marked; everything else is plain.
pub(crate) fn highlight(code: &str, language: CodeLanguage) -> Vec<Token<'_>> {
    let mut spans = Vec::new();
    let mut plain = 0;
    let mut i = 0;
    while i < code.len() {
        let Some((end, kind)) = next_token(code, i, language) else {
            let Some(c) = code.get(i..).and_then(|s| s.chars().next()) else { break };
            i += c.len_utf8();
            continue;
        };
        push_plain(&mut spans, code, &mut plain, i);
        push(&mut spans, code, i, end, kind);
        i = end;
        plain = i;
    }
    push_plain(&mut spans, code, &mut plain, code.len());
    merge_spans(code, spans)
}

fn next_token(code: &str, i: usize, language: CodeLanguage) -> Option<(usize, TokenKind)> {
    let rest = code.get(i..)?;
    let c = rest.chars().next()?;
    if c == '#' {
        let end = rest.find('\n').map_or(code.len(), |n| i + n);
        return Some((end, TokenKind::Comment));
    }
    if c == '"' || c == '\'' {
        let triple = language == CodeLanguage::Python && rest.starts_with(&format!("{c}{c}{c}"));
        return Some((string_end(code, i, c, triple), TokenKind::String));
    }
    if c.is_ascii_digit() && !ident_char_before(code, i) {
        let end = scan_while(code, i, |ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '_');
        return Some((end, TokenKind::Number));
    }
    if ident_start(c) {
        let end = scan_while(code, i, ident_char);
        let word = code.get(i..end).unwrap_or_default();
        let kind = classify_ident(code, end, word, language);
        return Some((end, kind));
    }
    None
}

fn classify_ident(code: &str, end: usize, word: &str, language: CodeLanguage) -> TokenKind {
    if keyword(word, language) {
        TokenKind::Keyword
    } else if builtin(word) {
        TokenKind::Builtin
    } else if code.get(end..).is_some_and(|s| s.trim_start().starts_with('(')) {
        TokenKind::Function
    } else {
        TokenKind::Plain
    }
}

/// Merge neighbouring spans of one kind so a plain run stays one section.
fn merge_spans<'a>(code: &'a str, spans: Vec<Token<'a>>) -> Vec<Token<'a>> {
    let mut merged: Vec<Token<'a>> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(last) if last.kind == span.kind => {
                let start = last.text.as_ptr() as usize - code.as_ptr() as usize;
                let end = span.text.as_ptr() as usize - code.as_ptr() as usize + span.text.len();
                if let Some(text) = code.get(start..end) {
                    last.text = text;
                }
            }
            _ => merged.push(span),
        }
    }
    merged
}

fn ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '.'
}

fn ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.'
}

fn ident_char_before(code: &str, i: usize) -> bool {
    code.get(..i).and_then(|s| s.chars().next_back()).is_some_and(ident_char)
}

fn scan_while(code: &str, from: usize, keep: impl Fn(char) -> bool) -> usize {
    let mut end = from;
    while let Some(c) = code.get(end..).and_then(|s| s.chars().next()) {
        if !keep(c) {
            break;
        }
        end += c.len_utf8();
    }
    end
}

fn string_end(code: &str, from: usize, quote: char, triple: bool) -> usize {
    let mut i = from + quote.len_utf8() * if triple { 3 } else { 1 };
    while let Some(c) = code.get(i..).and_then(|s| s.chars().next()) {
        if c == '\\' {
            i += c.len_utf8();
            if let Some(next) = code.get(i..).and_then(|s| s.chars().next()) {
                i += next.len_utf8();
            }
            continue;
        }
        if c == quote {
            if triple {
                let closing = format!("{quote}{quote}{quote}");
                if code.get(i..).is_some_and(|s| s.starts_with(&closing)) {
                    return i + 3;
                }
                i += c.len_utf8();
                continue;
            }
            return i + c.len_utf8();
        }
        if c == '\n' && !triple {
            return i;
        }
        i += c.len_utf8();
    }
    code.len()
}

fn push_plain<'a>(spans: &mut Vec<Token<'a>>, code: &'a str, plain: &mut usize, at: usize) {
    if at > *plain {
        push(spans, code, *plain, at, TokenKind::Plain);
    }
    *plain = at;
}

fn push<'a>(spans: &mut Vec<Token<'a>>, code: &'a str, from: usize, to: usize, kind: TokenKind) {
    if let Some(text) = code.get(from..to)
        && !text.is_empty()
    {
        spans.push(Token { text, kind });
    }
}

fn keyword(word: &str, language: CodeLanguage) -> bool {
    match language {
        CodeLanguage::Python => matches!(
            word,
            "False"
                | "None"
                | "True"
                | "and"
                | "as"
                | "assert"
                | "async"
                | "await"
                | "break"
                | "case"
                | "class"
                | "continue"
                | "def"
                | "del"
                | "elif"
                | "else"
                | "except"
                | "finally"
                | "for"
                | "from"
                | "global"
                | "if"
                | "import"
                | "in"
                | "is"
                | "lambda"
                | "match"
                | "nonlocal"
                | "not"
                | "or"
                | "pass"
                | "raise"
                | "return"
                | "try"
                | "while"
                | "with"
                | "yield"
        ),
        CodeLanguage::R => matches!(
            word,
            "NA" | "NA_character_"
                | "NA_integer_"
                | "NA_real_"
                | "NULL"
                | "NaN"
                | "TRUE"
                | "FALSE"
                | "Inf"
                | "break"
                | "else"
                | "for"
                | "function"
                | "if"
                | "in"
                | "next"
                | "repeat"
                | "return"
                | "while"
        ),
    }
}

fn builtin(word: &str) -> bool {
    matches!(
        word,
        "aes"
            | "c"
            | "cat"
            | "data.frame"
            | "dict"
            | "enumerate"
            | "float"
            | "ggplot"
            | "int"
            | "len"
            | "library"
            | "list"
            | "max"
            | "mean"
            | "min"
            | "plot"
            | "print"
            | "range"
            | "read.csv"
            | "seq"
            | "set"
            | "str"
            | "sum"
            | "summary"
            | "zip"
    )
}
