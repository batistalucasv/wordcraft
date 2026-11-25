# Code chunks (R and Python)

> **Last updated:** 2026-11-24 · The engine commands and unified run-planning API behind the chunk
> model (`wordcraft_doc::chunk`) and `wordcraft-kernel`: insert, plan, run one, run all, apply
> results, clear output, and restart.

A document can hold executable **code chunks** in the style of Quarto / R Markdown / Jupyter. A
chunk is ordinary document content, not a sidecar file:

- a block-level **content control** tagged `wordcraft:chunk:<language>?<options>` (or
  `wordcraft:chunk:<language>?` when empty) whose content is the code, one paragraph per line in
  the `CodeChunk` paragraph style (a shaded, bordered code box). Keeping a compact, bounded form of
  the Quarto options (`eval`, `echo`, `fig-cap`, `label`…) in `w:tag` (up to 1024 characters, with
  `eval` serialized first) makes options travel with the control when copied and pasted in Word
  under a fresh `w:id`;
- the full options JSON in the custom document property `wordcraft.chunk.<control id>`, which Word
  preserves on a normal round trip and which remains authoritative when options exceed the 1024-char
  `w:tag` budget;
- if a tagged chunk has neither a matching `wordcraft.chunk.<id>` property nor tag-carried options
  (for example a legacy `wordcraft:chunk:<language>` control copied in Word without `?` options, or
  an overflowed tag missing `eval`), `code.run_all` and `plan_run_all` refuse to run it rather than
  assuming `eval: true`;
- the model lives in `wordcraft_doc::chunk` and survives `.docx` Save/Open round trips.

R and Python run in an isolated subprocess with a timeout, and the run captures `stdout`,
`stderr`, the exit status, captured `wc_table()` tables, and (when requested) PNG and/or SVG plots.
The kernel crate is `wordcraft-kernel` (re-exported through `wordcraft_engine::chunks`); the engine
owns run planning, validation, transient output caching, and stale-apply protection.

## Commands

All commands are in the engine registry, so the ribbon, the shortcuts, the control channel, the
CLI (`wordcraft-cli run`), and MCP (`execute`) call them the same way.

| Command | Default shortcut | Params | What it does |
|---|---|---|---|
| `code.insert_chunk` | `Mod+Shift+Alt+I` | `{"language"?: "r"\|"python", "code"?: string, "options"?: object}` | Inserts a block-level chunk below the caret (or in the empty paragraph there) and puts the caret at its last line. `options` takes Quarto options by name; `eval: false` and `fig-cap: "…"` are written as given. |
| `code.run_chunk` | `Mod+Alt+Enter` | `{"chunkId"?: number, "timeoutMs"?: number, "plot"?: "png"\|"svg"\|"both"\|"none", "includePlot"?: bool, "includeTables"?: bool}` | Runs the chunk `chunkId`, or the chunk the caret is inside when `chunkId` is omitted. |
| `code.run_all` | `Mod+Shift+Alt+Enter` | same as `run_chunk` (without `chunkId`) plus `"stopOnError"?: bool` | Runs the body's chunks in document order. `eval: false` chunks are reported as skipped. Stops at the first failure unless `stopOnError` is `false`. |
| `code.restart_kernel` | — | `{}` | Forgets the recorded outputs and starts a new kernel generation. |
| `code.clear_output` | — | `{"chunkId"?: number, "all"?: bool}` | Forgets the output of `chunkId`, the chunk at the caret when omitted, or every chunk with `"all": true`. |
| `code.update_chunk` | — | `{"chunkId"?: number, "code"?: string, "language"?: "r"\|"python", "label"?: string}` | Updates the chunk source, language, or label in the document as an undoable edit while preserving Quarto options and section breaks. |
| `code.delete_chunk` | — | `{"chunkId"?: number}` | Deletes the block-level chunk control, its custom options property, and any cached output. Targeting a chunk requires a top-level body chunk, not a chunk inside a table cell. |
| `code.exit_below_chunk` | — | `{"chunkId"?: number}` | Moves the caret below a top-level body chunk, creating a trailing normal paragraph when it ends the body. |
| `code.insert_plot` | — | `{"chunkId"?: number, "plotIndex"?: number, "png"?: string, "svg"?: string, "width"?: number, "alt"?: string, "atCaret"?: bool}` | Inserts the chunk's PNG plot inline below the chunk (or at the caret when `"atCaret": true`). Targeting a chunk requires a top-level body chunk, not a chunk inside a table cell. |
| `code.insert_table` | — | `{"chunkId"?: number, "headers": [string], "rows"?: [[string]], "atCaret"?: bool}` | Inserts a formatted native Word table with a bold repeating header row below the chunk or at the caret. |
| `code.insert_output` | — | `{"chunkId"?: number, "text"?: string, "atCaret"?: bool}` | Inserts cached or explicit console output as `CodeOutput`-styled paragraphs below the chunk or at the caret. |

