use super::*;
use graft_core::{Constraint, FileChangeKind, PlanId, TreeEntry, TreeSnapshot};

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("graft-scratch-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn seeded_engine(name: &str) -> (std::path::PathBuf, ScratchEngine, String) {
    let dir = temp_dir(name);
    let store = GraftStore::open(&dir);
    store.init().unwrap();
    let hash = store
        .write_blob(b"pub fn hello() {\n    println!(\"hello\");\n}\n")
        .unwrap();
    let snapshot = TreeSnapshot::new(vec![TreeEntry {
        path: "src/lib.rs".to_string(),
        hash,
        size: 43,
    }]);
    let (tree_id, _) = store.write_tree_snapshot(&snapshot).unwrap();
    (dir, ScratchEngine::new(store), tree_id)
}

#[test]
fn hashline_rendering_uses_expected_shape_and_alphabet() {
    let rendered = render_hashlines("hello\n!!!\n");
    assert!(rendered.contains("1#"));
    assert!(rendered.contains("2#"));
    for hash in rendered
        .lines()
        .filter_map(|line| line.split_once('#'))
        .map(|(_, rest)| &rest[..2])
    {
        assert_eq!(hash.len(), 2);
        assert!(
            hash.chars()
                .all(|ch| HASHLINE_ALPHABET.contains(&(ch as u8)))
        );
    }
}

/// Lock the hashline algorithm to a stable byte form across versions.
///
/// The combination of (line number, content) -> 2-character anchor must
/// stay stable so existing edits and stale-anchor diagnostics remain
/// reproducible. `line_hash` is also content-addressed by line number for
/// lines without alphanumerics (significant-line seed rule), so we cover
/// pure-punctuation, ASCII, and Unicode separately.
#[test]
fn hashline_fixed_vectors_are_byte_stable() {
    // Cases captured from the current implementation; if the algorithm
    // changes intentionally these values must be re-snapshotted in the
    // same commit that changes the algorithm.
    let cases: &[(usize, &str, &str)] = &[
        (1, "hello", "TH"),
        (2, "hello", "TH"),
        (1, "world", "SK"),
        (1, "!!!", "TB"),
        (2, "!!!", "QT"),
        (3, "!!!", "MP"),
        (1, "你好", "SS"),
        (1, "fn main() {", "MP"),
        (1, "    println!(\"hello\");", "PR"),
    ];
    // Fail loudly with the actual values so the snapshot is easy to
    // refresh when the algorithm changes deliberately.
    let actual: Vec<(usize, &str, String)> = cases
        .iter()
        .map(|(line, text, _)| (*line, *text, line_hash(*line, text)))
        .collect();
    let expected: Vec<(usize, &str, String)> = cases
        .iter()
        .map(|(line, text, hash)| (*line, *text, hash.to_string()))
        .collect();
    assert_eq!(actual, expected, "hashline byte form drift");
}

/// Pure-punctuation lines must be seeded by their line number so that two
/// identical lines at different positions don’t share an anchor. ASCII
/// lines with letters/digits must NOT be seeded, so a duplicated line at
/// any position keeps the same anchor.
#[test]
fn hashline_significant_line_seed_rule() {
    // Line number affects pure-punct lines.
    assert_ne!(line_hash(1, "!!!"), line_hash(2, "!!!"));
    assert_ne!(line_hash(2, "!!!"), line_hash(3, "!!!"));
    // Line number does NOT affect lines with alphanumerics.
    assert_eq!(line_hash(1, "hello"), line_hash(2, "hello"));
    assert_eq!(line_hash(7, "fn main() {"), line_hash(99, "fn main() {"));
    // Distinct content must produce distinct anchors most of the time;
    // we don’t require the alphabet to be collision-free, just that
    // these specific samples differ.
    assert_ne!(line_hash(1, "hello"), line_hash(1, "world"));
}

