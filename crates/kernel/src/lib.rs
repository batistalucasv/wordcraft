//! Execution runner for interactive code chunks in WordCraft (R and Python).
//!
//! Provides isolated, safe subprocess execution with timeout enforcement,
//! output capture (stdout, stderr, exit status), and plot interception (PNG and vector SVG).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

use std::fmt;
use std::fs;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(target_family = "wasm")]
mod process_group;

#[cfg(target_family = "wasm")]
use crate::process_group::{CommandGroup, GroupChild};
#[cfg(not(target_family = "wasm"))]
use command_group::{CommandGroup, GroupChild};
use serde::{Deserialize, Serialize};

static SCRIPT_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Attempts to create a unique scratch directory before giving up.
const MAX_SCRATCH_DIR_ATTEMPTS: u64 = 10;
/// How long to wait for a killed process group to be reaped.
const PROCESS_GROUP_REAP_TIMEOUT: Duration = Duration::from_secs(2);
/// Most bytes retained from each stream; the rest is drained and discarded.
const MAX_CAPTURED_BYTES: usize = 10 * 1024 * 1024;
/// Size of each read from a child pipe.
const CAPTURE_CHUNK_BYTES: usize = 8 * 1024;
/// How long a cancellable reader sleeps between non-blocking reads.
const CAPTURE_POLL_INTERVAL: Duration = Duration::from_millis(5);
/// How long to keep draining queued output after cancellation.
const CAPTURE_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);
/// Grace for a blocking reader to see EOF after its group is killed.
#[cfg(not(unix))]
const CAPTURE_STOP_TIMEOUT: Duration = Duration::from_secs(1);
/// A complete PNG ends with the empty IEND chunk.
const PNG_HEADER: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
const PNG_IEND: &[u8] = &[0x00, 0x00, 0x00, 0x00, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82];

/// Programming language for a code chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodeLanguage {
    R,
    Python,
}

impl CodeLanguage {
    /// Canonical name of the language (lowercase).
    pub fn as_str(self) -> &'static str {
        match self {
            CodeLanguage::R => "r",
            CodeLanguage::Python => "python",
        }
    }

    /// Primary binary executable name searched on PATH.
    pub fn binary_name(self) -> &'static str {
        match self {
            CodeLanguage::R => "Rscript",
            CodeLanguage::Python => "python3",
        }
    }

    /// File extension for scripts written in this language.
    pub fn extension(self) -> &'static str {
        match self {
            CodeLanguage::R => "R",
            CodeLanguage::Python => "py",
        }
    }
}

impl fmt::Display for CodeLanguage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl FromStr for CodeLanguage {
    type Err = KernelError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "r" => Ok(CodeLanguage::R),
            "python" | "py" => Ok(CodeLanguage::Python),
            other => Err(KernelError::UnsupportedLanguage(other.to_string())),
        }
    }
}

/// Image format for plot/figure capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PlotFormat {
    /// High-resolution raster PNG image.
    #[default]
    Png,
    /// Lossless vector SVG graphic.
    Svg,
    /// Capture both PNG and SVG if generated.
    Both,
}

impl PlotFormat {
    /// Whether PNG capture is requested.
    pub fn captures_png(self) -> bool {
        matches!(self, PlotFormat::Png | PlotFormat::Both)
    }

    /// Whether SVG capture is requested.
    pub fn captures_svg(self) -> bool {
        matches!(self, PlotFormat::Svg | PlotFormat::Both)
    }
}

/// A table captured from a chunk through `wc_table()`, in R (`data.frame` or matrix) or Python
/// (`pandas.DataFrame`).
///
/// Every row has exactly `headers.len()` cells, padded with empty strings when the source was
/// ragged. When the source has meaningful row labels (R row names other than the default
/// sequence, Python index other than a `RangeIndex`), they are kept as an extra first column with
/// an empty header.
///
/// Capture is bounded: at most 16 tables per run, 10,000 rows and 256 columns per table, 4,096
/// characters per cell, and 250,000 cells across all tables. Anything past those limits is
/// dropped, and a payload larger than 64 MiB is ignored entirely.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TabularOutput {
    /// Column names in display order.
    pub headers: Vec<String>,
    /// Body rows, each with one text cell per header.
    pub rows: Vec<Vec<String>>,
}

/// Maximum number of tables kept from a single execution.
const MAX_TABLES: usize = 16;
/// Maximum number of body rows kept per table.
const MAX_TABLE_ROWS: usize = 10_000;
/// Maximum number of columns kept per table.
const MAX_TABLE_COLUMNS: usize = 256;
/// Maximum number of characters kept in a single cell.
const MAX_CELL_CHARS: usize = 4_096;
/// Maximum aggregate cell count across all tables, before padding rows.
const MAX_TOTAL_CELLS: usize = 250_000;
/// Maximum size of the table payload a script may hand back.
const MAX_TABLES_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Request to execute code in a specific language.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRequest {
    /// Target language (R or Python).
    pub language: CodeLanguage,
    /// Script code to execute.
    pub code: String,
    /// Maximum time in milliseconds before terminating the execution.
    pub timeout_ms: u64,
    /// Whether to attempt intercepting plots/figures generated by the script.
    pub capture_plots: bool,
    /// Desired format for captured plots (PNG, SVG, or Both).
    pub plot_format: PlotFormat,
    /// Optional working directory.
    pub working_dir: Option<PathBuf>,
}

impl ExecutionRequest {
    /// Create a new execution request with reasonable defaults (30s timeout, plot capture enabled, PNG format).
    pub fn new(language: CodeLanguage, code: impl Into<String>) -> Self {
        Self { language, code: code.into(), timeout_ms: 30_000, capture_plots: true, plot_format: PlotFormat::Png, working_dir: None }
    }

    /// Set a custom timeout in milliseconds.
    pub fn with_timeout(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    /// Set whether plots should be captured.
    pub fn with_plot_capture(mut self, capture: bool) -> Self {
        self.capture_plots = capture;
        self
    }

    /// Set the desired plot format.
    pub fn with_plot_format(mut self, format: PlotFormat) -> Self {
        self.plot_format = format;
        self
    }

    /// Set an explicit working directory.
    pub fn with_working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }
}