`timeoutMs` is clamped to 100…600000 (default 30000). `code` is capped at the model's 4 MiB chunk
limit; longer code is refused, never truncated.

### Why these shortcuts

RStudio uses Ctrl+Enter, Ctrl+Shift+Enter and Ctrl+Alt+I. In a word processor those belong to
Word: Ctrl+Enter is a page break (`text.pageBreak`), Ctrl+Shift+Enter a column break
(`text.columnBreak`) and Ctrl+Alt+I Print (`file.print`). The registry refuses duplicate
shortcuts, and Word parity outranks matching RStudio, so the chunk commands use free
combinations instead.

## Unified Run-Planning API & Outputs

Both synchronous engine commands and asynchronous front ends share one L4 planning and apply path in
`wordcraft_engine::chunks` / `wordcraft_engine::cmd::code`:

- `plan_chunk_run(&mut Session, &Value) -> Result<PlannedChunkRun, CmdError>` and
  `plan_run_all(&mut Session, &Value) -> Result<Vec<RunAllEntry>, CmdError>` validate automation
  confinement (`file_access::current().is_some()`), `chunk_options_missing`, `eval: false`,
  `CodeLanguage`, `timeoutMs`, and `PlotFormat`, and stamp `(doc_id, kernel_gen)` onto each
  `PlannedChunkRun`.
- `apply_chunk_run_result(&mut Session, &PlannedChunkRun, Result<ExecutionResult, String>) -> Result<ApplyChunkRunOutcome, CmdError>`
  guards against stale results arriving after switching/replacing the document (`doc_id`),
  restarting the kernel (`kernel_gen`), or deleting/editing the chunk.
- `wordcraft_engine::chunks` re-exports `wordcraft_kernel::{CodeLanguage, ExecutionRequest,
  ExecutionResult, KernelError, PlotFormat, PlotOutput, TabularOutput, execute,
  is_language_available}` so UI crates need no direct `wordcraft-kernel` dependency.

Execution in `code.run_chunk` / `code.run_all` is synchronous: the call returns when the process
ends or `timeoutMs` expires, and the kernel kills the interpreter then. The result is reported in
the command's JSON (`stdout`, `stderr`, `exitCode`, `success`, `timedOut`, `outputTruncated`,
`durationMs`, `hasPlot`, `plotCount`, `tableCount`), and the full output — plot bytes and captured
tables included — is kept in the session for the front end under the chunk's control id (bounded
to 64 MiB / 4,096 entries per open document). `includePlot` adds the SVG text and the PNG as
base64 to the JSON, and `includeTables` adds the captured `wc_table()` tables (streams are
truncated at 256 KiB in the report).

That output is **not** part of the document or its undo history. It is kept per open document and
forgotten by another run of the same chunk, `code.clear_output`, `code.restart_kernel`, Undo/Redo
removing or changing the chunk, and replacing the document (`set_document`). `code.restart_kernel`
reports the generation the front end can use to redraw.

When automation filesystem roots (`--automation-read-root` / `--automation-write-root`) are active,
`code.run_chunk`, `code.run_all`, `plan_chunk_run`, and `plan_run_all` are disabled because
interpreter subprocesses are not confined to those roots.

## Example

```sh
wordcraft-cli run --template sample \
  --cmd 'code.insert_chunk={"language":"python","code":"print(2 + 2)"}' \
  --cmd code.run_chunk \
  --save chunk.docx
```

```json
{"command": "code.run_all", "params": {"timeoutMs": 5000, "plot": "svg"}}
```

## Not in this command layer

- Persistent interpreter sessions that keep variables between chunks. Every run is a fresh process
  today, so `code.restart_kernel` drops stale outputs and bumps `generation`.
- Asynchronous UI execution & canvas mounting: engine commands stay synchronous for CLI/MCP, while
  `wordcraft-ui-egui` mounts interactive `CodeChunk` widgets over `Placed::Chunk` rectangles and
  dispatches interactive runs asynchronously off the UI thread using `plan_chunk_run`,
  `plan_run_all`, and `apply_chunk_run_result`.

## Tests

`crates/engine/src/tests/code.rs` covers insertion (including multi-line chunks, Quarto options,
refusing to nest in another content control), dispatch and enabling, the kernel request
(timeout, plot format), output storage and clearing, `eval: false`, `stopOnError`, unified
run-planning and stale-apply guards, and a `.docx` round trip with the chunk and its options
intact. Under `cfg(test)`, `ChunkState::new()` installs a non-spawning stub runner unless overridden
with `ChunkState::set_runner`, so tests (including command fuzzing in `hostile_params_never_panic`)
never spawn real `Rscript` or `python3` processes.
