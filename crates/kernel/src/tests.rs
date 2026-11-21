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

/// Matplotlib is optional; plot tests skip cleanly when it is not installed.
fn matplotlib_available() -> bool {
    execute(&ExecutionRequest::new(CodeLanguage::Python, "import matplotlib").with_plot_capture(false).with_timeout(15_000))
        .is_ok_and(|result| result.success)
}

#[test]
fn python_plot_png_capture() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }
    if !matplotlib_available() {
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
    let png_bytes = result.plot_png.expect("PNG plot missing with Matplotlib installed");
    assert!(png_bytes.starts_with(&[0x89, b'P', b'N', b'G']));
    assert!(result.plot_svg.is_none());
}

#[test]
fn python_plot_svg_capture() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }
    if !matplotlib_available() {
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
    let svg = result.plot_svg.expect("SVG plot missing with Matplotlib installed");
    assert!(svg.contains("<svg"));
    assert!(svg.trim_end().ends_with("</svg>"));
}

#[test]
fn python_plot_both_capture() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }
    if !matplotlib_available() {
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
    let png = result.plot_png.expect("PNG plot missing with Matplotlib installed");
    assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
    let svg = result.plot_svg.expect("SVG plot missing with Matplotlib installed");
    assert!(svg.contains("<svg"));
    assert!(svg.trim_end().ends_with("</svg>"));
}

#[test]
fn python_large_output_is_drained_while_running() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let req = ExecutionRequest::new(CodeLanguage::Python, "print('x' * 200_000)").with_plot_capture(false).with_timeout(10_000);
    let result = execute(&req).expect("execution failed");

    assert!(!result.timed_out, "large output must not cause a false timeout");
    assert!(result.success);
    assert!(!result.output_truncated);
    assert_eq!(result.stdout.trim_end().len(), 200_000);
}

#[test]
fn python_background_descendant_does_not_hold_the_pipes() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let code = r#"
import subprocess
import sys
subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
print("chunk finished")
"#;
    let req = ExecutionRequest::new(CodeLanguage::Python, code).with_plot_capture(false).with_timeout(10_000);
    let started = Instant::now();
    let result = execute(&req).expect("execution failed");

    assert!(started.elapsed() < Duration::from_secs(5), "a background descendant must not delay the chunk");
    assert!(!result.timed_out, "a background descendant must not extend the chunk");
    assert!(result.success);
    assert!(result.stdout.contains("chunk finished"));
}

#[test]
fn capture_retains_at_most_the_limit() {
    let sink = CaptureSink::new();
    let chunk = [b'x'; 128];
    for _ in 0..8 {
        sink.retain(&chunk, 256);
    }
    let captured = sink.snapshot();
    assert_eq!(captured.bytes.len(), 256);
    assert!(captured.truncated);
}

#[cfg(unix)]
#[test]
fn capture_keeps_draining_after_the_limit() {
    use std::io::Write;

    let (reader, mut writer) = std::io::pipe().expect("create pipe");
    set_pipe_nonblocking(&reader).expect("set pipe nonblocking");
    let capture = spawn_capture(reader, 256).expect("spawn capture");
    let payload = vec![b'x'; 200_000];
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let writer_thread = thread::spawn(move || {
        let result = writer.write_all(&payload);
        let _ = done_tx.send(result);
    });
    let written = done_rx.recv_timeout(Duration::from_secs(5)).expect("reader must keep draining");
    written.expect("write payload");
    writer_thread.join().expect("writer thread");

    let captured = capture.stop();
    assert_eq!(captured.bytes.len(), 256);
    assert!(captured.truncated);
}

struct PendingReader {
    first_read: Arc<AtomicBool>,
    has_data: Arc<AtomicBool>,
    payload: &'static [u8],
    delivered: bool,
}

impl Read for PendingReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if !self.first_read.swap(true, Ordering::AcqRel) {
            return Err(std::io::Error::from(ErrorKind::WouldBlock));
        }
        if !self.has_data.load(Ordering::Acquire) {
            return Err(std::io::Error::from(ErrorKind::WouldBlock));
        }
        if self.delivered {
            return Ok(0);
        }
        self.delivered = true;
        let read = self.payload.len().min(buffer.len());
        buffer[..read].copy_from_slice(&self.payload[..read]);
        Ok(read)
    }
}