/// Result of executing a code chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
    /// Process exit code, or None if terminated by signal/timeout.
    pub exit_code: Option<i32>,
    /// Whether the process exited cleanly with code 0.
    pub success: bool,
    /// True if execution was halted due to timeout limit.
    pub timed_out: bool,
    /// True when stdout or stderr was cut at the capture limit.
    #[serde(default)]
    pub output_truncated: bool,
    /// PNG bytes of plot/chart if generated and captured.
    pub plot_png: Option<Vec<u8>>,
    /// Vector SVG XML string of plot/chart if generated and captured.
    pub plot_svg: Option<String>,
    /// Tables passed to `wc_table()`, in call order.
    #[serde(default)]
    pub tables: Vec<TabularOutput>,
    /// Elapsed wall-clock execution time in milliseconds.
    pub duration_ms: u64,
}

/// Error encountered during kernel operations.
#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    #[error("Language '{0}' is not supported")]
    UnsupportedLanguage(String),

    #[error("Interpreter for {language} ('{binary}') was not found in PATH")]
    InterpreterNotFound { language: CodeLanguage, binary: &'static str },

    #[error("I/O error during execution: {0}")]
    Io(String),

    #[error("Failed to prepare script: {0}")]
    ScriptPreparation(String),
}

/// Check if the interpreter for a given language is available on the host system.
pub fn is_language_available(language: CodeLanguage) -> bool {
    which_binary(language).is_some()
}

/// Search PATH for the language's binary.
pub fn which_binary(language: CodeLanguage) -> Option<PathBuf> {
    let name = language.binary_name();
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe_candidate = dir.join(format!("{name}.exe"));
            if exe_candidate.is_file() {
                return Some(exe_candidate);
            }
        }
    }
    None
}

/// Execute a code chunk and collect outputs.
pub fn execute(request: &ExecutionRequest) -> Result<ExecutionResult, KernelError> {
    let binary = which_binary(request.language)
        .ok_or_else(|| KernelError::InterpreterNotFound { language: request.language, binary: request.language.binary_name() })?;

    let mut created_dir = None;
    // The wrapper embeds these paths as string literals, so a lossy conversion would point the
    // script at a file that does not exist.
    let base = std::env::temp_dir();
    if base.to_str().is_none() {
        return Err(KernelError::Io("the temporary directory path is not valid UTF-8".to_owned()));
    }
    for _ in 0..MAX_SCRATCH_DIR_ATTEMPTS {
        let unique_id = SCRIPT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = base.join(format!("wordcraft-chunk-{}-{}", std::process::id(), unique_id));
        match create_scratch_dir(&candidate) {
            Ok(()) => {
                created_dir = Some(candidate);
                break;
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(KernelError::Io(format!("create temp dir {}: {error}", candidate.display()))),
        }
    }
    let temp_dir =
        created_dir.ok_or_else(|| KernelError::Io(format!("failed to create an exclusive temp dir after {MAX_SCRATCH_DIR_ATTEMPTS} attempts")))?;
    let _guard = TempDirGuard(temp_dir.clone());

    let plot_png_path = temp_dir.join("__wordcraft_plot__.png");
    let plot_svg_path = temp_dir.join("__wordcraft_plot__.svg");
    let tables_file = temp_dir.join("__wordcraft_tables__.json");
    let script_file = temp_dir.join(format!("script.{}", request.language.extension()));
    let user_file = temp_dir.join(format!("chunk.{}", request.language.extension()));

    // The chunk runs as its own file in both languages: R sources it with printing of visible
    // results, and Python compiles it as a module, so a docstring and a `__future__` import keep
    // the placement the language requires.
    fs::write(&user_file, request.code.as_bytes()).map_err(|e| KernelError::ScriptPreparation(format!("write {}: {e}", user_file.display())))?;

    let captured = CapturePaths { png: &plot_png_path, svg: &plot_svg_path, tables: &tables_file, user: &user_file };
    let wrapped_code = prepare_wrapped_code(request.language, request.capture_plots, request.plot_format, captured);
    fs::write(&script_file, wrapped_code.as_bytes()).map_err(|e| KernelError::ScriptPreparation(format!("write {}: {e}", script_file.display())))?;

    let mut cmd = Command::new(&binary);
    cmd.arg(&script_file);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    if let Some(ref dir) = request.working_dir {
        cmd.current_dir(dir);
    } else {
        cmd.current_dir(&temp_dir);
    }

    let start_time = Instant::now();
    let mut child = cmd.group_spawn().map_err(|e| KernelError::Io(format!("spawn {}: {e}", binary.display())))?;

    let (stdout_reader, stderr_reader) = match spawn_capture_pair(&mut child, MAX_CAPTURED_BYTES) {
        Ok(readers) => readers,
        Err(error) => {
            terminate_process_group(&mut child);
            return Err(KernelError::Io(format!("start output capture: {error}")));
        }
    };

    let (exit_status, timed_out) = wait_with_timeout(&mut child, Duration::from_millis(request.timeout_ms));
    let duration_ms = start_time.elapsed().as_millis().min(u64::MAX as u128) as u64;

    // Kill the whole group first: descendants may outlive the interpreter and
    // keep the pipes open. The readers are then stopped without losing bytes.
    terminate_process_group(&mut child);
    let stdout = stdout_reader.map(CaptureReader::stop).unwrap_or_default();
    let stderr = stderr_reader.map(CaptureReader::stop).unwrap_or_default();
    let output_truncated = stdout.truncated || stderr.truncated;
    let stdout = String::from_utf8_lossy(&stdout.bytes).into_owned();
    let stderr = String::from_utf8_lossy(&stderr.bytes).into_owned();

    let mut plot_png = None;
    if request.capture_plots && request.plot_format.captures_png() && plot_png_path.is_file() {
        plot_png = fs::read(&plot_png_path).ok().filter(|bytes| is_complete_png(bytes));
    }

    let mut plot_svg = None;
    if request.capture_plots && request.plot_format.captures_svg() && plot_svg_path.is_file() {
        plot_svg = fs::read_to_string(&plot_svg_path).ok().filter(|content| is_complete_svg(content));
    }

    let tables = read_tables(&tables_file);
    let exit_code = exit_status.and_then(|s| s.code());
    let success = exit_status.map(|s| s.success()).unwrap_or(false) && !timed_out;

    Ok(ExecutionResult { stdout, stderr, exit_code, success, timed_out, output_truncated, plot_png, plot_svg, tables, duration_ms })
}

/// Create the per-run scratch directory exclusively, with owner-only permissions.
fn create_scratch_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = fs::DirBuilder::new();
    builder.create(path)
}

