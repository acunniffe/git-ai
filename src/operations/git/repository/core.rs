//! The [`Repository`] struct and its core methods: config access, identity
//! resolution, ref/HEAD lookup, object lookup, and basic git plumbing. Diff and
//! bulk object-read methods live in the sibling `diff` and `object_reads`
//! modules; both extend this same `impl Repository`.

use super::commits::{Commit, Object};
use super::discovery_no_exec::git_config_file_for_repo_paths;
use super::git_objects::{Blob, Reference, Tree};
use super::identity::{
    GitAuthorIdentity, GitIdentityResolution, git_config_identity_resolution_from_config,
    resolve_git_var_identity_with_args,
};
use crate::clients::git_cli::exec_git;
use crate::config;
use crate::error::GitAiError;
use crate::operations::git::repo_storage::RepoStorage;
use crate::operations::git::sync_authorship::push_authorship_notes;
use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::diff::parse_git_version;

#[derive(Debug, Clone)]
pub struct Repository {
    pub(super) global_args: Vec<String>,
    pub(super) git_dir: PathBuf,
    pub(super) git_common_dir: PathBuf,
    pub storage: RepoStorage,
    pub pre_command_base_commit: Option<String>,
    pub pre_command_refname: Option<String>,
    pub pre_reset_target_commit: Option<String>,
    pub pre_update_ref_refname: Option<String>,
    pub pre_update_ref_old_target: Option<String>,
    pub pre_update_ref_affects_checked_out_branch: Option<bool>,
    pub(super) workdir: PathBuf,
    /// Canonical (absolute, resolved) version of workdir for reliable path comparisons
    /// On Windows, this uses the \\?\ UNC prefix format
    pub(super) canonical_workdir: PathBuf,
    /// Cached git author identity resolved via `git var GIT_COMMITTER_IDENT`.
    pub(super) cached_author_identity: std::sync::OnceLock<GitAuthorIdentity>,
}

impl Repository {
    // Util for preparing global args for execution
    pub fn global_args_for_exec(&self) -> Vec<String> {
        let mut args = self.global_args.clone();
        if !args.iter().any(|arg| arg == "--no-pager") {
            args.push("--no-pager".to_string());
        }
        args
    }

    pub fn require_pre_command_head(&mut self) {
        if self.pre_command_base_commit.is_some() || self.pre_command_refname.is_some() {
            return;
        }

        // Safely handle empty repositories
        if let Ok(head_ref) = self.head()
            && let Ok(target) = head_ref.target()
        {
            let target_string = target;
            let refname = head_ref.name().map(|n| n.to_string());
            self.pre_command_base_commit = Some(target_string);
            self.pre_command_refname = refname;
        }
    }

    // Internal util to get the git object type for a given OID
    pub(super) fn object_type(&self, oid: &str) -> Result<String, GitAiError> {
        let reader =
            crate::operations::git::fast_reader::FastObjectReader::new(&self.git_common_dir);
        if let Some(typ) = reader.try_read_object_type(oid) {
            return Ok(typ);
        }

        let mut args = self.global_args_for_exec();
        args.push("cat-file".to_string());
        args.push("-t".to_string());
        args.push(oid.to_string());
        let output = exec_git(&args)?;
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    // Retrieve and resolve the reference pointed at by HEAD.
    // If HEAD is a symbolic ref, return the refname (e.g., "refs/heads/main").
    // Otherwise, return "HEAD".
    pub fn head<'a>(&'a self) -> Result<Reference<'a>, GitAiError> {
        use crate::operations::git::fast_reader::{FastRefReader, HeadKind};
        let reader = FastRefReader::new(&self.git_dir, &self.git_common_dir);
        match reader.try_read_head() {
            Some(HeadKind::Symbolic(refname)) => {
                return Ok(Reference {
                    repo: self,
                    ref_name: refname,
                });
            }
            Some(HeadKind::Detached(_)) => {
                return Ok(Reference {
                    repo: self,
                    ref_name: "HEAD".to_string(),
                });
            }
            None => {}
        }

        let mut args = self.global_args_for_exec();
        args.push("symbolic-ref".to_string());
        args.push("HEAD".to_string());

