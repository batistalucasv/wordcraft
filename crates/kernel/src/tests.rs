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
    let result = execute(&req).expect("execution failed");

    assert!(!result.timed_out, "a background descendant must not extend the chunk");
    assert!(result.success);
    assert!(result.stdout.contains("chunk finished"));
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