/// Removes the scratch directory recursively on every exit path.
struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Bytes captured from one stream, plus whether the limit was reached.
#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}

/// Shared sink: reads and snapshots go through one mutex, so the bytes and
/// their truncation flag are always consistent.
#[derive(Clone)]
struct CaptureSink {
    state: Arc<Mutex<CaptureState>>,
}

#[derive(Default)]
struct CaptureState {
    bytes: Vec<u8>,
    truncated: bool,
}

impl CaptureSink {
    fn new() -> Self {
        Self { state: Arc::new(Mutex::new(CaptureState::default())) }
    }

    fn retain(&self, chunk: &[u8], limit: usize) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let remaining = limit.saturating_sub(state.bytes.len());
        if chunk.len() > remaining {
            state.bytes.extend_from_slice(&chunk[..remaining]);
            state.truncated = true;
        } else {
            state.bytes.extend_from_slice(chunk);
        }
    }

    fn snapshot(&self) -> Captured {
        let state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        Captured { bytes: state.bytes.clone(), truncated: state.truncated }
    }
}

/// A running reader plus the handles needed to stop it.
struct CaptureReader {
    sink: CaptureSink,
    cancel: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl CaptureReader {
    /// Stop the reader, close its pipe, and return everything captured so far.
    fn stop(self) -> Captured {
        self.cancel.store(true, Ordering::Release);
        #[cfg(unix)]
        {
            // Non-blocking reads observe the flag within one poll interval.
            let _ = self.handle.join();
        }
        #[cfg(not(unix))]
        {
            // A killed job closes the pipe, so the blocking reader sees EOF.
            // A breakaway process outside the job can keep it open: bound the
            // wait and detach, keeping the bytes already captured.
            let deadline = Instant::now() + CAPTURE_STOP_TIMEOUT;
            while !self.handle.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            if self.handle.is_finished() {
                let _ = self.handle.join();
            }
        }
        self.sink.snapshot()
    }
}

/// Start draining one pipe, retaining at most `limit` bytes.
fn spawn_capture<R: Read + Send + 'static>(pipe: R, limit: usize) -> std::io::Result<CaptureReader> {
    let sink = CaptureSink::new();
    let thread_sink = sink.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let thread_cancel = Arc::clone(&cancel);
    let handle =
        thread::Builder::new().name("wordcraft-output-capture".to_string()).spawn(move || capture_into(pipe, &thread_sink, &thread_cancel, limit))?;
    Ok(CaptureReader { sink, cancel, handle })
}

fn capture_into<R: Read>(mut pipe: R, sink: &CaptureSink, cancel: &AtomicBool, limit: usize) {
    let mut chunk = [0_u8; CAPTURE_CHUNK_BYTES];
    let mut drain_deadline = None;
    loop {
        if cancel.load(Ordering::Acquire) && drain_deadline.is_none() {
            drain_deadline = Some(Instant::now() + CAPTURE_DRAIN_TIMEOUT);
        }
        if let Some(deadline) = drain_deadline
            && Instant::now() >= deadline
        {
            return;
        }
        match pipe.read(&mut chunk) {
            Ok(0) => return,
            Ok(read) => sink.retain(&chunk[..read], limit),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(CAPTURE_POLL_INTERVAL);
            }
            Err(_) => return,
        }
    }
}

/// Take both pipes and start their readers.
fn spawn_capture_pair(child: &mut GroupChild, limit: usize) -> std::io::Result<(Option<CaptureReader>, Option<CaptureReader>)> {
    let stdout = child.inner().stdout.take();
    let stderr = child.inner().stderr.take();
    #[cfg(unix)]
    {
        if let Some(pipe) = &stdout {
            set_pipe_nonblocking(pipe)?;
        }
        if let Some(pipe) = &stderr {
            set_pipe_nonblocking(pipe)?;
        }
    }
    let stdout_reader = match stdout {
        Some(pipe) => Some(spawn_capture(pipe, limit)?),
        None => None,
    };
    let stderr_reader = match stderr {
        Some(pipe) => match spawn_capture(pipe, limit) {
            Ok(reader) => Some(reader),
            Err(error) => {
                if let Some(reader) = stdout_reader {
                    let _ = reader.stop();
                }
                return Err(error);
            }
        },
        None => None,
    };
    Ok((stdout_reader, stderr_reader))
}

#[cfg(unix)]
fn set_pipe_nonblocking(pipe: &impl std::os::fd::AsRawFd) -> std::io::Result<()> {
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    let flags = fcntl(pipe.as_raw_fd(), FcntlArg::F_GETFL).map_err(std::io::Error::other)?;
    let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    fcntl(pipe.as_raw_fd(), FcntlArg::F_SETFL(flags)).map(|_| ()).map_err(std::io::Error::other)
}

/// Kill the whole process group and reap it, so no descendant outlives the chunk.
fn terminate_process_group(child: &mut GroupChild) {
    let _ = child.kill();
    let deadline = Instant::now() + PROCESS_GROUP_REAP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) if Instant::now() >= deadline => return,
            Ok(None) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Wait for the interpreter with a timeout. The caller terminates the group afterwards.
fn wait_with_timeout(child: &mut GroupChild, timeout: Duration) -> (Option<ExitStatus>, bool) {
    let start = Instant::now();
    let step = Duration::from_millis(20);

    loop {
        // Wait for the direct child only: descendants are cleaned up afterwards.
        match child.inner().try_wait() {
            Ok(Some(status)) => return (Some(status), false),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    return (None, true);
                }
                thread::sleep(step);
            }
            Err(_) => return (None, false),
        }
    }
}

