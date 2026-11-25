use super::codec::{TagOptions, decode_tag_options, percent_decode, unquote};
use super::*;
use crate::control::{ControlLock, MAX_CONTROL_TEXT};

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
fn chunk_options_are_bounded_and_try_set_validates() {
    let mut o = ChunkOptions::new();
    assert!(matches!(o.try_set("", "1"), Err(DocError::Invalid(_))));
    assert!(matches!(o.try_set("k".repeat(MAX_CHUNK_OPTION_CHARS + 1), "1"), Err(DocError::Invalid(_))));
    for i in 0..MAX_CHUNK_OPTIONS {
        o.try_set(format!("k{i}"), "1").unwrap();
    }
    assert_eq!(o.len(), MAX_CHUNK_OPTIONS);
    // Adding a new key beyond MAX_CHUNK_OPTIONS is rejected, while updating an existing key succeeds.
    assert!(matches!(o.try_set("extra", "1"), Err(DocError::Invalid(_))));
    o.try_set("k0", "2").unwrap();
    assert_eq!(o.get("k0"), Some("2"));

    // Oversized value is rejected without overwriting the existing valid entry.
    assert!(matches!(o.try_set("k0", "x".repeat(MAX_CHUNK_OPTION_CHARS + 1)), Err(DocError::Invalid(_))));
    assert_eq!(o.get("k0"), Some("2"));

    // `set` remains non-failing and leaves bounds intact.
    o.set("overflow", "1");
    assert_eq!(o.len(), MAX_CHUNK_OPTIONS);
    assert_eq!(o.get("overflow"), None);
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
fn chunk_options_deserialization_rejects_out_of_bounds_without_silent_corruption() {
    // Exceeding MAX_CHUNK_OPTIONS is rejected rather than silently dropping entries like `eval: false`.
    let mut too_many = std::collections::BTreeMap::new();
    for i in 0..(MAX_CHUNK_OPTIONS + 1) {
        too_many.insert(format!("opt{i}"), "val".to_string());
    }
    let json_many = serde_json::to_string(&too_many).unwrap();
    assert!(serde_json::from_str::<ChunkOptions>(&json_many).is_err());

    // Oversized values and empty keys are also rejected cleanly.
    let mut huge = std::collections::BTreeMap::new();
    huge.insert("huge".to_string(), "x".repeat(MAX_CHUNK_OPTION_CHARS + 1));
    assert!(serde_json::from_str::<ChunkOptions>(&serde_json::to_string(&huge).unwrap()).is_err());

    let mut empty_key = std::collections::BTreeMap::new();
    empty_key.insert(String::new(), "val".to_string());
    assert!(serde_json::from_str::<ChunkOptions>(&serde_json::to_string(&empty_key).unwrap()).is_err());

    // When a custom property violates bounds, `options_of` rejects it and falls back to `w:tag`
    // so `eval: false` on the control tag still protects against execution.
    let mut doc = Document::new();
    let mut chunk = CodeChunk::new("r", "unlink('data')");
    chunk.options.set("eval", "false");
    let id = doc.push_chunk(&chunk).unwrap();
    doc.set_custom_prop(&chunk_prop_name(id), &serde_json::to_string(&huge).unwrap());
    let loaded = doc.chunks();
    assert!(!loaded[0].options.eval(), "w:tag fallback must preserve eval: false when custom prop is rejected");
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
    assert!(doc.has_chunks(StoryRef::Body));
    assert!(doc.has_any_chunks());
    assert_eq!(doc.chunk_count(StoryRef::Body), 1);
    assert_eq!(doc.chunk_bounds(StoryRef::Body).len(), 1);
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
    assert!(!doc.has_chunks(StoryRef::Body));
    assert!(!doc.has_any_chunks());
    assert_eq!(doc.chunk_count(StoryRef::Body), 0);
    assert!(doc.chunk_bounds(StoryRef::Body).is_empty());
}

#[test]
fn lightweight_chunk_helpers_match_full_chunk_refs_and_skip_non_chunk_docs() {
    use crate::{Part, PartKind, table::Table};

    let empty = Document::from_text("plain paragraph\nsecond paragraph");
    assert!(!empty.has_chunks(StoryRef::Body));
    assert!(!empty.has_any_chunks());
    assert_eq!(empty.chunk_count(StoryRef::Body), 0);
    assert!(empty.chunk_bounds(StoryRef::Body).is_empty());

    let mut doc = Document::new();
    let mut c1 = CodeChunk::new("r", "x <- 1\ny <- 2");
    c1.options.set("label", "first");
    let id1 = doc.push_chunk(&c1).unwrap();

    let cell_blocks = doc.chunk_blocks(&CodeChunk::new("python", "z = 3")).unwrap();
    let mut table = Table::new(1, 1, 120.0);
    table.rows[0].cells[0].blocks = cell_blocks;
    doc.body.push(Arc::new(Block::Table(table)));

    let part_blocks = doc.chunk_blocks(&CodeChunk::new("r", "p <- 4")).unwrap();
    doc.parts.insert(1, Part { kind: PartKind::Header, blocks: part_blocks, ..Default::default() });

    assert!(doc.has_chunks(StoryRef::Body));
    assert!(doc.has_chunks(StoryRef::Part(1)));
    assert!(doc.has_any_chunks());

    let refs = doc.chunk_refs(StoryRef::Body);
    let bounds = doc.chunk_bounds(StoryRef::Body);
    assert_eq!(doc.chunk_count(StoryRef::Body), refs.len());
    assert_eq!(bounds.len(), refs.len());
    for (b, r) in bounds.iter().zip(&refs) {
        assert_eq!(b.id, r.chunk.id);
        assert_eq!(b.language, r.chunk.language);
        assert_eq!(b.start, r.start);
        assert_eq!(b.end, r.end);
        assert_eq!(doc.chunk_options_missing_at(&b.start), doc.chunk_options_missing(r));
    }

    let only_first = doc.chunk_ref_by_id(StoryRef::Body, id1).unwrap();
    assert_eq!(only_first, refs[0]);
    assert!(doc.chunk_ref_by_id(StoryRef::Body, 999_999).is_none());
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
fn copied_chunk_with_truncated_tag_keeps_priority_eval_option() {
    let mut doc = Document::new();
    let mut c = CodeChunk::new("r", "unlink('data')");
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

    let oversized = format!("wordcraft:chunk:r?k={}", "x".repeat(MAX_CHUNK_OPTION_CHARS + 1));
    assert!(matches!(decode_tag_options(&oversized), TagOptions::Truncated(_)));
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
    assert!(doc.chunk_options_missing(&after[0]), "copied chunk with truncated tag and no custom property must keep incomplete options after edit");
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

    // Expanding single-paragraph chunk to multi-line must keep enclosing markers on first/last paragraphs.
    assert!(doc.update_chunk(chunk_id, "r", "x <- 1\ny <- 2\nz <- 3").unwrap());
    assert_nested_controls(&doc, chunk_id, &outer, &repeating, 2);

    // Collapsing multi-line chunk back to single paragraph must preserve marker nesting.
    assert!(doc.update_chunk(chunk_id, "python", "z = 42").unwrap());
    assert_nested_controls(&doc, chunk_id, &outer, &repeating, 0);
}

#[test]
fn update_chunk_preserves_partially_spanning_enclosing_controls() {
    let mut doc = Document::new();
    let mark = CharProps::default();
    let outer = locked_outer_control();

    // Case 1: outer control starts on a preceding paragraph and closes on the chunk's last paragraph.
    let cid1 = doc.push_chunk(&CodeChunk::new("r", "x <- 1")).unwrap();
    {
        let p0 = doc.para_mut(StoryRef::Body, &crate::Path::top(0)).unwrap();
        p0.insert_object(0, InlineObject::ControlStart { control: Box::new(outer.clone()) }, &mark).unwrap();
        let p1 = doc.para_mut(StoryRef::Body, &crate::Path::top(1)).unwrap();
        p1.insert_object(p1.len(), InlineObject::ControlEnd, &mark).unwrap();
    }
    assert!(!doc.has_unbalanced_controls());
    assert!(doc.update_chunk(cid1, "r", "x <- 1\ny <- 2").unwrap());
    assert!(!doc.has_unbalanced_controls());
    let r1 = doc.control_ranges(StoryRef::Body);
    let out1 = r1.iter().find(|r| r.control.id == Some(9001)).expect("outer control preserved");
    assert_eq!(out1.start.path, crate::Path::top(0));
    assert_eq!(out1.end.path, crate::Path::top(2));
    assert_eq!(out1.control, outer);

    // Case 2: outer control starts on a preceding paragraph and closes on the chunk's first paragraph (multi-line chunk).
    let mut doc2 = Document::new();
    let cid2 = doc2.push_chunk(&CodeChunk::new("r", "a <- 1\nb <- 2\nc <- 3")).unwrap();
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

    // Case 3: outer control starting on the chunk's first paragraph and closing on a following paragraph.
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

#[test]
fn set_chunk_label_preserves_incomplete_options_on_legacy_chunk() {
    // Legacy chunk: no `?` query in tag and no `wordcraft.chunk.<id>` custom property.
    let mut doc = Document::new();
    let id = doc.push_chunk(&CodeChunk::new("r", "x <- 1")).unwrap();
    let start = doc.control_ranges(StoryRef::Body)[0].start.clone();
    doc.control_mut(&start).unwrap().tag = "wordcraft:chunk:r".to_string();
    doc.remove_custom_prop(&chunk_prop_name(id));

    let before = doc.chunk_refs(StoryRef::Body);
    assert!(doc.chunk_options_missing(&before[0]));
    assert_eq!(before[0].chunk.options.label(), None);

    // Renaming must persist the label without writing a complete custom property or clearing `!`.
    assert!(doc.set_chunk_label(id, Some("fig-legacy")).unwrap());
    assert_eq!(doc.custom_prop(&chunk_prop_name(id)), None);
    let after = doc.chunk_refs(StoryRef::Body);
    assert_eq!(after[0].chunk.options.label(), Some("fig-legacy"));
    assert!(doc.chunk_options_missing(&after[0]), "renaming a legacy chunk must not mark unknown options complete");
    let range = &doc.control_ranges(StoryRef::Body)[0];
    assert_eq!(range.control.tag, "wordcraft:chunk:r?!label=fig-legacy");
    assert_eq!(range.control.title, "R code chunk (fig-legacy)");

    // Subsequent source edits and clearing the label keep the incomplete state intact.
    assert!(doc.update_chunk(id, "python", "x = 2").unwrap());
    let after_edit = doc.chunk_refs(StoryRef::Body);
    assert_eq!(after_edit[0].chunk.options.label(), Some("fig-legacy"));
    assert!(doc.chunk_options_missing(&after_edit[0]));
    assert_eq!(doc.custom_prop(&chunk_prop_name(id)), None);

    assert!(doc.set_chunk_label(id, None).unwrap());
    let after_clear = doc.chunk_refs(StoryRef::Body);
    assert_eq!(after_clear[0].chunk.options.label(), None);
    assert!(doc.chunk_options_missing(&after_clear[0]));
    assert_eq!(doc.custom_prop(&chunk_prop_name(id)), None);
    assert_eq!(doc.control_ranges(StoryRef::Body)[0].control.tag, "wordcraft:chunk:python?!");

    // Legacy chunk with malformed JSON custom property stays incomplete and does not overwrite the property.
    let mut bad_prop_doc = Document::new();
    let bad_id = bad_prop_doc.push_chunk(&CodeChunk::new("r", "w <- 1")).unwrap();
    let bad_start = bad_prop_doc.control_ranges(StoryRef::Body)[0].start.clone();
    bad_prop_doc.control_mut(&bad_start).unwrap().tag = "wordcraft:chunk:r".to_string();
    bad_prop_doc.set_custom_prop(&chunk_prop_name(bad_id), "{not valid json");
    assert!(bad_prop_doc.set_chunk_label(bad_id, Some("fig-bad")).unwrap());
    assert_eq!(bad_prop_doc.custom_prop(&chunk_prop_name(bad_id)), Some("{not valid json"));
    assert!(bad_prop_doc.chunk_options_missing(&bad_prop_doc.chunk_refs(StoryRef::Body)[0]));

    // Incomplete truncated tag whose label overflows MAX_CONTROL_TEXT rejects rename cleanly.
    let mut tight_doc = Document::new();
    let tight_id = tight_doc.push_chunk(&CodeChunk::new("r", "y <- 1")).unwrap();
    let tight_start = tight_doc.control_ranges(StoryRef::Body)[0].start.clone();
    let prefix = "wordcraft:chunk:r?!";
    let pad = "a".repeat(MAX_CONTROL_TEXT - prefix.len() - "opt=".len());
    tight_doc.control_mut(&tight_start).unwrap().tag = format!("{prefix}opt={pad}");
    tight_doc.remove_custom_prop(&chunk_prop_name(tight_id));
    assert!(matches!(tight_doc.set_chunk_label(tight_id, Some("fig-too-long")), Err(DocError::Invalid(_))));
    assert_eq!(tight_doc.chunks()[0].options.label(), None);
    assert_eq!(tight_doc.custom_prop(&chunk_prop_name(tight_id)), None);
}

#[test]
fn set_chunk_label_enforces_max_options_and_updates_existing_in_place() {
    let mut id_doc = Document::new();
    let mut id_chunk = CodeChunk::new("r", "q <- 1");
    id_chunk.options.set("id", "old-id");
    for i in 0..(MAX_CHUNK_OPTIONS - 1) {
        id_chunk.options.set(format!("opt{i}"), "1");
    }
    assert_eq!(id_chunk.options.len(), MAX_CHUNK_OPTIONS);
    let id_alias_id = id_doc.push_chunk(&id_chunk).unwrap();
    let id_prop_before = id_doc.custom_prop(&chunk_prop_name(id_alias_id)).map(str::to_owned);
    assert!(matches!(id_doc.set_chunk_label(id_alias_id, Some("new-label")), Err(DocError::Invalid(_))));
    assert_eq!(id_doc.custom_prop(&chunk_prop_name(id_alias_id)), id_prop_before.as_deref());
    assert_eq!(id_doc.chunks()[0].options.label(), Some("old-id"));

    // Updating an existing `label` at capacity must succeed in place.
    let mut existing_doc = Document::new();
    let mut existing_chunk = CodeChunk::new("r", "z <- 1");
    existing_chunk.options.set("label", "fig-old");
    for i in 0..(MAX_CHUNK_OPTIONS - 1) {
        existing_chunk.options.set(format!("opt{i}"), "1");
    }
    assert_eq!(existing_chunk.options.len(), MAX_CHUNK_OPTIONS);
    let existing_id = existing_doc.push_chunk(&existing_chunk).unwrap();

    assert!(existing_doc.set_chunk_label(existing_id, Some("fig-updated")).unwrap());
    let updated = existing_doc.chunks();
    assert_eq!(updated[0].options.label(), Some("fig-updated"));
    assert_eq!(updated[0].options.len(), MAX_CHUNK_OPTIONS);
    assert_eq!(existing_doc.control_ranges(StoryRef::Body)[0].control.title, "R code chunk (fig-updated)");
    assert!(!existing_doc.set_chunk_label(existing_id, Some("fig-updated")).unwrap());

    // Clearing the label at capacity also works and frees one option slot.
    assert!(existing_doc.set_chunk_label(existing_id, None).unwrap());
    let cleared = existing_doc.chunks();
    assert_eq!(cleared[0].options.label(), None);
    assert_eq!(cleared[0].options.len(), MAX_CHUNK_OPTIONS - 1);
}

#[test]
fn chunk_blocks_with_ids_avoids_collisions_and_handles_overflow() {
    use crate::{Part, PartKind};

    let mut doc = Document::new();
    let mark = CharProps::default();

    // Body control with id = 1.
    let mut c1 = ContentControl::new(ControlKind::RichText);
    c1.id = Some(1);
    let mut p1 = Paragraph::with_text("existing", CharProps::default());
    p1.insert_object(0, InlineObject::ControlStart { control: Box::new(c1) }, &mark).unwrap();
    p1.insert_object(p1.len(), InlineObject::ControlEnd, &mark).unwrap();
    doc.body = vec![para_block(p1)];

    // Part control with id = 2.
    let mut c2 = ContentControl::new(ControlKind::RichText);
    c2.id = Some(2);
    let mut p2 = Paragraph::with_text("header", CharProps::default());
    p2.insert_object(0, InlineObject::ControlStart { control: Box::new(c2) }, &mark).unwrap();
    p2.insert_object(p2.len(), InlineObject::ControlEnd, &mark).unwrap();
    doc.parts.insert(1, Part { kind: PartKind::Header, blocks: vec![para_block(p2)], ..Default::default() });

    // Case-insensitive reserved property id = 3 and overflowing id = i64::MAX.
    doc.set_custom_prop("WORDCRAFT.CHUNK.3", "{\"eval\":\"false\"}");
    doc.set_custom_prop(&chunk_prop_name(i64::MAX), "{}");

    let mut ids = doc.control_id_allocator();
    let block_id = |blocks: &Blocks| {
        blocks.first().and_then(|b| b.as_para()).and_then(|p| p.objects.first()).and_then(|o| match o {
            InlineObject::ControlStart { control } => control.id,
            _ => None,
        })
    };

    // Colliding preferred id (1) gets the lowest free positive id (4) since i64::MAX is used.
    let mut colliding = CodeChunk::new("r", "a <- 1");
    colliding.id = Some(1);
    let b_a = doc.chunk_blocks_with_ids(&colliding, &mut ids).unwrap();
    assert_eq!(block_id(&b_a), Some(4));

    // Unused preferred id (10) is kept.
    let mut preferred = CodeChunk::new("r", "b <- 2");
    preferred.id = Some(10);
    let b_b = doc.chunk_blocks_with_ids(&preferred, &mut ids).unwrap();
    assert_eq!(block_id(&b_b), Some(10));

    // Next auto-allocated id continues at 5 without repeating i64::MAX.
    let b_c = doc.chunk_blocks_with_ids(&CodeChunk::new("r", "c <- 3"), &mut ids).unwrap();
    assert_eq!(block_id(&b_c), Some(5));

    // Invalid chunk leaves allocator and custom properties untouched.
    let props_before = doc.custom_props.len();
    assert!(doc.chunk_blocks_with_ids(&CodeChunk::new("r", format!("bad{OBJ}")), &mut ids).is_err());
    assert_eq!(doc.custom_props.len(), props_before);

    // Next valid chunk gets 6.
    let b_d = doc.chunk_blocks_with_ids(&CodeChunk::new("r", "d <- 4"), &mut ids).unwrap();
    assert_eq!(block_id(&b_d), Some(6));

    // External custom property addition is absorbed rather than overwritten.
    doc.set_custom_prop("wordcraft.chunk.7", "{\"label\":\"external\"}");
    let b_e = doc.chunk_blocks_with_ids(&CodeChunk::new("r", "e <- 5"), &mut ids).unwrap();
    assert_eq!(block_id(&b_e), Some(8));
    assert_eq!(doc.custom_prop("wordcraft.chunk.7"), Some("{\"label\":\"external\"}"));
    assert_eq!(doc.custom_prop("WORDCRAFT.CHUNK.3"), Some("{\"eval\":\"false\"}"));
}
