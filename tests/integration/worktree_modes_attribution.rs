use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use std::fs;
use std::path::{Path, PathBuf};

struct LinkedWorktree<'a> {
    repo: &'a TestRepo,
    path: PathBuf,
    registered: bool,
}

impl<'a> LinkedWorktree<'a> {
    fn new(repo: &'a TestRepo, path: PathBuf) -> Self {
        Self {
            repo,
            path,
            registered: true,
        }
    }

    fn move_to(&mut self, new_path: PathBuf) {
        self.repo
            .git(&[
                "worktree",
                "move",
                self.path.to_str().unwrap(),
                new_path.to_str().unwrap(),
            ])
            .unwrap();
        self.path = new_path;
    }

    fn remove(mut self, force: bool) {
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(self.path.to_str().unwrap());
        self.repo.git(&args).unwrap();
        self.registered = false;
    }
}

impl Drop for LinkedWorktree<'_> {
    fn drop(&mut self) {
        if self.registered {
            let _ = self
                .repo
                .git(&["worktree", "remove", "--force", self.path.to_str().unwrap()]);
        }
    }
}

fn seeded_repo() -> TestRepo {
    let repo = TestRepo::new();
    let mut seed = repo.filename("seed.txt");
    seed.set_contents(vec!["seed".human()]);
    repo.stage_all_and_commit("initial").unwrap();
    repo
}

fn linked_path(repo: &TestRepo, suffix: &str) -> PathBuf {
    let name = repo.path().file_name().unwrap().to_string_lossy();
    repo.path()
        .parent()
        .unwrap()
        .join(format!("{name}-worktree-{suffix}"))
}

fn checkpoint_commit_and_assert_ai(repo: &TestRepo, path: &Path, message: &str) {
    fs::write(path.join("generated.txt"), "linked worktree ai\n").unwrap();
    repo.git_ai_from_working_dir(path, &["checkpoint", "mock_ai"])
        .unwrap();
    repo.git_from_working_dir(path, &["add", "-A"]).unwrap();
    repo.git_from_working_dir(path, &["commit", "-m", message])
        .unwrap();
    let blame = repo
        .git_ai_from_working_dir(path, &["blame", "generated.txt"])
        .unwrap();
    assert!(blame.contains("mock_ai"), "expected AI blame:\n{blame}");
    assert!(blame.contains("linked worktree ai"));
}

#[test]
fn worktree_add_inferred_branch_routes_attribution_and_clean_remove() {
    let repo = seeded_repo();
    let path = linked_path(&repo, "inferred");
    repo.git(&["worktree", "add", path.to_str().unwrap()])
        .unwrap();
    let linked = LinkedWorktree::new(&repo, path);
    checkpoint_commit_and_assert_ai(&repo, &linked.path, "inferred branch commit");
    linked.remove(false);
}

#[test]
fn worktree_add_explicit_branch_and_force_reset_route_attribution() {
    let repo = seeded_repo();
    let path = linked_path(&repo, "explicit");
    repo.git(&[
        "worktree",
        "add",
        "-b",
        "linked-explicit",
        path.to_str().unwrap(),
        "HEAD",
    ])
    .unwrap();
    let linked = LinkedWorktree::new(&repo, path);
    checkpoint_commit_and_assert_ai(&repo, &linked.path, "explicit branch commit");
    linked.remove(false);

    let reset_path = linked_path(&repo, "force-reset");
    repo.git(&[
        "worktree",
        "add",
        "-B",
        "linked-explicit",
        reset_path.to_str().unwrap(),
        "HEAD",
    ])
    .unwrap();
    let reset = LinkedWorktree::new(&repo, reset_path);
    checkpoint_commit_and_assert_ai(&repo, &reset.path, "force-reset branch commit");
    reset.remove(false);
}

#[test]
fn worktree_add_detached_routes_attribution() {
    let repo = seeded_repo();
    let path = linked_path(&repo, "detached");
    repo.git(&[
        "worktree",
        "add",
        "--detach",
        path.to_str().unwrap(),
        "HEAD",
    ])
    .unwrap();
    let linked = LinkedWorktree::new(&repo, path);
    checkpoint_commit_and_assert_ai(&repo, &linked.path, "detached worktree commit");
    linked.remove(false);
}

#[test]
fn worktree_add_orphan_routes_attribution_to_root_commit() {
    let repo = seeded_repo();
    let path = linked_path(&repo, "orphan");
    repo.git(&[
        "worktree",
        "add",
        "--orphan",
        "-b",
        "linked-orphan",
        path.to_str().unwrap(),
    ])
    .unwrap();
    let linked = LinkedWorktree::new(&repo, path);
    checkpoint_commit_and_assert_ai(&repo, &linked.path, "orphan worktree root");
    linked.remove(false);
}