/// A PNG is complete only when both its signature and its IEND chunk are present.
fn is_complete_png(bytes: &[u8]) -> bool {
    bytes.len() > PNG_HEADER.len() + PNG_IEND.len() && bytes.starts_with(PNG_HEADER) && bytes.ends_with(PNG_IEND)
}

/// An SVG is complete only when the document is closed, not merely started.
fn is_complete_svg(content: &str) -> bool {
    let trimmed = content.trim_end();
    trimmed.len() > 64 && (trimmed.starts_with("<?xml") || trimmed.starts_with("<svg")) && trimmed.ends_with("</svg>")
}

/// Escape a path for a double-quoted R or Python string literal.
fn escape_path_literal(path: &Path) -> String {
    let mut escaped = String::with_capacity(path.as_os_str().len());
    for character in path.to_string_lossy().chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\u{08}' => escaped.push_str("\\b"),
            '\u{0c}' => escaped.push_str("\\f"),
            control if control.is_ascii_control() => {
                escaped.push_str(&format!("\\x{:02x}", u32::from(control)));
            }
            other => escaped.push(other),
        }
    }
    escaped
}

/// Paths the wrapped script writes its captured plot and tables to, and the chunk's own source.
#[derive(Clone, Copy)]
struct CapturePaths<'a> {
    png: &'a Path,
    svg: &'a Path,
    tables: &'a Path,
    user: &'a Path,
}

