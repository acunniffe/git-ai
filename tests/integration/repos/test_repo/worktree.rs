use super::*;

pub fn with_worktree_mode<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    WORKTREE_MODE.with(|flag| {
        let previous = flag.replace(true);

        struct Reset<'a> {
            flag: &'a Cell<bool>,
            previous: bool,
        }
        impl<'a> Drop for Reset<'a> {
            fn drop(&mut self) {
                self.flag.set(self.previous);
            }
        }
        let _reset = Reset { flag, previous };

        let mut settings = Settings::clone_current();
        settings.set_snapshot_suffix("worktree");
        settings.bind(f)
    })
}

#[cfg(unix)]
pub(super) fn create_file_symlink(target: &PathBuf, link: &PathBuf) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
pub(super) fn create_file_symlink(target: &PathBuf, link: &PathBuf) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
        .or_else(|_| std::fs::copy(target, link).map(|_| ()))
}

pub(super) fn resolve_test_db_path(
    base: &std::path::Path,
    id: u64,
    _test_home: &std::path::Path,
) -> PathBuf {
    base.join(format!("{}-db", id))
}