#[test]
fn capture_drains_pending_output_after_cancel() {
    let sink = CaptureSink::new();
    let cancel = Arc::new(AtomicBool::new(false));
    let has_data = Arc::new(AtomicBool::new(false));
    let first_read = Arc::new(AtomicBool::new(false));
    let thread_sink = sink.clone();
    let thread_cancel = Arc::clone(&cancel);
    let thread_data = Arc::clone(&has_data);
    let thread_first = Arc::clone(&first_read);
    let handle = thread::spawn(move || {
        capture_into(
            PendingReader { first_read: thread_first, has_data: thread_data, payload: b"pending", delivered: false },
            &thread_sink,
            &thread_cancel,
            1024,
        )
    });

    while !first_read.load(Ordering::Acquire) {
        thread::yield_now();
    }
    cancel.store(true, Ordering::Release);
    has_data.store(true, Ordering::Release);
    handle.join().expect("reader thread");

    assert_eq!(sink.snapshot().bytes.as_slice(), b"pending");
}

struct EndlessReader;

impl Read for EndlessReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        buffer.fill(b'x');
        Ok(buffer.len())
    }
}

#[test]
fn capture_stops_draining_at_the_deadline() {
    let sink = CaptureSink::new();
    let cancel = Arc::new(AtomicBool::new(false));
    let thread_sink = sink.clone();
    let thread_cancel = Arc::clone(&cancel);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let handle = thread::spawn(move || {
        capture_into(EndlessReader, &thread_sink, &thread_cancel, 1024);
        let _ = done_tx.send(());
    });

    thread::sleep(Duration::from_millis(10));
    cancel.store(true, Ordering::Release);
    assert!(done_rx.recv_timeout(Duration::from_secs(1)).is_ok(), "drain must stop at the deadline");
    handle.join().expect("reader thread");
    assert_eq!(sink.snapshot().bytes.len(), 1024);
}

#[cfg(unix)]
fn process_is_running(pid: i32) -> std::io::Result<bool> {
    process_tree::is_pid_running(pid)
}

/// Fallback cleanup guard used only when a test fails, so an assertion panic never leaks a helper.
#[cfg(unix)]
struct TestPidGuard(Option<i32>);

#[cfg(unix)]
impl TestPidGuard {
    fn new(pid: i32) -> Self {
        Self(Some(pid))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

#[cfg(unix)]
impl Drop for TestPidGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            use nix::sys::signal::{Signal, kill};
            use nix::unistd::Pid;
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
    }
}

#[cfg(unix)]
fn assert_runner_terminated_pid(pid: i32, context: &str) {
    let mut guard = TestPidGuard::new(pid);
    let deadline = Instant::now() + Duration::from_secs(2);
    while process_is_running(pid).expect("liveness probe") {
        assert!(Instant::now() < deadline, "{context}");
        thread::sleep(Duration::from_millis(20));
    }
    guard.disarm();
}

#[cfg(unix)]
#[test]
fn python_timeout_kills_same_group_descendant() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let code = r#"
import subprocess
import sys
import time
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"])
print(child.pid, flush=True)
time.sleep(30)
"#;
    let started = Instant::now();
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code).with_plot_capture(false).with_timeout(2_000)).expect("execution failed");

    assert!(result.timed_out);
    assert!(started.elapsed() < Duration::from_secs(8), "timeout must not wait for the descendant");
    let pid: i32 = result.stdout.trim().parse().expect("descendant pid");
    assert_runner_terminated_pid(pid, "the timed-out group must not leave a descendant");
}

#[cfg(unix)]
#[test]
fn python_escaped_descendant_does_not_block_capture() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let code = r#"
import subprocess
import sys
import time
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"], start_new_session=True)
print(f"{child.pid} started", flush=True)
time.sleep(30)
"#;
    let started = Instant::now();
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code).with_plot_capture(false).with_timeout(2_000)).expect("execution failed");

    assert!(result.timed_out);
    assert!(result.stdout.contains("started"), "captured output must survive reader cancellation");
    assert!(started.elapsed() < Duration::from_secs(8), "an escaped descendant must not delay the chunk");

    let pid: i32 = result.stdout.split_whitespace().next().and_then(|value| value.parse().ok()).expect("escaped pid");
    assert_runner_terminated_pid(pid, "the runner must terminate helpers that escape the original process group");
}