/// The R half of `wc_table()`: a private environment holding the captured tables, exposed to user
/// code only through the `wc_table` function, so assignments such as `tables <- 1` cannot reach
/// the driver's state. Every lookup resolves against `baseenv()` rather than the user's workspace.
const R_TABLES_PRELUDE: &str = r##"local({
  drv <- new.env(parent = baseenv())
  drv$path <- "__WC_TABLES_PATH__"
  drv$tables <- list()
  drv$cells <- 0L
  drv$bytes <- 0L
  drv$dirty <- FALSE

  # The hard caps always apply. A test may lower them through `.wc_cells_budget` / `.wc_budget`
  # in the workspace, which is read at capture time so a chunk can set it before its first table.
  cells_budget <- function() {
    if (exists(".wc_cells_budget", envir = globalenv())) {
      override <- suppressWarnings(as.numeric(get(".wc_cells_budget", envir = globalenv())))
      if (length(override) == 1L && !is.na(override)) return(as.integer(min(max(override, 0), 250000)))
    }
    250000L
  }

  bytes_budget <- function() {
    if (exists(".wc_budget", envir = globalenv())) {
      override <- suppressWarnings(as.numeric(get(".wc_budget", envir = globalenv())))
      if (length(override) == 1L && !is.na(override)) return(as.numeric(min(max(override, 0), 65011712)))
    }
    65011712
  }

  json_escape <- function(value) {
    text <- as.character(value)
    if (length(text) == 0L || is.na(text[1L])) return("\"\"")
    text <- iconv(text[1L], from = "", to = "UTF-8", sub = "byte")
    if (is.na(text)) return("\"\"")
    chars <- strsplit(text, "", fixed = TRUE)[[1L]]
    if (length(chars) == 0L) return("\"\"")
    escaped <- vapply(chars, function(ch) {
      code <- suppressWarnings(utf8ToInt(ch))
      if (is.na(code)) ""
      else if (ch == "\"") "\\\""
      else if (ch == "\\") "\\\\"
      else if (ch == "\b") "\\b"
      else if (ch == "\f") "\\f"
      else if (ch == "\n") "\\n"
      else if (ch == "\r") "\\r"
      else if (ch == "\t") "\\t"
      else if (code < 32L) sprintf("\\u%04x", code)
      else ch
    }, character(1), USE.NAMES = FALSE)
    paste0("\"", paste0(escaped, collapse = ""), "\"")
  }

  cell <- function(value, depth = 0L) {
    if (is.null(value) || length(value) == 0L || depth > 8L) return("")
    if (is.list(value)) return(cell(value[[1L]], depth + 1L))
    if (length(value) > 1L) value <- value[1L]
    if (is.na(value)) return("NA")
    if (is.factor(value)) {
      text <- tryCatch(as.character(value), error = function(e) "")
      if (length(text) == 0L || is.na(text[1L])) return("NA")
      return(substr(text[1L], 1L, 4096L))
    }
    text <- tryCatch(as.character(value), error = function(e) "")
    if (length(text) == 0L || is.na(text[1L])) return("")
    substr(text[1L], 1L, 4096L)
  }

  capture <- function(value) {
    if (length(drv$tables) >= 16L) return(invisible(FALSE))
    if (!is.data.frame(value) && !is.matrix(value)) return(invisible(FALSE))
    input_rows <- nrow(value)
    count <- min(input_rows, 10000L)

    has_labels <- FALSE
    labels <- NULL
    if (is.matrix(value)) {
      rn_mat <- rownames(value)
      if (!is.null(rn_mat)) {
        retained_labels <- as.character(rn_mat[seq_len(count)])
        has_labels <- !identical(retained_labels, as.character(seq_len(count)))
        labels <- retained_labels
      }
    } else if (.row_names_info(value, 1L) > 0L) {
      rn_df <- .row_names_info(value, 0L)
      is_compact <- is.integer(rn_df) && length(rn_df) == 2L && is.na(rn_df[1L])
      if (!is_compact) {
        retained_labels <- as.character(rn_df[seq_len(count)])
        has_labels <- !identical(retained_labels, as.character(seq_len(count)))
        labels <- retained_labels
      }
    }

    max_cols <- if (has_labels) 255L else 256L
    cols_count <- min(ncol(value), max_cols)
    if (cols_count == 0L && !has_labels) return(invisible(FALSE))

    raw_headers <- if (is.matrix(value)) colnames(value) else names(value)
    df <- if (is.matrix(value)) {
      base::as.data.frame(value[seq_len(count), seq_len(cols_count), drop = FALSE])
    } else {
      base::as.data.frame(value)
    }
    if (is.null(raw_headers) || length(raw_headers) < cols_count) raw_headers <- names(df)
    if (is.null(raw_headers)) raw_headers <- rep("", cols_count)
    headers <- raw_headers[seq_len(cols_count)]
    headers <- vapply(headers, function(h) substr(as.character(h), 1L, 4096L), character(1), USE.NAMES = FALSE)
    if (has_labels) headers <- c("", headers)
    width <- length(headers)

    # Cells are measured after JSON escaping, which can turn one character into six bytes, and the
    # table framing is reserved before the first row is accepted.
    budget <- bytes_budget()
    escaped_headers <- vapply(headers, json_escape, character(1), USE.NAMES = FALSE)
    framing <- sum(nchar(escaped_headers, type = "bytes")) + length(escaped_headers) + 40
    if (drv$bytes + framing > budget) return(invisible(FALSE))

    # A table with columns but no rows is still worth showing.
    if (input_rows == 0L) {
      drv$tables[[length(drv$tables) + 1L]] <- list(headers = as.list(escaped_headers), rows = list())
      drv$bytes <- drv$bytes + framing
      drv$dirty <- TRUE
      return(invisible(TRUE))
    }

    remaining_cells <- cells_budget() - drv$cells
    if (remaining_cells < width) return(invisible(FALSE))
    count <- min(count, remaining_cells %/% width)
    if (count == 0L) return(invisible(FALSE))

    rows <- vector("list", count)
    sizes <- numeric(count)
    kept <- 0L
    row_bytes <- 0
    for (index in seq_len(count)) {
      cells <- character(0)
      for (column in seq_len(cols_count)) {
        value <- df[[column]]
        if (is.matrix(value)) {
          # A data frame can hold a matrix column; the row is one cell instead of only its first
          # element, which linear indexing would otherwise keep.
          rendered <- paste(as.character(value[index, , drop = TRUE]), collapse = ", ")
          cells <- c(cells, substr(rendered, 1L, 4096L))
        } else {
          cells <- c(cells, cell(value[index]))
        }
      }
      if (has_labels) cells <- c(cell(labels[index]), cells)
      escaped <- vapply(cells, json_escape, character(1), USE.NAMES = FALSE)
      size <- sum(nchar(escaped, type = "bytes")) + length(escaped) + 1
      if (drv$bytes + framing + row_bytes + size > budget) break
      row_bytes <- row_bytes + size
      kept <- kept + 1L
      rows[[kept]] <- escaped
      sizes[[kept]] <- size
    }
    if (kept == 0L) return(invisible(FALSE))

    # A nested capture can reach drv state while the cells above are converted, so the live budgets
    # and table count are checked again before anything is committed.
    if (length(drv$tables) >= 16L) return(invisible(FALSE))
    remaining_cells <- cells_budget() - drv$cells
    allowed <- budget - drv$bytes - framing
    if (remaining_cells < width || allowed < 0) return(invisible(FALSE))
    keep <- min(kept, remaining_cells %/% width)
    if (keep == 0L) return(invisible(FALSE))
    cumulative <- cumsum(sizes[seq_len(keep)])
    keep <- sum(cumulative <= allowed)
    if (keep == 0L) return(invisible(FALSE))
    rows <- rows[seq_len(keep)]
    drv$tables[[length(drv$tables) + 1L]] <- list(headers = as.list(escaped_headers), rows = rows)
    drv$cells <- drv$cells + width * keep
    drv$bytes <- drv$bytes + framing + cumulative[keep]
    drv$dirty <- TRUE
    invisible(TRUE)
  }

  # Replace the snapshot without truncating the previous one: a plain copy over an existing file
  # empties it before the new bytes land.
  replace_file <- function(tmp, target) {
    if (suppressWarnings(base::file.rename(tmp, target))) return(TRUE)
    backup <- paste0(target, ".bak")
    suppressWarnings(base::file.remove(backup))
    if (base::file.exists(target) && !suppressWarnings(base::file.rename(target, backup))) return(FALSE)
    if (suppressWarnings(base::file.rename(tmp, target))) {
      suppressWarnings(base::file.remove(backup))
      return(TRUE)
    }
    if (base::file.exists(backup)) suppressWarnings(base::file.rename(backup, target))
    FALSE
  }

  write_tables <- function() {
    if (length(drv$tables) == 0L) return(invisible(NULL))
    # The hard output limit, not the admission override: a chunk that lowered the budget after a
    # capture must not lose the rows that capture already accepted.
    budget <- 65011712
    tmp <- paste0(drv$path, ".tmp")
    # A binary connection keeps the already escaped UTF-8 bytes from being re-encoded through the
    # native locale; `cat()` would mangle them under a C locale.
    con <- tryCatch(base::file(tmp, open = "wb"), error = function(e) NULL)
    if (is.null(con)) return(invisible(NULL))
    write_fragment <- function(text) base::writeBin(base::charToRaw(base::enc2utf8(text)), con)
    written <- tryCatch(withCallingHandlers({
      write_fragment("{\"tables\":[")
      bytes_written <- 11L
      table_written <- 0L
      closing_reserve <- 4L
      for (table in drv$tables) {
        headers <- base::unlist(table$headers, use.names = FALSE)
        h_json <- paste0("[", paste0(headers, collapse = ","), "]")
        prefix <- paste0(if (table_written > 0L) "," else "", "{\"headers\":", h_json, ",\"rows\":[")
        prefix_bytes <- nchar(prefix, type = "bytes")
        if (bytes_written + prefix_bytes + closing_reserve > budget) break
        write_fragment(prefix)
        bytes_written <- bytes_written + prefix_bytes
        table_written <- table_written + 1L
        row_written <- 0L
        for (row in table$rows) {
          r_json <- paste0("[", paste0(row, collapse = ","), "]")
          r_str <- paste0(if (row_written > 0L) "," else "", r_json)
          r_bytes <- nchar(r_str, type = "bytes")
          if (bytes_written + r_bytes + closing_reserve > budget) break
          write_fragment(r_str)
          bytes_written <- bytes_written + r_bytes
          row_written <- row_written + 1L
        }
        write_fragment("]}")
        bytes_written <- bytes_written + 2L
        if (bytes_written + 2L > budget) break
      }
      write_fragment("]}\n")
      TRUE
    }, warning = function(w) stop(w)), error = function(e) FALSE)
    if (!written) {
      suppressWarnings(try(base::close(con), silent = TRUE))
      suppressWarnings(base::file.remove(tmp))
      return(invisible(NULL))
    }
    # A failed flush on close leaves an incomplete file: publish only after both steps succeed,
    # and treat a warning from either step as a failure.
    closed <- isTRUE(tryCatch(withCallingHandlers({ base::close(con); TRUE }, warning = function(w) stop(w)), error = function(e) FALSE))
    if (!closed) {
      suppressWarnings(base::file.remove(tmp))
      return(invisible(NULL))
    }
    if (replace_file(tmp, drv$path)) drv$dirty <- FALSE else suppressWarnings(base::file.remove(tmp))
    invisible(NULL)
  }

  assign("wc_table", function(value) {
    if (!is.data.frame(value) && !is.matrix(value)) {
      stop("wc_table() expects a data.frame or matrix")
    }
    # Publish each accepted capture: a timeout kills R before the finally block can run.
    if (isTRUE(capture(value))) write_tables()
    invisible(value)
  }, envir = globalenv())
  assign(".wc_write_tables", function() write_tables(), envir = globalenv())
  assign(".wc_tables_dirty", function() isTRUE(drv$dirty), envir = globalenv())
}, envir = new.env(parent = baseenv()))
"##;