        let output = exec_git(&args);

        match output {
            Ok(output) if output.status.success() => {
                let refname = String::from_utf8(output.stdout)?;
                Ok(Reference {
                    repo: self,
                    ref_name: refname.trim().to_string(),
                })
            }
            _ => Ok(Reference {
                repo: self,
                ref_name: "HEAD".to_string(),
            }),
        }
    }

    // Returns the path to the .git folder for normal repositories or the repository itself for bare repositories.
    // TODO Test on bare repositories.
    pub fn path(&self) -> &Path {
        self.git_dir.as_path()
    }

    /// Returns the common git directory shared by linked worktrees.
    /// For non-worktree repositories, this is the same as `path()`.
    pub fn common_dir(&self) -> &Path {
        self.git_common_dir.as_path()
    }

    // Get the path of the working directory for this repository.
    // If this repository is bare, then None is returned.
    pub fn workdir(&self) -> Result<PathBuf, GitAiError> {
        // TODO Remove Result since this is determined at initialization now
        Ok(self.workdir.clone())
    }

    /// Canonical (symlink-resolved) working directory root.
    pub fn canonical_workdir(&self) -> &Path {
        self.canonical_workdir.as_path()
    }

    /// Returns true when this repository is bare.
    pub fn is_bare_repository(&self) -> Result<bool, GitAiError> {
        let mut args = self.global_args_for_exec();
        args.push("rev-parse".to_string());
        args.push("--is-bare-repository".to_string());
        let output = exec_git(&args)?;
        let value = String::from_utf8(output.stdout)?;
        Ok(value.trim() == "true")
    }

    /// Get the canonical (absolute, resolved) path of the working directory
    /// Check if a path is within the repository's working directory.
    ///
    /// Returns `false` for paths inside nested independent git repos (subdirectories
    /// with their own `.git/` directory), since those files belong to the nested repo,
    /// not this one. Submodules (`.git` file, not directory) are transparent and still
    /// considered part of this repo.
    pub fn path_is_in_workdir(&self, path: &Path) -> bool {
        // Try canonical comparison first (most reliable, especially on Windows)
        if let Ok(canonical_path) = path.canonicalize() {
            if !canonical_path.starts_with(&self.canonical_workdir) {
                return false;
            }
            return !super::discovery_no_exec::has_intervening_git_dir(
                &canonical_path,
                &self.canonical_workdir,
            );
        }

        // Fallback for paths that don't exist yet: try to canonicalize the parent directory
        // and append the filename. This handles cases where the path contains symlinks
        // (e.g., /var -> /private/var on macOS).
        if let Some(parent) = path.parent()
            && let Some(filename) = path.file_name()
            && let Ok(canonical_parent) = parent.canonicalize()
        {
            let canonical_path = canonical_parent.join(filename);
            if !canonical_path.starts_with(&self.canonical_workdir) {
                return false;
            }
            return !super::discovery_no_exec::has_intervening_git_dir(
                &canonical_path,
                &self.canonical_workdir,
            );
        }

        // Final fallback: normalize by resolving .. and . and check against both
        // canonical and non-canonical workdir (in case of symlinks)
        let normalized = path
            .components()
            .fold(std::path::PathBuf::new(), |mut acc, component| {
                match component {
                    std::path::Component::ParentDir => {
                        acc.pop();
                    }
                    std::path::Component::CurDir => {}
                    _ => acc.push(component),
                }
                acc
            });

        // Try both canonical and non-canonical workdir to handle symlinks
        let in_canonical = normalized.starts_with(&self.canonical_workdir);
        let in_workdir = normalized.starts_with(&self.workdir);

        if !in_canonical && !in_workdir {
            return false;
        }

        // Use canonical_workdir if path matches it, otherwise use workdir
        let base = if in_canonical {
            &self.canonical_workdir
        } else {
            &self.workdir
        };

        !super::discovery_no_exec::has_intervening_git_dir(&normalized, base)
    }

    pub fn remotes(&self) -> Result<Vec<String>, GitAiError> {
        Ok(self
            .remotes_with_urls()?
            .into_iter()
            .map(|(name, _)| name)
            .collect())
    }

    // List all remotes with their URLs as tuples (name, url)
    pub fn remotes_with_urls(&self) -> Result<Vec<(String, String)>, GitAiError> {
        let config = self.get_git_config_file()?;
        Ok(super::policy::remotes_with_urls_from_config(&config))
    }

    /// Whether git-ai collection is allowed for this repository under `config`.
    ///
    /// Fetches the repository's remotes and root once and delegates to the pure
    /// `config::Config::is_allowed_repository_with_context` policy. Collection
    /// is opt-in: an empty `allowed_repositories` list denies every repository.
    pub fn is_collection_allowed(&self, config: &config::Config) -> bool {
        let remotes = self.remotes_with_urls().ok();
        let repo_root = self.canonical_workdir();
        config.is_allowed_repository_with_context(remotes.as_ref(), Some(repo_root))
    }

    pub(super) fn load_optional_config_file(
        path: &Path,
        source: gix_config::Source,
    ) -> Result<Option<gix_config::File<'static>>, GitAiError> {
        if !path.exists() {
            return Ok(None);
        }
        gix_config::File::from_path_no_includes(path.to_path_buf(), source)
            .map(Some)
            .map_err(|e| GitAiError::GixError(e.to_string()))
    }

    pub(crate) fn get_git_config_file(&self) -> Result<gix_config::File<'static>, GitAiError> {
        git_config_file_for_repo_paths(self.path(), self.common_dir())
    }

    /// Get config value for a given key as a String.
    pub fn config_get_str(&self, key: &str) -> Result<Option<String>, GitAiError> {
        self.get_git_config_file()
            .map(|cfg| cfg.string(key).map(|cow| cow.to_string()))
    }

    /// Get the effective raw Git user identity for this repository.
    ///
    /// Uses `git var GIT_COMMITTER_IDENT` which respects the full git identity precedence:
    /// `GIT_COMMITTER_NAME`/`GIT_COMMITTER_EMAIL` env vars > `user.name`/`user.email` config >
    /// system defaults.
    ///
    /// Falls back to `git config user.name` / `user.email` if `git var` fails.
    /// The result is cached per Repository instance for performance.
    ///
    /// For git-ai authorship metadata, use [`Self::effective_author_identity`] so the
    /// git-ai author config can override this raw Git identity.
    pub fn git_author_identity(&self) -> &GitAuthorIdentity {
        self.cached_author_identity
            .get_or_init(|| self.resolve_git_var_identity("GIT_COMMITTER_IDENT"))
    }

    pub fn git_author_identity_resolution(&self) -> GitIdentityResolution {
        self.resolve_git_var_identity_resolution("GIT_COMMITTER_IDENT")
    }

    /// Get the git-ai effective author identity for metadata and display.
    ///
    /// This starts from Git's effective committer identity, then overlays any
    /// configured `author.name` and/or `author.email` from git-ai config.
    pub fn effective_author_identity(&self) -> GitAuthorIdentity {
        let git_id = self.git_author_identity();
        git_id.with_author_config(&config::Config::fresh_author_cached())
    }

    /// Get the effective git commit author identity for this repository.
    ///
    /// Uses `git var GIT_AUTHOR_IDENT` which respects:
    /// `GIT_AUTHOR_NAME`/`GIT_AUTHOR_EMAIL` env vars > `user.name`/`user.email` config >
    /// system defaults.
    ///
    /// Falls back to `git config user.name` / `user.email` if `git var` fails.
    ///
    /// This is the correct method to use when resolving the commit **author** identity
    /// (as opposed to committer), e.g. in commit hooks.
    pub fn git_commit_author_identity(&self) -> GitAuthorIdentity {
        self.resolve_git_var_identity("GIT_AUTHOR_IDENT")
    }

    /// Internal: resolve git identity via the specified `git var` variable.
    fn resolve_git_var_identity(&self, git_var: &str) -> GitAuthorIdentity {
        self.resolve_git_var_identity_resolution(git_var).identity
    }

    fn resolve_git_var_identity_resolution(&self, git_var: &str) -> GitIdentityResolution {
        resolve_git_var_identity_with_args(self.global_args_for_exec(), git_var, || {
            self.get_git_config_file()
                .ok()
                .map(|config| git_config_identity_resolution_from_config(&config).identity)
                .unwrap_or_default()
        })
    }

    /// Get all config values matching a regex pattern.
    ///
    /// Regular expression matching is currently case-sensitive
    /// and done against a canonicalized version of the key
    /// in which section and variable names are lowercased, but subsection names are not.
    ///
    /// Returns a HashMap of key -> value for all matching config entries.
    pub fn config_get_regexp(
        &self,
        pattern: &str,
    ) -> Result<std::collections::HashMap<String, String>, GitAiError> {
        let re = Regex::new(pattern)
            .map_err(|e| GitAiError::Generic(format!("Invalid regex pattern: {}", e)))?;

        let config = self.get_git_config_file()?;
        let mut matches: HashMap<String, String> = HashMap::new();

        for section in config.sections() {
            let section_name = section.header().name().to_string().to_lowercase();
            let subsection = section.header().subsection_name();

            for value_name in section.body().value_names() {
                let value_name_str = value_name.to_string().to_lowercase();
                let full_key = if let Some(sub) = subsection {
                    format!("{}.{}.{}", section_name, sub, value_name_str)
                } else {
                    format!("{}.{}", section_name, value_name_str)
                };

                if re.is_match(&full_key)
                    && let Some(value) = section.body().value(value_name).map(|c| c.to_string())
                {
                    matches.insert(full_key, value);
                }
            }
        }

        Ok(matches)
    }

    /// Get the git version as a tuple (major, minor, patch).
    /// Returns None if the version cannot be parsed.
    pub fn git_version(&self) -> Option<(u32, u32, u32)> {
        let args = vec!["--version".to_string()];
        let output = exec_git(&args).ok()?;
        let version_str = String::from_utf8(output.stdout).ok()?;
        parse_git_version(&version_str)
    }

    /// Check if the current git version supports --ignore-revs-file flag for blame.
    /// This flag was added in git 2.23.0.
    pub fn git_supports_ignore_revs_file(&self) -> bool {
        if let Some((major, minor, _)) = self.git_version() {
            // --ignore-revs-file was added in git 2.23.0
            major > 2 || (major == 2 && minor >= 23)
        } else {
            // If we can't determine the version, assume it's supported
            // to avoid breaking existing functionality
            true
        }
    }

    // Write an in-memory buffer to the ODB as a blob.
    #[allow(dead_code)]
    pub fn remote_head(&self, remote_name: &str) -> Result<String, GitAiError> {
        let mut args = self.global_args_for_exec();
        args.push("symbolic-ref".to_string());
        args.push(format!("refs/remotes/{}/HEAD", remote_name));
        args.push("--short".to_string());

        let output = exec_git(&args)?;
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    // Find a merge base between two commits
    pub fn merge_base(&self, one: String, two: String) -> Result<String, GitAiError> {
        let mut args = self.global_args_for_exec();
        args.push("merge-base".to_string());
        args.push(one.to_string());
        args.push(two.to_string());
        let output = exec_git(&args)?;
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    // Find a single object, as specified by a revision string.
    pub fn revparse_single(&self, spec: &str) -> Result<Object<'_>, GitAiError> {
        let mut args = self.global_args_for_exec();
        args.push("rev-parse".to_string());
        // args.push("-q".to_string());
        args.push("--verify".to_string());
        args.push(spec.to_string());
        let output = exec_git(&args)?;
        Ok(Object {
            repo: self,
            oid: String::from_utf8(output.stdout)?.trim().to_string(),
        })
    }

    // Non-standard method of getting a 'default' remote
    pub fn get_default_remote(&self) -> Result<Option<String>, GitAiError> {
        let remotes = self.remotes()?;
        if remotes.is_empty() {
            return Ok(None);
        }
        // Prefer 'origin' if it exists
        for i in 0..remotes.len() {
            if let Some(name) = remotes.get(i)
                && name == "origin"
            {
                return Ok(Some("origin".to_string()));
            }
        }
        // Otherwise, just use the first remote
        Ok(remotes.first().map(|s| s.to_string()))
    }

    #[allow(dead_code)]
    pub fn push_authorship(&self, remote_name: &str) -> Result<(), GitAiError> {
        push_authorship_notes(self, remote_name)
    }

    pub fn upstream_remote(&self) -> Result<Option<String>, GitAiError> {
        // Get current branch name using exec_git
        let mut args = self.global_args_for_exec();
        args.push("branch".to_string());
        args.push("--show-current".to_string());
        let output = exec_git(&args)?;
        let branch = String::from_utf8(output.stdout)?.trim().to_string();
        if branch.is_empty() {
            return Ok(None);
        }
        let config_key = format!("branch.{}.remote", branch);
        self.config_get_str(&config_key)
    }

    pub fn resolve_author_spec(&self, author_spec: &str) -> Result<Option<String>, GitAiError> {
        // Use git rev-list to find the first commit by this author pattern
        let mut args = self.global_args_for_exec();
        args.push("rev-list".to_string());
        args.push("--all".to_string());
        args.push("-i".to_string());
        args.push("--max-count=1".to_string());
        args.push(format!("--author={}", author_spec));
        let output = match exec_git(&args) {
            Ok(output) => output,
            Err(GitAiError::GitCliError { code: Some(1), .. }) => {
                // No commit found
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let commit_oid = String::from_utf8(output.stdout)?.trim().to_string();
        if commit_oid.is_empty() {
            return Ok(None);
        }

        // Now get the author name/email from that commit
        let mut show_args = self.global_args_for_exec();
        show_args.push("show".to_string());
        show_args.push("-s".to_string());
        show_args.push("--format=%an <%ae>".to_string());
        show_args.push(commit_oid);
        let show_output = exec_git(&show_args)?;
        let author_line = String::from_utf8(show_output.stdout)?.trim().to_string();
        if author_line.is_empty() {
            Ok(None)
        } else {
            Ok(Some(author_line))
        }
    }

    // Lookup a reference to one of the commits in a repository.
    pub fn find_commit(&self, oid: String) -> Result<Commit<'_>, GitAiError> {
        let typ = self.object_type(&oid)?;
        if typ != "commit" {
            return Err(GitAiError::Generic(format!(
                "Object is not a commit: {} (type: {})",
                oid, typ
            )));
        }
        Ok(Commit { repo: self, oid })
    }

    // Lookup a reference to one of the objects in a repository.
    pub fn find_blob(&self, oid: String) -> Result<Blob<'_>, GitAiError> {
        let typ = self.object_type(&oid)?;
        if typ != "blob" {
            return Err(GitAiError::Generic(format!(
                "Object is not a blob: {} (type: {})",
                oid, typ
            )));
        }
        Ok(Blob { repo: self, oid })
    }

    // Lookup a reference to one of the objects in a repository.
    pub fn find_tree(&self, oid: String) -> Result<Tree<'_>, GitAiError> {
        let typ = self.object_type(&oid)?;
        if typ != "tree" {
            return Err(GitAiError::Generic(format!(
                "Object is not a tree: {} (type: {})",
                oid, typ
            )));
        }
        Ok(Tree { repo: self, oid })
    }

    /// Read file content from a tree, using fast filesystem reads with git CLI fallback.
    pub fn read_file_blob_at_tree(
        &self,
        tree_oid: &str,
        path: &Path,
    ) -> Result<Vec<u8>, GitAiError> {
        let reader =
            crate::operations::git::fast_reader::FastObjectReader::new(&self.git_common_dir);
        if let Some(blob_oid) = reader.try_tree_entry_for_path(tree_oid, path) {
            if let Some(content) = reader.try_read_blob(&blob_oid) {
                return Ok(content);
            }
            let blob = Blob {
                repo: self,
                oid: blob_oid,
            };
            return blob.content();
        }
        let tree = Tree {
            repo: self,
            oid: tree_oid.to_string(),
        };
        let entry = tree.get_path(path)?;
        let blob = Blob {
            repo: self,
            oid: entry.id(),
        };
        blob.content()
    }
}