#[cfg(unix)]
#[test]
fn python_normal_exit_kills_escaped_descendant() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let code = r#"
import subprocess
import sys
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"], start_new_session=True)
print(f"{child.pid} finished", flush=True)
"#;
    let started = Instant::now();
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code).with_plot_capture(false).with_timeout(10_000)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.exit_code, Some(0));
    assert!(!result.timed_out);
    assert!(result.stdout.contains("finished"), "stdout: {}", result.stdout);
    assert!(started.elapsed() < Duration::from_secs(5), "an escaped descendant must not delay normal exit");

    let pid: i32 = result.stdout.split_whitespace().next().and_then(|value| value.parse().ok()).expect("escaped pid");
    assert_runner_terminated_pid(pid, "normal exit must terminate escaped helpers");
}

#[cfg(unix)]
#[test]
fn python_timeout_kills_escaped_descendant_with_cleared_env() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let code = r#"
import subprocess
import sys
import time
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"], start_new_session=True, env={})
print(f"{child.pid} started", flush=True)
time.sleep(30)
"#;
    let started = Instant::now();
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code).with_plot_capture(false).with_timeout(2_000)).expect("execution failed");

    assert!(result.timed_out);
    assert!(result.stdout.contains("started"), "stdout: {}", result.stdout);
    assert!(started.elapsed() < Duration::from_secs(8), "an escaped helper with cleared env must not delay timeout");

    let pid: i32 = result.stdout.split_whitespace().next().and_then(|value| value.parse().ok()).expect("escaped pid");
    assert_runner_terminated_pid(pid, "timeout cleanup must terminate escaped helpers even when env is cleared");
}

#[cfg(unix)]
#[test]
fn python_double_forked_descendant_is_terminated() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let code = r#"
import os
import time
r_fd, w_fd = os.pipe()
pid = os.fork()
if pid == 0:
    os.close(r_fd)
    os.setsid()
    pid2 = os.fork()
    if pid2 == 0:
        os.write(w_fd, f"{os.getpid()}\n".encode())
        os.close(w_fd)
        time.sleep(30)
        os._exit(0)
    os.close(w_fd)
    os._exit(0)
os.close(w_fd)
os.waitpid(pid, 0)
grandchild_pid = os.read(r_fd, 64).decode().strip()
os.close(r_fd)
print(f"{grandchild_pid} detached", flush=True)
"#;
    let started = Instant::now();
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code).with_plot_capture(false).with_timeout(10_000)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.exit_code, Some(0));
    assert!(!result.timed_out);
    assert!(result.stdout.contains("detached"), "stdout: {}", result.stdout);
    assert!(started.elapsed() < Duration::from_secs(5), "a double-forked helper must not delay exit");

    let pid: i32 = result.stdout.split_whitespace().next().and_then(|value| value.parse().ok()).expect("grandchild pid");
    assert_runner_terminated_pid(pid, "double-forked descendants must be terminated by the runner");
}

#[test]
fn r_grid_plot_capture() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }

    let code = r#"
library(grid)
grid.newpage()
grid.circle(x = 0.5, y = 0.5, r = 0.3, gp = gpar(fill = "steelblue"))
"#;
    let req = ExecutionRequest::new(CodeLanguage::R, code).with_plot_format(PlotFormat::Png);
    let result = execute(&req).expect("execution failed");

    assert!(result.success);
    assert!(result.plot_png.is_some(), "grid-based plots (ggplot2/lattice) must be captured");
}

#[test]
fn scratch_directory_is_private_and_exclusive() {
    let path = std::env::temp_dir().join(format!("wordcraft-kernel-test-{}-{}", std::process::id(), SCRIPT_COUNTER.fetch_add(1, Ordering::Relaxed)));
    create_scratch_dir(&path).expect("create scratch dir");
    let _guard = TempDirGuard(path.clone());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path).expect("scratch dir metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "scratch dir must be private");
    }

    let second = create_scratch_dir(&path);
    assert!(second.is_err(), "an existing scratch dir must be rejected");
    assert_eq!(second.expect_err("second creation must fail").kind(), ErrorKind::AlreadyExists);
}

