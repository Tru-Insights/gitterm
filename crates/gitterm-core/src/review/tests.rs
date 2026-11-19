use super::{budget::*, model::*, patch::*};
use std::collections::HashSet;

#[test]
fn classifier_ports_t3_cases() {
    // T3 Code's changedFileKind.test.ts, with "mocks" and "tests" both
    // folding into Test.
    for (path, kind) in [
        ("src/contest.py", FileKind::Code),
        ("test/fixtures/pr.json", FileKind::Test),
        ("pnpm-lock.yaml", FileKind::Generated),
        ("README.md", FileKind::Docs),
        ("tests/README.md", FileKind::Test),
    ] {
        assert_eq!(classify_path(path), kind, "{path}");
    }
}

#[test]
fn classifier_follows_t3_naming_rules() {
    for (path, kind) in [
        // Generated: lockfiles, build output, generated dirs.
        ("Cargo.lock", FileKind::Generated),
        ("web/yarn.lock", FileKind::Generated),
        ("src/api.generated.ts", FileKind::Generated),
        ("proto/acme.pb.go", FileKind::Generated),
        ("acme_pb2_grpc.py", FileKind::Generated),
        ("dist/app.min.js", FileKind::Generated),
        ("dist/app.js.map", FileKind::Generated),
        ("src/__generated__/schema.ts", FileKind::Generated),
        ("lib/Generated/x.swift", FileKind::Generated),
        // Mocks and fixtures.
        ("src/__snapshots__/view.ts.snap", FileKind::Test),
        ("src/user.mock.ts", FileKind::Test),
        ("src/mockData.ts", FileKind::Test),
        ("src/mock_api.rs", FileKind::Test),
        ("src/mockery.go", FileKind::Code),
        ("pkg/testdata/in.txt", FileKind::Test),
        // Tests.
        ("src/app.test.tsx", FileKind::Test),
        ("src/App.Spec.ts", FileKind::Test),
        ("pkg/server_test.go", FileKind::Test),
        ("test_acme.py", FileKind::Test),
        ("conftest.py", FileKind::Test),
        ("vitest.setup.ts", FileKind::Test),
        ("src/FooTest.java", FileKind::Test),
        ("src/FooTests.cs", FileKind::Test),
        ("src/Latest.java", FileKind::Code),
        ("src/footest.java", FileKind::Code),
        ("e2e/login.ts", FileKind::Test),
        // Docs, but not tool input.
        ("docs/guide.MD", FileKind::Docs),
        ("notes.txt", FileKind::Docs),
        ("requirements-dev.txt", FileKind::Config),
        ("CMakeLists.txt", FileKind::Config),
        // Config.
        ("Cargo.toml", FileKind::Config),
        ("tsconfig.json", FileKind::Config),
        (".github/workflows/ci.yml", FileKind::Config),
        (".gitignore", FileKind::Config),
        (".env.local", FileKind::Config),
        // Code, with Windows separators too.
        ("src/main.rs", FileKind::Code),
        ("src\\tests\\a.rs", FileKind::Test),
    ] {
        assert_eq!(classify_path(path), kind, "{path}");
    }
}

fn summary_bytes(records: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for record in records {
        out.extend_from_slice(record.as_bytes());
        out.push(0);
    }
    out
}

const A: &str = "1111111111111111111111111111111111111111";
const B: &str = "2222222222222222222222222222222222222222";
const Z: &str = "0000000000000000000000000000000000000000";

#[test]
fn raw_numstat_parses_modified_renamed_deleted_and_binary() {
    let raw_m = format!(":100644 100644 {A} {B} M");
    let raw_r = format!(":100644 100644 {A} {B} R087");
    let raw_d = format!(":100644 000000 {A} {Z} D");
    let raw_b = format!(":100644 100644 {A} {B} M");
    let bytes = summary_bytes(&[
        &raw_m,
        "src/a.rs",
        &raw_r,
        "src/old name.rs",
        "src/new name.rs",
        &raw_d,
        "gone.txt",
        &raw_b,
        "logo.png",
        "3\t1\tsrc/a.rs",
        "2\t2\t",
        "src/old name.rs",
        "src/new name.rs",
        "0\t4\tgone.txt",
        "-\t-\tlogo.png",
    ]);
    let entries = parse_raw_numstat(&bytes).unwrap();
    assert_eq!(entries.len(), 4);
    assert_eq!(
        (
            entries[0].path.as_str(),
            entries[0].additions,
            entries[0].deletions
        ),
        ("src/a.rs", 3, 1)
    );
    assert_eq!(entries[1].status, 'R');
    assert_eq!(entries[1].old_path.as_deref(), Some("src/old name.rs"));
    assert_eq!(entries[1].path, "src/new name.rs");
    assert_eq!((entries[1].additions, entries[1].deletions), (2, 2));
    assert_eq!(tracked_status(&entries[1]), FileStatus::Renamed);
    assert_eq!(tracked_status(&entries[2]), FileStatus::Deleted);
    assert_eq!(entries[2].old_blob, A);
    assert!(entries[3].binary);
    assert_eq!(tracked_status(&entries[3]), FileStatus::Binary);
    assert_eq!(entries[0].new_blob, B);

    // A numstat record for a file with no raw record is an error.
    let bad = summary_bytes(&[&raw_m, "a", "1\t1\tb"]);
    assert!(parse_raw_numstat(&bad).is_err());
    assert_eq!(parse_raw_numstat(b"").unwrap(), Vec::new());
}

