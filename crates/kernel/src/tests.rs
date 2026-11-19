use super::*;

#[test]
fn code_language_parsing() {
    assert_eq!("r".parse::<CodeLanguage>().unwrap(), CodeLanguage::R);
    assert_eq!("R".parse::<CodeLanguage>().unwrap(), CodeLanguage::R);
    assert_eq!("python".parse::<CodeLanguage>().unwrap(), CodeLanguage::Python);
    assert_eq!("py".parse::<CodeLanguage>().unwrap(), CodeLanguage::Python);
    assert_eq!("Python".parse::<CodeLanguage>().unwrap(), CodeLanguage::Python);
    assert!("javascript".parse::<CodeLanguage>().is_err());
}

#[test]
fn execution_request_defaults() {
    let req = ExecutionRequest::new(CodeLanguage::Python, "print('hello')");
    assert_eq!(req.language, CodeLanguage::Python);
    assert_eq!(req.timeout_ms, 30_000);
    assert!(req.capture_plots);
    assert_eq!(req.plot_format, PlotFormat::Png);
    assert_eq!(req.code, "print('hello')");

    let customized = req.with_timeout(5_000).with_plot_capture(false).with_plot_format(PlotFormat::Svg);
    assert_eq!(customized.timeout_ms, 5_000);
    assert!(!customized.capture_plots);
    assert_eq!(customized.plot_format, PlotFormat::Svg);
}

#[test]
fn python_basic_execution() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let req = ExecutionRequest::new(CodeLanguage::Python, "print('Hello from Python!')");
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert_eq!(result.exit_code, Some(0));
    assert!(result.stdout.contains("Hello from Python!"));
    assert!(!result.timed_out);
    assert!(result.plot_png.is_none());
    assert!(result.plot_svg.is_none());
}

#[test]
fn python_syntax_error_handled_safely() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let req = ExecutionRequest::new(CodeLanguage::Python, "def broken(: syntax error");
    let result = execute(&req).expect("execution failed");

    assert!(!result.success);
    assert_ne!(result.exit_code, Some(0));
    assert!(result.stderr.contains("SyntaxError"));
    assert!(!result.timed_out);
}

#[test]
fn python_timeout_kills_runaway_process() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    // Runaway infinite loop with 300ms timeout
    let req = ExecutionRequest::new(CodeLanguage::Python, "import time\nwhile True:\n    time.sleep(0.05)").with_timeout(300);
    let result = execute(&req).expect("execution failed");

    assert!(!result.success);
    assert!(result.timed_out);
}

#[test]
fn r_basic_execution() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let req = ExecutionRequest::new(CodeLanguage::R, "cat('Hello from R!\n')");
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert_eq!(result.exit_code, Some(0));
    assert!(result.stdout.contains("Hello from R!"));
    assert!(!result.timed_out);
    assert!(result.plot_png.is_none());
    assert!(result.plot_svg.is_none());
}

#[test]
fn r_syntax_error_handled_safely() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let req = ExecutionRequest::new(CodeLanguage::R, "this is not valid R syntax %%%");
    let result = execute(&req).expect("execution failed");

    assert!(!result.success);
    assert!(!result.timed_out);
}

#[test]
fn r_timeout_kills_runaway_process() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let req = ExecutionRequest::new(CodeLanguage::R, "repeat { Sys.sleep(0.05) }").with_timeout(300);
    let result = execute(&req).expect("execution failed");

    assert!(!result.success);
    assert!(result.timed_out);
}

#[test]
fn r_plot_png_capture() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = r#"
x <- 1:10
y <- x^2
plot(x, y, main = "Test Curve")
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code).with_plot_format(PlotFormat::Png);
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert!(result.plot_png.is_some(), "Expected plot PNG bytes to be captured");
    assert!(result.plot_svg.is_none());
    let png_bytes = result.plot_png.unwrap();
    // Valid PNG header: 0x89 'P' 'N' 'G'
    assert!(png_bytes.starts_with(&[0x89, b'P', b'N', b'G']));
}

