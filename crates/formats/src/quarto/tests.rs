use super::options::MAX_OPTION_LINE;
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
    let text = export(&doc).unwrap();
    assert!(text.starts_with("---\ntitle: \"Field methods\"\nformat: docx\n---\n"));
    assert!(text.contains(
        "```{r}\n#| echo: FALSE\n#| fig-cap: \"Species abundance\"\n#| label: fig-abundance\nabundance <- c(10, 25, 5)\nbarplot(abundance)\n```"
    ));
    assert!(text.contains("```{python}\n#| eval: false\n#| label: tbl-sites\nsites = [\"A\", \"B\"]\nprint(sites)\n```"));
    assert!(text.contains("Plan the season before going out."));
}

#[test]
fn export_import_is_stable() {
    let once = export(&import(SOURCE)).unwrap();
    let twice = export(&import(&once)).unwrap();
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
    let again = import(&export(&doc).unwrap());
    assert_eq!(again.chunks()[0].code, "a\n\nb");
}

#[test]
fn an_empty_chunk_round_trips() {
    let doc = import("```{r}\n```\n");
    assert_eq!(doc.chunks().len(), 1);
    assert_eq!(doc.chunks()[0].code, "");
    let again = import(&export(&doc).unwrap());
    assert_eq!(again.chunks().len(), 1);
    assert_eq!(again.chunks()[0].code, "");
}

#[test]
fn an_ordinary_code_fence_is_not_a_chunk() {
    let doc = import("```r\nx <- 1\n```\n");
    assert!(!doc.has_any_chunks());
    assert!(text_of(&doc).contains("x <- 1"));
}

#[test]
fn a_brace_fence_of_another_language_stays_code() {
    for source in ["```{julia}\n1 + 1\n```\n", "```{mermaid}\ngraph TD; A-->B\n```\n"] {
        let doc = import(source);
        assert!(!doc.has_any_chunks(), "{source}");
        assert!(text_of(&doc).contains("1 + 1") || text_of(&doc).contains("A-->B"), "{source}");
    }
}

#[test]
fn an_unclosed_chunk_fence_does_not_panic_or_lose_text() {
    let doc = import("```{r}\nx <- 1\n");
    assert!(!doc.has_any_chunks());
    assert!(text_of(&doc).contains("x <- 1"));
}

#[test]
fn a_long_fence_in_the_code_is_escaped() {
    let code = "`\u{0060}\u{0060}\u{0060}";
    let mut chunk = CodeChunk::new("r", code);
    chunk.options.set("label", "x");
    let mut doc = Document::new();
    doc.push_chunk(&chunk).unwrap();
    let out = export(&doc).unwrap();
    assert!(out.starts_with("~~~{r}") || out.starts_with("````{r}"), "{out}");
    assert_eq!(import(&out).chunks()[0].code, code);
}

#[test]
fn option_values_never_break_the_option_line() {
    let mut chunk = CodeChunk::new("r", "1");
    chunk.options.set("label", "a\nb");
    let mut doc = Document::new();
    doc.push_chunk(&chunk).unwrap();
    let out = export(&doc).unwrap();
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
    assert_eq!(export(&doc).unwrap().matches("title: T").count(), 1);
}

#[test]
fn prose_only_document_with_front_matter_exports_cleanly() {
    let source = "---\ntitle: Prose\n---\n\nJust a paragraph.\n";
    let doc = import(source);
    assert!(!doc.has_any_chunks());
    let exported = export(&doc).unwrap();
    assert!(exported.starts_with("---\ntitle: Prose\n---\n\n"));
    assert!(exported.contains("Just a paragraph."));
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
    let exported = export(&doc).unwrap();
    assert!(exported.contains("```{r}\n#| label: test\n#| fig-cap: |"));
}

