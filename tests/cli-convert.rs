// Integration tests for the CLI `convert` subcommand, focused on the --verify
// gate.
//
// Conversion is otherwise entirely local. --verify is what sends the sentences
// around any residual issue to Google Translate for anchor matching, so the
// default has to stay off and the flag has to be the only way in.

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn binary_path() -> std::path::PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("zhtw-mcp");
    path
}

fn run_convert(extra_args: &[&str], input: &str) -> Output {
    let bin = binary_path();
    Command::new(&bin)
        .arg("convert")
        .args(extra_args)
        .arg("--")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("RUST_LOG")
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
            child.wait_with_output()
        })
        .unwrap()
}

/// The default path converts and says nothing about verification, because
/// it never reaches the network.
#[test]
fn convert_default_does_not_verify() {
    let out = run_convert(&[], "用户使用软件\n");
    assert!(out.status.success(), "convert should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("使用者") && stdout.contains("軟體"),
        "conversion must still happen; got {stdout:?}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("convert: verify"),
        "default convert must not run anchor verification; got {stderr:?}"
    );
}

/// --verify is accepted.  The input converts cleanly, so no issue survives
/// to be calibrated and this test stays offline; it pins the flag wiring,
/// not the network call.
#[cfg(feature = "translate")]
#[test]
fn convert_accepts_verify_flag() {
    let out = run_convert(&["--verify"], "用户使用软件\n");
    assert!(
        out.status.success(),
        "convert --verify should be accepted; stderr={:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Built without the feature, the flag has to fail with the explanation
/// rather than be silently ignored.  Asserting this instead of skipping
/// keeps both build configurations covered.
#[cfg(not(feature = "translate"))]
#[test]
fn convert_verify_flag_explains_missing_feature() {
    let out = run_convert(&["--verify"], "用户使用软件\n");
    assert!(
        !out.status.success(),
        "--verify must fail without the feature"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("requires the 'translate' feature"),
        "expected the feature explanation; got {stderr:?}"
    );
}

/// Unknown flags still fail loudly rather than being read as filenames.
#[test]
fn convert_rejects_unknown_flag() {
    let out = run_convert(&["--verifyy"], "用户\n");
    assert!(!out.status.success(), "unknown flag must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown convert flag"),
        "expected a flag error; got {stderr:?}"
    );
}

/// A file name that means Markdown means it everywhere.
///
/// `convert` used to decide this itself, case-sensitively and on the `md`
/// extension alone, so `.markdown` and `README.MD` were read as plain text and
/// the fixer rewrote terminology inside code fences that Markdown protects.
#[test]
fn convert_reads_markdown_file_names_the_way_lint_does() {
    let dir = tempfile::tempdir().expect("temp dir");
    let body = "正文软件\n\n```\n代码软件\n```\n";

    let mut outputs = Vec::new();
    for name in ["t.md", "t.markdown", "T.MD"] {
        let path = dir.path().join(name);
        std::fs::write(&path, body).unwrap();
        let out = Command::new(binary_path())
            .arg("convert")
            .arg(&path)
            .output()
            .expect("run convert");
        assert!(out.status.success(), "convert {name} failed");
        outputs.push((name, String::from_utf8_lossy(&out.stdout).into_owned()));
    }

    // The fence content is what distinguishes the two readings: Markdown
    // leaves the term inside it alone, plain text rewrites it.
    for (name, text) in &outputs {
        assert!(
            text.contains("代碼軟件"),
            "{name}: the fenced term should be left alone, got: {text:?}"
        );
        assert!(
            !text.contains("程式碼軟體"),
            "{name}: the fixer reached inside a code fence, got: {text:?}"
        );
    }
    assert_eq!(
        outputs[0].1, outputs[1].1,
        ".md and .markdown must convert alike"
    );
    assert_eq!(
        outputs[0].1, outputs[2].1,
        "the extension is not case-sensitive"
    );
}