#[test]
fn r_plot_svg_capture() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = r#"
x <- 1:10
y <- x^2
plot(x, y, main = "Test SVG Curve")
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code).with_plot_format(PlotFormat::Svg);
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert!(result.plot_png.is_none());
    assert!(result.plot_svg.is_some(), "Expected plot SVG string to be captured");
    let svg = result.plot_svg.unwrap();
    assert!(svg.contains("<svg"), "SVG output must contain '<svg'");
}

#[test]
fn r_plot_both_capture() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = r#"
x <- 1:10
y <- x^2
plot(x, y, main = "Test Both Curve")
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code).with_plot_format(PlotFormat::Both);
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert!(result.plot_png.is_some(), "Expected PNG bytes");
    assert!(result.plot_svg.is_some(), "Expected SVG string");
    assert!(result.plot_svg.unwrap().contains("<svg"));
}

#[test]
fn python_plot_png_capture() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let code = r#"
import matplotlib.pyplot as plt
plt.figure()
plt.plot([1, 2, 3], [10, 20, 30])
"#;
    let req = ExecutionRequest::new(CodeLanguage::Python, code).with_plot_format(PlotFormat::Png);
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    if let Some(png_bytes) = result.plot_png {
        assert!(png_bytes.starts_with(&[0x89, b'P', b'N', b'G']));
    }
    assert!(result.plot_svg.is_none());
}

#[test]
fn python_plot_svg_capture() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let code = r#"
import matplotlib.pyplot as plt
plt.figure()
plt.plot([1, 2, 3], [10, 20, 30])
"#;
    let req = ExecutionRequest::new(CodeLanguage::Python, code).with_plot_format(PlotFormat::Svg);
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert!(result.plot_png.is_none());
    if let Some(svg) = result.plot_svg {
        assert!(svg.contains("<svg"));
    }
}

#[test]
fn python_plot_both_capture() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let code = r#"
import matplotlib.pyplot as plt
plt.figure()
plt.plot([1, 2, 3], [10, 20, 30])
"#;
    let req = ExecutionRequest::new(CodeLanguage::Python, code).with_plot_format(PlotFormat::Both);
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    if result.plot_png.is_some() {
        assert!(result.plot_svg.is_some());
        assert!(result.plot_svg.unwrap().contains("<svg"));
    }
}

// --- Structured tabular capture -------------------------------------------------------------

