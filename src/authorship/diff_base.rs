pub(crate) const SHA1_EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
pub(crate) const SHA256_EMPTY_TREE: &str =
    "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321";

pub(crate) fn empty_tree_for_oid(oid: &str) -> &'static str {
    if oid.len() == 64 {
        SHA256_EMPTY_TREE
    } else {
        SHA1_EMPTY_TREE
    }
}

pub(crate) fn is_empty_tree_oid(oid: &str) -> bool {
    oid == SHA1_EMPTY_TREE || oid == SHA256_EMPTY_TREE
}

/// Resolve the diff base for post-commit diff parsing so the diff is always
/// bounded to the single commit being finalized.
///
/// The caller's `parent_sha` is normally the immediate parent already, but on
/// the daemon's fast-forward `update-ref` path it can be the old branch tip from
/// before a pull. Using `<commit_sha>^` lets Git resolve the finalized commit's
/// first parent inside the existing diff spawn. Root commits use Git's empty
/// tree hash because there is no parent revision.
pub(crate) fn single_commit_diff_base(parent_sha: &str, commit_sha: &str) -> String {
    if parent_sha == "initial" {
        empty_tree_for_oid(commit_sha).to_string()
    } else {
        format!("{commit_sha}^")
    }
}