#[test]
fn ordinary_fence_containing_chunk_header_remains_unparsed() {
    let source = "````markdown\nHere is an example:\n```{r}\nx <- 1\n```\n````\n";
    let doc = import(source);
    assert!(!doc.has_any_chunks());
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
    let exported = export(&doc).unwrap();
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
    let exported = export(&doc).unwrap();
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
    assert!(!doc.has_any_chunks());
    let text = text_of(&doc);
    assert!(text.contains("x = 1"));
    let exported = export(&doc).unwrap();
    assert!(!exported.contains("```{python}"));
}

#[test]
fn tab_indented_line_does_not_close_fence() {
    let source = "````markdown\n\t````\n```{r}\nx <- 1\n```\n````\n";
    let doc = import(source);
    assert!(!doc.has_any_chunks());
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
    let exported = export(&doc).unwrap();
    let imported = import(&exported);
    assert_eq!(imported.chunks().len(), 1);
    assert_eq!(imported.chunks()[0].code, code);
}

#[test]
fn source_comments_with_existing_blank_line_preserve_exact_source_and_do_not_accumulate() {
    let mut exports = Vec::new();
    for (code, with_option) in [
        ("#| note\n1", true),
        ("\n#| note\n1", true),
        ("\n\n#| note\n1", true),
        ("  \n#| note\n1", true),
        ("\n#| note\n1", false),
        ("\nx <- 1", true),
    ] {
        let mut chunk = CodeChunk::new("r", code);
        if with_option {
            chunk.options.set("echo", "false");
        }
        let mut doc = Document::new();
        doc.push_chunk(&chunk).unwrap();

        let exported1 = export(&doc).unwrap();
        let imported1 = import(&exported1);
        let chunks1 = imported1.chunks();
        assert_eq!(chunks1.len(), 1, "code={code:?} with_option={with_option}");
        assert_eq!(chunks1[0].code, code, "imported source must match exactly for {code:?}");
        assert_eq!(chunks1[0].options.get("echo"), with_option.then_some("false"));

        let exported2 = export(&imported1).unwrap();
        assert_eq!(exported1, exported2, "round trips must not accumulate blank lines for {code:?}");
        let imported2 = import(&exported2);
        assert_eq!(imported2.chunks()[0].code, code, "second import must preserve {code:?}");

        if with_option && matches!(code, "#| note\n1" | "\n#| note\n1") {
            exports.push(exported1);
        }
    }
    assert_ne!(exports[0], exports[1], "exported form must distinguish a source blank line from an inserted separator");
}

#[test]
fn backtick_line_with_backticks_in_info_string_does_not_open_fence() {
    let prose_then_chunk = "```inline``` starts a paragraph.\n\n```{r}\nx <- 1\n```\n";
    let doc = import(prose_then_chunk);
    let chunks = doc.chunks();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].code, "x <- 1");

    let chunk_span_in_prose = "```{r}``` opens an R chunk.\n\n```{r}\ny <- 2\n```\n";
    let doc = import(chunk_span_in_prose);
    let chunks = doc.chunks();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].code, "y <- 2");

    let four_backticks_span = "```` ```{r} ```` opens an R chunk.\n\n  ```code``` indented prose.\n\n```{r}\nz <- 3\n```\n";
    let doc = import(four_backticks_span);
    let chunks = doc.chunks();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].code, "z <- 3");

    // An inline code span at column 0 is a lazy continuation of a list item, not a fence that ends it.
    let list_with_inline_span = "- Item\n```inline```\n\n  ```{r}\n  x <- 1\n  ```\n";
    let doc = import(list_with_inline_span);
    assert!(!doc.has_any_chunks());
    assert_eq!(doc.body, crate::markdown::import(list_with_inline_span).body);

    // Valid ordinary fences (including tilde fences with backticks in their info string) still shield examples.
    for shielded in
        ["````markdown\n```{r}\nx <- 1\n```\n````\n", "````\n```{r}\nx <- 1\n```\n````\n", "~~~markdown `example`\n```{r}\nx <- 1\n```\n~~~\n"]
    {
        let doc = import(shielded);
        assert!(!doc.has_any_chunks(), "{shielded:?}");
    }
}