fn python_has_pandas() -> bool {
    Command::new("python3")
        .args(["-c", "import pandas"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[test]
fn malformed_table_payloads_capture_nothing() {
    let tables = read_tables_from_reader(&b"not json"[..], MAX_TABLES_FILE_BYTES, MAX_TOTAL_CELLS);
    assert!(tables.is_empty(), "a broken payload is not a table");
    let truncated = read_tables_from_reader(&br#"{"tables":[{"headers":["a"],"rows":[["1"]"#[..], MAX_TABLES_FILE_BYTES, MAX_TOTAL_CELLS);
    assert!(truncated.is_empty(), "a truncated payload is not a table");
}

#[test]
fn a_table_payload_over_the_byte_budget_is_ignored() {
    let payload = r#"{"tables":[{"headers":["a"],"rows":[["1"]]}]}"#;
    let tables = read_tables_from_reader(payload.as_bytes(), 16, MAX_TOTAL_CELLS);
    assert!(tables.is_empty(), "the reader stops before parsing an oversized payload");
}

#[test]
fn rows_are_padded_and_extra_cells_are_dropped() {
    let payload = br#"{"tables":[{"headers":["a","b"],"rows":[["1"],["1","2","3"]]}]}"#;
    let tables = read_tables_from_reader(&payload[..], MAX_TABLES_FILE_BYTES, MAX_TOTAL_CELLS);
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].headers, vec!["a".to_owned(), "b".to_owned()]);
    assert_eq!(tables[0].rows, vec![vec!["1".to_owned(), String::new()], vec!["1".to_owned(), "2".to_owned()]], "every row has one cell per header");
}

#[test]
fn padding_may_not_exceed_the_aggregate_cell_budget() {
    let payload = br#"{"tables":[{"headers":["a","b"],"rows":[["1","2"],["3","4"]]}]}"#;
    let tables = read_tables_from_reader(&payload[..], MAX_TABLES_FILE_BYTES, 3);
    assert!(tables.is_empty(), "two rows of two columns project four cells, over a budget of three");
}

#[test]
fn extra_columns_and_long_cells_are_clamped() {
    let headers: Vec<String> = (0..(MAX_TABLE_COLUMNS + 8)).map(|index| format!("\"c{index}\"")).collect();
    let payload = format!(r#"{{"tables":[{{"headers":[{}],"rows":[]}}]}}"#, headers.join(","));
    let tables = read_tables_from_reader(payload.as_bytes(), MAX_TABLES_FILE_BYTES, MAX_TOTAL_CELLS);
    assert_eq!(tables[0].headers.len(), MAX_TABLE_COLUMNS);

    let long = "λ".repeat(MAX_CELL_CHARS + 10);
    let payload = format!(r#"{{"tables":[{{"headers":["x"],"rows":[["{long}"]]}}]}}"#);
    let tables = read_tables_from_reader(payload.as_bytes(), MAX_TABLES_FILE_BYTES, MAX_TOTAL_CELLS);
    assert_eq!(tables[0].rows[0][0].chars().count(), MAX_CELL_CHARS, "the cut never splits a character");
}

#[test]
fn a_table_without_headers_is_dropped() {
    let payload = br#"{"tables":[{"headers":[],"rows":[["1"]]}]}"#;
    let tables = read_tables_from_reader(&payload[..], MAX_TABLES_FILE_BYTES, MAX_TOTAL_CELLS);
    assert!(tables.is_empty(), "a table with no columns cannot be shown");
}

#[test]
fn an_old_result_payload_without_tables_still_deserializes() {
    let payload = br#"{"stdout":"","stderr":"","exit_code":null,"success":true,"timed_out":false,"plot_png":null,"plot_svg":null,"duration_ms":1}"#;
    let result: ExecutionResult = serde_json::from_slice(payload).expect("older payloads stay readable");
    assert!(result.tables.is_empty());
}

#[test]
fn script_paths_are_escaped_for_both_wrappers() {
    let literal = script_string_literal(Path::new("/tmp/a\"b\\c\nd"));
    assert_eq!(literal, r#""/tmp/a\"b\\c\nd""#);
}

#[test]
fn r_captures_a_data_frame_passed_to_wc_table() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    let code = r#"
wc_table(data.frame(species = c("setosa", "virginica"), n = c(50L, 50L)))
"#;
    let result = execute(&ExecutionRequest::new(CodeLanguage::R, code)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.tables.len(), 1, "one captured table");
    assert_eq!(result.tables[0].headers, vec!["species".to_owned(), "n".to_owned()]);
    assert_eq!(result.tables[0].rows, vec![vec!["setosa".to_owned(), "50".to_owned()], vec!["virginica".to_owned(), "50".to_owned()]]);
}

#[test]
fn r_keeps_meaningful_row_names_as_the_first_column() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    let code = r#"
wc_table(data.frame(estimate = 1.5, row.names = "(Intercept)"))
"#;
    let result = execute(&ExecutionRequest::new(CodeLanguage::R, code)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.tables[0].headers, vec![String::new(), "estimate".to_owned()]);
    assert_eq!(result.tables[0].rows, vec![vec!["(Intercept)".to_owned(), "1.5".to_owned()]]);
}

#[test]
fn r_captures_a_matrix_and_keeps_utf8_cells() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    let code = r#"
wc_table(matrix(c("São Paulo", "Brasília"), nrow = 1))
"#;
    let result = execute(&ExecutionRequest::new(CodeLanguage::R, code)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.tables[0].headers, vec!["V1".to_owned(), "V2".to_owned()]);
    assert_eq!(result.tables[0].rows, vec![vec!["São Paulo".to_owned(), "Brasília".to_owned()]], "accents survive the JSON payload");
}

#[test]
fn r_captures_tables_even_when_plots_are_off() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    let req = ExecutionRequest::new(CodeLanguage::R, "wc_table(data.frame(x = 7L))").with_plot_capture(false);
    let result = execute(&req).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert!(result.plot_png.is_none());
    assert_eq!(result.tables.len(), 1);
    assert_eq!(result.tables[0].rows, vec![vec!["7".to_owned()]]);
}

#[test]
fn r_survives_a_table_that_cannot_be_read() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    let result = execute(&ExecutionRequest::new(CodeLanguage::R, "wc_table(42)")).expect("execution failed");
    assert!(!result.success, "wc_table rejects what is not a table");
    assert!(result.tables.is_empty());
    assert!(result.stderr.contains("wc_table()"), "stderr: {}", result.stderr);
}