/// The Python half of `wc_table()`. pandas is imported on first use, so a chunk that never passes
/// a table does not pay for it.
const PY_TABLES_PRELUDE: &str = r##"import json as _wc_json
import os as _wc_os
import sys as _wc_sys
import threading as _wc_threading
import types as _wc_types

_wc_tables = []
_wc_tables_path = "__WC_TABLES_PATH__"
_wc_pandas = None
_wc_cells = 0
_wc_bytes = 0
# The hard caps apply on their own; a test may lower these before calling wc_table().
_wc_cells_budget = 250000
_wc_bytes_budget = 62 * 1024 * 1024
_wc_lock = _wc_threading.Lock()
_wc_pending = False


def _wc_capture_limits(cells=None, bytes_=None):
    """Lower the hard capture caps. A test hook: it can never raise them."""
    global _wc_cells_budget, _wc_bytes_budget
    if cells is not None:
        _wc_cells_budget = max(0, min(int(cells), 250000))
    if bytes_ is not None:
        _wc_bytes_budget = max(0, min(int(bytes_), 62 * 1024 * 1024))


def _wc_cell(value):
    if value is None:
        return "NA"
    if _wc_pandas is not None:
        try:
            if _wc_pandas.isna(value):
                return "NA"
        except (TypeError, ValueError):
            pass
    text = str(value)
    if len(text) > 4096:
        text = text[:4096]
    return text.encode("utf-8", "replace").decode("utf-8")