#[test]
fn retained_yaml_cell_options_export_without_preceding_blank_line() {
    let source = "```{python}\n#| fig-cap: |\n#|   Caption\n#| eval: false\nprint(1)\n```\n";
    let doc = import(source);
    let exported = export(&doc).unwrap();
    assert!(exported.contains("```{python}\n#| fig-cap: |"));
    assert!(!exported.contains("```{python}\n\n#| fig-cap: |"));
}

#[test]
fn source_starting_with_hash_pipe_without_options_round_trips_losslessly() {
    let chunk = CodeChunk::new("r", "#| label: literal\n1");
    let mut doc = Document::new();
    doc.push_chunk(&chunk).unwrap();
    let exported = export(&doc).unwrap();
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
    assert!(!doc.has_any_chunks(), "oversized chunk should not be kept as a CodeChunk control");
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
fn apply_chunks_moves_non_chunk_blocks_and_preserves_existing_control_and_property_ids() {
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
    let intro_block = para_block(body_para);
    let outro_block = para_block(Paragraph::with_text("outro", CharProps::default()));
    let intro_ptr = Arc::as_ptr(&intro_block);
    let outro_ptr = Arc::as_ptr(&outro_block);
    doc.body = vec![
        intro_block,
        para_block(Paragraph::with_text(&s1, CharProps::default())),
        para_block(Paragraph::with_text("x <- 1", CharProps::default())),
        para_block(Paragraph::with_text(&s2, CharProps::default())),
        para_block(Paragraph::with_text("y = 2", CharProps::default())),
        outro_block,
    ];

    let mut c1 = CodeChunk::new("r", "x <- 1");
    c1.options.set("label", "first");
    let mut c2 = CodeChunk::new("python", "y = 2");
    c2.id = Some(2); // collides with header part control
    c2.options.set("label", "second");

    apply_chunks(&mut doc, vec![Pending { sentinel: s1, chunk: c1 }, Pending { sentinel: s2, chunk: c2 }]);

    assert_eq!(Arc::as_ptr(&doc.body[0]), intro_ptr, "non-chunk block must be moved without reallocating");
    assert_eq!(Arc::as_ptr(doc.body.last().unwrap()), outro_ptr, "trailing non-chunk block must be moved");
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

    let exported = export(&doc).unwrap();
    assert!(exported.contains("```{python}\n#| fig-cap: |\n#|   Caption\n#| eval: false\nprint(1)\n```"), "{exported}");
    assert_eq!(exported.matches("#| eval:").count(), 1, "eval must not be emitted twice: {exported}");
    assert_eq!(export(&import(&exported)).unwrap(), exported, "round trip must be idempotent");
}

#[test]
fn retained_yaml_ignores_nested_and_block_scalar_lines_while_reading_top_level_scalars() {
    let block_scalar = "```{python}\n#| fig-cap: |\n#|   Example line:\n#|   eval: false\n#|\n#|   second paragraph\n#| echo: false\nprint(1)\n```\n";
    let doc = import(block_scalar);
    let chunks = doc.chunks();
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].options.eval(), "indented eval: false inside fig-cap block scalar must not disable eval");
    assert!(!chunks[0].options.echo(), "top-level echo: false after block scalar must be parsed");
    let exported = export(&doc).unwrap();
    assert_eq!(exported.matches("#| echo:").count(), 1, "{exported}");
    assert_eq!(export(&import(&exported)).unwrap(), exported);

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
    let exported = export(&doc).unwrap();
    assert_eq!(exported.matches("#| label:").count(), 1, "{exported}");
    assert_eq!(exported.matches("fig.width:").count(), 1, "{exported}");
    assert_eq!(exported.matches("fig-width:").count(), 0, "alias must not duplicate as canonical key: {exported}");
    assert_eq!(exported.matches("#| eval:").count(), 1, "{exported}");
    assert_eq!(export(&import(&exported)).unwrap(), exported);

    let multiline_quoted = "```{python}\n#| fig-cap: \"First line\n#| eval: false\"\n#| echo: false\nprint(1)\n```\n";
    let doc = import(multiline_quoted);
    let chunks = doc.chunks();
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].options.eval(), "eval: false inside multiline double-quoted caption must not disable eval");
    assert!(!chunks[0].options.echo());
    let exported = export(&doc).unwrap();
    assert_eq!(exported.matches("#| echo:").count(), 1, "{exported}");
    assert_eq!(export(&import(&exported)).unwrap(), exported);
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

    let exported = export(&doc).unwrap();
    assert!(exported.contains("```{r}\n#| echo: FALSE\n#| fig-cap: |\n#|   Multiline\n#|   caption\n#| eval: false\nplot(1)\n```"), "{exported}");
    assert_eq!(exported.matches("#| echo:").count(), 1, "{exported}");
    assert_eq!(exported.matches("#| eval:").count(), 1, "{exported}");
    assert_eq!(exported.matches("#| fig-cap:").count(), 1, "{exported}");
    assert_eq!(export(&import(&exported)).unwrap(), exported, "second round trip must be idempotent");
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
    let exported = export(&doc).unwrap();
    assert!(exported.contains(&overlong_line), "caption must survive export after edit");
    assert!(exported.contains("```{r}\n#| echo: FALSE\n#| eval: false\n#| fig-cap:"), "retained block must stay attached to options");
    assert_eq!(exported.matches("#| eval:").count(), 1, "eval must not be duplicated");
    assert_eq!(exported.matches("#| fig-cap:").count(), 1, "fig-cap must not be duplicated");
    assert_eq!(export(&import(&exported)).unwrap(), exported, "round trip must be idempotent");

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

    let exported = export(&doc).unwrap();
    assert!(exported.contains("#| opt-000: 0"));
    assert!(exported.contains("#| opt-259: 259"));
    assert_eq!(exported.matches("#| eval:").count(), 1);
    assert!(!exported.contains("```{r}\n\n#| opt-000:"));
    assert_eq!(export(&import(&exported)).unwrap(), exported);

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

    let combined_exported = export(&combined_doc).unwrap();
    assert!(combined_exported.contains("#| h000: 0"));
    assert!(combined_exported.contains("#| h249: 249"));
    assert!(combined_exported.contains("#| c000: 0"));
    assert!(combined_exported.contains("#| c009: 9"));
    assert_eq!(combined_exported.matches("#| eval:").count(), 1);
    assert!(!combined_exported.contains("\n\n#| c000:"));
    assert_eq!(export(&import(&combined_exported)).unwrap(), combined_exported);
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
    let exported = export(&doc).unwrap();
    assert!(exported.contains(&huge_line));
    assert_eq!(exported.matches("#| eval:").count(), 1);
    assert!(!exported.contains("```{r}\n\n#| fig-cap:"));
    assert_eq!(export(&import(&exported)).unwrap(), exported);

    // Non-YAML syntax (missing space after `:`) with an unstoreable value must still not be consumed.
    let compact_inner = "z".repeat(wordcraft_doc::chunk::MAX_CHUNK_OPTION_CHARS + 1);
    let compact_line = format!("#| fig-cap:\"{compact_inner}\"");
    let compact_source = format!("```{{r}}\n#| eval: false\n{compact_line}\nplot(1)\n```\n");
    let compact_doc = import(&compact_source);
    let compact_chunks = compact_doc.chunks();
    assert_eq!(compact_chunks.len(), 1);
    assert!(!compact_chunks[0].options.eval());
    assert!(compact_chunks[0].code.contains(&compact_line));
    assert!(export(&compact_doc).unwrap().contains(&compact_line));
}