#[test]
fn temp_dir_guard_removes_non_empty_directory() {
    let path = std::env::temp_dir().join(format!("wordcraft-kernel-test-{}-{}", std::process::id(), SCRIPT_COUNTER.fetch_add(1, Ordering::Relaxed)));
    create_scratch_dir(&path).expect("create scratch dir");
    fs::write(path.join("user-output.csv"), b"a,b\n1,2\n").expect("write user file");

    {
        let _guard = TempDirGuard(path.clone());
    }

    assert!(!path.exists(), "the guard must remove non-empty directories");
}

#[test]
fn truncated_plot_artifacts_are_rejected() {
    let mut png = Vec::new();
    png.extend_from_slice(PNG_HEADER);
    png.extend_from_slice(&[0_u8; 128]);
    assert!(!is_complete_png(&png));
    png.extend_from_slice(PNG_IEND);
    assert!(is_complete_png(&png));

    let mut svg = String::from("<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\">");
    svg.push_str(&"<g/>".repeat(32));
    assert!(!is_complete_svg(&svg));
    svg.push_str("</svg>\n");
    assert!(is_complete_svg(&svg));
}

#[test]
fn plot_paths_are_escaped_as_string_literals() {
    let win_png = Path::new(r"C:\Users\bob\Temp\wc.png");
    let win_svg = Path::new(r"C:\Users\bob\Temp\wc.svg");
    let win_tables = Path::new(r"C:\Users\bob\Temp\wc.json");
    let win_user = Path::new(r"C:\Users\bob\Temp\chunk.py");

    let captured = CapturePaths { png: win_png, svg: win_svg, tables: win_tables, user: win_user };
    let r = prepare_wrapped_code(CodeLanguage::R, true, PlotFormat::Both, captured);
    assert!(r.contains(r#".wc_plot_png <- "C:\\Users\\bob\\Temp\\wc.png""#));
    assert!(r.contains(r#".wc_plot_svg <- "C:\\Users\\bob\\Temp\\wc.svg""#));

    let py = prepare_wrapped_code(CodeLanguage::Python, true, PlotFormat::Png, captured);
    assert!(py.contains(r#"plt.savefig("C:\\Users\\bob\\Temp\\wc.png", dpi=200"#));

    let unix_png = Path::new(r"/tmp/a\b/wc.png");
    let unix_tables = Path::new(r"/tmp/a\b/wc.json");
    let unix_user = Path::new(r"/tmp/a\b/chunk.R");
    let captured = CapturePaths { png: unix_png, svg: unix_png, tables: unix_tables, user: unix_user };
    let r = prepare_wrapped_code(CodeLanguage::R, true, PlotFormat::Png, captured);
    assert!(r.contains(r#".wc_plot_png <- "/tmp/a\\b/wc.png""#));
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
    // Two empty rows carry no cells themselves; padding them to two headers projects four cells.
    let payload = br#"{"tables":[{"headers":["a","b"],"rows":[[],[]]}]}"#;
    let tables = read_tables_from_reader(&payload[..], MAX_TABLES_FILE_BYTES, 3);
    assert!(tables.is_empty(), "padding two empty rows to two headers projects four cells, over a budget of three");
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
_wc_capture_limits(cells=7)
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


_wc_capture_limits(bytes_=100)
rows = [[_Counting("aaaaaaaaaa"), _Counting("bbbbbbbbbb")] for _ in range(20)]
wc_table(_DataFrame(["a", "b"], rows))
print("converted", _Counting.calls)
"#
    );
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.tables[0].rows.len(), 1, "one row of 27 encoded bytes fits a 100-byte budget");
    assert!(
        result.stdout.lines().any(|line| line.trim() == "converted 4"),
        "only the kept row and the one that did not fit are converted: {}",
        result.stdout
    );
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


_wc_capture_limits(bytes_=150)
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
    let result = execute(&ExecutionRequest::new(CodeLanguage::R, code).with_timeout(2_000)).expect("execution failed");

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
_wc_capture_limits(cells=250001)
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


_wc_capture_limits(cells=2)
wc_table(_DataFrame(["a"], [[1]]))
wc_table(_DataFrame(["b"], [[_Nested(_DataFrame(["c"], [[2]]))]]))
"#
    );
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code).with_timeout(3_000)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.tables.len(), 2, "the nested capture spends the last cell, so the outer one is dropped");
    let headers: Vec<&str> = result.tables.iter().map(|table| table.headers[0].as_str()).collect();
    assert_eq!(headers, vec!["a", "c"], "the outer table is the one that did not fit");
    let cells: usize = result.tables.iter().map(|table| table.headers.len() * table.rows.len()).sum();
    assert_eq!(cells, 2, "the outer capture rechecked the remaining budget before committing");
}

#[test]
fn r_prints_visible_results_with_or_without_plot_capture() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    for capture in [true, false] {
        let request = ExecutionRequest::new(CodeLanguage::R, "1 + 1\ninvisible(NULL)\n").with_plot_capture(capture);
        let result = execute(&request).expect("execution failed");
        assert!(result.success, "plot capture {capture}: stderr: {}", result.stderr);
        assert!(result.stdout.contains("[1] 2"), "plot capture {capture}: {}", result.stdout);
        assert_eq!(result.stdout.matches("[1]").count(), 1, "an invisible result is not printed: {}", result.stdout);
    }
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

#[test]
fn r_keeps_every_value_of_a_matrix_column() {
    if !is_language_available(CodeLanguage::R) {
        return;
    }
    let code = "wc_table(data.frame(x = I(matrix(1:4, nrow = 2))))";
    let result = execute(&ExecutionRequest::new(CodeLanguage::R, code)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.tables[0].headers, vec!["x".to_owned()]);
    assert_eq!(
        result.tables[0].rows,
        vec![vec!["1, 3".to_owned()], vec!["2, 4".to_owned()]],
        "a matrix column keeps every value of the row instead of only the first"
    );
}

#[test]
fn python_chunk_names_do_not_shadow_the_capture_driver() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }
    let code = format!("{PY_FAKE_PANDAS}\nopen = lambda *args, **kwargs: None\n_wc_json = None\nwc_table(_DataFrame([\"a\"], [[1]]))\n");
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(result.tables.len(), 1, "the driver keeps its own builtins and state");
    assert_eq!(result.tables[0].rows, vec![vec!["1".to_owned()]]);
}

#[test]
fn a_missing_snapshot_falls_back_to_the_backup() {
    let dir = std::env::temp_dir().join(format!("wc-tables-bak-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join("tables.json");
    let backup = dir.join("tables.json.bak");
    fs::write(&backup, br#"{"tables":[{"headers":["x"],"rows":[["1"]]}]}"#).expect("backup");

    let tables = read_tables(&path);
    assert_eq!(tables.len(), 1, "the previous complete snapshot is used");
    assert_eq!(tables[0].rows, vec![vec!["1".to_owned()]]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_valid_empty_snapshot_wins_over_the_backup() {
    let dir = std::env::temp_dir().join(format!("wc-tables-empty-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join("tables.json");
    let backup = dir.join("tables.json.bak");
    fs::write(&path, br#"{"tables":[]}"#).expect("primary");
    fs::write(&backup, br#"{"tables":[{"headers":["x"],"rows":[["1"]]}]}"#).expect("backup");

    assert!(read_tables(&path).is_empty(), "an empty primary is valid and is not replaced by an older snapshot");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn python_registers_the_chunk_as_main_for_pickling() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }
    let code = "import pickle\n\n\nclass C:\n    pass\n\n\npickle.dumps(C())\nprint('pickled')\n";
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert!(result.stdout.contains("pickled"));
}

fn test_scratch_dir(prefix: &str) -> (PathBuf, TempDirGuard) {
    let path = std::env::temp_dir().join(format!("{prefix}-{}-{}", std::process::id(), SCRIPT_COUNTER.fetch_add(1, Ordering::Relaxed)));
    create_scratch_dir(&path).expect("create scratch dir");
    (path.clone(), TempDirGuard(path))
}

#[test]
fn bounded_plot_reads_reject_oversized_streams() {
    let limit = 256_u64;

    let mut exact_png = Vec::new();
    exact_png.extend_from_slice(PNG_HEADER);
    exact_png.resize(limit as usize - PNG_IEND.len(), 0);
    exact_png.extend_from_slice(PNG_IEND);
    assert_eq!(read_plot_png_from_reader(&exact_png[..], limit).as_deref(), Some(exact_png.as_slice()));

    let mut oversized_png = Vec::new();
    oversized_png.extend_from_slice(PNG_HEADER);
    oversized_png.resize(limit as usize + 1 - PNG_IEND.len(), 0);
    oversized_png.extend_from_slice(PNG_IEND);
    assert!(read_plot_png_from_reader(&oversized_png[..], limit).is_none());
    assert!(read_plot_png_from_reader(EndlessReader, limit).is_none());

    let svg_prefix = "<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\">";
    let svg_suffix = "</svg>";
    let mut exact_svg = String::from(svg_prefix);
    exact_svg.push_str(&" ".repeat(limit as usize - svg_prefix.len() - svg_suffix.len()));
    exact_svg.push_str(svg_suffix);
    assert_eq!(read_plot_svg_from_reader(exact_svg.as_bytes(), limit).as_deref(), Some(exact_svg.as_str()));

    let mut oversized_svg = String::from(svg_prefix);
    oversized_svg.push_str(&" ".repeat(limit as usize + 1 - svg_prefix.len() - svg_suffix.len()));
    oversized_svg.push_str(svg_suffix);
    assert!(read_plot_svg_from_reader(oversized_svg.as_bytes(), limit).is_none());
    assert!(read_plot_svg_from_reader(EndlessReader, limit).is_none());
}

#[test]
fn read_plot_files_reject_oversized_png_and_svg() {
    use std::io::{Seek, SeekFrom, Write};

    let (dir, _guard) = test_scratch_dir("wc-plot-cap-file");
    let png_path = dir.join("oversized.png");
    {
        let mut file = fs::File::create(&png_path).expect("create png");
        file.write_all(PNG_HEADER).expect("write png header");
        file.seek(SeekFrom::Start(MAX_PLOT_BYTES + 1 - PNG_IEND.len() as u64)).expect("seek png");
        file.write_all(PNG_IEND).expect("write png iend");
    }
    assert!(read_plot_png(&png_path, MAX_PLOT_BYTES).is_none(), "oversized PNG must be rejected");

    let svg_path = dir.join("oversized.svg");
    {
        let mut file = fs::File::create(&svg_path).expect("create svg");
        let prefix = b"<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\">";
        file.write_all(prefix).expect("write svg prefix");
        file.write_all(&[b' '; 32]).expect("write svg padding");
        file.seek(SeekFrom::Start(MAX_PLOT_BYTES + 1 - b"</svg>".len() as u64)).expect("seek svg");
        file.write_all(b"</svg>").expect("write svg close");
    }
    assert!(read_plot_svg(&svg_path, MAX_PLOT_BYTES).is_none(), "oversized SVG must be rejected");
}

#[test]
fn execute_rejects_oversized_plot_files() {
    if !is_language_available(CodeLanguage::Python) {
        return;
    }

    let code = format!(
        r#"with open("__wordcraft_plot__.png", "wb") as f:
    f.write(b"\x89PNG\r\n\x1a\n")
    f.seek({max_bytes} + 1 - 12)
    f.write(b"\x00\x00\x00\x00IEND\xaeB`\x82")

with open("__wordcraft_plot__.svg", "wb") as f:
    f.write(b'<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg">')
    f.write(b" " * 32)
    f.seek({max_bytes} + 1 - 6)
    f.write(b"</svg>")
"#,
        max_bytes = MAX_PLOT_BYTES
    );
    let result = execute(&ExecutionRequest::new(CodeLanguage::Python, code).with_plot_format(PlotFormat::Both)).expect("execution failed");

    assert!(result.success, "stderr: {}", result.stderr);
    assert!(result.plot_png.is_none(), "oversized PNG must be rejected by execute");
    assert!(result.plot_svg.is_none(), "oversized SVG must be rejected by execute");
}

#[cfg(unix)]
#[test]
fn which_binary_skips_non_executable_candidates() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let (dir, _guard) = test_scratch_dir("wc-which-binary");
    let non_exec_dir = dir.join("01-non-exec");
    let other_exec_only_dir = dir.join("02-other-exec-only");
    let dir_named_bin = dir.join("03-directory");
    let broken_link_dir = dir.join("04-broken-link");
    let link_to_non_exec_dir = dir.join("05-link-non-exec");
    let exec_dir = dir.join("06-exec");
    for sub in [&non_exec_dir, &other_exec_only_dir, &dir_named_bin, &broken_link_dir, &link_to_non_exec_dir, &exec_dir] {
        fs::create_dir(sub).expect("create path dir");
    }

    for language in [CodeLanguage::Python, CodeLanguage::R] {
        let name = language.binary_name();

        let non_exec = non_exec_dir.join(name);
        fs::write(&non_exec, b"#!/bin/sh\n").expect("write non-exec");
        fs::set_permissions(&non_exec, fs::Permissions::from_mode(0o644)).expect("chmod 0644");

        // Mode 0o641 sets only the other-execute bit while leaving the owner bits rw-.
        let other_exec_only = other_exec_only_dir.join(name);
        fs::write(&other_exec_only, b"#!/bin/sh\n").expect("write other-exec-only");
        fs::set_permissions(&other_exec_only, fs::Permissions::from_mode(0o641)).expect("chmod 0641");

        fs::create_dir(dir_named_bin.join(name)).expect("create dir named like binary");
        symlink(dir.join("missing-target"), broken_link_dir.join(name)).expect("create broken symlink");
        symlink(&non_exec, link_to_non_exec_dir.join(name)).expect("create symlink to non-exec");

        let exec_bin = exec_dir.join(name);
        fs::write(&exec_bin, b"#!/bin/sh\n").expect("write exec");
        fs::set_permissions(&exec_bin, fs::Permissions::from_mode(0o755)).expect("chmod 0755");

        // For unprivileged callers on an executable filesystem, owner permissions (rw-) reject
        // mode 0o641 even though its other-execute bit (--x) is set. Privileged callers (root or
        // CAP_DAC_OVERRIDE) can execute 0o641 directly, so include it only when execve denies it.
        let other_exec_denied = matches!(
            Command::new(&other_exec_only).stdout(Stdio::null()).stderr(Stdio::null()).status(),
            Err(ref err) if err.kind() == ErrorKind::PermissionDenied
        ) && !matches!(
            Command::new(&exec_bin).stdout(Stdio::null()).stderr(Stdio::null()).status(),
            Err(ref err) if err.kind() == ErrorKind::PermissionDenied
        );

        let mut bad_dirs = vec![&non_exec_dir, &dir_named_bin, &broken_link_dir, &link_to_non_exec_dir];
        if other_exec_denied {
            bad_dirs.insert(1, &other_exec_only_dir);
        }

        let only_bad_paths = std::env::join_paths(&bad_dirs).expect("join bad paths");
        assert_eq!(which_binary_in(language, &only_bad_paths), None, "{name}: non-executable candidates must be ignored");

        bad_dirs.push(&exec_dir);
        let all_paths = std::env::join_paths(&bad_dirs).expect("join all paths");
        assert_eq!(which_binary_in(language, &all_paths), Some(exec_bin), "{name}: search must fall through to the executable candidate");
    }
}

#[test]
fn captures_multiple_plots_in_creation_order() {
    if is_language_available(CodeLanguage::Python) && matplotlib_available() {
        let py_code = r#"
import matplotlib.pyplot as plt
fig1, ax1 = plt.subplots()
ax1.plot([0, 1], [0, 1])
fig2, ax2 = plt.subplots()
ax2.plot([0, 1], [1, 0])
"#;
        let res = execute(&ExecutionRequest::new(CodeLanguage::Python, py_code).with_plot_format(PlotFormat::Both)).expect("python execute");
        assert!(res.success, "stderr: {}", res.stderr);
        assert_eq!(res.plots.len(), 2, "must capture both Python figures");
        assert!(res.plots[0].png.is_some() && res.plots[0].svg.is_some());
        assert!(res.plots[1].png.is_some() && res.plots[1].svg.is_some());
        assert_eq!(res.plot_png.as_ref(), res.plots[0].png.as_ref());
    }

    if is_language_available(CodeLanguage::R) {
        let r_code = "plot(1:5, 1:5)\nplot(1:5, 5:1)\n";
        let res = execute(&ExecutionRequest::new(CodeLanguage::R, r_code).with_plot_format(PlotFormat::Png)).expect("r execute");
        assert!(res.success, "stderr: {}", res.stderr);
        assert_eq!(res.plots.len(), 2, "must capture both R plots");
        assert!(res.plots[0].png.is_some() && res.plots[1].png.is_some());
        assert_eq!(res.plot_png.as_ref(), res.plots[0].png.as_ref());
    }
}

#[cfg(unix)]
#[test]
fn captures_multiple_plots_skips_python_without_matplotlib_and_runs_r() {
    use std::os::unix::fs::PermissionsExt;

    let Some(real_python) = which_binary(CodeLanguage::Python) else {
        return;
    };
    let real_python = std::path::absolute(&real_python).expect("absolute python path");
    let real_r = which_binary(CodeLanguage::R).map(|path| std::path::absolute(&path).expect("absolute R path"));

    let (dir, _guard) = test_scratch_dir("wc-no-matplotlib");
    let dir = std::path::absolute(&dir).expect("absolute scratch dir");
    let py_marker = dir.join("python-calls");
    let py_shim = dir.join(CodeLanguage::Python.binary_name());
    fs::write(&py_shim, b"#!/bin/sh\nprintf 'call\\n' >> \"$WC_TEST_PY_MARKER\"\nexec \"$WC_TEST_REAL_PYTHON\" -I -S \"$@\"\n")
        .expect("write python shim");
    fs::set_permissions(&py_shim, fs::Permissions::from_mode(0o755)).expect("chmod python shim");

    let r_marker = dir.join("r-calls");
    if real_r.is_some() {
        let r_shim = dir.join(CodeLanguage::R.binary_name());
        fs::write(&r_shim, b"#!/bin/sh\nprintf 'call\\n' >> \"$WC_TEST_R_MARKER\"\nexec \"$WC_TEST_REAL_R\" \"$@\"\n").expect("write R shim");
        fs::set_permissions(&r_shim, fs::Permissions::from_mode(0o755)).expect("chmod R shim");
    }

    let existing_path = std::env::var_os("PATH").unwrap_or_default();
    let mut path_entries = vec![dir.clone()];
    path_entries.extend(std::env::split_paths(&existing_path));
    let shim_path = std::env::join_paths(path_entries).expect("join shim PATH");

    let test_bin = std::env::current_exe().expect("current test executable");
    let mut cmd = Command::new(test_bin);
    cmd.args(["--exact", "tests::captures_multiple_plots_in_creation_order", "--nocapture"])
        .env("PATH", shim_path)
        .env("WC_TEST_REAL_PYTHON", &real_python)
        .env("WC_TEST_PY_MARKER", &py_marker);
    if let Some(real_r) = &real_r {
        cmd.env("WC_TEST_REAL_R", real_r).env("WC_TEST_R_MARKER", &r_marker);
    }

    let output = cmd.output().expect("run captures_multiple_plots_in_creation_order without matplotlib");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "test must pass when Python lacks matplotlib\nstdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("1 passed"), "expected exact test to execute\nstdout:\n{stdout}\nstderr:\n{stderr}");

    let py_calls = fs::read_to_string(&py_marker).expect("python probe must have run").lines().count();
    assert_eq!(py_calls, 1, "only matplotlib_available() should invoke python3 when matplotlib is absent");

    if real_r.is_some() {
        let r_calls = fs::read_to_string(&r_marker).expect("R branch must still run when matplotlib is absent").lines().count();
        assert_eq!(r_calls, 1, "R multi-plot branch must run independently of matplotlib availability");
    }
}