#[test]
fn python_captures_a_data_frame_passed_to_wc_table() {
    if !is_language_available(CodeLanguage::Python) || !python_has_pandas() {
        return;
    }
    let code = r#"
import pandas as pd
wc_table(pd.DataFrame({"n": [1, 2], "name": ["setosa", "virginica"]}))
"#;
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.tables.len(), 1);
    assert_eq!(result.tables[0].headers, vec!["n".to_owned(), "name".to_owned()]);
    assert_eq!(result.tables[0].rows, vec![vec!["1".to_owned(), "setosa".to_owned()], vec!["2".to_owned(), "virginica".to_owned()]]);
}

/// A stand-in for the pandas API `wc_table()` reads, so the wrapper's JSON path can be exercised
/// without pandas installed. The chunk installs it in `sys.modules` before calling `wc_table`.
const PY_FAKE_PANDAS: &str = r#"
import sys
import types

_fake = types.ModuleType("pandas")


class _RangeIndex:
    def __init__(self, length=0):
        self.length = length

    def __len__(self):
        return self.length

    def equals(self, other):
        return isinstance(other, _RangeIndex) and other.length == self.length


class _DataFrame:
    def __init__(self, columns, rows):
        self.columns = columns
        self.rows = rows
        self.index = _RangeIndex(len(rows))

    def __len__(self):
        return len(self.rows)

    @property
    def iloc(self):
        return self

    def __getitem__(self, key):
        return self

    def itertuples(self, index=False, name=None):
        for number, row in enumerate(self.rows):
            yield ((number,) + tuple(row)) if index else tuple(row)


_fake.RangeIndex = _RangeIndex
_fake.DataFrame = _DataFrame
_fake.isna = lambda value: False
sys.modules["pandas"] = _fake
"#;

/// Exercises the wrapper's own JSON path without pandas installed: the fake module stands in for
/// the DataFrame API that `wc_table` reads, so a missing import or a broken payload still fails.
#[test]
fn python_captures_a_table_through_the_wrapper_protocol() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }
    let code = format!("{PY_FAKE_PANDAS}\nwc_table(_DataFrame([\"n\", \"name\"], [[1, \"setosa\"], [2, \"virginica\"]]))\n");
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.tables.len(), 1, "the wrapper writes and reads the table payload");
    assert_eq!(result.tables[0].headers, vec!["n".to_owned(), "name".to_owned()]);
    assert_eq!(result.tables[0].rows, vec![vec!["1".to_owned(), "setosa".to_owned()], vec!["2".to_owned(), "virginica".to_owned()]]);
}

#[test]
fn python_keeps_earlier_tables_when_the_cell_budget_runs_out() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }
    let code = format!(
        r#"{PY_FAKE_PANDAS}
_wc_cells_budget = 7
wc_table(_DataFrame(["a", "b"], [[1, 3], [2, 4]]))
wc_table(_DataFrame(["c", "d"], [[1, 4], [2, 5], [3, 6]]))
wc_table(_DataFrame(["e", "f"], [[1, 3], [2, 4]]))
"#
    );
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.tables.len(), 2, "the earlier tables survive; the last does not fit");
    assert_eq!(result.tables[0].rows.len(), 2);
    assert_eq!(result.tables[1].rows.len(), 1, "the second table is truncated to the remaining budget");
}

#[test]
fn python_stops_converting_rows_at_the_byte_budget() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }
    let code = format!(
        r#"{PY_FAKE_PANDAS}
class _Counting:
    calls = 0

    def __init__(self, text):
        self.text = text

    def __str__(self):
        _Counting.calls += 1
        return self.text


_wc_bytes_budget = 100
rows = [[_Counting("aaaaaaaaaa"), _Counting("bbbbbbbbbb")] for _ in range(20)]
wc_table(_DataFrame(["a", "b"], rows))
print("converted", _Counting.calls)
"#
    );
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.tables[0].rows.len(), 1, "one row of 27 encoded bytes fits a 100-byte budget");
    assert!(result.stdout.contains("converted 4"), "only the kept row and the one that did not fit are converted: {}", result.stdout);
}

