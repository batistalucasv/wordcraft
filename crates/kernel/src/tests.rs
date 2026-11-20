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

#[cfg(target_os = "linux")]
fn process_is_running(pid: i32) -> std::io::Result<bool> {
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return Err(std::io::Error::new(ErrorKind::InvalidData, "unexpected /proc/<pid>/stat format"));
    };
    Ok(!rest.trim_start().starts_with('Z'))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_is_running(pid: i32) -> std::io::Result<bool> {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    match kill(Pid::from_raw(pid), None) {
        Ok(()) => Ok(true),
        Err(Errno::ESRCH) => Ok(false),
        Err(error) => Err(std::io::Error::other(error.to_string())),
    }
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
    let deadline = Instant::now() + Duration::from_secs(2);
    while process_is_running(pid).expect("liveness probe") {
        assert!(Instant::now() < deadline, "the timed-out group must not leave a descendant");
        thread::sleep(Duration::from_millis(20));
    }
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
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
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

    let r = prepare_wrapped_code(CodeLanguage::R, "1", true, PlotFormat::Both, win_png, win_svg);
    assert!(r.contains(r#".wc_plot_png <- "C:\\Users\\bob\\Temp\\wc.png""#));
    assert!(r.contains(r#".wc_plot_svg <- "C:\\Users\\bob\\Temp\\wc.svg""#));

    let py = prepare_wrapped_code(CodeLanguage::Python, "1", true, PlotFormat::Png, win_png, win_svg);
    assert!(py.contains(r#"plt.savefig("C:\\Users\\bob\\Temp\\wc.png", dpi=200"#));

    let unix_png = Path::new(r"/tmp/a\b/wc.png");
    let r = prepare_wrapped_code(CodeLanguage::R, "1", true, PlotFormat::Png, unix_png, unix_png);
    assert!(r.contains(r#".wc_plot_png <- "/tmp/a\\b/wc.png""#));
}
