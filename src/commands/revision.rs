/// Normalize a revision token that the user may have typed with a lowercase
/// `head` prefix. On case-insensitive file systems, Git can resolve `head`
/// through the common Git directory instead of a linked worktree's `HEAD`.
///
/// Only the four-character prefix is replaced; suffixes like `~2`, `^1`, and
/// `@{0}` are preserved verbatim.
pub(crate) fn normalize_head_rev(rev: &str) -> String {
    if rev
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("head"))
    {
        let suffix = &rev[4..];
        if suffix.is_empty()
            || suffix.starts_with('~')
            || suffix.starts_with('^')
            || suffix.starts_with('@')
        {
            return format!("HEAD{}", suffix);
        }
    }
    rev.to_string()
}