#[test]
fn diff_headers_quote_like_git() {
    assert_eq!(quote_git_path("a/", "src/x y.rs"), "a/src/x y.rs");
    assert_eq!(quote_git_path("b/", "caf\u{e9}.md"), "b/caf\u{e9}.md");
    assert_eq!(quote_git_path("a/", "tab\there"), "\"a/tab\\there\"");
    assert_eq!(quote_git_path("a/", "q\"x\\y"), "\"a/q\\\"x\\\\y\"");
    assert_eq!(quote_git_path("a/", "bell\u{1}"), "\"a/bell\\001\"");
}

fn feed(lines: &[&str]) -> PatchParser {
    let mut parser = PatchParser::default();
    for line in lines {
        parser.feed(line);
    }
    parser
}

#[test]
fn patch_parses_hunks_numbers_and_tricky_content() {
    let parser = feed(&[
        "diff --git a/src/a.rs b/src/a.rs",
        "index 1..2 100644",
        "--- a/src/a.rs",
        "+++ b/src/a.rs",
        "@@ -10,3 +10,4 @@ fn x() {",
        " keep",
        "-old",
        "+new",
        "+++ looks like a header",
        "\\ No newline at end of file",
        "diff --git a/b.bin b/b.bin",
        "Binary files a/b.bin and b/b.bin differ",
        "diff --git a/gone.txt b/gone.txt",
        "deleted file mode 100644",
        "--- a/gone.txt",
        "+++ /dev/null",
        "@@ -1 +0,0 @@",
        "-bye",
    ]);
    assert_eq!(parser.files.len(), 3);
    let a = &parser.files[0];
    assert_eq!(a.header, "diff --git a/src/a.rs b/src/a.rs");
    assert_eq!(a.hunks.len(), 1);
    let lines: Vec<(LineOp, Option<u32>, Option<u32>, &str)> = a.hunks[0]
        .lines
        .iter()
        .map(|l| (l.op, l.old_no, l.new_no, l.text.as_str()))
        .collect();
    assert_eq!(
        lines,
        [
            (LineOp::Context, Some(10), Some(10), "keep"),
            (LineOp::Del, Some(11), None, "old"),
            (LineOp::Add, None, Some(11), "new"),
            (LineOp::Add, None, Some(12), "++ looks like a header"),
        ]
    );
    assert!(parser.files[1].binary);
    assert!(parser.files[1].hunks.is_empty());
    let gone = &parser.files[2].hunks[0].lines;
    assert_eq!(gone.len(), 1);
    assert_eq!((gone[0].op, gone[0].old_no), (LineOp::Del, Some(1)));
    assert!(!parser.stopped);
}

#[test]
fn patch_cuts_a_file_at_its_line_cap() {
    let mut lines = vec![
        "diff --git a/big.rs b/big.rs".to_string(),
        format!("@@ -0,0 +1,{} @@", FILE_LINE_CAP + 5),
    ];
    for i in 0..FILE_LINE_CAP + 5 {
        lines.push(format!("+line {i}"));
    }
    lines.push("diff --git a/next.rs b/next.rs".to_string());
    lines.push("@@ -1 +1 @@".to_string());
    lines.push("-a".to_string());
    lines.push("+b".to_string());
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let parser = feed(&refs);
    assert!(parser.files[0].truncated);
    assert_eq!(parser.files[0].hunks[0].lines.len(), FILE_LINE_CAP);
    assert!(!parser.files[1].truncated);
    assert_eq!(parser.files[1].hunks[0].lines.len(), 2);
}

