use crate::commands::{blame, checkpoint};
use crate::error::GitAiError;
use crate::git::post_commit::post_commit;
use git2::{Repository, Signature};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

pub struct TmpFile {
    repo: TmpRepo,
    filename: String,
    contents: String,
}

impl TmpFile {
    /// Updates the entire contents of the file
    pub fn update(&mut self, new_contents: &str) -> Result<(), GitAiError> {
        self.contents = new_contents.to_string();
        self.write_to_disk()?;
        self.flush_to_disk()
    }

    /// Appends content to the end of the file
    pub fn append(&mut self, content: &str) -> Result<(), GitAiError> {
        // Refresh from disk first – the file may have changed due to a branch checkout
        if let Ok(disk_contents) = fs::read_to_string(self.repo.path.join(&self.filename)) {
            self.contents = disk_contents;
        }

        // Guarantee we have a newline separator before appending
        if !self.contents.ends_with('\n') {
            self.contents.push('\n');
        }

        self.contents.push_str(content);
        self.write_to_disk()?;
        self.flush_to_disk()
    }

    /// Inserts content at a specific position
    pub fn insert_at(&mut self, position: usize, content: &str) -> Result<(), GitAiError> {
        if position > self.contents.len() {
            return Err(GitAiError::Generic(format!(
                "Position {} is out of bounds for file with {} characters",
                position,
                self.contents.len()
            )));
        }

        let mut new_contents = String::new();
        new_contents.push_str(&self.contents[..position]);
        new_contents.push_str(content);
        new_contents.push_str(&self.contents[position..]);

        self.contents = new_contents;
        self.write_to_disk()?;
        self.flush_to_disk()
    }

    /// Replaces content at a specific position with new content
    pub fn replace_at(&mut self, position: usize, new_content: &str) -> Result<(), GitAiError> {
        if position > self.contents.len() {
            return Err(GitAiError::Generic(format!(
                "Position {} is out of bounds for file with {} characters",
                position,
                self.contents.len()
            )));
        }
        let mut new_contents = self.contents.clone();
        new_contents.replace_range(position..position + new_content.len(), new_content);
        self.contents = new_contents;
        self.write_to_disk()?;
        self.flush_to_disk()
    }

    /// Replaces a range of lines with new content
    pub fn replace_range(
        &mut self,
        start_line: usize,
        end_line: usize,
        new_content: &str,
    ) -> Result<(), GitAiError> {
        // Refresh from disk first to stay in sync with the current branch version
        if let Ok(disk_contents) = fs::read_to_string(self.repo.path.join(&self.filename)) {
            self.contents = disk_contents;
        }

        let file_lines = self.contents.lines().collect::<Vec<&str>>();

        if start_line > file_lines.len() || end_line > file_lines.len() || start_line >= end_line {
            return Err(GitAiError::Generic(format!(
                "Invalid line range [{}, {}) for file with {} lines",
                start_line,
                end_line,
                file_lines.len()
            )));
        }

        let mut new_contents = String::new();

        // Add lines before the range (1-indexed to 0-indexed conversion)
        for line in file_lines[..(start_line - 1)].iter() {
            new_contents.push_str(line);
            new_contents.push('\n');
        }

        // Add the new content (split into lines and add each line)
        for line in new_content.lines() {
            new_contents.push_str(line);
            new_contents.push('\n');
        }

        // Add lines after the range (1-indexed to 0-indexed conversion)
        for line in file_lines[(end_line - 1)..].iter() {
            new_contents.push_str(line);
            new_contents.push('\n');
        }

        // Remove trailing newline if the original didn't have one
        if !self.contents.ends_with('\n') && !new_contents.is_empty() {
            new_contents.pop();
        }

        self.contents = new_contents;
        self.write_to_disk()?;
        self.flush_to_disk()
    }

    /// Gets the current contents of the file
    pub fn contents(&self) -> &str {
        &self.contents
    }

    /// Gets the filename
    pub fn filename(&self) -> &str {
        &self.filename
    }