#[test]
fn open_and_read_hashlines() {
    let (dir, engine, tree_id) = seeded_engine("read");
    let scratch = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
    let read = engine
        .read(&scratch, "src/lib.rs", ReadMode::Hashlines)
        .unwrap();

    assert_eq!(read.scratch, scratch);
    assert!(read.scratch.as_str().starts_with("scratch:"));
    assert_eq!(read.path, "src/lib.rs");
    assert!(read.file_view_hash.as_str().starts_with("file_view:"));
    assert!(read.content.contains("println!"));
    assert!(read.content.lines().all(|line| line.contains('#')));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn write_creates_new_scratch_and_preserves_parent() {
    let (dir, engine, tree_id) = seeded_engine("write");
    let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
    let write = engine
        .write(&root, "src/main.rs", b"fn main() {}\n")
        .unwrap();
    let read = engine
        .read(&write.scratch, "src/main.rs", ReadMode::Text)
        .unwrap();

    assert_eq!(write.parent, root);
    assert_ne!(write.parent, write.scratch);
    assert_eq!(read.content, "fn main() {}\n");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn delete_creates_new_scratch_and_diff_reports_deleted_path() {
    let (dir, engine, tree_id) = seeded_engine("delete");
    let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
    let delete = engine.delete(&root, "src/lib.rs").unwrap();

    assert_eq!(delete.parent, root);
    assert_ne!(delete.parent, delete.scratch);
    assert_eq!(delete.path, "src/lib.rs");
    let deleted_state = engine.state(&delete.scratch).unwrap();
    assert!(matches!(
        deleted_state.node.op,
        Some(CanonicalScratchOp::Delete { path }) if path == "src/lib.rs"
    ));
    assert!(
        deleted_state
            .tree
            .entries
            .iter()
            .all(|entry| entry.path != "src/lib.rs")
    );
    assert!(matches!(
        engine
            .read(&delete.scratch, "src/lib.rs", ReadMode::Text)
            .unwrap_err(),
        ScratchError::Store(graft_store::StoreError::VirtualPathNotFound(path))
            if path == "src/lib.rs"
    ));
    let diff = engine.diff(&root, &delete.scratch).unwrap();
    assert_eq!(diff.changed_paths, vec!["src/lib.rs".to_string()]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn delete_missing_path_fails_loudly() {
    let (dir, engine, tree_id) = seeded_engine("delete_missing");
    let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
    let err = engine.delete(&root, "missing.rs").unwrap_err();

    assert!(matches!(
        err,
        ScratchError::Store(graft_store::StoreError::VirtualPathNotFound(path))
            if path == "missing.rs"
    ));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn edit_replace_line_checks_hash_and_returns_new_scratch() {
    let (dir, engine, tree_id) = seeded_engine("edit");
    let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
    let read = engine.read(&root, "src/lib.rs", ReadMode::Text).unwrap();
    let lines = logical_lines(&read.content);
    let hash = line_hash(2, &lines[1]);
    let edit = engine
        .edit(
            &root,
            "src/lib.rs",
            vec![HashlineEdit::ReplaceLine {
                line: 2,
                hash,
                old: "    println!(\"hello\");".to_string(),
                new: "    println!(\"hi\");".to_string(),
            }],
        )
        .unwrap();

    assert_ne!(edit.parent, edit.scratch);
    assert!(edit.updated_anchors.contains("hi"));
    let read_after = engine
        .read(&edit.scratch, "src/lib.rs", ReadMode::Text)
        .unwrap();
    assert!(read_after.content.contains("hi"));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn edit_rejects_stale_anchor_and_display_prefix_payloads() {
    let (dir, engine, tree_id) = seeded_engine("stale");
    let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
    let stale = engine
        .edit(
            &root,
            "src/lib.rs",
            vec![HashlineEdit::ReplaceLine {
                line: 2,
                hash: "ZZ".to_string(),
                old: "    println!(\"hello\");".to_string(),
                new: "    println!(\"hi\");".to_string(),
            }],
        )
        .unwrap_err();
    assert!(matches!(stale, ScratchError::StaleAnchor { .. }));

    let read = engine.read(&root, "src/lib.rs", ReadMode::Text).unwrap();
    let lines = logical_lines(&read.content);
    let hash = line_hash(2, &lines[1]);
    let invalid = engine
        .edit(
            &root,
            "src/lib.rs",
            vec![HashlineEdit::ReplaceLine {
                line: 2,
                hash,
                old: "    println!(\"hello\");".to_string(),
                new: "2#TX:    println!(\"hi\");".to_string(),
            }],
        )
        .unwrap_err();
    assert!(matches!(invalid, ScratchError::InvalidPatch(_)));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn candidate_from_scratch_allows_empty_diff() {
    let (dir, engine, tree_id) = seeded_engine("empty_change");
    let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
    let write = engine
        .write(
            &root,
            "src/lib.rs",
            b"pub fn hello() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();
    let result = engine
        .candidate_from_scratch(&write.scratch, Constraint::Top, "test", None)
        .unwrap();

    assert!(result.changed_paths.is_empty());
    assert!(result.candidate.as_str().starts_with("candidate:"));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn capture_tree_creates_root_scratch_for_snapshot_diff() {
    let dir = temp_dir("capture_tree");
    let store = GraftStore::open(&dir);
    store.init().unwrap();
    let readme_hash = store.write_blob(b"# demo\n").unwrap();
    let old_lib_hash = store.write_blob(b"old\n").unwrap();
    let new_lib_hash = store.write_blob(b"new\n").unwrap();
    let main_hash = store.write_blob(b"fn main() {}\n").unwrap();
    let base = TreeSnapshot::new(vec![
        TreeEntry {
            path: "README.md".to_string(),
            hash: readme_hash,
            size: 7,
        },
        TreeEntry {
            path: "src/lib.rs".to_string(),
            hash: old_lib_hash,
            size: 4,
        },
    ]);
    let target = TreeSnapshot::new(vec![
        TreeEntry {
            path: "src/lib.rs".to_string(),
            hash: new_lib_hash,
            size: 4,
        },
        TreeEntry {
            path: "src/main.rs".to_string(),
            hash: main_hash,
            size: 13,
        },
    ]);
    let (base_tree, _) = store.write_tree_snapshot(&base).unwrap();
    let (target_tree, _) = store.write_tree_snapshot(&target).unwrap();
    let engine = ScratchEngine::new(store);

    let capture = engine
        .capture_tree(
            StateId::GraftTree(base_tree.clone()),
            &base_tree,
            &target_tree,
        )
        .unwrap();

    assert!(capture.scratch.as_str().starts_with("scratch:"));
    assert_eq!(capture.base_tree, base_tree);
    assert_eq!(capture.target_tree, target_tree);
    assert_eq!(
        capture.changed_paths,
        vec![
            "README.md".to_string(),
            "src/lib.rs".to_string(),
            "src/main.rs".to_string(),
        ]
    );
    assert_eq!(
        engine
            .read(&capture.scratch, "src/lib.rs", ReadMode::Text)
            .unwrap()
            .content,
        "new\n"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn capture_tree_rejects_empty_diff() {
    let (dir, engine, tree_id) = seeded_engine("capture_empty");

    let err = engine
        .capture_tree(StateId::GraftTree(tree_id.clone()), &tree_id, &tree_id)
        .unwrap_err();

    assert!(matches!(err, ScratchError::EmptyChange));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn read_after_lost_lease_returns_scratch_lost() {
    let (dir, engine, tree_id) = seeded_engine("scratch_lost");
    let scratch = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
    let restarted = ScratchEngine::new(GraftStore::open(&dir));
    let err = restarted
        .read(&scratch, "src/lib.rs", ReadMode::Text)
        .unwrap_err();

    assert!(matches!(err, ScratchError::ScratchLost(_)));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn candidate_from_scratch_writes_candidate_change_and_empty_evidence_index() {
    let dir = temp_dir("candidate_from_scratch");
    let store = GraftStore::open(&dir);
    store.init().unwrap();
    let lib_hash = store.write_blob(b"pub fn hello() {}\n").unwrap();
    let readme_hash = store.write_blob(b"# demo\n").unwrap();
    let snapshot = TreeSnapshot::new(vec![
        TreeEntry {
            path: "README.md".to_string(),
            hash: readme_hash,
            size: 7,
        },
        TreeEntry {
            path: "src/lib.rs".to_string(),
            hash: lib_hash,
            size: 18,
        },
    ]);
    let (tree_id, _) = store.write_tree_snapshot(&snapshot).unwrap();
    let engine = ScratchEngine::new(store);
    let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();
    let modified = engine
        .write(
            &root,
            "src/lib.rs",
            b"pub fn hello() { println!(\"hi\"); }\n",
        )
        .unwrap();
    let added = engine
        .write(&modified.scratch, "src/main.rs", b"fn main() {}\n")
        .unwrap();
    let deleted = engine.delete(&added.scratch, "README.md").unwrap();
    let review_constraint = Constraint::any_of(vec![
        Constraint::primitive(PlanId::new("plan:fast-review")),
        Constraint::primitive(PlanId::new("plan:slow-review")),
    ]);
    let result = engine
        .candidate_from_scratch(
            &deleted.scratch,
            review_constraint.clone(),
            "test",
            Some("demo".to_string()),
        )
        .unwrap();

    assert_eq!(result.scratch, deleted.scratch);
    assert!(result.candidate.as_str().starts_with("candidate:"));
    assert_eq!(
        result.changed_paths,
        vec![
            "README.md".to_string(),
            "src/lib.rs".to_string(),
            "src/main.rs".to_string(),
        ]
    );

    let candidate = engine
        .store
        .read_candidate(result.candidate.as_str())
        .unwrap();
    assert_eq!(candidate.constraint, review_constraint);
    let resolved = engine
        .store
        .resolve_application(&candidate.application)
        .unwrap();
    let change = resolved.change;
    assert!(
        change
            .endpoint_diff()
            .iter()
            .any(|file| { file.path == "README.md" && file.kind == FileChangeKind::Deleted })
    );
    assert!(
        change
            .endpoint_diff()
            .iter()
            .any(|file| { file.path == "src/lib.rs" && file.kind == FileChangeKind::Modified })
    );
    assert!(
        change
            .endpoint_diff()
            .iter()
            .any(|file| { file.path == "src/main.rs" && file.kind == FileChangeKind::Added })
    );
    assert_eq!(
        engine
            .store
            .candidate_evidence_index(result.candidate.as_str())
            .unwrap(),
        Vec::<String>::new()
    );

    let _ = std::fs::remove_dir_all(dir);
}

/// A `replace_range` edit whose start anchor still matches but whose end
/// anchor has drifted must surface a stale-anchor error that
/// includes BOTH endpoints in `fresh_anchors`. This guards against the
/// previous behavior where only the first-failing endpoint’s context
/// window was returned, forcing the caller to re-read the entire file
/// just to find a fresh end anchor.
#[test]
fn replace_range_single_end_stale_preserves_other_end() {
    let dir = temp_dir("range_stale");
    let store = GraftStore::open(&dir);
    store.init().unwrap();
    let body = b"alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\n";
    let hash = store.write_blob(body).unwrap();
    let snapshot = TreeSnapshot::new(vec![TreeEntry {
        path: "file.txt".to_string(),
        hash,
        size: body.len() as u64,
    }]);
    let (tree_id, _) = store.write_tree_snapshot(&snapshot).unwrap();
    let engine = ScratchEngine::new(store);
    let root = engine.open(VirtualBaseRef::Tree(tree_id)).unwrap();

    let read = engine.read(&root, "file.txt", ReadMode::Text).unwrap();
    let lines = logical_lines(&read.content);
    let start_hash = line_hash(2, &lines[1]);
    let bogus_end_hash = "ZZ".to_string();
    let err = engine
        .edit(
            &root,
            "file.txt",
            vec![HashlineEdit::ReplaceRange {
                start_line: 2,
                start_hash: start_hash.clone(),
                end_line: 5,
                end_hash: bogus_end_hash,
                new_lines: vec!["X".to_string()],
            }],
        )
        .unwrap_err();

    match err {
        ScratchError::StaleAnchor {
            line,
            fresh_anchors,
            ..
        } => {
            // The stale endpoint is line 5.
            assert_eq!(line, 5);
            // Both endpoints must appear, each marked with `>>>` so the
            // caller can re-anchor without losing the other end.
            assert!(
                fresh_anchors.contains(&format!(">>> 2#{start_hash}:beta")),
                "start anchor missing or unmarked: {fresh_anchors}"
            );
            let end_actual_hash = line_hash(5, &lines[4]);
            assert!(
                fresh_anchors.contains(&format!(">>> 5#{end_actual_hash}:epsilon")),
                "end anchor missing or unmarked: {fresh_anchors}"
            );
        }
        other => panic!("expected StaleAnchor, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(dir);
}