#[test]
fn chunk_fence_inside_a_list_item_stays_in_the_list() {
    for source in [
        "- Analysis\n\n  ```{r}\n  x <- 1\n  ```\n\n- Next\n",
        "- Analysis\n  ```{r}\n  x <- 1\n  ```\n- Next\n",
        "1. Analysis\ncontinued lazily\n\n   ```{python}\n   x = 1\n   ```\n\n2. Next\n",
        "- Analysis\n\n  More about it.\n\n  ```{r}\n  x <- 1\n  ```\n",
    ] {
        let doc = import(source);
        assert!(!doc.has_any_chunks(), "{source:?}");
        // Left as written: the Markdown importer sees exactly the list it would without chunks.
        assert_eq!(doc.body, crate::markdown::import(source).body, "{source:?}");
    }
}

#[test]
fn chunk_fence_outside_a_list_is_a_chunk_without_the_fence_indent() {
    let doc = import("Intro\n\n  ```{python}\n  if x:\n      y = 1\n   z\n  ```\n");
    assert_eq!(doc.chunks().len(), 1);
    assert_eq!(doc.chunks()[0].code, "if x:\n    y = 1\n z");

    // An unindented fence ends a list, so it is a chunk even right after an item.
    let doc = import("- Analysis\n```{r}\nx <- 1\n```\n");
    assert_eq!(doc.chunks().len(), 1);
    assert_eq!(doc.chunks()[0].code, "x <- 1");

    // A list closed by an unindented paragraph does not hold a later indented fence.
    let doc = import("- Analysis\n\nAfter the list.\n\n  ```{r}\n  x <- 1\n  ```\n");
    assert_eq!(doc.chunks().len(), 1);
    assert_eq!(doc.chunks()[0].code, "x <- 1");
}