#[test]
fn patch_stops_at_the_total_byte_cap() {
    let mut parser = PatchParser::default();
    // Three half-cap lines per file: the third is past the file's byte
    // cap, so each file shows two and is cut.
    let long = format!("+{}", "x".repeat(FILE_BYTE_CAP / 2));
    for file in 0..64 {
        if !parser.feed(&format!("diff --git a/f{file} b/f{file}")) {
            break;
        }
        parser.feed("@@ -0,0 +1,3 @@");
        for _ in 0..3 {
            if !parser.feed(&long) {
                break;
            }
        }
    }
    assert!(parser.stopped);
    assert_eq!(parser.budget.bytes, TOTAL_BYTE_CAP);
    let files = TOTAL_BYTE_CAP / FILE_BYTE_CAP;
    assert_eq!(parser.files.len(), files);
    assert!(parser.files.iter().all(|file| file.truncated));
    assert_eq!(parser.files[0].hunks[0].lines.len(), 2);
    // Nothing more is read once stopped.
    assert!(!parser.feed("diff --git a/late b/late"));
    assert_eq!(parser.files.len(), files);
}

#[test]
fn untracked_text_becomes_added_lines_and_is_cut() {
    let mut budget = Budget::default();
    let (hunks, shown, truncated) = untracked_hunks(b"one\r\ntwo\nthree\n", false, &mut budget);
    assert_eq!((shown, truncated), (3, false));
    assert_eq!(hunks[0].header, "@@ -0,0 +1,3 @@");
    let texts: Vec<&str> = hunks[0].lines.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(texts, ["one", "two", "three"]);
    assert_eq!(hunks[0].lines[2].new_no, Some(3));
    assert!(hunks[0].lines.iter().all(|l| l.op == LineOp::Add));

    let many: String = (0..FILE_LINE_CAP + 10).map(|i| format!("{i}\n")).collect();
    let (hunks, shown, truncated) = untracked_hunks(many.as_bytes(), false, &mut budget);
    assert_eq!((shown, truncated), (FILE_LINE_CAP, true));
    assert_eq!(hunks[0].lines.len(), FILE_LINE_CAP);

    // Read only up to the byte cap: cut, even when every line fits.
    let (_, shown, truncated) = untracked_hunks(b"a\nb", true, &mut budget);
    assert_eq!((shown, truncated), (2, true));

    let (hunks, shown, _) = untracked_hunks(b"", false, &mut budget);
    assert!(hunks.is_empty());
    assert_eq!(shown, 0);
}

#[test]
fn untracked_binary_is_sniffed() {
    let dir = tempfile::tempdir().unwrap();
    let text = dir.path().join("notes.txt");
    let binary = dir.path().join("logo.png");
    std::fs::write(&text, "hello\n").unwrap();
    std::fs::write(&binary, [0x89, b'P', b'N', b'G', 0, 1, 2]).unwrap();
    assert_eq!(sniff_untracked(&text), Sniff::Text { size: 6 });
    assert_eq!(sniff_untracked(&binary), Sniff::Binary);
    assert!(matches!(sniff_untracked(dir.path()), Sniff::Unreadable(_)));
    assert!(matches!(
        sniff_untracked(&dir.path().join("missing")),
        Sniff::Unreadable(_)
    ));
}

#[test]
fn tree_order_puts_folders_first() {
    let mut paths = vec![
        "z.rs",
        "src/b.rs",
        "Cargo.lock",
        "src/a/x.rs",
        "a.md",
        "src/a.rs",
    ];
    paths.sort_by(|a, b| tree_order(a, b));
    assert_eq!(
        paths,
        [
            "src/a/x.rs",
            "src/a.rs",
            "src/b.rs",
            "Cargo.lock",
            "a.md",
            "z.rs"
        ]
    );
}

#[test]
fn admission_prefers_code_and_skips_what_does_not_fit() {
    let costs = [
        (FileKind::Generated, 15_000),
        (FileKind::Code, 1_500),
        (FileKind::Test, 1_900),
        (FileKind::Code, 50_000),
        (FileKind::Docs, 10),
    ];
    // Kind order: code 1,500 + 2,000 (capped), docs 10, test 1,900 =
    // 5,410; the generated file's 2,000 does not fit in what is left.
    assert_eq!(admit(&costs, 7_000), HashSet::from([1, 3, 4, 2]));
    assert_eq!(admit(&costs, 3_510), HashSet::from([1, 3, 4]));
    assert_eq!(admit(&costs, 20_000).len(), 5);
    // A file that does not fit is skipped; a smaller one after it fits.
    let costs = [
        (FileKind::Code, 1_500),
        (FileKind::Code, 1_000),
        (FileKind::Code, 400),
    ];
    assert_eq!(admit(&costs, 1_900), HashSet::from([0, 2]));
}