def _wc_capture(value):
    global _wc_cells, _wc_bytes
    if len(_wc_tables) >= 16:
        return False
    if _wc_pandas is None or not isinstance(value, _wc_pandas.DataFrame):
        return False
    try:
        has_labels = not (
            isinstance(value.index, _wc_pandas.RangeIndex)
            and value.index.equals(_wc_pandas.RangeIndex(len(value)))
        )
        max_cols = 255 if has_labels else 256
        sliced = value.iloc[:10000, :max_cols]
        headers = [_wc_json.dumps(_wc_cell(col), ensure_ascii=False) for col in sliced.columns]
        if has_labels:
            headers = ['""'] + headers
        if not headers:
            return False
        width = len(headers)
        # Cells are measured after JSON encoding, which can turn one character into six bytes, and
        # the table framing is reserved before the first row is accepted.
        framing = sum(len(header.encode("utf-8")) for header in headers) + width + 40
        cells_cap = min(_wc_cells_budget, 250000)
        bytes_cap = min(_wc_bytes_budget, 62 * 1024 * 1024)
        allowed = bytes_cap - _wc_bytes - framing
        if allowed < 0:
            return False
        max_rows = max(0, min(10000, (cells_cap - _wc_cells) // width))
        # Rows are converted outside the lock: a cell formatter may call wc_table() again, and
        # holding the lock here would stall the chunk until its timeout.
        rows = []
        sizes = []
        used = 0
        for row in sliced.itertuples(index=has_labels, name=None):
            if len(rows) >= max_rows:
                break
            converted = [_wc_json.dumps(_wc_cell(cell), ensure_ascii=False) for cell in row]
            size = sum(len(cell.encode("utf-8")) for cell in converted) + len(converted) + 1
            if used + size > allowed:
                break
            used += size
            rows.append(converted)
            sizes.append(size)
        empty = not rows and len(sliced) == 0
        if not rows and not empty:
            return False
    except Exception:
        return False

    with _wc_lock:
        # A nested capture may have spent cells or bytes while the rows above were converted, so
        # the live budgets are checked again before anything is committed.
        if len(_wc_tables) >= 16:
            return False
        if empty:
            if _wc_bytes + framing > min(_wc_bytes_budget, 62 * 1024 * 1024):
                return False
            _wc_tables.append({"headers": headers, "rows": []})
            _wc_bytes += framing
            return True
        available_cells = min(_wc_cells_budget, 250000) - _wc_cells
        available_bytes = min(_wc_bytes_budget, 62 * 1024 * 1024) - _wc_bytes - framing
        keep = 0
        used = 0
        for size in sizes:
            if keep >= available_cells // width or used + size > available_bytes:
                break
            used += size
            keep += 1
        if keep == 0:
            return False
        _wc_tables.append({"headers": headers, "rows": rows[:keep]})
        _wc_cells += width * keep
        _wc_bytes += framing + used
    return True


def _wc_write_tables():
    global _wc_pending
    if not _wc_tables:
        _wc_pending = False
        return
    budget = 62 * 1024 * 1024
    tmp_path = _wc_tables_path + ".tmp"
    try:
        with open(tmp_path, "w", encoding="utf-8") as handle:
            handle.write('{"tables":[')
            bytes_written = 11
            table_written = 0
            closing_reserve = 4
            for table in _wc_tables:
                headers = table.get("headers", [])
                rows = table.get("rows", [])
                h_json = "[" + ",".join(headers) + "]"
                prefix = ("," if table_written > 0 else "") + '{"headers":' + h_json + ',"rows":['
                prefix_bytes = len(prefix.encode("utf-8"))
                if bytes_written + prefix_bytes + closing_reserve > budget:
                    break
                handle.write(prefix)
                bytes_written += prefix_bytes
                table_written += 1
                row_written = 0
                for row in rows:
                    r_json = "[" + ",".join(row) + "]"
                    r_str = ("," if row_written > 0 else "") + r_json
                    r_bytes = len(r_str.encode("utf-8"))
                    if bytes_written + r_bytes + closing_reserve > budget:
                        break
                    handle.write(r_str)
                    bytes_written += r_bytes
                    row_written += 1
                handle.write("]}")
                bytes_written += 2
                if bytes_written + 2 > budget:
                    break
            handle.write("]}\n")
        # Replacing the target only after the rewrite finishes keeps the previous complete snapshot
        # if the interpreter is killed mid-write.
        _wc_os.replace(tmp_path, _wc_tables_path)
        _wc_pending = False
    except Exception:
        try:
            _wc_os.remove(tmp_path)
        except Exception:
            pass


def wc_table(value):
    global _wc_pandas, _wc_pending
    if _wc_pandas is None:
        try:
            import pandas as _pandas
            _wc_pandas = _pandas
        except Exception as exc:
            raise TypeError("wc_table() needs pandas") from exc
    if not isinstance(value, _wc_pandas.DataFrame):
        raise TypeError("wc_table() expects a pandas.DataFrame")
    # Only publication takes the lock: capture commits under it too, and conversion stays outside
    # so a nested call from a cell formatter cannot deadlock the chunk.
    if _wc_capture(value):
        _wc_pending = True
        with _wc_lock:
            _wc_write_tables()
    return None

"##;

/// The figure cleanup that follows a Python chunk when plots are captured.
const PY_PLOT_CLEANUP: &str = r#"if _has_plt and plt is not None:
    try:
        if plt.get_fignums():
            __WC_SAVE__
            plt.close('all')
    except Exception:
        pass
"#;

/// R plot device setup and the final part of its `finally` block, or empty strings when plot
/// capture is off. Tables are still written in that case.
fn r_plot_sections(capture: bool, format: PlotFormat, captured: CapturePaths<'_>) -> (String, String) {
    if !capture {
        return (String::new(), String::new());
    }
    let png = escape_path_literal(captured.png);
    let svg = escape_path_literal(captured.svg);
    match format {
        PlotFormat::Png => (
            format!(
                r#"options(warn = 1)
.wc_plot_png <- "{png}"
setHook("plot.new", function() assign(".wc_has_plot", TRUE, envir = .GlobalEnv))
setHook("grid.newpage", function() assign(".wc_has_plot", TRUE, envir = .GlobalEnv))
png(.wc_plot_png, width = 1600, height = 1200, res = 200)"#
            ),
            r#"  if (dev.cur() != 1) dev.off()
  if (!exists(".wc_has_plot", envir = .GlobalEnv) || !get(".wc_has_plot", envir = .GlobalEnv)) {
    if (file.exists(.wc_plot_png)) file.remove(.wc_plot_png)
  }
"#
            .to_owned(),
        ),
        PlotFormat::Svg => (
            format!(
                r#"options(warn = 1)
.wc_plot_svg <- "{svg}"
setHook("plot.new", function() assign(".wc_has_plot", TRUE, envir = .GlobalEnv))
setHook("grid.newpage", function() assign(".wc_has_plot", TRUE, envir = .GlobalEnv))
svg(.wc_plot_svg, width = 8, height = 6)"#
            ),
            r#"  if (dev.cur() != 1) dev.off()
  if (!exists(".wc_has_plot", envir = .GlobalEnv) || !get(".wc_has_plot", envir = .GlobalEnv)) {
    if (file.exists(.wc_plot_svg)) file.remove(.wc_plot_svg)
  }
"#
            .to_owned(),
        ),
        PlotFormat::Both => (
            format!(
                r#"options(warn = 1)
.wc_plot_png <- "{png}"
.wc_plot_svg <- "{svg}"
setHook("plot.new", function() assign(".wc_has_plot", TRUE, envir = .GlobalEnv))
setHook("grid.newpage", function() assign(".wc_has_plot", TRUE, envir = .GlobalEnv))
png(.wc_plot_png, width = 1600, height = 1200, res = 200)
dev.control("enable")"#
            ),
            r#"  if (exists(".wc_has_plot", envir = .GlobalEnv) && get(".wc_has_plot", envir = .GlobalEnv)) {
    dev.copy(svg, .wc_plot_svg, width = 8, height = 6)
    dev.off()
  }
  if (dev.cur() != 1) dev.off()
  if (!exists(".wc_has_plot", envir = .GlobalEnv) || !get(".wc_has_plot", envir = .GlobalEnv)) {
    if (file.exists(.wc_plot_png)) file.remove(.wc_plot_png)
    if (file.exists(.wc_plot_svg)) file.remove(.wc_plot_svg)
  }
"#
            .to_owned(),
        ),
    }
}

/// Python's plot-saving lines for the chosen format.
fn python_plot_save(format: PlotFormat, captured: CapturePaths<'_>) -> String {
    let png = escape_path_literal(captured.png);
    let svg = escape_path_literal(captured.svg);
    match format {
        PlotFormat::Png => format!(r#"plt.savefig("{png}", dpi=200, bbox_inches='tight')"#),
        PlotFormat::Svg => format!(r#"plt.savefig("{svg}", format='svg', bbox_inches='tight')"#),
        PlotFormat::Both => format!(
            r#"plt.savefig("{png}", dpi=200, bbox_inches='tight')
            plt.savefig("{svg}", format='svg', bbox_inches='tight')"#
        ),
    }
}

/// Wrap the chunk so tables are captured and, unless disabled, plots are intercepted.
fn prepare_wrapped_code(lang: CodeLanguage, capture_plots: bool, format: PlotFormat, captured: CapturePaths<'_>) -> String {
    let tables_str = escape_path_literal(captured.tables);
    let user_str = escape_path_literal(captured.user);
    match lang {
        CodeLanguage::R => {
            let prelude = R_TABLES_PRELUDE.replace("__WC_TABLES_PATH__", &tables_str);
            let (setup, finally) = r_plot_sections(capture_plots, format, captured);
            // `source(print.eval = TRUE)` keeps the automatic printing a plain Rscript run gives a
            // bare expression; evaluating the chunk inline inside tryCatch would swallow it.
            format!(
                r#"{prelude}
{setup}
tryCatch({{
  source("{user_str}", local = globalenv(), print.eval = TRUE, encoding = "UTF-8")
}}, finally = {{
  if (exists(".wc_tables_dirty", envir = globalenv()) && .wc_tables_dirty()) try(.wc_write_tables(), silent = TRUE)
{finally}}})
"#
            )
        }
        CodeLanguage::Python => {
            let prelude = PY_TABLES_PRELUDE.replace("__WC_TABLES_PATH__", &tables_str);
            // matplotlib is only touched when plots are captured: importing it and forcing the Agg
            // backend would override a backend the chunk chose through MPLBACKEND or matplotlibrc.
            let setup = if capture_plots {
                r#"import sys

_has_plt = False
try:
    import matplotlib
    matplotlib.use('Agg')
    import matplotlib.pyplot as plt
    _has_plt = True
except Exception:
    pass
"#
            } else {
                ""
            };
            let cleanup = if capture_plots {
                let save = python_plot_save(format, captured);
                PY_PLOT_CLEANUP.replace("__WC_SAVE__", &save)
            } else {
                String::new()
            };
            // The chunk is compiled as its own module: a module docstring, a `from __future__`
            // import and anything sharing its line keep the placement Python requires.
            format!(
                r#"{prelude}
{setup}
_wc_user_path = "{user_str}"
with open(_wc_user_path, "rb") as _wc_user_file:
    _wc_user_source = _wc_user_file.read()
if _wc_sys.argv:
    _wc_sys.argv[0] = _wc_user_path
# The chunk runs as a module registered as __main__: a name it assigns cannot shadow the capture
# driver, while classes and functions it defines are still findable by pickle and inspect.
_wc_module = _wc_types.ModuleType("__main__")
_wc_module.__file__ = _wc_user_path
_wc_module.wc_table = wc_table
_wc_module._wc_capture_limits = _wc_capture_limits
_wc_sys.modules["__main__"] = _wc_module
try:
    exec(compile(_wc_user_source, _wc_user_path, "exec"), _wc_module.__dict__)
finally:
    # A write that failed during the run is retried once on ordinary exit; the immediate write
    # still covers a timeout kill.
    if _wc_pending:
        with _wc_lock:
            _wc_write_tables()

{cleanup}"#
            )
        }
    }
}

/// Anything unreadable, oversized or malformed yields no tables instead of an error. When the
/// primary snapshot cannot be read, the previous complete one beside it is tried under the same
/// limits: a failed replacement leaves `<path>.bak` behind.
fn read_tables(path: &Path) -> Vec<TabularOutput> {
    if let Some(tables) = read_tables_file(path) {
        return tables;
    }
    let mut backup = path.as_os_str().to_owned();
    backup.push(".bak");
    read_tables_file(Path::new(&backup)).unwrap_or_default()
}

/// `None` means the file was missing, oversized or malformed; `Some` may still be empty.
fn read_tables_file(path: &Path) -> Option<Vec<TabularOutput>> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_TABLES_FILE_BYTES {
        return None;
    }
    let file = fs::File::open(path).ok()?;
    read_tables_payload(file, MAX_TABLES_FILE_BYTES, MAX_TOTAL_CELLS)
}

#[derive(Deserialize)]
struct RawTables {
    #[serde(default)]
    tables: Vec<RawTable>,
}

#[derive(Deserialize)]
struct RawTable {
    #[serde(default)]
    headers: Vec<String>,
    #[serde(default)]
    rows: Vec<Vec<String>>,
}

/// Read and validate tabular data from a reader under strict byte and cell budgets.
#[cfg(test)]
fn read_tables_from_reader<R: Read>(reader: R, max_bytes: u64, max_cells: usize) -> Vec<TabularOutput> {
    read_tables_payload(reader, max_bytes, max_cells).unwrap_or_default()
}

/// `None` covers an unreadable, oversized or malformed payload; `Some` may still be empty.
fn read_tables_payload<R: Read>(reader: R, max_bytes: u64, max_cells: usize) -> Option<Vec<TabularOutput>> {
    let mut raw = Vec::new();
    let mut limited = reader.take(max_bytes.saturating_add(1));
    if limited.read_to_end(&mut raw).is_err() || raw.len() > max_bytes as usize {
        return None;
    }
    let payload: RawTables = serde_json::from_slice(&raw).ok()?;
    let tables: Vec<RawTable> = payload.tables.into_iter().take(MAX_TABLES).collect();
    let mut projected: usize = 0;
    for table in &tables {
        let width = table.headers.len().min(MAX_TABLE_COLUMNS);
        let rows = table.rows.len().min(MAX_TABLE_ROWS);
        projected = width.checked_mul(rows).and_then(|cells| projected.checked_add(cells))?;
    }
    if projected > max_cells {
        return None;
    }
    Some(tables.into_iter().filter_map(normalize_table).collect())
}

/// Clamp a table to the documented limits and make every row as wide as the headers.
fn normalize_table(table: RawTable) -> Option<TabularOutput> {
    let mut headers: Vec<String> = table.headers.into_iter().map(truncate_cell).collect();
    if headers.is_empty() {
        return None;
    }
    headers.truncate(MAX_TABLE_COLUMNS);
    let width = headers.len();
    let rows = table
        .rows
        .into_iter()
        .take(MAX_TABLE_ROWS)
        .map(|row| {
            let mut row: Vec<String> = row.into_iter().map(truncate_cell).collect();
            row.truncate(width);
            row.resize(width, String::new());
            row
        })
        .collect();
    Some(TabularOutput { headers, rows })
}

/// Keep at most [`MAX_CELL_CHARS`] characters, never splitting a UTF-8 sequence.
fn truncate_cell(text: String) -> String {
    if text.chars().count() <= MAX_CELL_CHARS {
        return text;
    }
    text.chars().take(MAX_CELL_CHARS).collect()
}

#[cfg(test)]
mod tests;
