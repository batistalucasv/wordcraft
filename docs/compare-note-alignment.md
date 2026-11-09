# Compare note alignment under resource limits

> **Last reviewed:** 2026-11-08 · **Last updated:** 2026-11-08 · **Change:** independent native Mac acceptance of the existing bounded scoring fix (#5870 review); no readiness estimate changes.

A synthetic thirty-note comparison on Word for Mac 16.114.4 (26110224),
Office Home & Business 2021, French UI, macOS 27.0.1 arm64, keeps each note
paired when one note word and its reference paragraph's context word change.
Word Compare used word granularity, enabled footnotes/endnotes, a new output
document and AutoSave off. The minimal fixtures opened in compatibility mode;
the result was saved as current DOCX after Word's format-upgrade prompt.
The saved result has thirty footnote parts and
thirty unchanged body references. Each note contains one inserted word and
one deleted word, rather than a whole-note replacement.

## Reproduce with original text

Create thirty footnotes, indexed `i = 0..29`, each containing fifty words
`note{i:02}word{j:02}` for `j = 0..49`. In the revised copy, replace word 25
with `note{i:02}replacement`. Change each referencing body paragraph from
`Reference {i:02} original context.` to `Reference {i:02} revised context.`.
Compare the two synthetic documents with the settings above. No issue files
or Word-produced files are needed by the engine tests. Keep native output
local and uncommitted.

On the previous [#5870](https://github.com/storytold/wordcraft/pull/5870) head `8d8214fe`, the engine pairs only the first three notes once
its exact similarity budget runs out. The command result has 57 note stories:
27 whole deleted notes and 27 whole inserted notes. The regression now keeps
thirty notes with one-word revisions and unchanged body references, matching
the observed native case.

## Bounded scoring

The existing bounded scoring implementation is retained. The 200,000-cell
exact note budget charges the DP core after common prefix/suffix tokens are removed.
When that core no longer fits the remaining budget, forward/backward greedy scans
look ahead at most sixteen tokens. Each produces an ordered common subsequence;
only the larger count is used. Common edges are removed first, preventing overlap.
The 200-token summaries, 200-note run cap, 40,000 note-pair guard and pairing
threshold remain. Existing exact-key and reference-context paths remain.

This conservative fallback can undercount complex reordering or shifts beyond its
window. It preserves useful similarity for the measured minor-edit case without
treating an unordered bag of words as an exact match or increasing the quadratic
budget. This independent contribution adds tests and observations only, with no
production algorithm changes. It is not a claim of complete note-comparison parity.
Native endnotes and Windows/web were not measured.

Regressions in `cmd::tools::compare::note_alignment_budget_tests` cover budget
exhaustion, interior/edge edits, shifted references, unrelated inserted notes,
reordered tokens and the exact-LCS upper bound. The command-level
`compare_many_edited_notes_keeps_minor_note_revisions` uses original Rust-authored
fixtures. Existing note insertion/deletion, endnote, accept/reject and DOCX
round-trip tests remain separate guards.

## Revision history

| Date | Change | Summary |
| --- | --- | --- |
| 2026-11-08 | trivial | #5870 review: native thirty-footnote observation, original text recipe and conservative bounded scoring; no readiness change. |
