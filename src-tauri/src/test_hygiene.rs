//! Repository-wide test hygiene checks (permanent guards for past flakes).
//!
//! A `#[tokio::test(start_paused = true)]` test runs on a paused clock that
//! auto-advances whenever the runtime is idle. A test that also talks to a
//! real-socket mock (the AxeOS HTTP `MockMiner`) is idle while that socket
//! I/O is in flight, so the clock jumps past resolve / op timeouts and the
//! test fails at random ("miner not found" / `Failed` instead of `NoPath`),
//! most often on the slower macOS CI runners. Such tests must run on real
//! time (`flavor = "multi_thread"`) instead.

use std::fs;
use std::path::{Path, PathBuf};

/// Calls that start a real-socket mock miner.
const SOCKET_MOCKS: [&str; 4] = ["MockMiner::spawn", "rig(true, true)", "rig(false, true)", "axe_target("];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Body of the first `fn` after `from`, by brace matching.
fn fn_body(src: &str, from: usize) -> (&str, &str) {
    let f = from + src[from..].find("fn ").unwrap();
    let name_end = f + src[f..].find('(').unwrap();
    let open = f + src[f..].find('{').unwrap();
    let mut depth = 0usize;
    for (i, c) in src[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return (&src[f + 3..name_end], &src[open..open + i + 1]);
                }
            }
            _ => {}
        }
    }
    (&src[f + 3..name_end], &src[open..])
}

fn violations(src: &str) -> Vec<String> {
    let mut v = Vec::new();
    let attr = "#[tokio::test(start_paused = true)]";
    let mut at = 0;
    while let Some(i) = src[at..].find(attr) {
        let start = at + i + attr.len();
        let (name, body) = fn_body(src, start);
        for m in SOCKET_MOCKS {
            if body.contains(m) {
                v.push(format!("{name} uses {m}"));
            }
        }
        at = start;
    }
    v
}

#[test]
fn paused_clock_tests_use_no_socket_mocks() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    let mut bad = Vec::new();
    for f in files {
        if f.ends_with("test_hygiene.rs") {
            continue;
        }
        for x in violations(&fs::read_to_string(&f).unwrap()) {
            bad.push(format!("{}: {x}", f.strip_prefix(&root).unwrap().display()));
        }
    }
    assert!(bad.is_empty(), "paused-clock tests must not start real-socket mocks; use flavor = \"multi_thread\":\n{}", bad.join("\n"));
}

#[test]
fn guard_detects_the_pattern() {
    let src = "#[tokio::test(start_paused = true)]\nasync fn t() {\n    let r = rig(true, true).await;\n    if x { y }\n}\n\
               #[tokio::test(flavor = \"multi_thread\")]\nasync fn ok() { rig(true, true); }\n";
    assert_eq!(violations(src), ["t uses rig(true, true)"]);
}