#[test]
fn list_markers_are_recognised() {
    for marker in ["- a", "* a", "+ a", "-", "1. a", "123456789) a", "1.\ta", "2.\n"] {
        assert!(is_list_marker(marker), "{marker:?}");
    }
    for text in ["-a", "***", "1234567890. a", "1:", "a. b", "", "```{r}"] {
        assert!(!is_list_marker(text), "{text:?}");
    }
}

#[test]
fn table_cell_chunk_is_reported_as_error_on_export() {
    use wordcraft_doc::{Block, CharProps, ContentControl, ControlKind, InlineObject, Table, para_block};

    let mut doc = Document::new();
    doc.body = vec![para_block(Paragraph::with_text("Intro", CharProps::default()))];
    doc.push_chunk(&CodeChunk::new("r", "x <- 1")).unwrap();

    let mut cell_chunk = CodeChunk::new("python", "y = 2");
    cell_chunk.options.set("eval", "false");
    cell_chunk.options.set("label", "tbl-chunk");
    let cell_blocks = doc.chunk_blocks(&cell_chunk).unwrap();

    let mut table = Table::new(1, 2, 100.0);
    table.rows[0].cells[0].blocks = cell_blocks;
    table.rows[0].cells[1].blocks = vec![para_block(Paragraph::with_text("note", CharProps::default()))];
    doc.body.push(Arc::new(Block::Table(table)));

    assert_eq!(doc.chunks().len(), 2, "model includes both top-level and table-cell chunks");
    let err = export(&doc).expect_err("table-cell chunk must not be silently dropped on Quarto export");
    assert!(err.contains("1 code chunk inside a table cell"), "{err}");

    for ext in ["qmd", "rmd"] {
        let err = crate::export(ext, &doc).expect("our format").expect_err("dispatch must report the loss");
        assert!(err.contains("table cell"), "{ext}: {err}");
        let err = crate::export_with(ext, &doc, &crate::ExportOptions { comments: true })
            .expect("our format")
            .expect_err("export_with must propagate the error");
        assert!(err.contains("table cell"), "{ext}: {err}");
    }

    // Multiple chunks across nested tables are all counted.
    let nested_blocks = doc.chunk_blocks(&CodeChunk::new("r", "z <- 3")).unwrap();
    let mut inner = Table::new(1, 1, 50.0);
    inner.rows[0].cells[0].blocks = nested_blocks;
    let mut outer = Table::new(1, 1, 100.0);
    outer.rows[0].cells[0].blocks = vec![Arc::new(Block::Table(inner))];
    doc.body.push(Arc::new(Block::Table(outer)));

    let err = export(&doc).expect_err("nested table chunks must be reported");
    assert!(err.contains("2 code chunks inside table cells"), "{err}");

    // Ordinary tables (even with non-chunk content controls) alongside top-level chunks still export cleanly.
    let mut plain_doc = Document::new();
    plain_doc.body = vec![para_block(Paragraph::with_text("Intro", CharProps::default()))];
    plain_doc.push_chunk(&CodeChunk::new("r", "x <- 1")).unwrap();
    let mut ctrl = ContentControl::new(ControlKind::RichText);
    ctrl.id = Some(42);
    ctrl.tag = "ordinary:control".to_string();
    ctrl.block = true;
    let mut cell_para = Paragraph::with_text("plain cell", CharProps::default());
    let mark = CharProps::default();
    cell_para.insert_object(0, InlineObject::ControlStart { control: Box::new(ctrl) }, &mark).unwrap();
    cell_para.insert_object(cell_para.len(), InlineObject::ControlEnd, &mark).unwrap();
    let mut plain_table = Table::new(1, 1, 100.0);
    plain_table.rows[0].cells[0].blocks = vec![para_block(cell_para)];
    plain_doc.body.push(Arc::new(Block::Table(plain_table)));

    let exported = export(&plain_doc).expect("plain table with top-level chunk should export");
    assert!(exported.contains("```{r}\nx <- 1\n```"), "{exported}");
    assert!(exported.contains("plain cell"), "{exported}");
}

