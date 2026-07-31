use std::fs;
use std::path::{Path, PathBuf};

/// Walk agent-owned transcript roots during a periodic sweep.
///
/// Adapters keep their filename and extension semantics in `include_path`; checkpoint validation
/// must resolve only its claimed path and never call this broad collector.
pub(super) fn collect_files_recursively(
    roots: impl IntoIterator<Item = PathBuf>,
    include_path: impl Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for root in roots {
        collect_files_from_dir(&root, &include_path, &mut paths);
    }
    paths
}

fn collect_files_from_dir(
    dir: &Path,
    include_path: &impl Fn(&Path) -> bool,
    paths: &mut Vec<PathBuf>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files_from_dir(&path, include_path, paths);
        } else if path.is_file() && include_path(&path) {
            paths.push(path);
        }
    }
}

pub(super) fn stem_session_binding(path: &Path) -> Option<(String, Option<String>)> {
    Some((transcript_file_stem(path)?, None))
}

pub(super) fn transcript_file_stem(path: &Path) -> Option<String> {
    path.file_stem()?.to_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn collects_nested_files_using_the_adapter_supplied_predicate() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().join("transcripts");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let root_jsonl = root.join("root.jsonl");
        let nested_jsonl = nested.join("child.jsonl");
        fs::write(&root_jsonl, "root").unwrap();
        fs::write(&nested_jsonl, "child").unwrap();
        fs::write(nested.join("ignored.txt"), "ignored").unwrap();

        let mut paths = collect_files_recursively(vec![root], |path| {
            path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
        });
        paths.sort();

        assert_eq!(paths, vec![nested_jsonl, root_jsonl]);
    }

    #[test]
    fn ignores_missing_roots() {
        let temp_dir = tempfile::tempdir().unwrap();

        let paths = collect_files_recursively(vec![temp_dir.path().join("missing")], |_| true);

        assert!(paths.is_empty());
    }

    #[test]
    fn binds_a_utf8_file_stem_without_a_parent_session() {
        assert_eq!(
            stem_session_binding(Path::new("/transcripts/session-123.jsonl")),
            Some(("session-123".to_string(), None))
        );
        assert_eq!(stem_session_binding(Path::new("/")), None);
    }
}
