use super::{FileTimestampsByPath, UnknownLinesByFile, file_timestamps_ns};
use std::path::Path;

pub(super) struct UnknownFileTimestampCollection {
    pub(super) unknown_by_file: UnknownLinesByFile,
    pub(super) timestamps_by_file: FileTimestampsByPath,
    pub(super) unique_timestamps: Vec<u128>,
}

pub(super) fn collect_unknown_file_timestamps(
    workdir: &Path,
    unknown_by_file: UnknownLinesByFile,
    captured_file_timestamps: Option<&FileTimestampsByPath>,
) -> UnknownFileTimestampCollection {
    let mut timestamps_by_file = FileTimestampsByPath::new();
    let mut unique_timestamps = Vec::new();
    for file_path in unknown_by_file.keys() {
        let timestamps = captured_file_timestamps
            .and_then(|timestamps| timestamps.get(file_path))
            .filter(|timestamps| !timestamps.is_empty())
            .cloned()
            .unwrap_or_else(|| file_timestamps_ns(workdir, file_path));
        if !timestamps.is_empty() {
            unique_timestamps.extend(timestamps.iter().copied());
            timestamps_by_file.insert(file_path.clone(), timestamps);
        }
    }
    unique_timestamps.sort_unstable();
    unique_timestamps.dedup();

    UnknownFileTimestampCollection {
        unknown_by_file,
        timestamps_by_file,
        unique_timestamps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};

    #[test]
    fn keeps_unknown_files_and_deduplicates_only_query_timestamps() {
        let unknown_by_file = BTreeMap::from([
            ("alpha.rs".to_string(), vec![1]),
            ("beta.rs".to_string(), vec![2]),
            ("missing.rs".to_string(), vec![3]),
        ]);
        let captured_timestamps = HashMap::from([
            ("alpha.rs".to_string(), vec![30, 10, 30]),
            ("beta.rs".to_string(), vec![20, 10]),
            ("missing.rs".to_string(), Vec::new()),
        ]);

        let collection = collect_unknown_file_timestamps(
            Path::new("/definitely-missing-workdir"),
            unknown_by_file.clone(),
            Some(&captured_timestamps),
        );

        assert_eq!(collection.unknown_by_file, unknown_by_file);
        assert_eq!(
            collection.timestamps_by_file.get("alpha.rs"),
            Some(&vec![30, 10, 30]),
            "captured per-file timestamp order and duplicates are preserved"
        );
        assert_eq!(
            collection.timestamps_by_file.get("beta.rs"),
            Some(&vec![20, 10])
        );
        assert!(
            !collection.timestamps_by_file.contains_key("missing.rs"),
            "an empty captured vector falls back to filesystem timestamps and stays absent when no file exists"
        );
        assert_eq!(collection.unique_timestamps, vec![10, 20, 30]);
    }

    #[test]
    fn returns_empty_collections_for_empty_unknown_files() {
        let collection = collect_unknown_file_timestamps(
            Path::new("/definitely-missing-workdir"),
            BTreeMap::new(),
            None,
        );

        assert!(collection.unknown_by_file.is_empty());
        assert!(collection.timestamps_by_file.is_empty());
        assert!(collection.unique_timestamps.is_empty());
    }
}