#[test]
fn non_body_and_overlapping_chunks_are_reported_as_error_on_export() {
    use wordcraft_doc::{CharProps, ContentControl, ControlKind, InlineObject, Part, PartKind, para_block};

    // A chunk stored in a non-body story (e.g. a header part) cannot be represented in Quarto.
    let mut part_doc = Document::new();
    let part_blocks = part_doc.chunk_blocks(&CodeChunk::new("r", "hdr <- 1")).unwrap();
    part_doc.parts.insert(1, Part { kind: PartKind::Header, blocks: part_blocks, ..Default::default() });
    let err = export(&part_doc).expect_err("non-body chunk must not be silently dropped");
    assert!(err.contains("1 code chunk outside the document body"), "{err}");

    // Overlapping / nested top-level chunks cannot both be emitted as flat fenced blocks.
    let mut nested_doc = Document::new();
    let mark = CharProps::default();
    let mut outer = ContentControl::new(ControlKind::RichText);
    outer.id = Some(1);
    outer.tag = "wordcraft:chunk:r?".to_string();
    outer.block = true;
    let mut inner = ContentControl::new(ControlKind::RichText);
    inner.id = Some(2);
    inner.tag = "wordcraft:chunk:python?".to_string();
    inner.block = true;

    let mut p0 = Paragraph::with_text("outer <- 1", CharProps::default());
    p0.insert_object(0, InlineObject::ControlStart { control: Box::new(outer) }, &mark).unwrap();
    let mut p1 = Paragraph::with_text("inner = 2", CharProps::default());
    p1.insert_object(0, InlineObject::ControlStart { control: Box::new(inner) }, &mark).unwrap();
    p1.insert_object(p1.len(), InlineObject::ControlEnd, &mark).unwrap();
    p1.insert_object(p1.len(), InlineObject::ControlEnd, &mark).unwrap();
    nested_doc.body = vec![para_block(p0), para_block(p1)];

    assert_eq!(nested_doc.chunk_bounds(StoryRef::Body).len(), 2);
    let err = export(&nested_doc).expect_err("overlapping top-level chunks must not silently drop the inner chunk");
    assert!(err.contains("overlapping"), "{err}");
}
