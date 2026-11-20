# Code chunks (R and Python)

> **Last updated:** 2026-11-18 · The engine commands behind the chunk model (`wordcraft_doc::chunk`) and
> `wordcraft-kernel`: insert, run one, run all, clear output and restart. Written for issue 7 of
> `batistalucasv/wordcraft-kernel`.

A document can hold executable **code chunks** in the style of Quarto / R Markdown / Jupyter. A
chunk is ordinary document content, not a sidecar file:

- a block-level **content control** tagged `wordcraft:chunk:<language>` whose content is the code,
  one paragraph per line in the `CodeChunk` paragraph style (a shaded, bordered code box);
- its Quarto options (`eval`, `echo`, `fig-cap`, `label`…) in the custom document property
  `wordcraft.chunk.<control id>`, which Word preserves on a round trip;
- the model and the `.qmd`/`.Rmd` reader and writer live in `wordcraft_doc::chunk` and
  `wordcraft_formats::quarto`, so a chunk survives Save, Open, and a Quarto export.

R and Python run in an isolated subprocess with a timeout, and the run captures `stdout`,
`stderr`, the exit status and (when requested) a PNG and/or SVG plot. The kernel crate is
`wordcraft-kernel`; the engine only decides what to run and keeps the result.

## Commands

All commands are in the engine registry, so the ribbon, the shortcuts, the control channel, the
CLI (`wordcraft-cli run`) and MCP (`execute`) call them the same way.

| Command | Default shortcut | Params | What it does |
|---|---|---|---|
| `code.insert_chunk` | `Mod+Shift+Alt+I` | `{"language"?: "r"\|"python", "code"?: string, "options"?: object}` | Inserts a block-level chunk below the caret (or in the empty paragraph there) and puts the caret at its last line. `options` takes Quarto options by name; `eval: false` and `fig-cap: "…"` are written as given. |
| `code.run_chunk` | `Mod+Alt+Enter` | `{"chunkId"?: number, "timeoutMs"?: number, "plot"?: "png"\|"svg"\|"both"\|"none", "includePlot"?: bool, "includeTables"?: bool}` | Runs the chunk `chunkId`, or the chunk the caret is inside when `chunkId` is omitted. |
| `code.run_all` | `Mod+Shift+Alt+Enter` | same as `run_chunk` (without `chunkId`) plus `"stopOnError"?: bool` | Runs the body's chunks in document order. `eval: false` chunks are reported as skipped. Stops at the first failure unless `stopOnError` is `false`. |
| `code.restart_kernel` | — | `{}` | Forgets the recorded outputs and starts a new kernel generation. |
| `code.clear_output` | — | `{"chunkId"?: number, "all"?: bool}` | Forgets the output of `chunkId`, the chunk at the caret when omitted, or every chunk with `"all": true`. |
| `code.update_chunk` | — | `{"chunkId"?: number, "code": string, "language"?: "r"\|"python"}` | Updates the chunk source/language in the document as an undoable edit while preserving Quarto options and section breaks. |
| `code.delete_chunk` | — | `{"chunkId"?: number}` | Deletes the block-level chunk control, its custom options property, and any cached output. |
| `code.insert_plot` | — | `{"chunkId"?: number, "png"?: string, "svg"?: string, "width"?: number, "alt"?: string}` | Inserts the chunk's PNG plot inline below the chunk (and archives any lossless SVG payload in document media). When `chunkId` is omitted, uses the chunk at the caret if inside one; otherwise explicit `"png"` data inserts at the caret. |
| `code.insert_table` | — | `{"chunkId"?: number, "headers": [string], "rows"?: [[string]]}` | Inserts a formatted native Word table with a bold repeating header row. When `chunkId` is supplied or the caret is inside a chunk, inserts directly below that chunk; when `chunkId` is omitted outside a chunk, inserts at the caret in the caret's story. |

`timeoutMs` is clamped to 100…600000 (default 30000). `code` is capped at the model's 4 MiB chunk
limit; longer code is refused, never truncated.

### Why these shortcuts

RStudio uses Ctrl+Enter, Ctrl+Shift+Enter and Ctrl+Alt+I. In a word processor those belong to
Word: Ctrl+Enter is a page break (`text.pageBreak`), Ctrl+Shift+Enter a column break
(`text.columnBreak`) and Ctrl+Alt+I Print (`file.print`). The registry refuses duplicate
shortcuts, and Word parity outranks matching RStudio, so the chunk commands use free
combinations instead.

## Outputs

Execution is synchronous in the command: the call returns when the process ends or `timeoutMs`
expires, and the kernel kills the interpreter then. The result is reported in the command's JSON
(`stdout`, `stderr`, `exitCode`, `success`, `timedOut`, `outputTruncated`, `durationMs`, `hasPlot`,
`tableCount`), and the full output — plot bytes and captured tables included — is kept in the
session for the front end under the chunk's control id. `includePlot` adds the SVG text and the
PNG as base64 to the JSON, and `includeTables` adds the captured `wc_table()` tables (streams are
truncated at 256 KiB in the report).

That output is **not** part of the document or its undo history. It is kept per open document and
forgotten by another run of the same chunk, `code.clear_output`, `code.restart_kernel`, Undo/Redo
removing or changing the chunk, and replacing the document (`set_document`). `code.restart_kernel`
reports the generation the front end can use to redraw. Persisting output in the document ("Insert
permanently into document") belongs to the UI widget.

When automation filesystem roots (`--automation-read-root` / `--automation-write-root`) are active,
`code.run_chunk` and `code.run_all` are disabled because interpreter subprocesses are not confined
to those roots.

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

- Persistent interpreter sessions that keep variables between chunks
  (`batistalucasv/wordcraft-kernel#4`). Every run is a fresh process today, so
  `code.restart_kernel` can only drop stale outputs.
- Asynchronous UI execution & canvas mounting: engine commands stay synchronous for CLI/MCP,
  while `wordcraft-ui-egui` mounts interactive [`CodeChunk`] widgets over `Placed::Chunk`
  rectangles and dispatches interactive runs asynchronously off the UI thread.

## Tests

`crates/engine/src/tests/code.rs` covers insertion (including multi-line chunks, Quarto options,
refusing to nest in another content control), dispatch and enabling, the kernel request
(timeout, plot format), output storage and clearing, `eval: false`, `stopOnError`, and a `.docx`
round trip with the chunk and its options intact. The kernel is replaced by a test runner, so the
tests need neither R nor Python installed.