#[test]
fn worktree_add_no_checkout_routes_attribution_after_population() {
    let repo = seeded_repo();
    let path = linked_path(&repo, "no-checkout");
    repo.git(&[
        "worktree",
        "add",
        "--no-checkout",
        "-b",
        "linked-no-checkout",
        path.to_str().unwrap(),
        "HEAD",
    ])
    .unwrap();
    let linked = LinkedWorktree::new(&repo, path);
    repo.git_from_working_dir(&linked.path, &["reset", "--hard", "HEAD"])
        .unwrap();
    checkpoint_commit_and_assert_ai(&repo, &linked.path, "no-checkout worktree commit");
    linked.remove(false);
}

#[test]
fn worktree_move_lock_unlock_and_repair_preserve_pending_attribution() {
    let repo = seeded_repo();
    let path = linked_path(&repo, "before-move");
    repo.git(&[
        "worktree",
        "add",
        "--lock",
        "--reason",
        "e2e lock",
        "-b",
        "linked-move",
        path.to_str().unwrap(),
        "HEAD",
    ])
    .unwrap();
    let mut linked = LinkedWorktree::new(&repo, path);
    fs::write(linked.path.join("generated.txt"), "linked worktree ai\n").unwrap();
    repo.git_ai_from_working_dir(&linked.path, &["checkpoint", "mock_ai"])
        .unwrap();
    repo.git(&["worktree", "unlock", linked.path.to_str().unwrap()])
        .unwrap();
    linked.move_to(linked_path(&repo, "after-move"));
    repo.git(&["worktree", "repair", linked.path.to_str().unwrap()])
        .unwrap();
    repo.git(&[
        "worktree",
        "lock",
        "--reason",
        "second lock",
        linked.path.to_str().unwrap(),
    ])
    .unwrap();
    repo.git(&["worktree", "unlock", linked.path.to_str().unwrap()])
        .unwrap();
    repo.git_from_working_dir(&linked.path, &["add", "-A"])
        .unwrap();
    repo.git_from_working_dir(&linked.path, &["commit", "-m", "after move"])
        .unwrap();
    let blame = repo
        .git_ai_from_working_dir(&linked.path, &["blame", "generated.txt"])
        .unwrap();
    assert!(blame.contains("mock_ai"), "expected AI blame:\n{blame}");
    linked.remove(false);
}

#[test]
fn worktree_force_remove_does_not_leak_discarded_attribution_on_recreate() {
    let repo = seeded_repo();
    let path = linked_path(&repo, "recreate");
    repo.git(&[
        "worktree",
        "add",
        "-b",
        "discarded-worktree",
        path.to_str().unwrap(),
        "HEAD",
    ])
    .unwrap();
    let linked = LinkedWorktree::new(&repo, path.clone());
    fs::write(linked.path.join("generated.txt"), "same bytes\n").unwrap();
    repo.git_ai_from_working_dir(&linked.path, &["checkpoint", "mock_ai"])
        .unwrap();
    linked.remove(true);

    repo.git(&[
        "worktree",
        "add",
        "--orphan",
        "-b",
        "recreated-worktree",
        path.to_str().unwrap(),
    ])
    .unwrap();
    let recreated = LinkedWorktree::new(&repo, path);
    fs::write(recreated.path.join("generated.txt"), "same bytes\n").unwrap();
    repo.git_from_working_dir(&recreated.path, &["add", "-A"])
        .unwrap();
    repo.git_from_working_dir(&recreated.path, &["commit", "-m", "human recreate"])
        .unwrap();
    let blame = repo
        .git_ai_from_working_dir(&recreated.path, &["blame", "generated.txt"])
        .unwrap();
    assert!(
        !blame.contains("mock_ai"),
        "stale AI blame leaked:\n{blame}"
    );
    assert!(blame.contains("Test User"));
    recreated.remove(false);
}

#[test]
fn worktree_prune_of_missing_sibling_does_not_corrupt_main_pending_ai() {
    let repo = seeded_repo();
    let path = linked_path(&repo, "pruned");
    repo.git(&[
        "worktree",
        "add",
        "--detach",
        path.to_str().unwrap(),
        "HEAD",
    ])
    .unwrap();
    let mut linked = LinkedWorktree::new(&repo, path);

    let mut pending = repo.filename("main-pending.txt");
    pending.set_contents(vec!["main ai survives prune".ai()]);
    repo.git_ai(&["checkpoint", "mock_ai"]).unwrap();

    fs::remove_dir_all(&linked.path).unwrap();
    repo.git(&["worktree", "prune", "--expire", "now"]).unwrap();
    linked.registered = false;
    repo.stage_all_and_commit("after prune").unwrap();
    pending.assert_lines_and_blame(vec!["main ai survives prune".ai()]);
}