#[test]
fn python_counts_escaped_bytes_before_keeping_a_row() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }
    let code = format!(
        r#"{PY_FAKE_PANDAS}
class _Counting:
    calls = 0

    def __init__(self, text):
        self.text = text

    def __str__(self):
        _Counting.calls += 1
        return self.text


_wc_bytes_budget = 150
rows = [[_Counting(chr(1) * 10)] for _ in range(20)]
wc_table(_DataFrame(["a"], rows))
print("converted", _Counting.calls)
"#
    );
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.tables[0].rows.len(), 1, "ten control characters take 62 bytes once escaped");
    assert!(result.stdout.lines().any(|line| line.trim() == "converted 2"), "the row that did not fit is converted once: {}", result.stdout);
}

#[test]
fn python_does_not_touch_matplotlib_when_plots_are_off() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }
    let code = "import sys\nprint('loaded', 'matplotlib' in sys.modules)\n";
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code).with_plot_capture(false)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert!(result.stdout.contains("loaded False"), "the wrapper leaves the backend alone: {}", result.stdout);
}

#[test]
fn python_keeps_an_ordinary_statement_after_a_future_import_on_the_same_line() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }
    let code = "from __future__ import annotations; assert callable(wc_table)\nprint('same line ok')\n";
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert!(result.stdout.contains("same line ok"));
}

#[cfg(unix)]
#[test]
fn the_scratch_directory_is_private() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }
    let result =
        execute(&ExecutionRequest::new(CodeLanguage::Python, "import os\nprint(oct(os.stat('.').st_mode & 0o777))")).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.stdout.trim(), "0o700", "the run directory is owner-only");
}

#[test]
fn r_drops_a_table_whose_escaped_first_row_cannot_fit() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    let code = r#"
.wc_budget <- 220
wc_table(data.frame(x = paste(rep(intToUtf8(1), 30), collapse = "")))
wc_table(data.frame(y = "ok"))
"#;
    let result = execute(&ExecutionRequest::new(CodeLanguage::R, code)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.tables.len(), 1, "the oversized table is dropped and the small one is kept");
    assert_eq!(result.tables[0].headers, vec!["y".to_owned()]);
}

#[test]
fn python_keeps_a_leading_future_import_above_the_wrapper() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }
    let code = "from __future__ import annotations\n\ndef render(value: Later) -> Later:\n    return value\n\n\nclass Later:\n    pass\n\n\nprint('future ok')\n";
    for capture in [true, false] {
        let request = ExecutionRequest::new(CodeLanguage::Python, code).with_plot_capture(capture);
        let result = execute(&request).expect("execution failed");
        assert!(result.success, "plot capture {capture}: stderr: {}", result.stderr);
        assert!(result.stdout.contains("future ok"), "plot capture {capture}");
    }
}

#[test]
fn r_keeps_earlier_tables_when_the_cell_budget_runs_out() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    let code = r#"
.wc_cells_budget <- 7
wc_table(data.frame(a = 1:2, b = 3:4))
wc_table(data.frame(c = 1:3, d = 4:6))
wc_table(data.frame(e = 1:2, f = 3:4))
"#;
    let result = execute(&ExecutionRequest::new(CodeLanguage::R, code)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.tables.len(), 2, "the earlier tables survive; the last does not fit");
    assert_eq!(result.tables[0].rows.len(), 2);
    assert_eq!(result.tables[1].rows.len(), 1, "the second table is truncated to the remaining budget");
}

#[test]
fn python_keeps_a_module_docstring_before_a_future_import() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }
    let code = "\"\"\"A chunk with a docstring.\"\"\"\nfrom __future__ import annotations\n\ndef render(value: Later) -> Later:\n    return value\n\n\nclass Later:\n    pass\n\n\nprint('docstring ok')\n";
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert!(result.stdout.contains("docstring ok"));
}