    /// Gets the full path of the file
    pub fn path(&self) -> PathBuf {
        self.repo.path.join(&self.filename)
    }

    /// Gets the length of the file contents
    pub fn len(&self) -> usize {
        self.contents.len()
    }

    /// Checks if the file is empty
    pub fn is_empty(&self) -> bool {
        self.contents.is_empty()
    }

    /// Clears all contents from the file
    pub fn clear(&mut self) -> Result<(), GitAiError> {
        self.contents.clear();
        self.write_to_disk()?;
        self.flush_to_disk()
    }

    /// Writes the current contents to disk
    fn write_to_disk(&self) -> Result<(), GitAiError> {
        let file_path = self.repo.path.join(&self.filename);

        // Create parent directories if they don't exist
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Write the file
        fs::write(&file_path, &self.contents)?;

        // Add to git index using the filename directly
        let mut index = self.repo.repo.index()?;
        index.add_path(&std::path::Path::new(&self.filename))?;
        index.write()?;

        Ok(())
    }

    /// Flushes the file to disk to ensure all changes are written
    fn flush_to_disk(&self) -> Result<(), GitAiError> {
        use std::fs::OpenOptions;
        use std::io::Write;
        let file_path = self.repo.path.join(&self.filename);
        if let Ok(mut file) = OpenOptions::new().write(true).open(&file_path) {
            file.flush()?;
        }
        Ok(())
    }
}

pub struct TmpRepo {
    path: PathBuf,
    repo: Repository,
}

impl TmpRepo {
    /// Creates a new temporary repository at the given path
    pub fn new(tmp_dir: PathBuf) -> Result<Self, GitAiError> {
        // Create the directory if it doesn't exist
        fs::create_dir_all(&tmp_dir)?;

        // Initialize git repository
        let repo = Repository::init(&tmp_dir)?;

        // Configure git user for commits
        let mut config = repo.config()?;
        config.set_str("user.name", "Test User")?;
        config.set_str("user.email", "test@example.com")?;

        // (No initial empty commit)
        Ok(TmpRepo {
            path: tmp_dir,
            repo,
        })
    }

    pub fn new_with_base_commit(tmp_dir: PathBuf) -> Result<(Self, TmpFile, TmpFile), GitAiError> {
        let repo = TmpRepo::new(tmp_dir)?;
        let lines_file = repo.write_file("lines.md", LINES, true)?;
        let alphabet_file = repo.write_file("alphabet.md", ALPHABET, true)?;
        repo.trigger_checkpoint_with_author("test_user")?;
        repo.commit_with_message("initial commit")?;
        Ok((repo, lines_file, alphabet_file))
    }

    /// Writes a file with the given filename and contents, returns a TmpFile for further updates
    pub fn write_file(
        &self,
        filename: &str,
        contents: &str,
        add_to_git: bool,
    ) -> Result<TmpFile, GitAiError> {
        let file_path = self.path.join(filename);

        // Create parent directories if they don't exist
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Write the file
        fs::write(&file_path, contents)?;

        if add_to_git {
            let mut index = self.repo.index()?;
            index.add_path(&file_path.strip_prefix(&self.path).unwrap())?;
            index.write()?;
        }

        Ok(TmpFile {
            repo: TmpRepo {
                path: self.path.clone(),
                repo: Repository::open(&self.path)?,
            },
            filename: filename.to_string(),
            contents: contents.to_string(),
        })
    }

    /// Triggers a checkpoint with the given author
    pub fn trigger_checkpoint_with_author(
        &self,
        author: &str,
    ) -> Result<(usize, usize, usize), GitAiError> {
        checkpoint(
            &self.repo, author, false, // show_working_log
            false, // reset
            true, None, // model
            None, // human_author
        )
    }

    /// Commits all changes with the given message and runs post-commit hook
    pub fn commit_with_message(&self, message: &str) -> Result<(), GitAiError> {
        // Add all files to the index
        let mut index = self.repo.index()?;
        index.add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)?;
        index.write()?;

        // Create the commit
        let tree_id = index.write_tree()?;
        let tree = self.repo.find_tree(tree_id)?;

