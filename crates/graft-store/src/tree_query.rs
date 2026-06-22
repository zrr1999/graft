use super::*;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TreeListOptions {
    pub path: Option<String>,
    pub glob: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct TreeListResult {
    pub entries: Vec<TreePathEntry>,
    pub total_matches: usize,
    pub limit: Option<usize>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct TreePathEntry {
    pub path: String,
    pub hash: String,
    pub size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeGrepOptions {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct TreeGrepResult {
    pub matches: Vec<TreeGrepMatch>,
    pub total_matches: usize,
    pub searched_paths: usize,
    pub skipped_binary_paths: Vec<String>,
    pub limit: Option<usize>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct TreeGrepMatch {
    pub path: String,
    pub line: usize,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct TreeMetadata {
    pub path: String,
    pub kind: TreeMetadataKind,
    pub hash: Option<String>,
    pub size: Option<u64>,
    pub is_utf8_text: Option<bool>,
    pub line_count: Option<usize>,
    pub child_count: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeMetadataKind {
    File,
    Directory,
}

impl GraftStore {
    pub fn tree_list(
        &self,
        snapshot: &TreeSnapshot,
        options: &TreeListOptions,
    ) -> Result<TreeListResult> {
        let prefix = normalize_optional_tree_prefix(options.path.as_deref())?;
        let glob = options.glob.as_deref();
        let mut total_matches = 0usize;
        let mut entries = Vec::new();
        for entry in snapshot
            .entries
            .iter()
            .filter(|entry| tree_entry_matches(entry, prefix.as_deref(), glob))
        {
            total_matches += 1;
            if options.limit.is_none_or(|limit| entries.len() < limit) {
                entries.push(TreePathEntry {
                    path: entry.path.clone(),
                    hash: entry.hash.clone(),
                    size: entry.size,
                });
            }
        }
        Ok(TreeListResult {
            entries,
            total_matches,
            limit: options.limit,
            truncated: options.limit.is_some_and(|limit| total_matches > limit),
        })
    }

    pub fn tree_grep(
        &self,
        snapshot: &TreeSnapshot,
        options: &TreeGrepOptions,
    ) -> Result<TreeGrepResult> {
        let prefix = normalize_optional_tree_prefix(options.path.as_deref())?;
        let glob = options.glob.as_deref();
        let mut matches = Vec::new();
        let mut total_matches = 0usize;
        let mut searched_paths = 0usize;
        let mut skipped_binary_paths = Vec::new();
        for entry in snapshot
            .entries
            .iter()
            .filter(|entry| tree_entry_matches(entry, prefix.as_deref(), glob))
        {
            let bytes = self.read_blob(&entry.hash)?;
            let Ok(text) = std::str::from_utf8(&bytes) else {
                skipped_binary_paths.push(entry.path.clone());
                continue;
            };
            searched_paths += 1;
            for (line_index, line) in text.lines().enumerate() {
                if !line.contains(&options.pattern) {
                    continue;
                }
                total_matches += 1;
                if options.limit.is_none_or(|limit| matches.len() < limit) {
                    matches.push(TreeGrepMatch {
                        path: entry.path.clone(),
                        line: line_index + 1,
                        text: line.to_string(),
                    });
                }
            }
        }
        Ok(TreeGrepResult {
            matches,
            total_matches,
            searched_paths,
            skipped_binary_paths,
            limit: options.limit,
            truncated: options.limit.is_some_and(|limit| total_matches > limit),
        })
    }

    pub fn tree_metadata(&self, snapshot: &TreeSnapshot, path: &str) -> Result<TreeMetadata> {
        let prefix = normalize_optional_tree_prefix(Some(path))?;
        let Some(path) = prefix else {
            return Ok(TreeMetadata {
                path: String::new(),
                kind: TreeMetadataKind::Directory,
                hash: None,
                size: None,
                is_utf8_text: None,
                line_count: None,
                child_count: Some(snapshot.entries.len()),
            });
        };
        if let Some(entry) = snapshot.entries.iter().find(|entry| entry.path == path) {
            let bytes = self.read_blob(&entry.hash)?;
            let text = std::str::from_utf8(&bytes).ok();
            return Ok(TreeMetadata {
                path: entry.path.clone(),
                kind: TreeMetadataKind::File,
                hash: Some(entry.hash.clone()),
                size: Some(entry.size),
                is_utf8_text: Some(text.is_some()),
                line_count: text.map(|text| text.lines().count()),
                child_count: None,
            });
        }
        let directory_prefix = format!("{path}/");
        let child_count = snapshot
            .entries
            .iter()
            .filter(|entry| entry.path.starts_with(&directory_prefix))
            .count();
        if child_count > 0 {
            return Ok(TreeMetadata {
                path,
                kind: TreeMetadataKind::Directory,
                hash: None,
                size: None,
                is_utf8_text: None,
                line_count: None,
                child_count: Some(child_count),
            });
        }
        Err(StoreError::VirtualPathNotFound(path))
    }
}

fn tree_entry_matches(entry: &TreeEntry, prefix: Option<&str>, glob: Option<&str>) -> bool {
    prefix
        .is_none_or(|prefix| entry.path == prefix || entry.path.starts_with(&format!("{prefix}/")))
        && glob.is_none_or(|glob| wildcard_matches(glob, &entry.path))
}

fn normalize_optional_tree_prefix(path: Option<&str>) -> Result<Option<String>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let mut path = path.trim();
    while let Some(rest) = path.strip_prefix("./") {
        path = rest;
    }
    path = path.trim_end_matches('/');
    if path.is_empty() || path == "." {
        return Ok(None);
    }
    Ok(Some(normalize_virtual_path(path)?))
}

fn wildcard_matches(pattern: &str, path: &str) -> bool {
    if pattern.is_empty() {
        return path.is_empty();
    }
    if pattern == "*" || pattern == "**" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == path;
    }
    let starts_with_wildcard = pattern.starts_with('*');
    let ends_with_wildcard = pattern.ends_with('*');
    let parts = pattern
        .split('*')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return true;
    }
    let mut rest = path;
    if !starts_with_wildcard && let Some(first) = parts.first() {
        if !rest.starts_with(first) {
            return false;
        }
        rest = &rest[first.len()..];
    }
    for part in parts.iter().skip(usize::from(!starts_with_wildcard)) {
        let Some(index) = rest.find(part) else {
            return false;
        };
        rest = &rest[index + part.len()..];
    }
    if !ends_with_wildcard && let Some(last) = parts.last() {
        return path.ends_with(last);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded_store(name: &str) -> (std::path::PathBuf, GraftStore, TreeSnapshot) {
        let dir = std::env::temp_dir().join(format!(
            "graft-store-tree-query-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = GraftStore::open(&dir);
        store.init_storage().unwrap();
        let base = store.write_blob(b"base needle\nsecond\n").unwrap();
        let docs = store.write_blob(b"docs\n").unwrap();
        let binary = store.write_blob(b"\xff\x00\x01").unwrap();
        let snapshot = TreeSnapshot::new(vec![
            TreeEntry {
                path: "src/base.rs".to_string(),
                hash: base,
                size: 19,
            },
            TreeEntry {
                path: "docs/readme.md".to_string(),
                hash: docs,
                size: 5,
            },
            TreeEntry {
                path: "assets/image.bin".to_string(),
                hash: binary,
                size: 3,
            },
        ]);
        (dir, store, snapshot)
    }

    #[test]
    fn tree_list_filters_by_path_glob_and_limit() {
        let (dir, store, snapshot) = seeded_store("list");

        let result = store
            .tree_list(
                &snapshot,
                &TreeListOptions {
                    path: Some("./src/".to_string()),
                    glob: Some("*.rs".to_string()),
                    limit: Some(1),
                },
            )
            .unwrap();

        assert_eq!(result.total_matches, 1);
        assert!(!result.truncated);
        assert_eq!(result.entries[0].path, "src/base.rs");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tree_grep_searches_text_and_reports_binary_skips_and_truncation() {
        let (dir, store, snapshot) = seeded_store("grep");

        let result = store
            .tree_grep(
                &snapshot,
                &TreeGrepOptions {
                    pattern: "e".to_string(),
                    path: None,
                    glob: None,
                    limit: Some(1),
                },
            )
            .unwrap();

        assert!(result.total_matches > result.matches.len());
        assert!(result.truncated);
        assert_eq!(result.matches[0].path, "src/base.rs");
        assert_eq!(result.skipped_binary_paths, vec!["assets/image.bin"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tree_metadata_reports_text_binary_and_directory_without_content() {
        let (dir, store, snapshot) = seeded_store("metadata");

        let text = store.tree_metadata(&snapshot, "src/base.rs").unwrap();
        assert_eq!(text.kind, TreeMetadataKind::File);
        assert_eq!(text.is_utf8_text, Some(true));
        assert_eq!(text.line_count, Some(2));
        assert!(text.hash.is_some());

        let binary = store.tree_metadata(&snapshot, "assets/image.bin").unwrap();
        assert_eq!(binary.kind, TreeMetadataKind::File);
        assert_eq!(binary.is_utf8_text, Some(false));
        assert_eq!(binary.line_count, None);

        let directory = store.tree_metadata(&snapshot, "src").unwrap();
        assert_eq!(directory.kind, TreeMetadataKind::Directory);
        assert_eq!(directory.child_count, Some(1));
        assert_eq!(directory.hash, None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