#[test]
fn r_publishes_a_table_before_a_timeout() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    let code = "wc_table(data.frame(x = 1L))\nrepeat { Sys.sleep(0.05) }\n";
    let result = execute(&ExecutionRequest::new(CodeLanguage::R, code).with_timeout(400)).expect("execution failed");

    assert!(result.timed_out, "the chunk runs past its timeout");
    assert_eq!(result.tables.len(), 1, "the capture was published before the process was killed");
}

#[test]
fn r_keeps_rows_accepted_before_a_lower_budget() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    let code = r#"
wc_table(data.frame(x = "aaaaaaaaaa"))
.wc_budget <- 10
"#;
    let result = execute(&ExecutionRequest::new(CodeLanguage::R, code)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.tables.len(), 1, "the accepted capture is still published");
    assert_eq!(result.tables[0].rows.len(), 1, "the accepted row survives a later override");
}

#[test]
fn python_clamps_a_raised_cell_budget() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }
    let code = format!(
        r#"{PY_FAKE_PANDAS}
_wc_cells_budget = 250001
wc_table(_DataFrame(["a"], [[1]]))
rows = [[1] * 250 for _ in range(1000)]
wc_table(_DataFrame(["c" + str(i) for i in range(250)], rows))
print("done")
"#
    );
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.tables.len(), 2, "the clamp keeps the total inside the reader's budget");
    assert_eq!(result.tables[1].rows.len(), 999, "the second table is truncated to the remaining cells");
}

#[test]
fn python_allows_a_nested_capture_while_converting_a_cell() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }
    let code = format!(
        r#"{PY_FAKE_PANDAS}
class _Nested:
    def __init__(self, inner):
        self.inner = inner
        self.done = False

    def __str__(self):
        if not self.done:
            self.done = True
            wc_table(self.inner)
        return "outer"


_wc_cells_budget = 3
wc_table(_DataFrame(["a"], [[1]]))
wc_table(_DataFrame(["b"], [[_Nested(_DataFrame(["c"], [[2]]))]]))
print("tables", len(_wc_tables), "cells", _wc_cells)
"#
    );
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code).with_timeout(3_000)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.tables.len(), 3, "the nested capture and the outer one both fit");
    let cells: usize = result.tables.iter().map(|table| table.headers.len() * table.rows.len()).sum();
    assert_eq!(cells, 3, "the outer capture rechecked the remaining budget before committing");
    assert!(result.stdout.lines().any(|line| line.trim() == "tables 3 cells 3"), "{}", result.stdout);
}

#[test]
fn python_runs_a_chunk_that_starts_with_a_byte_order_mark() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }
    let code = "\u{feff}print('bom ok')\n";
    for capture in [true, false] {
        let request = ExecutionRequest::new(CodeLanguage::Python, code).with_plot_capture(capture);
        let result = execute(&request).expect("execution failed");
        assert!(result.success, "plot capture {capture}: stderr: {}", result.stderr);
        assert!(result.stdout.contains("bom ok"), "plot capture {capture}");
    }
}

#[test]
fn python_points_file_and_argv_at_the_chunk() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }
    let code = "import sys\nprint(__file__.endswith('chunk.py'), sys.argv[0].endswith('chunk.py'))\n";
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code).with_plot_capture(false)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert!(result.stdout.lines().any(|line| line.trim() == "True True"), "{}", result.stdout);
}

#[test]
fn r_publishes_a_table_with_columns_but_no_rows() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    let result = execute(&ExecutionRequest::new(CodeLanguage::R, "wc_table(data.frame(x = integer()))")).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.tables.len(), 1);
    assert_eq!(result.tables[0].headers, vec!["x".to_owned()]);
    assert!(result.tables[0].rows.is_empty());
}

#[test]
fn python_publishes_a_table_with_columns_but_no_rows() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }
    let code = format!("{PY_FAKE_PANDAS}\nwc_table(_DataFrame([\"x\"], []))\n");
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.tables.len(), 1);
    assert_eq!(result.tables[0].headers, vec!["x".to_owned()]);
    assert!(result.tables[0].rows.is_empty());
}