        // Use a fixed timestamp for stable test results
        // Unix timestamp for 2023-01-01 12:00:00 UTC
        let fixed_time = git2::Time::new(1672574400, 0);
        let signature = Signature::new("Test User", "test@example.com", &fixed_time)?;

        // Check if there's a parent commit before we use it
        let _has_parent = if let Ok(head) = self.repo.head() {
            if let Some(target) = head.target() {
                self.repo.find_commit(target).is_ok()
            } else {
                false
            }
        } else {
            false
        };

        // Get the current HEAD for the parent commit
        let parent_commit = if let Ok(head) = self.repo.head() {
            if let Some(target) = head.target() {
                Some(self.repo.find_commit(target)?)
            } else {
                None
            }
        } else {
            None
        };

        let _commit_id = if let Some(parent) = parent_commit {
            self.repo.commit(
                Some(&"HEAD"),
                &signature,
                &signature,
                message,
                &tree,
                &[&parent],
            )?
        } else {
            self.repo
                .commit(Some(&"HEAD"), &signature, &signature, message, &tree, &[])?
        };

        println!("Commit ID: {}", _commit_id);

        // Run the post-commit hook for all commits (including initial commit)
        post_commit(&self.repo, false)?; // false = not force

        Ok(())
    }

    /// Creates a new branch and switches to it
    pub fn create_branch(&self, branch_name: &str) -> Result<(), GitAiError> {
        let head = self.repo.head()?;
        let commit = self.repo.find_commit(head.target().unwrap())?;
        let _branch = self.repo.branch(branch_name, &commit, false)?;

        // Switch to the new branch
        let branch_ref = self
            .repo
            .find_reference(&format!("refs/heads/{}", branch_name))?;
        self.repo.set_head(branch_ref.name().unwrap())?;

        // Update the working directory
        let mut checkout_opts = git2::build::CheckoutBuilder::new();
        checkout_opts.force();
        self.repo.checkout_head(Some(&mut checkout_opts))?;

        Ok(())
    }

    /// Switches to an existing branch
    pub fn switch_branch(&self, branch_name: &str) -> Result<(), GitAiError> {
        let branch_ref = self
            .repo
            .find_reference(&format!("refs/heads/{}", branch_name))?;
        self.repo.set_head(branch_ref.name().unwrap())?;

        let mut checkout_opts = git2::build::CheckoutBuilder::new();
        checkout_opts.force();
        self.repo.checkout_head(Some(&mut checkout_opts))?;

        Ok(())
    }

    /// Merges a branch into the current branch using real git CLI, always picking 'theirs' in conflicts
    pub fn merge_branch(&self, branch_name: &str, message: &str) -> Result<(), GitAiError> {
        let output = Command::new("git")
            .current_dir(&self.path)
            .args(&["merge", branch_name, "-m", message, "-X", "theirs"])
            .output()
            .map_err(|e| GitAiError::Generic(format!("Failed to run git merge: {}", e)))?;

        if !output.status.success() {
            return Err(GitAiError::Generic(format!(
                "git merge failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        // Run post-commit hook
        post_commit(&self.repo, false)?;

        Ok(())
    }

    /// Rebases the current branch onto another branch using real git CLI, always picking 'theirs' in conflicts
    pub fn rebase_onto(&self, _base_branch: &str, onto_branch: &str) -> Result<(), GitAiError> {
        // First, get the current commit SHA before rebase
        // let old_sha = self.head_commit_sha()?;

        let mut rebase = Command::new("git")
            .current_dir(&self.path)
            .args(&["rebase", onto_branch])
            .output()
            .map_err(|e| GitAiError::Generic(format!("Failed to run git rebase: {}", e)))?;

        // If rebase fails due to conflict, always pick 'theirs' and continue
        while !rebase.status.success()
            && String::from_utf8_lossy(&rebase.stderr).contains("could not apply")
        {
            // Find conflicted files (for our tests, just lines.md)
            let conflicted_file = self.path.join("lines.md");
            // Overwrite with theirs (the branch we're rebasing onto)
            let theirs_content = Command::new("git")
                .current_dir(&self.path)
                .args(&["show", &format!("{}:lines.md", onto_branch)])
                .output()
                .map_err(|e| GitAiError::Generic(format!("Failed to get theirs: {}", e)))?;
            fs::write(&conflicted_file, &theirs_content.stdout)?;
            // Add and continue
            Command::new("git")
                .current_dir(&self.path)
                .args(&["add", "lines.md"])
                .output()
                .map_err(|e| GitAiError::Generic(format!("Failed to git add: {}", e)))?;
            rebase = Command::new("git")
                .current_dir(&self.path)
                .args(&["rebase", "--continue"])
                .output()
                .map_err(|e| {
                    GitAiError::Generic(format!("Failed to git rebase --continue: {}", e))
                })?;
        }

        if !rebase.status.success() {
            return Err(GitAiError::Generic(format!(
                "git rebase failed: {}",
                String::from_utf8_lossy(&rebase.stderr)
            )));
        }

        // Get the new commit SHA after rebase
        // let new_sha = self.head_commit_sha()?;

        // // Call the shared remapping function to update authorship logs
        // crate::log_fmt::authorship_log::remap_authorship_log_for_rewrite(
        //     &self.repo, &old_sha, &new_sha,
        // )?;

        // Run post-commit hook
        post_commit(&self.repo, false)?;

        Ok(())
    }

    /// Gets the current branch name
    pub fn current_branch(&self) -> Result<String, GitAiError> {
        let head = self.repo.head()?;
        let branch_name = head
            .shorthand()
            .ok_or_else(|| GitAiError::Generic("Could not get branch name".to_string()))?;
        Ok(branch_name.to_string())
    }

    /// Gets the commit SHA of the current HEAD
    pub fn head_commit_sha(&self) -> Result<String, GitAiError> {
        let head = self.repo.head()?;
        let commit_sha = head
            .target()
            .ok_or_else(|| GitAiError::Generic("No HEAD commit found".to_string()))?
            .to_string();
        Ok(commit_sha)
    }

    /// Gets the default branch name (first branch created)
    pub fn get_default_branch(&self) -> Result<String, GitAiError> {
        // Try to find the first branch that's not the current one
        let current = self.current_branch()?;

        // List all references and find the first branch
        let refs = self.repo.references()?;
        for reference in refs {
            let reference = reference?;
            if let Some(name) = reference.name() {
                if name.starts_with("refs/heads/") {
                    let branch_name = name.strip_prefix("refs/heads/").unwrap();
                    if branch_name != current {
                        return Ok(branch_name.to_string());
                    }
                }
            }
        }

        // If no other branch found, return current
        Ok(current)
    }

    /// Gets the repository path
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Gets a reference to the underlying git2 Repository
    pub fn repo(&self) -> &Repository {
        &self.repo
    }

    /// Runs blame on a file in the repository
    pub fn blame_for_file(
        &self,
        tmp_file: &TmpFile,
        line_range: Option<(u32, u32)>,
    ) -> Result<BTreeMap<u32, String>, GitAiError> {
        // Use the filename (relative path) instead of the absolute path
        // Convert the blame result to BTreeMap for deterministic order
        let mut options = blame::GitAiBlameOptions::default();
        if let Some((start, end)) = line_range {
            options.line_ranges.push((start, end));
        }

        // Set pager environment variables to avoid interactive pager in tests
        unsafe {
            std::env::set_var("GIT_PAGER", "cat");
            std::env::set_var("PAGER", "cat");
        }

        let blame_map = blame::run(&self.repo, &tmp_file.filename, &options)?;
        Ok(blame_map.into_iter().collect())
    }
}

const ALPHABET: &str = "A
B
C
D
E
F
G
H
I
J
K
L
M
N
O
P
Q
R
S
T
U
V
W
X
Y
Z";

const LINES: &str = "1
2
3
4
5
6
7
8
9
10
11
12
13
14
15
16
17
18
19
20
21
22
23
24
25
26
27
28
29
30
31
32
33";
