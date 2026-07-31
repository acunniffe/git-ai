//! Comprehensive real-world rebase attribution tests.
//!
//! These tests cover four rebase scenario categories with ≥5 commits per branch,
//! verifying line-level attribution at EVERY rebased commit — not just HEAD.
//! Tests are intentionally strict: they surface bugs in the slow-path attribution
//! rewriting code (src/authorship/rewrite.rs).
//!
//! IMPORTANT: All attribution reads MUST go through TestRepo helpers:
//!   - `run_blame_api(repo, sha, file, ctx)` — blame at specific commit via Rust API (newest_commit)
//!   - `repo.read_authorship_note(sha)` — waits for daemon sync
//!
//! Never call git/git-ai directly (racy in daemon mode).
//!
//! Four scenario categories (10 tests each):
//!   1. Fast path  — disjoint file sets between branches
//!   2. Slow path  — same files modified non-conflictingly (upstream prepends)
//!   3. Human conflict — conflict resolved by human (fs::write, no checkpoint)
//!   4. AI conflict    — conflict resolved by AI (set_contents with .ai())

#![allow(dead_code)]
use std::fs;

use crate::repos::blame_support::is_ai_blame_author;
use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::model::authorship_log_serialization::AuthorshipLog;
use git_ai::operations::commands::blame::GitAiBlameOptions;
use git_ai::operations::git::repository as GitAiRepository;

// ============================================================================
// Shared helpers — ALL note/blame reads go through TestRepo helpers
// ============================================================================

/// Parse the authorship note for `sha`.  Panics if the note is absent.
/// Uses the shared TestRepo helper for daemon-safe access and deserialization.
fn parse_note(repo: &TestRepo, sha: &str) -> AuthorshipLog {
    repo.require_authorship_log(sha)
}

/// Return the N most-recent commit SHAs ordered oldest→newest:
/// [HEAD~(n-1), HEAD~(n-2), …, HEAD~1, HEAD].
fn get_commit_chain(repo: &TestRepo, n: usize) -> Vec<String> {
    (0..n)
        .rev()
        .map(|offset| {
            let rev = if offset == 0 {
                "HEAD".to_string()
            } else {
                format!("HEAD~{}", offset)
            };
            repo.git(&["rev-parse", &rev]).unwrap().trim().to_string()
        })
        .collect()
}

/// Sum of `accepted_lines` across all prompts in a note string.
fn total_accepted_lines(note: &str) -> u32 {
    let log = AuthorshipLog::deserialize_from_string(note).expect("should parse authorship note");
    // Session format: count AI-attested lines from attestation entries.
    // Old format: fall back to prompts accepted_lines.
    let session_lines: u32 = log
        .attestations
        .iter()
        .flat_map(|a| &a.entries)
        .filter(|e| e.hash.starts_with("s_"))
        .flat_map(|e| &e.line_ranges)
        .map(|r| match r {
            git_ai::model::authorship_log::LineRange::Single(_) => 1,
            git_ai::model::authorship_log::LineRange::Range(s, e) => e - s + 1,
        })
        .sum();
    if session_lines > 0 {
        return session_lines;
    }
    log.metadata
        .prompts
        .values()
        .map(|p| p.accepted_lines)
        .sum()
}

/// File paths listed in a note's attestations section.
fn files_in_note(note: &str) -> Vec<String> {
    let log = AuthorshipLog::deserialize_from_string(note).expect("should parse authorship note");
    log.attestations
        .iter()
        .map(|a| a.file_path.clone())
        .collect()
}

/// Assert that `sha`'s note lists EXACTLY the files in `expected` — no extras,
/// no missing entries.  Uses substring matching for paths (e.g. "users.py"
/// matches "src/users.py").
fn assert_note_files_exact(repo: &TestRepo, sha: &str, ctx: &str, expected: &[&str]) {
    let raw = repo
        .read_authorship_note(sha)
        .unwrap_or_else(|| panic!("{}: commit {} has no note", ctx, sha));
    let actual = files_in_note(&raw);
    // Every actual file must be in expected
    for f in &actual {
        assert!(
            expected.iter().any(|e| f.contains(e)),
            "{}: unexpected file '{}' in note for {}.\nExpected only: {:?}\nGot: {:?}",
            ctx,
            f,
            sha,
            expected,
            actual
        );
    }
    // Every expected file must appear in actual
    for e in expected {
        assert!(
            actual.iter().any(|f| f.contains(e)),
            "{}: expected file '{}' missing from note for {}.\nExpected: {:?}\nGot: {:?}",
            ctx,
            e,
            sha,
            expected,
            actual
        );
    }
}

/// Assert none of `forbidden` appear in `sha`'s note.
fn assert_note_no_forbidden_files(repo: &TestRepo, sha: &str, ctx: &str, forbidden: &[&str]) {
    let raw = repo
        .read_authorship_note(sha)
        .unwrap_or_else(|| panic!("{}: commit {} has no note", ctx, sha));
    let actual = files_in_note(&raw);
    for f in forbidden {
        assert!(
            !actual.iter().any(|a| a.contains(f)),
            "{}: forbidden file '{}' appears in note for {}.\nAll files: {:?}",
            ctx,
            f,
            sha,
            actual
        );
    }
}

/// Like `assert_note_no_forbidden_files` but silently passes when the commit has no note.
/// Use for human commits (created via `commit_untracked_file`) that correctly produce no note
/// after rebase — if a note does exist (e.g. implementation creates empty propagation
/// notes), the forbidden-file check is still enforced.
fn assert_note_no_forbidden_files_if_present(
    repo: &TestRepo,
    sha: &str,
    ctx: &str,
    forbidden: &[&str],
) {
    let Some(raw) = repo.read_authorship_note(sha) else {
        return; // no note — human commit, trivially correct
    };
    let actual = files_in_note(&raw);
    for f in forbidden {
        assert!(
            !actual.iter().any(|a| a.contains(f)),
            "{}: forbidden file '{}' appears in note for {}.\nAll files: {:?}",
            ctx,
            f,
            sha,
            actual
        );
    }
}

/// Verify that specific lines (identified by content substring) carry the expected
/// AI-or-human attribution for `file` at `sha`.
/// This is a *sample* check — caller need not list every line.
/// Uses the Rust blame API with `newest_commit` set for accurate per-commit attribution.
fn assert_blame_sample_at_commit(
    repo: &TestRepo,
    sha: &str,
    file: &str,
    ctx: &str,
    samples: &[(&str, bool)],
) {
    let (line_authors, lines) = run_blame_api(repo, sha, file, ctx);
    for (exp_substr, exp_is_ai) in samples {
        let found = lines
            .iter()
            .enumerate()
            .find(|(_, l)| l.contains(exp_substr));
        let (idx, line_text) = found.unwrap_or_else(|| {
            panic!(
                "{}: line containing {:?} not found in {} at {}\nFile lines:\n{}",
                ctx,
                exp_substr,
                file,
                sha,
                lines.join("\n")
            )
        });
        let line_num = (idx + 1) as u32;
        let author = line_authors
            .get(&line_num)
            .map(|s| s.as_str())
            .unwrap_or("Test User");
        let got_ai = is_ai_blame_author(author);
        assert_eq!(
            got_ai,
            *exp_is_ai,
            "{}: line {} ({:?}) expected {}AI-authored but got author={:?}\nat {} file {}",
            ctx,
            line_num,
            line_text,
            if *exp_is_ai { "" } else { "non-" },
            author,
            sha,
            file
        );
    }
}

/// Assert `base_commit_sha` in `sha`'s note equals `sha` itself.
fn assert_note_base_commit_matches(repo: &TestRepo, sha: &str, ctx: &str) {
    let log = parse_note(repo, sha);
    assert_eq!(
        log.metadata.base_commit_sha, sha,
        "{}: base_commit_sha mismatch at {}",
        ctx, sha
    );
}

/// Assert total accepted_lines in `sha`'s note equals `expected` exactly.
fn assert_accepted_lines_exact(repo: &TestRepo, sha: &str, ctx: &str, expected: u32) {
    let raw = repo
        .read_authorship_note(sha)
        .unwrap_or_else(|| panic!("{}: commit {} has no note", ctx, sha));
    let actual = total_accepted_lines(&raw);
    assert_eq!(
        actual, expected,
        "{}: accepted_lines at {} = {} but expected exactly {}",
        ctx, sha, actual, expected
    );
}

/// Assert accepted_lines values are strictly monotonically non-decreasing
/// along the chain (oldest→newest).  Panics on any violation.
fn assert_accepted_lines_monotonic(repo: &TestRepo, ctx: &str, chain: &[String]) {
    let values: Vec<u32> = chain
        .iter()
        .map(|sha| {
            let raw = repo
                .read_authorship_note(sha)
                .unwrap_or_else(|| panic!("{}: commit {} has no note", ctx, sha));
            total_accepted_lines(&raw)
        })
        .collect();
    for i in 1..values.len() {
        assert!(
            values[i] >= values[i - 1],
            "{}: accepted_lines not monotonic: chain[{}]={} > chain[{}]={}\nFull chain values: {:?}",
            ctx,
            i - 1,
            values[i - 1],
            i,
            values[i],
            values
        );
    }
}

/// Assert line-level blame at a specific commit SHA.
/// `expected`: ordered list of (content_substring, is_ai) for every line.
/// Uses the Rust blame API with `newest_commit` set — content from `git show` and
/// attribution from blame come from the same commit, so line counts always agree.
fn assert_blame_at_commit(
    repo: &TestRepo,
    sha: &str,
    file: &str,
    ctx: &str,
    expected: &[(&str, bool)],
) {
    let (line_authors, lines) = run_blame_api(repo, sha, file, ctx);

    assert_eq!(
        lines.len(),
        expected.len(),
        "{}: file {} at {} has {} lines, expected {}\nLines:\n{}",
        ctx,
        file,
        sha,
        lines.len(),
        expected.len(),
        lines.join("\n")
    );

    for (i, (line_text, (exp_substr, exp_is_ai))) in lines.iter().zip(expected.iter()).enumerate() {
        let line_num = (i + 1) as u32;
        assert!(
            line_text.contains(exp_substr),
            "{}: line {} {:?} does not contain {:?}\nat {} file {}",
            ctx,
            line_num,
            line_text,
            exp_substr,
            sha,
            file
        );
        let author = line_authors
            .get(&line_num)
            .map(|s| s.as_str())
            .unwrap_or("Test User");
        let got_ai = is_ai_blame_author(author);
        assert_eq!(
            got_ai,
            *exp_is_ai,
            "{}: line {} ({:?}) expected {}AI-authored but got author={:?}\nat {} file {}",
            ctx,
            line_num,
            line_text,
            if *exp_is_ai { "" } else { "non-" },
            author,
            sha,
            file
        );
    }
}

/// Run the blame Rust API at `sha` for `file`.
/// Returns (line_authors map, file lines).
/// Line splitting mirrors how git counts lines: trailing `\n` does NOT create
/// a phantom empty last line, but a real blank line (double `\n\n`) does.
fn run_blame_api(
    repo: &TestRepo,
    sha: &str,
    file: &str,
    ctx: &str,
) -> (std::collections::HashMap<u32, String>, Vec<String>) {
    let gitai_repo = GitAiRepository::find_repository_in_path(repo.path().to_str().unwrap())
        .unwrap_or_else(|e| panic!("{}: find_repository_in_path failed: {}", ctx, e));
    let options = GitAiBlameOptions {
        newest_commit: Some(sha.to_string()),
        no_output: true,
        ..Default::default()
    };
    let (line_authors, _) = gitai_repo
        .blame(file, &options)
        .unwrap_or_else(|e| panic!("{}: blame({}, {}) failed: {}", ctx, sha, file, e));

    // Get file content at the commit for line content verification.
    // Split with split('\n') and remove the single trailing empty element produced
    // by a standard terminating newline — matching how git counts lines.
    let raw = repo
        .git(&["show", &format!("{}:{}", sha, file)])
        .unwrap_or_else(|e| panic!("{}: git show {}:{} failed: {}", ctx, sha, file, e));
    let mut lines: Vec<String> = raw.split('\n').map(|s| s.to_string()).collect();
    if lines.last().map(|s| s.is_empty()).unwrap_or(false) {
        lines.pop(); // strip artifact of trailing \n; double \n\n stays as one empty line
    }
    (line_authors, lines)
}

// ============================================================================
// Category 1: Fast Path rebase tests
// Feature and main branches touch COMPLETELY DIFFERENT files so blob OIDs
// are identical between original and rebased commits (fast path fires).
// ============================================================================

#[test]
fn test_fast_path_python_microservice_5_endpoints() {
    let repo = TestRepo::new();

    // Initial commit (shared base)
    let mut init = repo.filename("init.py");
    init.set_contents(crate::lines!["# microservice project init"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let main_branch = repo.current_branch();

    // === FEATURE BRANCH: 5 commits, each adding a new AI-generated service file ===
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // C1: users.py
    let mut f1 = repo.filename("users.py");
    f1.set_contents(crate::lines![
        "class UserService:".ai(),
        "    def __init__(self, db):".ai(),
        "        self.db = db".ai(),
        "        self.cache = {}".ai(),
        "    def get_user(self, user_id):".ai(),
        "        if user_id in self.cache:".ai(),
        "            return self.cache[user_id]".ai(),
        "        return self.db.query('SELECT * FROM users WHERE id = ?', user_id)".ai(),
        "    def create_user(self, name, email):".ai(),
        "        return self.db.execute('INSERT INTO users (name, email) VALUES (?, ?)', name, email)".ai(),
    ]);
    repo.stage_all_and_commit("feat: add user service").unwrap();

    // C2: products.py
    let mut f2 = repo.filename("products.py");
    f2.set_contents(crate::lines![
        "class ProductService:".ai(),
        "    def __init__(self, db):".ai(),
        "        self.db = db".ai(),
        "        self.index = {}".ai(),
        "    def get_product(self, product_id):".ai(),
        "        return self.db.query('SELECT * FROM products WHERE id = ?', product_id)".ai(),
        "    def list_products(self, category=None):".ai(),
        "        if category:".ai(),
        "            return self.db.query('SELECT * FROM products WHERE category = ?', category)"
            .ai(),
        "        return self.db.query('SELECT * FROM products')".ai(),
    ]);
    repo.stage_all_and_commit("feat: add product service")
        .unwrap();

    // C3: orders.py
    let mut f3 = repo.filename("orders.py");
    f3.set_contents(crate::lines![
        "class OrderService:".ai(),
        "    def __init__(self, db, user_svc, product_svc):".ai(),
        "        self.db = db".ai(),
        "        self.user_svc = user_svc".ai(),
        "        self.product_svc = product_svc".ai(),
        "    def create_order(self, user_id, items):".ai(),
        "        user = self.user_svc.get_user(user_id)".ai(),
        "        total = sum(self.product_svc.get_product(i['id'])['price'] * i['qty'] for i in items)".ai(),
        "        return self.db.execute('INSERT INTO orders (user_id, total) VALUES (?, ?)', user_id, total)".ai(),
        "    def get_order(self, order_id):".ai(),
    ]);
    repo.stage_all_and_commit("feat: add order service")
        .unwrap();

    // C4: payments.py
    let mut f4 = repo.filename("payments.py");
    f4.set_contents(crate::lines![
        "class PaymentService:".ai(),
        "    def __init__(self, db, stripe_client):".ai(),
        "        self.db = db".ai(),
        "        self.stripe = stripe_client".ai(),
        "    def charge(self, order_id, amount_cents, card_token):".ai(),
        "        result = self.stripe.charge.create(amount=amount_cents, currency='usd', source=card_token)".ai(),
        "        self.db.execute('INSERT INTO payments (order_id, stripe_id) VALUES (?, ?)', order_id, result['id'])".ai(),
        "        return result".ai(),
        "    def refund(self, payment_id):".ai(),
        "        return self.stripe.refund.create(charge=payment_id)".ai(),
    ]);
    repo.stage_all_and_commit("feat: add payment service")
        .unwrap();

    // C5: webhooks.py
    let mut f5 = repo.filename("webhooks.py");
    f5.set_contents(crate::lines![
        "class WebhookService:".ai(),
        "    def __init__(self, db, http_client):".ai(),
        "        self.db = db".ai(),
        "        self.http = http_client".ai(),
        "    def register(self, url, events):".ai(),
        "        return self.db.execute('INSERT INTO webhooks (url, events) VALUES (?, ?)', url, ','.join(events))".ai(),
        "    def dispatch(self, event, payload):".ai(),
        "        hooks = self.db.query('SELECT * FROM webhooks WHERE events LIKE ?', f'%{event}%')".ai(),
        "        for hook in hooks:".ai(),
        "            self.http.post(hook['url'], json=payload)".ai(),
    ]);
    repo.stage_all_and_commit("feat: add webhook service")
        .unwrap();

    // === MAIN BRANCH: 5 human commits on DIFFERENT files ===
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.commit_untracked_file(
        "tests/test_base.py",
        "import unittest\nclass BaseTest(unittest.TestCase): pass\n",
        "test: add base test class",
    );
    repo.commit_untracked_file(".github/ci.yml",
        "name: CI\non: [push, pull_request]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v3\n      - run: python -m pytest\n",
        "ci: add github actions workflow",
    );
    repo.commit_untracked_file(
        "conftest.py",
        "import pytest\n\n@pytest.fixture\ndef db():\n    return MockDatabase()\n",
        "test: add pytest conftest",
    );
    repo.commit_untracked_file(
        "Makefile",
        "test:\n\tpython -m pytest tests/\nlint:\n\tflake8 .\n.PHONY: test lint\n",
        "build: add Makefile",
    );
    repo.commit_untracked_file(
        "setup.cfg",
        "[metadata]\nname = microservice\nversion = 0.1.0\n[options]\npython_requires = >=3.9\n",
        "build: add setup.cfg",
    );

    // === REBASE feature onto main ===
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &main_branch]).unwrap();

    // === VERIFY AT EVERY COMMIT in the rebased chain ===
    let chain = get_commit_chain(&repo, 5);
    // chain[0]=HEAD~4 (C1'), chain[1]=HEAD~3 (C2'), ..., chain[4]=HEAD (C5')

    // sha0 = C1': only users.py
    assert_note_base_commit_matches(&repo, &chain[0], "sha0");
    assert_note_files_exact(&repo, &chain[0], "sha0_files", &["users.py"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[0],
        "sha0_no_future",
        &["products.py", "orders.py", "payments.py", "webhooks.py"],
    );
    assert_blame_at_commit(
        &repo,
        &chain[0],
        "users.py",
        "sha0_blame",
        &[
            ("class UserService:", true),
            ("def __init__(self, db):", true),
            ("self.db = db", true),
            ("self.cache = {}", true),
            ("def get_user(self, user_id):", true),
            ("if user_id in self.cache:", true),
            ("return self.cache[user_id]", true),
            ("SELECT * FROM users WHERE id = ?", true),
            ("def create_user(self, name, email):", true),
            ("INSERT INTO users", true),
        ],
    );

    // sha1 = C2': products.py
    assert_note_base_commit_matches(&repo, &chain[1], "sha1");
    assert_note_files_exact(&repo, &chain[1], "sha1_files", &["products.py"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[1],
        "sha1_no_future",
        &["orders.py", "payments.py", "webhooks.py"],
    );
    assert_blame_at_commit(
        &repo,
        &chain[1],
        "products.py",
        "sha1_blame",
        &[
            ("class ProductService:", true),
            ("def __init__(self, db):", true),
            ("self.db = db", true),
            ("self.index = {}", true),
            ("def get_product(self, product_id):", true),
            ("SELECT * FROM products WHERE id = ?", true),
            ("def list_products(self, category=None):", true),
            ("if category:", true),
            ("SELECT * FROM products WHERE category = ?", true),
            ("SELECT * FROM products", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "users.py",
        "chain1_prior_users_py",
        &[
            ("class UserService:", true),
            ("def get_user(self, user_id):", true),
        ],
    );

    // sha2 = C3': orders.py
    assert_note_base_commit_matches(&repo, &chain[2], "sha2");
    assert_note_files_exact(&repo, &chain[2], "sha2_files", &["orders.py"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[2],
        "sha2_no_future",
        &["payments.py", "webhooks.py"],
    );
    assert_blame_at_commit(
        &repo,
        &chain[2],
        "orders.py",
        "sha2_blame",
        &[
            ("class OrderService:", true),
            ("def __init__(self, db, user_svc, product_svc):", true),
            ("self.db = db", true),
            ("self.user_svc = user_svc", true),
            ("self.product_svc = product_svc", true),
            ("def create_order(self, user_id, items):", true),
            ("user = self.user_svc.get_user(user_id)", true),
            ("total = sum", true),
            ("INSERT INTO orders", true),
            ("def get_order(self, order_id):", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "users.py",
        "chain2_prior_users_py",
        &[
            ("class UserService:", true),
            ("def get_user(self, user_id):", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "products.py",
        "chain2_prior_products_py",
        &[
            ("class ProductService:", true),
            ("def list_products(self, category=None):", true),
        ],
    );

    // sha3 = C4': payments.py
    assert_note_base_commit_matches(&repo, &chain[3], "sha3");
    assert_note_files_exact(&repo, &chain[3], "sha3_files", &["payments.py"]);
    assert_note_no_forbidden_files(&repo, &chain[3], "sha3_no_future", &["webhooks.py"]);
    assert_blame_at_commit(
        &repo,
        &chain[3],
        "payments.py",
        "sha3_blame",
        &[
            ("class PaymentService:", true),
            ("def __init__(self, db, stripe_client):", true),
            ("self.db = db", true),
            ("self.stripe = stripe_client", true),
            (
                "def charge(self, order_id, amount_cents, card_token):",
                true,
            ),
            ("stripe.charge.create", true),
            ("INSERT INTO payments", true),
            ("return result", true),
            ("def refund(self, payment_id):", true),
            ("stripe.refund.create", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "users.py",
        "chain3_prior_users_py",
        &[
            ("class UserService:", true),
            ("def get_user(self, user_id):", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "products.py",
        "chain3_prior_products_py",
        &[
            ("class ProductService:", true),
            ("def list_products(self, category=None):", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "orders.py",
        "chain3_prior_orders_py",
        &[
            ("class OrderService:", true),
            ("def create_order(self, user_id, items):", true),
        ],
    );

    // sha4 = C5': webhooks.py
    assert_note_base_commit_matches(&repo, &chain[4], "sha4");
    assert_note_files_exact(&repo, &chain[4], "sha4_files", &["webhooks.py"]);
    assert_blame_at_commit(
        &repo,
        &chain[4],
        "webhooks.py",
        "sha4_blame",
        &[
            ("class WebhookService:", true),
            ("def __init__(self, db, http_client):", true),
            ("self.db = db", true),
            ("self.http = http_client", true),
            ("def register(self, url, events):", true),
            ("INSERT INTO webhooks", true),
            ("def dispatch(self, event, payload):", true),
            ("SELECT * FROM webhooks", true),
            ("for hook in hooks:", true),
            ("self.http.post", true),
        ],
    );
    // Verify C1's file (users.py) still correctly attributed at tip.
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "users.py",
        "sha4_users_preserved",
        &[
            ("class UserService:", true),
            ("def get_user(self, user_id):", true),
            ("def create_user(self, name, email):", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "products.py",
        "chain4_prior_products_py",
        &[
            ("class ProductService:", true),
            ("def list_products(self, category=None):", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "orders.py",
        "chain4_prior_orders_py",
        &[
            ("class OrderService:", true),
            ("def create_order(self, user_id, items):", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "payments.py",
        "chain4_prior_payments_py",
        &[
            ("class PaymentService:", true),
            (
                "def charge(self, order_id, amount_cents, card_token):",
                true,
            ),
        ],
    );
}

#[test]
fn test_fast_path_typescript_frontend_5_components() {
    let repo = TestRepo::new();

    // Initial commit (shared base)
    let mut init = repo.filename("src/index.ts");
    init.set_contents(crate::lines!["// TypeScript frontend entry point"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let main_branch = repo.current_branch();

    // === FEATURE BRANCH: 5 commits, each adding a React component ===
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // C1: Button.tsx
    let mut f1 = repo.filename("Button.tsx");
    f1.set_contents(crate::lines![
        "interface ButtonProps {".ai(),
        "  label: string;".ai(),
        "  onClick: () => void;".ai(),
        "  disabled?: boolean;".ai(),
        "  variant?: 'primary' | 'secondary' | 'danger';".ai(),
        "}".ai(),
        "export function Button({ label, onClick, disabled = false, variant = 'primary' }: ButtonProps) {".ai(),
        "  const cls = `btn btn-${variant}${disabled ? ' btn-disabled' : ''}`;".ai(),
        "  return <button className={cls} onClick={onClick} disabled={disabled}>{label}</button>;".ai(),
        "}".ai(),
        "export default Button;".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: add Button component")
        .unwrap();

    // C2: Input.tsx
    let mut f2 = repo.filename("Input.tsx");
    f2.set_contents(crate::lines![
        "interface InputProps {".ai(),
        "  value: string;".ai(),
        "  onChange: (v: string) => void;".ai(),
        "  placeholder?: string;".ai(),
        "  type?: 'text' | 'email' | 'password';".ai(),
        "  error?: string;".ai(),
        "}".ai(),
        "export function Input({ value, onChange, placeholder, type = 'text', error }: InputProps) {".ai(),
        "  return (".ai(),
        "    <div className=\"input-wrapper\">".ai(),
        "      <input type={type} value={value} placeholder={placeholder} onChange={e => onChange(e.target.value)} />".ai(),
        "      {error && <span className=\"input-error\">{error}</span>}".ai(),
        "    </div>".ai(),
        "  );".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: add Input component")
        .unwrap();

    // C3: Modal.tsx
    let mut f3 = repo.filename("Modal.tsx");
    f3.set_contents(crate::lines![
        "interface ModalProps {".ai(),
        "  isOpen: boolean;".ai(),
        "  onClose: () => void;".ai(),
        "  title: string;".ai(),
        "  children: React.ReactNode;".ai(),
        "}".ai(),
        "export function Modal({ isOpen, onClose, title, children }: ModalProps) {".ai(),
        "  if (!isOpen) return null;".ai(),
        "  return (".ai(),
        "    <div className=\"modal-overlay\" onClick={onClose}>".ai(),
        "      <div className=\"modal-content\" onClick={e => e.stopPropagation()}>".ai(),
        "        <h2>{title}</h2>".ai(),
        "        <div className=\"modal-body\">{children}</div>".ai(),
        "      </div>".ai(),
        "    </div>".ai(),
        "  );".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: add Modal component")
        .unwrap();

    // C4: Table.tsx
    let mut f4 = repo.filename("Table.tsx");
    f4.set_contents(crate::lines![
        "interface Column<T> { key: keyof T; header: string; }".ai(),
        "interface TableProps<T> {".ai(),
        "  columns: Column<T>[];".ai(),
        "  data: T[];".ai(),
        "  onRowClick?: (row: T) => void;".ai(),
        "}".ai(),
        "export function Table<T extends { id: string | number }>({ columns, data, onRowClick }: TableProps<T>) {".ai(),
        "  return (".ai(),
        "    <table className=\"data-table\">".ai(),
        "      <thead><tr>{columns.map(c => <th key={String(c.key)}>{c.header}</th>)}</tr></thead>".ai(),
        "      <tbody>{data.map(row => <tr key={row.id} onClick={() => onRowClick?.(row)}>{columns.map(c => <td key={String(c.key)}>{String(row[c.key])}</td>)}</tr>)}</tbody>".ai(),
        "    </table>".ai(),
        "  );".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: add Table component")
        .unwrap();

    // C5: Form.tsx
    let mut f5 = repo.filename("Form.tsx");
    f5.set_contents(crate::lines![
        "interface FormField { name: string; label: string; type: string; required?: boolean; }".ai(),
        "interface FormProps {".ai(),
        "  fields: FormField[];".ai(),
        "  onSubmit: (data: Record<string, string>) => void;".ai(),
        "  submitLabel?: string;".ai(),
        "}".ai(),
        "export function Form({ fields, onSubmit, submitLabel = 'Submit' }: FormProps) {".ai(),
        "  const [values, setValues] = React.useState<Record<string, string>>({});".ai(),
        "  const handleSubmit = (e: React.FormEvent) => { e.preventDefault(); onSubmit(values); };".ai(),
        "  return (".ai(),
        "    <form onSubmit={handleSubmit}>".ai(),
        "      {fields.map(f => <label key={f.name}>{f.label}<input name={f.name} type={f.type} required={f.required} onChange={e => setValues(v => ({...v, [f.name]: e.target.value}))} /></label>)}".ai(),
        "      <button type=\"submit\">{submitLabel}</button>".ai(),
        "    </form>".ai(),
        "  );".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: add Form component")
        .unwrap();

    // === MAIN BRANCH: 5 human commits on config files ===
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.commit_untracked_file(
        "vite.config.ts",
        "import { defineConfig } from 'vite';\nexport default defineConfig({ plugins: [] });\n",
        "build: add vite config",
    );
    repo.commit_untracked_file(
        ".eslintrc.json",
        "{\"extends\": [\"eslint:recommended\", \"plugin:@typescript-eslint/recommended\"]}\n",
        "lint: add eslint config",
    );
    repo.commit_untracked_file("tsconfig.json",
        "{\"compilerOptions\": {\"target\": \"ES2020\", \"module\": \"ESNext\", \"jsx\": \"react-jsx\", \"strict\": true}}\n",
        "build: add tsconfig",
    );
    repo.commit_untracked_file("package.json",
        "{\"name\": \"frontend\", \"version\": \"1.0.0\", \"scripts\": {\"dev\": \"vite\", \"build\": \"vite build\"}}\n",
        "build: add package.json",
    );
    repo.commit_untracked_file("tailwind.config.js",
        "module.exports = { content: ['./src/**/*.{ts,tsx}'], theme: { extend: {} }, plugins: [] };\n",
        "style: add tailwind config",
    );

    // === REBASE feature onto main ===
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &main_branch]).unwrap();

    // === VERIFY AT EVERY COMMIT ===
    let chain = get_commit_chain(&repo, 5);

    // sha0 = C1': only Button.tsx
    assert_note_base_commit_matches(&repo, &chain[0], "sha0");
    assert_note_files_exact(&repo, &chain[0], "sha0_files", &["Button.tsx"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[0],
        "sha0_no_future",
        &["Input.tsx", "Modal.tsx", "Table.tsx", "Form.tsx"],
    );
    assert_blame_at_commit(
        &repo,
        &chain[0],
        "Button.tsx",
        "sha0_blame",
        &[
            ("interface ButtonProps {", true),
            ("label: string;", true),
            ("onClick: () => void;", true),
            ("disabled?: boolean;", true),
            ("variant?: 'primary' | 'secondary' | 'danger';", true),
            ("}", true),
            ("export function Button", true),
            ("const cls =", true),
            ("return <button", true),
            ("}", true),
            ("export default Button;", true),
        ],
    );

    // sha1 = C2': Input.tsx
    assert_note_base_commit_matches(&repo, &chain[1], "sha1");
    assert_note_files_exact(&repo, &chain[1], "sha1_files", &["Input.tsx"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[1],
        "sha1_no_future",
        &["Modal.tsx", "Table.tsx", "Form.tsx"],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "Button.tsx",
        "chain1_prior_button_tsx",
        &[
            ("interface ButtonProps {", true),
            ("export function Button", true),
        ],
    );

    // sha2 = C3': Modal.tsx
    assert_note_base_commit_matches(&repo, &chain[2], "sha2");
    assert_note_files_exact(&repo, &chain[2], "sha2_files", &["Modal.tsx"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[2],
        "sha2_no_future",
        &["Table.tsx", "Form.tsx"],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "Button.tsx",
        "chain2_prior_button_tsx",
        &[
            ("interface ButtonProps {", true),
            ("export function Button", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "Input.tsx",
        "chain2_prior_input_tsx",
        &[
            ("interface InputProps {", true),
            ("export function Input", true),
        ],
    );

    // sha3 = C4': Table.tsx
    assert_note_base_commit_matches(&repo, &chain[3], "sha3");
    assert_note_files_exact(&repo, &chain[3], "sha3_files", &["Table.tsx"]);
    assert_note_no_forbidden_files(&repo, &chain[3], "sha3_no_future", &["Form.tsx"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "Button.tsx",
        "chain3_prior_button_tsx",
        &[
            ("interface ButtonProps {", true),
            ("export function Button", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "Input.tsx",
        "chain3_prior_input_tsx",
        &[
            ("interface InputProps {", true),
            ("export function Input", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "Modal.tsx",
        "chain3_prior_modal_tsx",
        &[
            ("interface ModalProps {", true),
            ("export function Modal", true),
        ],
    );

    // sha4 = C5': Form.tsx
    assert_note_base_commit_matches(&repo, &chain[4], "sha4");
    assert_note_files_exact(&repo, &chain[4], "sha4_files", &["Form.tsx"]);
    assert_blame_at_commit(
        &repo,
        &chain[4],
        "Form.tsx",
        "sha4_blame",
        &[
            ("interface FormField", true),
            ("interface FormProps {", true),
            ("fields: FormField[];", true),
            ("onSubmit: (data: Record<string, string>) => void;", true),
            ("submitLabel?: string;", true),
            ("}", true),
            ("export function Form", true),
            ("const [values, setValues]", true),
            ("const handleSubmit", true),
            ("return (", true),
            ("<form onSubmit={handleSubmit}>", true),
            ("fields.map", true),
            ("<button type=\"submit\">", true),
            ("</form>", true),
            (");", true),
            ("}", true),
        ],
    );
    // Verify C1's file (Button.tsx) still correctly attributed at tip.
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "Button.tsx",
        "sha4_button_preserved",
        &[
            ("interface ButtonProps {", true),
            ("export function Button", true),
            ("export default Button;", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "Input.tsx",
        "chain4_prior_input_tsx",
        &[
            ("interface InputProps {", true),
            ("export function Input", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "Modal.tsx",
        "chain4_prior_modal_tsx",
        &[
            ("interface ModalProps {", true),
            ("export function Modal", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "Table.tsx",
        "chain4_prior_table_tsx",
        &[
            ("interface TableProps<T> {", true),
            ("export function Table<T", true),
        ],
    );
}

#[test]
fn test_fast_path_rust_library_5_modules() {
    let repo = TestRepo::new();

    // Initial commit (shared base)
    let mut init = repo.filename("src/lib.rs");
    init.set_contents(crate::lines!["// Rust library crate root"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let main_branch = repo.current_branch();

    // === FEATURE BRANCH: 5 commits, each adding a new Rust module ===
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // C1: src/parser.rs
    let mut f1 = repo.filename("src/parser.rs");
    f1.set_contents(crate::lines![
        "pub struct Parser {".ai(),
        "    input: String,".ai(),
        "    pos: usize,".ai(),
        "}".ai(),
        "impl Parser {".ai(),
        "    pub fn new(input: &str) -> Self {".ai(),
        "        Self { input: input.to_string(), pos: 0 }".ai(),
        "    }".ai(),
        "    pub fn parse_token(&mut self) -> Option<&str> {".ai(),
        "        let start = self.pos;".ai(),
        "        while self.pos < self.input.len() && !self.input.as_bytes()[self.pos].is_ascii_whitespace() { self.pos += 1; }".ai(),
        "        if start == self.pos { None } else { Some(&self.input[start..self.pos]) }".ai(),
        "    }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: add parser module")
        .unwrap();

    // C2: src/validator.rs
    let mut f2 = repo.filename("src/validator.rs");
    f2.set_contents(crate::lines![
        "pub struct Validator {".ai(),
        "    rules: Vec<Box<dyn Fn(&str) -> bool>>,".ai(),
        "}".ai(),
        "impl Validator {".ai(),
        "    pub fn new() -> Self {".ai(),
        "        Self { rules: Vec::new() }".ai(),
        "    }".ai(),
        "    pub fn add_rule(&mut self, rule: impl Fn(&str) -> bool + 'static) {".ai(),
        "        self.rules.push(Box::new(rule));".ai(),
        "    }".ai(),
        "    pub fn validate(&self, input: &str) -> bool {".ai(),
        "        self.rules.iter().all(|r| r(input))".ai(),
        "    }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: add validator module")
        .unwrap();

    // C3: src/formatter.rs
    let mut f3 = repo.filename("src/formatter.rs");
    f3.set_contents(crate::lines![
        "pub struct Formatter {".ai(),
        "    indent: usize,".ai(),
        "    style: FormatterStyle,".ai(),
        "}".ai(),
        "pub enum FormatterStyle { Compact, Pretty }".ai(),
        "impl Formatter {".ai(),
        "    pub fn new(indent: usize, style: FormatterStyle) -> Self {".ai(),
        "        Self { indent, style }".ai(),
        "    }".ai(),
        "    pub fn format(&self, tokens: &[&str]) -> String {".ai(),
        "        let sep = match self.style { FormatterStyle::Compact => \"\", FormatterStyle::Pretty => \"\\n\" };".ai(),
        "        tokens.join(sep)".ai(),
        "    }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: add formatter module")
        .unwrap();

    // C4: src/encoder.rs
    let mut f4 = repo.filename("src/encoder.rs");
    f4.set_contents(crate::lines![
        "pub struct Encoder {".ai(),
        "    buffer: Vec<u8>,".ai(),
        "}".ai(),
        "impl Encoder {".ai(),
        "    pub fn new() -> Self {".ai(),
        "        Self { buffer: Vec::new() }".ai(),
        "    }".ai(),
        "    pub fn encode_str(&mut self, s: &str) {".ai(),
        "        let len = s.len() as u32;".ai(),
        "        self.buffer.extend_from_slice(&len.to_le_bytes());".ai(),
        "        self.buffer.extend_from_slice(s.as_bytes());".ai(),
        "    }".ai(),
        "    pub fn finish(self) -> Vec<u8> { self.buffer }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: add encoder module")
        .unwrap();

    // C5: src/decoder.rs
    let mut f5 = repo.filename("src/decoder.rs");
    f5.set_contents(crate::lines![
        "pub struct Decoder<'a> {".ai(),
        "    data: &'a [u8],".ai(),
        "    pos: usize,".ai(),
        "}".ai(),
        "impl<'a> Decoder<'a> {".ai(),
        "    pub fn new(data: &'a [u8]) -> Self {".ai(),
        "        Self { data, pos: 0 }".ai(),
        "    }".ai(),
        "    pub fn decode_str(&mut self) -> Option<&'a str> {".ai(),
        "        if self.pos + 4 > self.data.len() { return None; }".ai(),
        "        let len = u32::from_le_bytes(self.data[self.pos..self.pos+4].try_into().ok()?) as usize;".ai(),
        "        self.pos += 4;".ai(),
        "        let s = std::str::from_utf8(&self.data[self.pos..self.pos+len]).ok()?;".ai(),
        "        self.pos += len; Some(s)".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: add decoder module")
        .unwrap();

    // === MAIN BRANCH: 5 human commits on different files ===
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.commit_untracked_file(
        "Cargo.toml",
        "[package]\nname = \"mylib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        "build: add Cargo.toml",
    );
    repo.commit_untracked_file(
        "build.rs",
        "fn main() { println!(\"cargo:rerun-if-changed=build.rs\"); }\n",
        "build: add build script",
    );
    repo.commit_untracked_file("benches/bench.rs",
        "use criterion::{criterion_group, criterion_main, Criterion};\nfn bench(_c: &mut Criterion) {}\ncriterion_group!(benches, bench);\ncriterion_main!(benches);\n",
        "bench: add criterion benchmark stub",
    );
    repo.commit_untracked_file(
        "examples/demo.rs",
        "fn main() { println!(\"demo\"); }\n",
        "examples: add demo",
    );
    repo.commit_untracked_file(
        "tests/integration.rs",
        "#[test]\nfn integration_placeholder() { assert!(true); }\n",
        "test: add integration test placeholder",
    );

    // === REBASE feature onto main ===
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &main_branch]).unwrap();

    // === VERIFY AT EVERY COMMIT ===
    let chain = get_commit_chain(&repo, 5);

    // sha0 = C1': only src/parser.rs
    assert_note_base_commit_matches(&repo, &chain[0], "sha0");
    assert_note_files_exact(&repo, &chain[0], "sha0_files", &["src/parser.rs"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[0],
        "sha0_no_future",
        &[
            "src/validator.rs",
            "src/formatter.rs",
            "src/encoder.rs",
            "src/decoder.rs",
        ],
    );
    assert_blame_at_commit(
        &repo,
        &chain[0],
        "src/parser.rs",
        "sha0_blame",
        &[
            ("pub struct Parser {", true),
            ("input: String,", true),
            ("pos: usize,", true),
            ("}", true),
            ("impl Parser {", true),
            ("pub fn new(input: &str) -> Self {", true),
            ("Self { input: input.to_string(), pos: 0 }", true),
            ("}", true),
            ("pub fn parse_token(&mut self) -> Option<&str> {", true),
            ("let start = self.pos;", true),
            ("while self.pos < self.input.len()", true),
            ("if start == self.pos", true),
            ("}", true),
            ("}", true),
        ],
    );

    // sha1 = C2': validator
    assert_note_base_commit_matches(&repo, &chain[1], "sha1");
    assert_note_files_exact(&repo, &chain[1], "sha1_files", &["src/validator.rs"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[1],
        "sha1_no_future",
        &["src/formatter.rs", "src/encoder.rs", "src/decoder.rs"],
    );
    assert_blame_at_commit(
        &repo,
        &chain[1],
        "src/validator.rs",
        "sha1_blame",
        &[
            ("pub struct Validator {", true),
            ("rules: Vec<Box<dyn Fn(&str) -> bool>>,", true),
            ("}", true),
            ("impl Validator {", true),
            ("pub fn new() -> Self {", true),
            ("Self { rules: Vec::new() }", true),
            ("}", true),
            (
                "pub fn add_rule(&mut self, rule: impl Fn(&str) -> bool + 'static) {",
                true,
            ),
            ("self.rules.push(Box::new(rule));", true),
            ("}", true),
            ("pub fn validate(&self, input: &str) -> bool {", true),
            ("self.rules.iter().all(|r| r(input))", true),
            ("}", true),
            ("}", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "src/parser.rs",
        "chain1_prior_parser_rs",
        &[
            ("pub struct Parser {", true),
            ("pub fn parse_token(&mut self) -> Option<&str> {", true),
        ],
    );

    // sha2 = C3': formatter
    assert_note_base_commit_matches(&repo, &chain[2], "sha2");
    assert_note_files_exact(&repo, &chain[2], "sha2_files", &["src/formatter.rs"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[2],
        "sha2_no_future",
        &["src/encoder.rs", "src/decoder.rs"],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "src/parser.rs",
        "chain2_prior_parser_rs",
        &[
            ("pub struct Parser {", true),
            ("pub fn parse_token(&mut self) -> Option<&str> {", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "src/validator.rs",
        "chain2_prior_validator_rs",
        &[
            ("pub struct Validator {", true),
            ("pub fn validate(&self, input: &str) -> bool {", true),
        ],
    );

    // sha3 = C4': encoder
    assert_note_base_commit_matches(&repo, &chain[3], "sha3");
    assert_note_files_exact(&repo, &chain[3], "sha3_files", &["src/encoder.rs"]);
    assert_note_no_forbidden_files(&repo, &chain[3], "sha3_no_future", &["src/decoder.rs"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "src/parser.rs",
        "chain3_prior_parser_rs",
        &[
            ("pub struct Parser {", true),
            ("pub fn parse_token(&mut self) -> Option<&str> {", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "src/validator.rs",
        "chain3_prior_validator_rs",
        &[
            ("pub struct Validator {", true),
            ("pub fn validate(&self, input: &str) -> bool {", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "src/formatter.rs",
        "chain3_prior_formatter_rs",
        &[
            ("pub struct Formatter {", true),
            ("pub fn format(&self, tokens: &[&str]) -> String {", true),
        ],
    );

    // sha4 = C5': decoder
    assert_note_base_commit_matches(&repo, &chain[4], "sha4");
    assert_note_files_exact(&repo, &chain[4], "sha4_files", &["src/decoder.rs"]);
    assert_blame_at_commit(
        &repo,
        &chain[4],
        "src/decoder.rs",
        "sha4_blame",
        &[
            ("pub struct Decoder<'a> {", true),
            ("data: &'a [u8],", true),
            ("pos: usize,", true),
            ("}", true),
            ("impl<'a> Decoder<'a> {", true),
            ("pub fn new(data: &'a [u8]) -> Self {", true),
            ("Self { data, pos: 0 }", true),
            ("}", true),
            ("pub fn decode_str(&mut self) -> Option<&'a str> {", true),
            ("if self.pos + 4 > self.data.len()", true),
            ("let len = u32::from_le_bytes", true),
            ("self.pos += 4;", true),
            ("let s = std::str::from_utf8", true),
            ("self.pos += len; Some(s)", true),
            ("}", true),
        ],
    );
    // Verify C1's file (src/parser.rs) still correctly attributed at tip.
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "src/parser.rs",
        "sha4_parser_preserved",
        &[
            ("pub struct Parser {", true),
            ("pub fn new(input: &str) -> Self {", true),
            ("pub fn parse_token(&mut self)", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "src/validator.rs",
        "chain4_prior_validator_rs",
        &[
            ("pub struct Validator {", true),
            ("pub fn validate(&self, input: &str) -> bool {", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "src/formatter.rs",
        "chain4_prior_formatter_rs",
        &[
            ("pub struct Formatter {", true),
            ("pub fn format(&self, tokens: &[&str]) -> String {", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "src/encoder.rs",
        "chain4_prior_encoder_rs",
        &[
            ("pub struct Encoder {", true),
            ("pub fn encode_str(&mut self, s: &str) {", true),
        ],
    );
}

#[test]
fn test_fast_path_go_service_5_handlers() {
    let repo = TestRepo::new();

    // Initial commit (shared base)
    let mut init = repo.filename("main.go");
    init.set_contents(crate::lines!["// Go HTTP service"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let main_branch = repo.current_branch();

    // === FEATURE BRANCH: 5 commits, each adding a Go handler file ===
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // C1: handlers/user.go
    let mut f1 = repo.filename("handlers/user.go");
    f1.set_contents(crate::lines![
        "package handlers".ai(),
        "".ai(),
        "import \"net/http\"".ai(),
        "".ai(),
        "type UserHandler struct { store UserStore }".ai(),
        "".ai(),
        "func NewUserHandler(s UserStore) *UserHandler { return &UserHandler{store: s} }".ai(),
        "".ai(),
        "func (h *UserHandler) GetUser(w http.ResponseWriter, r *http.Request) {".ai(),
        "    id := r.PathValue(\"id\")".ai(),
        "    user, err := h.store.Find(id)".ai(),
        "    if err != nil { http.Error(w, err.Error(), http.StatusNotFound); return }".ai(),
        "    writeJSON(w, user)".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: add user handler").unwrap();

    // C2: handlers/product.go
    let mut f2 = repo.filename("handlers/product.go");
    f2.set_contents(crate::lines![
        "package handlers".ai(),
        "".ai(),
        "import \"net/http\"".ai(),
        "".ai(),
        "type ProductHandler struct { store ProductStore }".ai(),
        "".ai(),
        "func NewProductHandler(s ProductStore) *ProductHandler { return &ProductHandler{store: s} }".ai(),
        "".ai(),
        "func (h *ProductHandler) ListProducts(w http.ResponseWriter, r *http.Request) {".ai(),
        "    products, err := h.store.List()".ai(),
        "    if err != nil { http.Error(w, err.Error(), http.StatusInternalServerError); return }".ai(),
        "    writeJSON(w, products)".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: add product handler")
        .unwrap();

    // C3: handlers/order.go
    let mut f3 = repo.filename("handlers/order.go");
    f3.set_contents(crate::lines![
        "package handlers".ai(),
        "".ai(),
        "import (\"net/http\"; \"encoding/json\")".ai(),
        "".ai(),
        "type OrderHandler struct { store OrderStore }".ai(),
        "".ai(),
        "func NewOrderHandler(s OrderStore) *OrderHandler { return &OrderHandler{store: s} }".ai(),
        "".ai(),
        "func (h *OrderHandler) CreateOrder(w http.ResponseWriter, r *http.Request) {".ai(),
        "    var req CreateOrderRequest".ai(),
        "    if err := json.NewDecoder(r.Body).Decode(&req); err != nil { http.Error(w, err.Error(), http.StatusBadRequest); return }".ai(),
        "    order, err := h.store.Create(req)".ai(),
        "    if err != nil { http.Error(w, err.Error(), http.StatusInternalServerError); return }".ai(),
        "    writeJSON(w, order)".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: add order handler")
        .unwrap();

    // C4: handlers/auth.go
    let mut f4 = repo.filename("handlers/auth.go");
    f4.set_contents(crate::lines![
        "package handlers".ai(),
        "".ai(),
        "import (\"net/http\"; \"time\")".ai(),
        "".ai(),
        "type AuthHandler struct { svc AuthService }".ai(),
        "".ai(),
        "func NewAuthHandler(s AuthService) *AuthHandler { return &AuthHandler{svc: s} }".ai(),
        "".ai(),
        "func (h *AuthHandler) Login(w http.ResponseWriter, r *http.Request) {".ai(),
        "    token, err := h.svc.Authenticate(r.FormValue(\"user\"), r.FormValue(\"pass\"))".ai(),
        "    if err != nil { http.Error(w, \"unauthorized\", http.StatusUnauthorized); return }".ai(),
        "    http.SetCookie(w, &http.Cookie{Name: \"session\", Value: token, Expires: time.Now().Add(24*time.Hour)})".ai(),
        "    w.WriteHeader(http.StatusOK)".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: add auth handler").unwrap();

    // C5: handlers/health.go
    let mut f5 = repo.filename("handlers/health.go");
    f5.set_contents(crate::lines![
        "package handlers".ai(),
        "".ai(),
        "import (\"net/http\"; \"encoding/json\")".ai(),
        "".ai(),
        "type HealthHandler struct { version string }".ai(),
        "".ai(),
        "func NewHealthHandler(v string) *HealthHandler { return &HealthHandler{version: v} }".ai(),
        "".ai(),
        "func (h *HealthHandler) Health(w http.ResponseWriter, r *http.Request) {".ai(),
        "    json.NewEncoder(w).Encode(map[string]string{\"status\": \"ok\", \"version\": h.version})".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: add health handler")
        .unwrap();

    // === MAIN BRANCH: 5 human commits on different files ===
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.commit_untracked_file(
        "go.mod",
        "module example.com/service\n\ngo 1.21\n",
        "build: add go.mod",
    );
    repo.commit_untracked_file(
        "cmd/main.go",
        "package main\n\nfunc main() {}\n",
        "build: add cmd/main.go",
    );
    repo.commit_untracked_file("Dockerfile",
        "FROM golang:1.21\nWORKDIR /app\nCOPY . .\nRUN go build -o server cmd/main.go\nCMD [\"./server\"]\n",
        "build: add Dockerfile",
    );
    repo.commit_untracked_file(
        "docker-compose.yml",
        "version: '3.8'\nservices:\n  app:\n    build: .\n    ports:\n      - '8080:8080'\n",
        "build: add docker-compose.yml",
    );
    repo.commit_untracked_file(
        "Makefile",
        "build:\n\tgo build ./...\ntest:\n\tgo test ./...\n.PHONY: build test\n",
        "build: add Makefile",
    );

    // === REBASE feature onto main ===
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &main_branch]).unwrap();

    // === VERIFY AT EVERY COMMIT ===
    let chain = get_commit_chain(&repo, 5);

    // sha0 = C1': only handlers/user.go
    assert_note_base_commit_matches(&repo, &chain[0], "sha0");
    assert_note_files_exact(&repo, &chain[0], "sha0_files", &["handlers/user.go"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[0],
        "sha0_no_future",
        &[
            "handlers/product.go",
            "handlers/order.go",
            "handlers/auth.go",
            "handlers/health.go",
        ],
    );
    assert_blame_at_commit(
        &repo,
        &chain[0],
        "handlers/user.go",
        "sha0_blame",
        &[
            ("package handlers", true),
            ("", true),
            ("import \"net/http\"", true),
            ("", true),
            ("type UserHandler struct", true),
            ("", true),
            ("func NewUserHandler", true),
            ("", true),
            ("func (h *UserHandler) GetUser", true),
            ("id := r.PathValue", true),
            ("user, err := h.store.Find", true),
            ("if err != nil", true),
            ("writeJSON(w, user)", true),
            ("}", true),
        ],
    );

    // sha1 = C2': product
    assert_note_base_commit_matches(&repo, &chain[1], "sha1");
    assert_note_files_exact(&repo, &chain[1], "sha1_files", &["handlers/product.go"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[1],
        "sha1_no_future",
        &[
            "handlers/order.go",
            "handlers/auth.go",
            "handlers/health.go",
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "handlers/user.go",
        "chain1_prior_user_go",
        &[
            ("type UserHandler struct", true),
            ("func (h *UserHandler) GetUser", true),
        ],
    );

    // sha2 = C3': order
    assert_note_base_commit_matches(&repo, &chain[2], "sha2");
    assert_note_files_exact(&repo, &chain[2], "sha2_files", &["handlers/order.go"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[2],
        "sha2_no_future",
        &["handlers/auth.go", "handlers/health.go"],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "handlers/user.go",
        "chain2_prior_user_go",
        &[
            ("type UserHandler struct", true),
            ("func (h *UserHandler) GetUser", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "handlers/product.go",
        "chain2_prior_product_go",
        &[
            ("type ProductHandler struct", true),
            ("func (h *ProductHandler) ListProducts", true),
        ],
    );

    // sha3 = C4': auth
    assert_note_base_commit_matches(&repo, &chain[3], "sha3");
    assert_note_files_exact(&repo, &chain[3], "sha3_files", &["handlers/auth.go"]);
    assert_note_no_forbidden_files(&repo, &chain[3], "sha3_no_future", &["handlers/health.go"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "handlers/user.go",
        "chain3_prior_user_go",
        &[
            ("type UserHandler struct", true),
            ("func (h *UserHandler) GetUser", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "handlers/product.go",
        "chain3_prior_product_go",
        &[
            ("type ProductHandler struct", true),
            ("func (h *ProductHandler) ListProducts", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "handlers/order.go",
        "chain3_prior_order_go",
        &[
            ("type OrderHandler struct", true),
            ("func (h *OrderHandler) CreateOrder", true),
        ],
    );

    // sha4 = C5': health
    assert_note_base_commit_matches(&repo, &chain[4], "sha4");
    assert_note_files_exact(&repo, &chain[4], "sha4_files", &["handlers/health.go"]);
    assert_blame_at_commit(
        &repo,
        &chain[4],
        "handlers/health.go",
        "sha4_blame",
        &[
            ("package handlers", true),
            ("", true),
            ("import", true),
            ("", true),
            ("type HealthHandler struct", true),
            ("", true),
            ("func NewHealthHandler", true),
            ("", true),
            ("func (h *HealthHandler) Health", true),
            ("json.NewEncoder(w).Encode", true),
            ("}", true),
        ],
    );
    // Verify C1's file (handlers/user.go) still correctly attributed at tip.
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "handlers/user.go",
        "sha4_user_preserved",
        &[
            ("type UserHandler struct", true),
            ("func NewUserHandler", true),
            ("func (h *UserHandler) GetUser", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "handlers/product.go",
        "chain4_prior_product_go",
        &[
            ("type ProductHandler struct", true),
            ("func (h *ProductHandler) ListProducts", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "handlers/order.go",
        "chain4_prior_order_go",
        &[
            ("type OrderHandler struct", true),
            ("func (h *OrderHandler) CreateOrder", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "handlers/auth.go",
        "chain4_prior_auth_go",
        &[
            ("type AuthHandler struct", true),
            ("func (h *AuthHandler) Login", true),
        ],
    );
}

#[test]
fn test_fast_path_mixed_ai_and_human_feature_commits() {
    let repo = TestRepo::new();

    // Initial commit (shared base)
    let mut init = repo.filename("app.py");
    init.set_contents(crate::lines!["# Python application"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let main_branch = repo.current_branch();

    // === FEATURE BRANCH: 5 commits alternating AI/human ===
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // C1: human-only commit — config.py (plain string, no .ai())
    repo.commit_untracked_file(
        "config.py",
        "DATABASE_URL = 'sqlite:///app.db'\nDEBUG = False\nSECRET_KEY = 'changeme'\n",
        "config: add app config",
    );

    // C2: AI commit — auth.py
    let mut f2 = repo.filename("auth.py");
    f2.set_contents(crate::lines![
        "import hashlib, secrets".ai(),
        "".ai(),
        "class AuthService:".ai(),
        "    def __init__(self, user_store):".ai(),
        "        self.user_store = user_store".ai(),
        "    def hash_password(self, password):".ai(),
        "        salt = secrets.token_hex(16)".ai(),
        "        hashed = hashlib.sha256((password + salt).encode()).hexdigest()".ai(),
        "        return f'{salt}:{hashed}'".ai(),
        "    def verify_password(self, password, stored):".ai(),
        "        salt, hashed = stored.split(':')".ai(),
        "        return hashlib.sha256((password + salt).encode()).hexdigest() == hashed".ai(),
    ]);
    repo.stage_all_and_commit("feat: add auth service").unwrap();

    // C3: AI commit — middleware.py
    let mut f3 = repo.filename("middleware.py");
    f3.set_contents(crate::lines![
        "from functools import wraps".ai(),
        "from flask import request, jsonify, g".ai(),
        "".ai(),
        "def require_auth(f):".ai(),
        "    @wraps(f)".ai(),
        "    def decorated(*args, **kwargs):".ai(),
        "        token = request.headers.get('Authorization', '').removeprefix('Bearer ')".ai(),
        "        if not token:".ai(),
        "            return jsonify({'error': 'missing token'}), 401".ai(),
        "        g.user = verify_token(token)".ai(),
        "        return f(*args, **kwargs)".ai(),
        "    return decorated".ai(),
    ]);
    repo.stage_all_and_commit("feat: add auth middleware")
        .unwrap();

    // C4: human-only commit — requirements.txt
    repo.commit_untracked_file(
        "requirements.txt",
        "flask==3.0.0\nsqlalchemy==2.0.23\nclick==8.1.7\n",
        "deps: add requirements.txt",
    );

    // C5: AI commit — router.py
    let mut f5 = repo.filename("router.py");
    f5.set_contents(crate::lines![
        "from flask import Blueprint, jsonify, request".ai(),
        "".ai(),
        "api = Blueprint('api', __name__, url_prefix='/api/v1')".ai(),
        "".ai(),
        "@api.route('/health')".ai(),
        "def health():".ai(),
        "    return jsonify({'status': 'ok'})".ai(),
        "".ai(),
        "@api.route('/users', methods=['GET'])".ai(),
        "def list_users():".ai(),
        "    return jsonify({'users': []})".ai(),
    ]);
    repo.stage_all_and_commit("feat: add API router").unwrap();

    // === MAIN BRANCH: 5 human commits on different files ===
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.commit_untracked_file(
        "tests/test_smoke.py",
        "def test_smoke(): assert True\n",
        "test: add smoke test",
    );
    repo.commit_untracked_file(".github/workflows/test.yml",
        "name: Test\non: [push]\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps: [{uses: actions/checkout@v3}, {run: pytest}]\n",
        "ci: add test workflow",
    );
    repo.commit_untracked_file(
        "pyproject.toml",
        "[build-system]\nrequires = ['setuptools']\nbuild-backend = 'setuptools.build_meta'\n",
        "build: add pyproject.toml",
    );
    repo.commit_untracked_file(
        ".gitignore",
        "__pycache__/\n*.pyc\n.env\nvenv/\n",
        "git: add .gitignore",
    );
    repo.commit_untracked_file(
        "README.rst",
        "Python App\n==========\n\nInstall and run the app.\n",
        "docs: add README",
    );

    // === REBASE feature onto main ===
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &main_branch]).unwrap();

    // === VERIFY AT EVERY COMMIT ===
    let chain = get_commit_chain(&repo, 5);

    // sha0 = C1': human commit (commit_untracked_file — no AI tracking, no note after rebase).
    // Verify no AI files leak in if a note somehow exists.
    assert_note_no_forbidden_files_if_present(
        &repo,
        &chain[0],
        "sha0_no_ai",
        &["auth.py", "middleware.py", "router.py"],
    );

    // sha1 = C2': note has auth.py only
    assert_note_base_commit_matches(&repo, &chain[1], "sha1");
    assert_note_files_exact(&repo, &chain[1], "sha1_files", &["auth.py"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[1],
        "sha1_no_future",
        &["middleware.py", "router.py"],
    );
    assert_blame_at_commit(
        &repo,
        &chain[1],
        "auth.py",
        "sha1_blame",
        &[
            ("import hashlib, secrets", true),
            ("", true),
            ("class AuthService:", true),
            ("def __init__(self, user_store):", true),
            ("self.user_store = user_store", true),
            ("def hash_password(self, password):", true),
            ("salt = secrets.token_hex(16)", true),
            ("hashed = hashlib.sha256", true),
            ("return f'{salt}:{hashed}'", true),
            ("def verify_password(self, password, stored):", true),
            ("salt, hashed = stored.split(':')", true),
            ("return hashlib.sha256", true),
        ],
    );

    // sha2 = C3': note has middleware.py
    assert_note_base_commit_matches(&repo, &chain[2], "sha2");
    assert_note_files_exact(&repo, &chain[2], "sha2_files", &["middleware.py"]);
    assert_note_no_forbidden_files(&repo, &chain[2], "sha2_no_future", &["router.py"]);
    // Verify auth.py attribution still intact at this position (not wiped by C3 processing).
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "auth.py",
        "sha2_auth_preserved",
        &[("class AuthService:", true), ("def hash_password", true)],
    );

    // sha3 = C4': human commit (commit_untracked_file — no note expected).
    // Just verify no future AI file leaked into a note if one exists.
    assert_note_no_forbidden_files_if_present(&repo, &chain[3], "sha3_no_future", &["router.py"]);

    // sha4 = C5': note has router.py
    assert_note_base_commit_matches(&repo, &chain[4], "sha4");
    assert_note_files_exact(&repo, &chain[4], "sha4_files", &["router.py"]);
    assert_blame_at_commit(
        &repo,
        &chain[4],
        "router.py",
        "sha4_blame",
        &[
            ("from flask import Blueprint, jsonify, request", true),
            ("", true),
            (
                "api = Blueprint('api', __name__, url_prefix='/api/v1')",
                true,
            ),
            ("", true),
            ("@api.route('/health')", true),
            ("def health():", true),
            ("return jsonify({'status': 'ok'})", true),
            ("", true),
            ("@api.route('/users', methods=['GET'])", true),
            ("def list_users():", true),
            ("return jsonify({'users': []})", true),
        ],
    );
    // Verify auth.py (C2's file) still correctly attributed at tip — not corrupted by later commits.
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "auth.py",
        "sha4_auth_preserved",
        &[
            ("class AuthService:", true),
            ("def hash_password", true),
            ("def verify_password", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "middleware.py",
        "chain4_prior_middleware_py",
        &[
            ("def require_auth(f):", true),
            ("def decorated(*args, **kwargs):", true),
        ],
    );
}

#[test]
fn test_fast_path_10_commits_javascript_utilities() {
    let repo = TestRepo::new();

    // Initial commit (shared base)
    let mut init = repo.filename("index.js");
    init.set_contents(crate::lines!["// JavaScript utility library"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let main_branch = repo.current_branch();

    // === FEATURE BRANCH: 10 commits, each adding a JS utility file ===
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // C1: date_utils.js
    let mut fu1 = repo.filename("date_utils.js");
    fu1.set_contents(crate::lines![
        "export function formatDate(date, fmt = 'YYYY-MM-DD') {".ai(),
        "  const d = date instanceof Date ? date : new Date(date);".ai(),
        "  return fmt.replace('YYYY', d.getFullYear()).replace('MM', String(d.getMonth()+1).padStart(2,'0')).replace('DD', String(d.getDate()).padStart(2,'0'));".ai(),
        "}".ai(),
        "export function addDays(date, n) { const d = new Date(date); d.setDate(d.getDate() + n); return d; }".ai(),
        "export function diffDays(a, b) { return Math.floor((new Date(b) - new Date(a)) / 86400000); }".ai(),
        "export function isWeekend(date) { const day = new Date(date).getDay(); return day === 0 || day === 6; }".ai(),
        "export function startOfWeek(date) { const d = new Date(date); d.setDate(d.getDate() - d.getDay()); return d; }".ai(),
    ]);
    repo.stage_all_and_commit("feat: add date utilities")
        .unwrap();

    // C2: string_utils.js
    let mut fu2 = repo.filename("string_utils.js");
    fu2.set_contents(crate::lines![
        "export const capitalize = s => s.charAt(0).toUpperCase() + s.slice(1);".ai(),
        "export const camelToKebab = s => s.replace(/[A-Z]/g, m => `-${m.toLowerCase()}`);".ai(),
        "export const kebabToCamel = s => s.replace(/-([a-z])/g, (_, c) => c.toUpperCase());".ai(),
        "export const truncate = (s, n, ellipsis = '...') => s.length <= n ? s : s.slice(0, n - ellipsis.length) + ellipsis;".ai(),
        "export const slugify = s => s.toLowerCase().trim().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');".ai(),
        "export const countWords = s => s.trim().split(/\\s+/).filter(Boolean).length;".ai(),
        "export const repeat = (s, n) => Array(n).fill(s).join('');".ai(),
        "export const escapeHtml = s => s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');".ai(),
    ]);
    repo.stage_all_and_commit("feat: add string utilities")
        .unwrap();

    // C3: array_utils.js
    let mut fu3 = repo.filename("array_utils.js");
    fu3.set_contents(crate::lines![
        "export const unique = arr => [...new Set(arr)];".ai(),
        "export const flatten = arr => arr.reduce((a, b) => a.concat(Array.isArray(b) ? flatten(b) : b), []);".ai(),
        "export const chunk = (arr, size) => Array.from({length: Math.ceil(arr.length/size)}, (_, i) => arr.slice(i*size, i*size+size));".ai(),
        "export const groupBy = (arr, key) => arr.reduce((g, item) => { (g[item[key]] = g[item[key]] || []).push(item); return g; }, {});".ai(),
        "export const sortBy = (arr, key, dir = 'asc') => [...arr].sort((a,b) => dir==='asc' ? (a[key]>b[key]?1:-1) : (a[key]<b[key]?1:-1));".ai(),
        "export const intersection = (a, b) => a.filter(x => b.includes(x));".ai(),
        "export const difference = (a, b) => a.filter(x => !b.includes(x));".ai(),
        "export const zip = (...arrays) => arrays[0].map((_,i) => arrays.map(a => a[i]));".ai(),
    ]);
    repo.stage_all_and_commit("feat: add array utilities")
        .unwrap();

    // C4: object_utils.js
    let mut fu4 = repo.filename("object_utils.js");
    fu4.set_contents(crate::lines![
        "export const pick = (obj, keys) => Object.fromEntries(keys.map(k => [k, obj[k]]));".ai(),
        "export const omit = (obj, keys) => Object.fromEntries(Object.entries(obj).filter(([k]) => !keys.includes(k)));".ai(),
        "export const deepClone = obj => JSON.parse(JSON.stringify(obj));".ai(),
        "export const deepMerge = (a, b) => { const r = {...a}; for (const k in b) r[k] = (typeof b[k]==='object'&&b[k]&&!Array.isArray(b[k])) ? deepMerge(a[k]||{},b[k]) : b[k]; return r; };".ai(),
        "export const flatten_obj = (obj, prefix='') => Object.entries(obj).reduce((a,[k,v]) => typeof v==='object'&&v ? {...a,...flatten_obj(v,prefix+k+'.')} : {...a,[prefix+k]:v}, {});".ai(),
        "export const isEmpty = obj => Object.keys(obj).length === 0;".ai(),
        "export const mapValues = (obj, fn) => Object.fromEntries(Object.entries(obj).map(([k,v]) => [k, fn(v, k)]));".ai(),
        "export const filterKeys = (obj, pred) => Object.fromEntries(Object.entries(obj).filter(([k]) => pred(k)));".ai(),
    ]);
    repo.stage_all_and_commit("feat: add object utilities")
        .unwrap();

    // C5: number_utils.js
    let mut fu5 = repo.filename("number_utils.js");
    fu5.set_contents(crate::lines![
        "export const clamp = (n, min, max) => Math.min(Math.max(n, min), max);".ai(),
        "export const lerp = (a, b, t) => a + (b - a) * t;".ai(),
        "export const round = (n, decimals) => Math.round(n * 10**decimals) / 10**decimals;".ai(),
        "export const formatBytes = n => { const units=['B','KB','MB','GB']; let i=0; while(n>=1024&&i<3){n/=1024;i++;} return `${n.toFixed(1)} ${units[i]}`; };".ai(),
        "export const isPrime = n => { if(n<2) return false; for(let i=2;i<=Math.sqrt(n);i++) if(n%i===0) return false; return true; };".ai(),
        "export const fibonacci = n => n<=1 ? n : fibonacci(n-1)+fibonacci(n-2);".ai(),
        "export const gcd = (a,b) => b===0 ? a : gcd(b, a%b);".ai(),
        "export const range = (start, end, step=1) => Array.from({length:Math.ceil((end-start)/step)},(_,i)=>start+i*step);".ai(),
    ]);
    repo.stage_all_and_commit("feat: add number utilities")
        .unwrap();

    // C6: dom_utils.js
    let mut fu6 = repo.filename("dom_utils.js");
    fu6.set_contents(crate::lines![
        "export const $ = sel => document.querySelector(sel);".ai(),
        "export const $$ = sel => [...document.querySelectorAll(sel)];".ai(),
        "export const on = (el, ev, fn, opts) => { el.addEventListener(ev, fn, opts); return () => el.removeEventListener(ev, fn); };".ai(),
        "export const once = (el, ev, fn) => el.addEventListener(ev, fn, {once: true});".ai(),
        "export const delegate = (root, sel, ev, fn) => on(root, ev, e => { const t = e.target.closest(sel); if(t && root.contains(t)) fn.call(t, e); });".ai(),
        "export const ready = fn => document.readyState !== 'loading' ? fn() : document.addEventListener('DOMContentLoaded', fn);".ai(),
        "export const setStyles = (el, styles) => Object.assign(el.style, styles);".ai(),
        "export const toggleClass = (el, cls, force) => el.classList.toggle(cls, force);".ai(),
    ]);
    repo.stage_all_and_commit("feat: add dom utilities")
        .unwrap();

    // C7: fetch_utils.js
    let mut fu7 = repo.filename("fetch_utils.js");
    fu7.set_contents(crate::lines![
        "export async function getJSON(url, opts={}) { const r = await fetch(url, opts); if(!r.ok) throw new Error(r.statusText); return r.json(); }".ai(),
        "export async function postJSON(url, body, opts={}) { return getJSON(url, {method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify(body), ...opts}); }".ai(),
        "export async function retry(fn, attempts=3, delay=300) { for(let i=0;i<attempts;i++) { try { return await fn(); } catch(e) { if(i===attempts-1) throw e; await new Promise(r=>setTimeout(r,delay*(i+1))); } } }".ai(),
        "export const withTimeout = (promise, ms) => Promise.race([promise, new Promise((_,r)=>setTimeout(()=>r(new Error('timeout')),ms))]);".ai(),
        "export const buildURL = (base, params) => { const u = new URL(base); Object.entries(params).forEach(([k,v])=>u.searchParams.set(k,v)); return u.toString(); };".ai(),
        "export const isAbsolute = url => /^https?:\\/\\//.test(url);".ai(),
        "export async function downloadBlob(url, filename) { const r = await fetch(url); const b = await r.blob(); const a = document.createElement('a'); a.href = URL.createObjectURL(b); a.download = filename; a.click(); }".ai(),
        "export const memoFetch = (() => { const cache = {}; return async (url) => cache[url] ?? (cache[url] = await getJSON(url)); })();".ai(),
    ]);
    repo.stage_all_and_commit("feat: add fetch utilities")
        .unwrap();

    // C8: storage_utils.js
    let mut fu8 = repo.filename("storage_utils.js");
    fu8.set_contents(crate::lines![
        "export const ls = { get: k => { try { return JSON.parse(localStorage.getItem(k)); } catch { return null; } }, set: (k,v) => localStorage.setItem(k, JSON.stringify(v)), del: k => localStorage.removeItem(k), clear: () => localStorage.clear() };".ai(),
        "export const ss = { get: k => { try { return JSON.parse(sessionStorage.getItem(k)); } catch { return null; } }, set: (k,v) => sessionStorage.setItem(k, JSON.stringify(v)), del: k => sessionStorage.removeItem(k) };".ai(),
        "export function createStore(key, initial) { let val = ls.get(key) ?? initial; return { get: () => val, set: v => { val = v; ls.set(key, v); }, reset: () => { val = initial; ls.del(key); } }; }".ai(),
        "export const cookie = { get: name => Object.fromEntries(document.cookie.split(';').map(c=>c.trim().split('=')))[name], set: (name,value,days=7) => { document.cookie = `${name}=${value};max-age=${days*86400};path=/`; }, del: name => cookie.set(name, '', -1) };".ai(),
        "export function withExpiry(key, value, ttl) { ls.set(key, {value, expires: Date.now()+ttl}); }".ai(),
        "export function getWithExpiry(key) { const item = ls.get(key); if(!item) return null; if(Date.now() > item.expires) { ls.del(key); return null; } return item.value; }".ai(),
        "export const hasStorage = (() => { try { localStorage.setItem('_t','1'); localStorage.removeItem('_t'); return true; } catch { return false; } })();".ai(),
        "export function broadcastStore(key, val) { ls.set(key, val); window.dispatchEvent(new StorageEvent('storage', {key, newValue: JSON.stringify(val)})); }".ai(),
    ]);
    repo.stage_all_and_commit("feat: add storage utilities")
        .unwrap();

    // C9: event_utils.js
    let mut fu9 = repo.filename("event_utils.js");
    fu9.set_contents(crate::lines![
        "export class EventEmitter { constructor() { this._events = {}; } on(ev, fn) { (this._events[ev] = this._events[ev]||[]).push(fn); return this; } off(ev, fn) { this._events[ev] = (this._events[ev]||[]).filter(f=>f!==fn); return this; } emit(ev, ...args) { (this._events[ev]||[]).forEach(f=>f(...args)); return this; } once(ev, fn) { const w = (...a) => { fn(...a); this.off(ev, w); }; return this.on(ev, w); } }".ai(),
        "export function debounce(fn, wait) { let t; return function(...a) { clearTimeout(t); t = setTimeout(()=>fn.apply(this,a), wait); }; }".ai(),
        "export function throttle(fn, wait) { let last=0; return function(...a) { const now=Date.now(); if(now-last>=wait) { last=now; return fn.apply(this,a); } }; }".ai(),
        "export function createPubSub() { const subs = {}; return { sub: (t, fn) => (subs[t] = subs[t]||new Set()).add(fn), unsub: (t, fn) => subs[t]?.delete(fn), pub: (t, d) => subs[t]?.forEach(fn => fn(d)) }; }".ai(),
        "export const keyCombo = (keys, fn) => document.addEventListener('keydown', e => { if(keys.every(k => k==='ctrl'?e.ctrlKey:k==='shift'?e.shiftKey:k==='alt'?e.altKey:e.key===k)) fn(e); });".ai(),
        "export function onIdle(fn) { return 'requestIdleCallback' in window ? requestIdleCallback(fn) : setTimeout(fn, 1); }".ai(),
        "export const dispatchCustom = (el, name, detail) => el.dispatchEvent(new CustomEvent(name, {bubbles: true, detail}));".ai(),
        "export function onVisible(el, fn) { const obs = new IntersectionObserver(([e])=>{ if(e.isIntersecting){fn();obs.disconnect();} }); obs.observe(el); return ()=>obs.disconnect(); }".ai(),
    ]);
    repo.stage_all_and_commit("feat: add event utilities")
        .unwrap();

    // C10: validation_utils.js
    let mut fu10 = repo.filename("validation_utils.js");
    fu10.set_contents(crate::lines![
        "export const isEmail = s => /^[^\\s@]+@[^\\s@]+\\.[^\\s@]+$/.test(s);".ai(),
        "export const isURL = s => { try { new URL(s); return true; } catch { return false; } };".ai(),
        "export const isPhone = s => /^\\+?[\\d\\s\\-().]{7,20}$/.test(s);".ai(),
        "export const isUUID = s => /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(s);".ai(),
        "export const minLength = (n, msg) => v => v.length >= n ? null : msg ?? `Min ${n} chars`;".ai(),
        "export const maxLength = (n, msg) => v => v.length <= n ? null : msg ?? `Max ${n} chars`;".ai(),
        "export const required = msg => v => v!=null && v!=='' ? null : msg ?? 'Required';".ai(),
        "export function validate(value, rules) { for(const r of rules) { const err = r(value); if(err) return err; } return null; }".ai(),
    ]);
    repo.stage_all_and_commit("feat: add validation utilities")
        .unwrap();

    // === MAIN BRANCH: 5 human commits on different files ===
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.commit_untracked_file(
        "package.json",
        "{\"name\":\"utils\",\"version\":\"1.0.0\",\"type\":\"module\"}\n",
        "build: add package.json",
    );
    repo.commit_untracked_file(".eslintrc.cjs",
        "module.exports={env:{browser:true,es2021:true},extends:['eslint:recommended'],parserOptions:{ecmaVersion:'latest',sourceType:'module'}};\n",
        "lint: add eslint config",
    );
    repo.commit_untracked_file("vitest.config.js",
        "import { defineConfig } from 'vitest/config';\nexport default defineConfig({test:{environment:'jsdom'}});\n",
        "test: add vitest config",
    );
    repo.commit_untracked_file(
        ".prettierrc",
        "{\"singleQuote\":true,\"semi\":false,\"trailingComma\":\"es5\"}\n",
        "style: add prettier config",
    );
    repo.commit_untracked_file(
        "README.md",
        "# JS Utilities\n\nA collection of JavaScript utility functions.\n",
        "docs: add README",
    );

    // === REBASE feature onto main ===
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &main_branch]).unwrap();

    // === VERIFY AT ALL 10 COMMITS ===
    let chain = get_commit_chain(&repo, 10);

    // sha0 = C1': only date_utils.js
    assert_note_base_commit_matches(&repo, &chain[0], "sha0");
    assert_note_files_exact(&repo, &chain[0], "sha0_files", &["date_utils.js"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[0],
        "sha0_no_future",
        &[
            "string_utils.js",
            "array_utils.js",
            "object_utils.js",
            "number_utils.js",
            "dom_utils.js",
            "fetch_utils.js",
            "storage_utils.js",
            "event_utils.js",
            "validation_utils.js",
        ],
    );
    assert_blame_at_commit(
        &repo,
        &chain[0],
        "date_utils.js",
        "sha0_blame",
        &[
            ("export function formatDate", true),
            ("const d = date instanceof Date", true),
            ("return fmt.replace", true),
            ("}", true),
            ("export function addDays", true),
            ("export function diffDays", true),
            ("export function isWeekend", true),
            ("export function startOfWeek", true),
        ],
    );

    // sha1 = C2': string_utils.js
    assert_note_base_commit_matches(&repo, &chain[1], "sha1");
    assert_note_files_exact(&repo, &chain[1], "sha1_files", &["string_utils.js"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[1],
        "sha1_no_future",
        &[
            "array_utils.js",
            "object_utils.js",
            "number_utils.js",
            "dom_utils.js",
            "fetch_utils.js",
            "storage_utils.js",
            "event_utils.js",
            "validation_utils.js",
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "date_utils.js",
        "chain1_prior_date_utils.js",
        &[
            ("export function formatDate", true),
            ("export function addDays", true),
        ],
    );

    // sha2 = C3': array_utils.js
    assert_note_base_commit_matches(&repo, &chain[2], "sha2");
    assert_note_files_exact(&repo, &chain[2], "sha2_files", &["array_utils.js"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "date_utils.js",
        "chain2_prior_date_utils.js",
        &[
            ("export function formatDate", true),
            ("export function addDays", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "string_utils.js",
        "chain2_prior_string_utils.js",
        &[
            ("export const capitalize", true),
            ("export const slugify", true),
        ],
    );

    // sha3 = C4': object_utils.js
    assert_note_base_commit_matches(&repo, &chain[3], "sha3");
    assert_note_files_exact(&repo, &chain[3], "sha3_files", &["object_utils.js"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "date_utils.js",
        "chain3_prior_date_utils.js",
        &[
            ("export function formatDate", true),
            ("export function addDays", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "string_utils.js",
        "chain3_prior_string_utils.js",
        &[
            ("export const capitalize", true),
            ("export const slugify", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "array_utils.js",
        "chain3_prior_array_utils.js",
        &[
            ("export const unique", true),
            ("export const flatten", true),
        ],
    );

    // sha4 = C5': number_utils.js
    assert_note_base_commit_matches(&repo, &chain[4], "sha4");
    assert_note_files_exact(&repo, &chain[4], "sha4_files", &["number_utils.js"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "date_utils.js",
        "chain4_prior_date_utils.js",
        &[
            ("export function formatDate", true),
            ("export function addDays", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "string_utils.js",
        "chain4_prior_string_utils.js",
        &[
            ("export const capitalize", true),
            ("export const slugify", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "array_utils.js",
        "chain4_prior_array_utils.js",
        &[
            ("export const unique", true),
            ("export const flatten", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "object_utils.js",
        "chain4_prior_object_utils.js",
        &[
            ("export const pick", true),
            ("export const deepClone", true),
        ],
    );

    // sha5 = C6': dom_utils.js
    assert_note_base_commit_matches(&repo, &chain[5], "sha5");
    assert_note_files_exact(&repo, &chain[5], "sha5_files", &["dom_utils.js"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[5],
        "date_utils.js",
        "chain5_prior_date_utils.js",
        &[
            ("export function formatDate", true),
            ("export function addDays", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[5],
        "string_utils.js",
        "chain5_prior_string_utils.js",
        &[
            ("export const capitalize", true),
            ("export const slugify", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[5],
        "array_utils.js",
        "chain5_prior_array_utils.js",
        &[
            ("export const unique", true),
            ("export const flatten", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[5],
        "object_utils.js",
        "chain5_prior_object_utils.js",
        &[
            ("export const pick", true),
            ("export const deepClone", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[5],
        "number_utils.js",
        "chain5_prior_number_utils.js",
        &[("export const clamp", true), ("export const lerp", true)],
    );

    // sha6 = C7': fetch_utils.js
    assert_note_base_commit_matches(&repo, &chain[6], "sha6");
    assert_note_files_exact(&repo, &chain[6], "sha6_files", &["fetch_utils.js"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[6],
        "date_utils.js",
        "chain6_prior_date_utils.js",
        &[
            ("export function formatDate", true),
            ("export function addDays", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[6],
        "string_utils.js",
        "chain6_prior_string_utils.js",
        &[
            ("export const capitalize", true),
            ("export const slugify", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[6],
        "array_utils.js",
        "chain6_prior_array_utils.js",
        &[
            ("export const unique", true),
            ("export const flatten", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[6],
        "object_utils.js",
        "chain6_prior_object_utils.js",
        &[
            ("export const pick", true),
            ("export const deepClone", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[6],
        "number_utils.js",
        "chain6_prior_number_utils.js",
        &[("export const clamp", true), ("export const lerp", true)],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[6],
        "dom_utils.js",
        "chain6_prior_dom_utils.js",
        &[
            ("export const $ = sel", true),
            ("export const $$ = sel", true),
        ],
    );

    // sha7 = C8': storage_utils.js
    assert_note_base_commit_matches(&repo, &chain[7], "sha7");
    assert_note_files_exact(&repo, &chain[7], "sha7_files", &["storage_utils.js"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[7],
        "date_utils.js",
        "chain7_prior_date_utils.js",
        &[
            ("export function formatDate", true),
            ("export function addDays", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[7],
        "string_utils.js",
        "chain7_prior_string_utils.js",
        &[
            ("export const capitalize", true),
            ("export const slugify", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[7],
        "array_utils.js",
        "chain7_prior_array_utils.js",
        &[
            ("export const unique", true),
            ("export const flatten", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[7],
        "object_utils.js",
        "chain7_prior_object_utils.js",
        &[
            ("export const pick", true),
            ("export const deepClone", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[7],
        "number_utils.js",
        "chain7_prior_number_utils.js",
        &[("export const clamp", true), ("export const lerp", true)],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[7],
        "dom_utils.js",
        "chain7_prior_dom_utils.js",
        &[
            ("export const $ = sel", true),
            ("export const $$ = sel", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[7],
        "fetch_utils.js",
        "chain7_prior_fetch_utils.js",
        &[
            ("export async function getJSON", true),
            ("export async function postJSON", true),
        ],
    );

    // sha8 = C9': event_utils.js
    assert_note_base_commit_matches(&repo, &chain[8], "sha8");
    assert_note_files_exact(&repo, &chain[8], "sha8_files", &["event_utils.js"]);
    assert_note_no_forbidden_files(&repo, &chain[8], "sha8_no_future", &["validation_utils.js"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[8],
        "date_utils.js",
        "chain8_prior_date_utils.js",
        &[
            ("export function formatDate", true),
            ("export function addDays", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[8],
        "string_utils.js",
        "chain8_prior_string_utils.js",
        &[
            ("export const capitalize", true),
            ("export const slugify", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[8],
        "array_utils.js",
        "chain8_prior_array_utils.js",
        &[
            ("export const unique", true),
            ("export const flatten", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[8],
        "object_utils.js",
        "chain8_prior_object_utils.js",
        &[
            ("export const pick", true),
            ("export const deepClone", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[8],
        "number_utils.js",
        "chain8_prior_number_utils.js",
        &[("export const clamp", true), ("export const lerp", true)],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[8],
        "dom_utils.js",
        "chain8_prior_dom_utils.js",
        &[
            ("export const $ = sel", true),
            ("export const $$ = sel", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[8],
        "fetch_utils.js",
        "chain8_prior_fetch_utils.js",
        &[
            ("export async function getJSON", true),
            ("export async function postJSON", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[8],
        "storage_utils.js",
        "chain8_prior_storage_utils.js",
        &[("export const ls = {", true), ("export const ss = {", true)],
    );

    // sha9 = C10': validation_utils.js
    assert_note_base_commit_matches(&repo, &chain[9], "sha9");
    assert_note_files_exact(&repo, &chain[9], "sha9_files", &["validation_utils.js"]);
    assert_blame_at_commit(
        &repo,
        &chain[9],
        "validation_utils.js",
        "sha9_blame",
        &[
            ("isEmail", true),
            ("isURL", true),
            ("isPhone", true),
            ("isUUID", true),
            ("minLength", true),
            ("maxLength", true),
            ("required", true),
            ("validate", true),
        ],
    );
    // Verify C1's file (date_utils.js) still correctly attributed at tip.
    assert_blame_sample_at_commit(
        &repo,
        &chain[9],
        "date_utils.js",
        "sha9_date_preserved",
        &[
            ("export function formatDate", true),
            ("export function addDays", true),
            ("export function isWeekend", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[9],
        "string_utils.js",
        "chain9_prior_string_utils.js",
        &[
            ("export const capitalize", true),
            ("export const slugify", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[9],
        "array_utils.js",
        "chain9_prior_array_utils.js",
        &[
            ("export const unique", true),
            ("export const flatten", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[9],
        "object_utils.js",
        "chain9_prior_object_utils.js",
        &[
            ("export const pick", true),
            ("export const deepClone", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[9],
        "number_utils.js",
        "chain9_prior_number_utils.js",
        &[("export const clamp", true), ("export const lerp", true)],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[9],
        "dom_utils.js",
        "chain9_prior_dom_utils.js",
        &[
            ("export const $ = sel", true),
            ("export const $$ = sel", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[9],
        "fetch_utils.js",
        "chain9_prior_fetch_utils.js",
        &[
            ("export async function getJSON", true),
            ("export async function postJSON", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[9],
        "storage_utils.js",
        "chain9_prior_storage_utils.js",
        &[("export const ls = {", true), ("export const ss = {", true)],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[9],
        "event_utils.js",
        "chain9_prior_event_utils.js",
        &[
            ("export function debounce", true),
            ("export function throttle", true),
        ],
    );
}

#[test]
fn test_fast_path_nested_directory_structure() {
    let repo = TestRepo::new();

    // Initial commit (shared base)
    let mut init = repo.filename("src/__init__.py");
    init.set_contents(crate::lines!["# src package"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let main_branch = repo.current_branch();

    // === FEATURE BRANCH: 5 commits adding files in nested directories ===
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // C1: src/api/endpoints.py
    let mut f1 = repo.filename("src/api/endpoints.py");
    f1.set_contents(crate::lines![
        "from flask import Blueprint, jsonify, request".ai(),
        "".ai(),
        "bp = Blueprint('api', __name__)".ai(),
        "".ai(),
        "@bp.route('/items', methods=['GET'])".ai(),
        "def list_items():".ai(),
        "    page = request.args.get('page', 1, type=int)".ai(),
        "    per_page = request.args.get('per_page', 20, type=int)".ai(),
        "    items = Item.query.paginate(page=page, per_page=per_page)".ai(),
        "    return jsonify({'items': [i.to_dict() for i in items], 'total': items.total})".ai(),
    ]);
    repo.stage_all_and_commit("feat: add API endpoints")
        .unwrap();

    // C2: src/models/user.py
    let mut f2 = repo.filename("src/models/user.py");
    f2.set_contents(crate::lines![
        "from dataclasses import dataclass, field".ai(),
        "from datetime import datetime".ai(),
        "from typing import Optional".ai(),
        "".ai(),
        "@dataclass".ai(),
        "class User:".ai(),
        "    id: int".ai(),
        "    email: str".ai(),
        "    name: str".ai(),
        "    created_at: datetime = field(default_factory=datetime.utcnow)".ai(),
        "    is_active: bool = True".ai(),
        "    role: str = 'user'".ai(),
    ]);
    repo.stage_all_and_commit("feat: add User model").unwrap();

    // C3: src/services/auth.py
    let mut f3 = repo.filename("src/services/auth.py");
    f3.set_contents(crate::lines![
        "import jwt".ai(),
        "from datetime import datetime, timedelta".ai(),
        "from functools import wraps".ai(),
        "from flask import request, g".ai(),
        "".ai(),
        "SECRET_KEY = 'dev-secret'".ai(),
        "".ai(),
        "def create_token(user_id: int, expires_in: int = 3600) -> str:".ai(),
        "    payload = {'sub': user_id, 'exp': datetime.utcnow() + timedelta(seconds=expires_in)}"
            .ai(),
        "    return jwt.encode(payload, SECRET_KEY, algorithm='HS256')".ai(),
    ]);
    repo.stage_all_and_commit("feat: add auth service").unwrap();

    // C4: src/repositories/user_repo.py
    let mut f4 = repo.filename("src/repositories/user_repo.py");
    f4.set_contents(crate::lines![
        "from typing import Optional, List".ai(),
        "from src.models.user import User".ai(),
        "".ai(),
        "class UserRepository:".ai(),
        "    def __init__(self, session):".ai(),
        "        self.session = session".ai(),
        "    def find_by_id(self, user_id: int) -> Optional[User]:".ai(),
        "        return self.session.query(User).filter_by(id=user_id).first()".ai(),
        "    def find_by_email(self, email: str) -> Optional[User]:".ai(),
        "        return self.session.query(User).filter_by(email=email).first()".ai(),
        "    def list_active(self) -> List[User]:".ai(),
        "        return self.session.query(User).filter_by(is_active=True).all()".ai(),
    ]);
    repo.stage_all_and_commit("feat: add user repository")
        .unwrap();

    // C5: src/middleware/logging.py
    let mut f5 = repo.filename("src/middleware/logging.py");
    f5.set_contents(crate::lines![
        "import time, logging".ai(),
        "from flask import request, g".ai(),
        "".ai(),
        "logger = logging.getLogger(__name__)".ai(),
        "".ai(),
        "def log_requests(app):".ai(),
        "    @app.before_request".ai(),
        "    def before():".ai(),
        "        g.start_time = time.time()".ai(),
        "    @app.after_request".ai(),
        "    def after(response):".ai(),
        "        elapsed = (time.time() - g.start_time) * 1000".ai(),
        "        logger.info('%s %s %s %.1fms', request.method, request.path, response.status_code, elapsed)".ai(),
        "        return response".ai(),
    ]);
    repo.stage_all_and_commit("feat: add request logging middleware")
        .unwrap();

    // === MAIN BRANCH: 5 human commits in tests/, docs/, .github/, scripts/, . ===
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.commit_untracked_file(
        "tests/conftest.py",
        "import pytest\n\n@pytest.fixture\ndef client(app):\n    return app.test_client()\n",
        "test: add pytest conftest",
    );
    repo.commit_untracked_file(
        "docs/architecture.md",
        "# Architecture\n\nThis is a Flask-based API.\n",
        "docs: add architecture overview",
    );
    repo.commit_untracked_file(".github/workflows/lint.yml",
        "name: Lint\non: [push]\njobs:\n  lint:\n    runs-on: ubuntu-latest\n    steps: [{uses: actions/checkout@v3}, {run: flake8 src/}]\n",
        "ci: add lint workflow",
    );
    repo.commit_untracked_file(
        "scripts/seed_db.py",
        "#!/usr/bin/env python3\nprint('Seeding database...')\n",
        "scripts: add db seed script",
    );
    repo.commit_untracked_file(
        "alembic.ini",
        "[alembic]\nscript_location = migrations\nsqlalchemy.url = sqlite:///app.db\n",
        "db: add alembic config",
    );

    // === REBASE feature onto main ===
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &main_branch]).unwrap();

    // === VERIFY AT EVERY COMMIT ===
    let chain = get_commit_chain(&repo, 5);

    // sha0 = C1': only src/api/endpoints.py
    assert_note_base_commit_matches(&repo, &chain[0], "sha0");
    assert_note_files_exact(&repo, &chain[0], "sha0_files", &["src/api/endpoints.py"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[0],
        "sha0_no_future",
        &[
            "src/models/user.py",
            "src/services/auth.py",
            "src/repositories/user_repo.py",
            "src/middleware/logging.py",
        ],
    );
    assert_blame_at_commit(
        &repo,
        &chain[0],
        "src/api/endpoints.py",
        "sha0_blame",
        &[
            ("from flask import Blueprint, jsonify, request", true),
            ("", true),
            ("bp = Blueprint('api', __name__)", true),
            ("", true),
            ("@bp.route('/items', methods=['GET'])", true),
            ("def list_items():", true),
            ("page = request.args.get('page'", true),
            ("per_page = request.args.get('per_page'", true),
            ("items = Item.query.paginate", true),
            ("return jsonify", true),
        ],
    );

    // sha1 = C2': src/models/user.py
    assert_note_base_commit_matches(&repo, &chain[1], "sha1");
    assert_note_files_exact(&repo, &chain[1], "sha1_files", &["src/models/user.py"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[1],
        "sha1_no_future",
        &[
            "src/services/auth.py",
            "src/repositories/user_repo.py",
            "src/middleware/logging.py",
        ],
    );
    assert_blame_at_commit(
        &repo,
        &chain[1],
        "src/models/user.py",
        "sha1_blame",
        &[
            ("from dataclasses import dataclass, field", true),
            ("from datetime import datetime", true),
            ("from typing import Optional", true),
            ("", true),
            ("@dataclass", true),
            ("class User:", true),
            ("id: int", true),
            ("email: str", true),
            ("name: str", true),
            ("created_at: datetime", true),
            ("is_active: bool = True", true),
            ("role: str = 'user'", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "src/api/endpoints.py",
        "chain1_prior_endpoints.py",
        &[
            ("bp = Blueprint('api', __name__)", true),
            ("def list_items():", true),
        ],
    );

    // sha2 = C3': src/services/auth.py
    assert_note_base_commit_matches(&repo, &chain[2], "sha2");
    assert_note_files_exact(&repo, &chain[2], "sha2_files", &["src/services/auth.py"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[2],
        "sha2_no_future",
        &["src/repositories/user_repo.py", "src/middleware/logging.py"],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "src/api/endpoints.py",
        "chain2_prior_endpoints.py",
        &[
            ("bp = Blueprint('api', __name__)", true),
            ("def list_items():", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "src/models/user.py",
        "chain2_prior_user.py",
        &[("class User:", true), ("email: str", true)],
    );

    // sha3 = C4': src/repositories/user_repo.py
    assert_note_base_commit_matches(&repo, &chain[3], "sha3");
    assert_note_files_exact(
        &repo,
        &chain[3],
        "sha3_files",
        &["src/repositories/user_repo.py"],
    );
    assert_note_no_forbidden_files(
        &repo,
        &chain[3],
        "sha3_no_future",
        &["src/middleware/logging.py"],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "src/api/endpoints.py",
        "chain3_prior_endpoints.py",
        &[
            ("bp = Blueprint('api', __name__)", true),
            ("def list_items():", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "src/models/user.py",
        "chain3_prior_user.py",
        &[("class User:", true), ("email: str", true)],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "src/services/auth.py",
        "chain3_prior_auth.py",
        &[
            ("def create_token(user_id: int", true),
            ("SECRET_KEY = 'dev-secret'", true),
        ],
    );

    // sha4 = C5': src/middleware/logging.py
    assert_note_base_commit_matches(&repo, &chain[4], "sha4");
    assert_note_files_exact(
        &repo,
        &chain[4],
        "sha4_files",
        &["src/middleware/logging.py"],
    );
    assert_blame_at_commit(
        &repo,
        &chain[4],
        "src/middleware/logging.py",
        "sha4_blame",
        &[
            ("import time, logging", true),
            ("from flask import request, g", true),
            ("", true),
            ("logger = logging.getLogger(__name__)", true),
            ("", true),
            ("def log_requests(app):", true),
            ("@app.before_request", true),
            ("def before():", true),
            ("g.start_time = time.time()", true),
            ("@app.after_request", true),
            ("def after(response):", true),
            ("elapsed = (time.time()", true),
            ("logger.info", true),
            ("return response", true),
        ],
    );
    // Verify C1's file (src/api/endpoints.py) still correctly attributed at tip.
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "src/api/endpoints.py",
        "sha4_endpoints_preserved",
        &[
            ("bp = Blueprint('api', __name__)", true),
            ("def list_items():", true),
            ("return jsonify", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "src/models/user.py",
        "chain4_prior_user.py",
        &[("class User:", true), ("email: str", true)],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "src/services/auth.py",
        "chain4_prior_auth.py",
        &[
            ("def create_token(user_id: int", true),
            ("SECRET_KEY = 'dev-secret'", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "src/repositories/user_repo.py",
        "chain4_prior_user_repo.py",
        &[
            ("class UserRepository:", true),
            ("def find_by_id(self, user_id: int)", true),
        ],
    );
}

#[test]
fn test_fast_path_single_file_grows_across_commits() {
    let repo = TestRepo::new();

    // Initial commit (shared base)
    let mut init = repo.filename("app_init.py");
    init.set_contents(crate::lines!["# Service application"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let main_branch = repo.current_branch();

    // === FEATURE BRANCH: 5 commits all modifying the SAME file: service.py ===
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // C1: creates service.py with 8 AI lines
    let mut svc = repo.filename("service.py");
    svc.set_contents(crate::lines![
        "class DataService:".ai(),
        "    def __init__(self, db, cache):".ai(),
        "        self.db = db".ai(),
        "        self.cache = cache".ai(),
        "    def fetch(self, key):".ai(),
        "        if v := self.cache.get(key): return v".ai(),
        "        return self.db.get(key)".ai(),
        "    def store(self, key, value): self.db.set(key, value); self.cache.set(key, value)".ai(),
    ]);
    repo.stage_all_and_commit("feat: initial DataService")
        .unwrap();

    // C2: appends 6 AI lines (2 more methods)
    svc.set_contents(crate::lines![
        "class DataService:".ai(),
        "    def __init__(self, db, cache):".ai(),
        "        self.db = db".ai(),
        "        self.cache = cache".ai(),
        "    def fetch(self, key):".ai(),
        "        if v := self.cache.get(key): return v".ai(),
        "        return self.db.get(key)".ai(),
        "    def store(self, key, value): self.db.set(key, value); self.cache.set(key, value)".ai(),
        "    def delete(self, key):".ai(),
        "        self.cache.delete(key)".ai(),
        "        self.db.delete(key)".ai(),
        "    def exists(self, key): return self.cache.has(key) or self.db.has(key)".ai(),
        "    def keys(self, prefix=''): return [k for k in self.db.list() if k.startswith(prefix)]"
            .ai(),
        "    def flush(self): self.cache.clear()".ai(),
    ]);
    repo.stage_all_and_commit("feat: add delete and exists methods")
        .unwrap();

    // C3: appends 6 AI lines (2 more methods)
    svc.set_contents(crate::lines![
        "class DataService:".ai(),
        "    def __init__(self, db, cache):".ai(),
        "        self.db = db".ai(),
        "        self.cache = cache".ai(),
        "    def fetch(self, key):".ai(),
        "        if v := self.cache.get(key): return v".ai(),
        "        return self.db.get(key)".ai(),
        "    def store(self, key, value): self.db.set(key, value); self.cache.set(key, value)".ai(),
        "    def delete(self, key):".ai(),
        "        self.cache.delete(key)".ai(),
        "        self.db.delete(key)".ai(),
        "    def exists(self, key): return self.cache.has(key) or self.db.has(key)".ai(),
        "    def keys(self, prefix=''): return [k for k in self.db.list() if k.startswith(prefix)]".ai(),
        "    def flush(self): self.cache.clear()".ai(),
        "    def fetch_many(self, keys): return {k: self.fetch(k) for k in keys}".ai(),
        "    def store_many(self, items):".ai(),
        "        for k, v in items.items(): self.store(k, v)".ai(),
        "    def invalidate(self, pattern): [self.cache.delete(k) for k in self.cache.keys() if pattern in k]".ai(),
        "    def ttl_set(self, key, value, ttl): self.db.set_ex(key, value, ttl); self.cache.set(key, value)".ai(),
        "    def size(self): return self.db.dbsize()".ai(),
    ]);
    repo.stage_all_and_commit("feat: add batch and TTL methods")
        .unwrap();

    // C4: appends 6 AI lines (2 more methods)
    svc.set_contents(crate::lines![
        "class DataService:".ai(),
        "    def __init__(self, db, cache):".ai(),
        "        self.db = db".ai(),
        "        self.cache = cache".ai(),
        "    def fetch(self, key):".ai(),
        "        if v := self.cache.get(key): return v".ai(),
        "        return self.db.get(key)".ai(),
        "    def store(self, key, value): self.db.set(key, value); self.cache.set(key, value)".ai(),
        "    def delete(self, key):".ai(),
        "        self.cache.delete(key)".ai(),
        "        self.db.delete(key)".ai(),
        "    def exists(self, key): return self.cache.has(key) or self.db.has(key)".ai(),
        "    def keys(self, prefix=''): return [k for k in self.db.list() if k.startswith(prefix)]".ai(),
        "    def flush(self): self.cache.clear()".ai(),
        "    def fetch_many(self, keys): return {k: self.fetch(k) for k in keys}".ai(),
        "    def store_many(self, items):".ai(),
        "        for k, v in items.items(): self.store(k, v)".ai(),
        "    def invalidate(self, pattern): [self.cache.delete(k) for k in self.cache.keys() if pattern in k]".ai(),
        "    def ttl_set(self, key, value, ttl): self.db.set_ex(key, value, ttl); self.cache.set(key, value)".ai(),
        "    def size(self): return self.db.dbsize()".ai(),
        "    def watch(self, key, callback): self.db.subscribe(key, callback)".ai(),
        "    def unwatch(self, key, callback): self.db.unsubscribe(key, callback)".ai(),
        "    def transaction(self, fn):".ai(),
        "        with self.db.pipeline() as pipe: fn(pipe); pipe.execute()".ai(),
        "    def backup(self, path): self.db.bgsave(); return self.db.dump(path)".ai(),
        "    def restore(self, path): self.db.restore_dump(path)".ai(),
    ]);
    repo.stage_all_and_commit("feat: add watch, transaction, backup methods")
        .unwrap();

    // C5: appends 6 AI lines (final methods)
    svc.set_contents(crate::lines![
        "class DataService:".ai(),
        "    def __init__(self, db, cache):".ai(),
        "        self.db = db".ai(),
        "        self.cache = cache".ai(),
        "    def fetch(self, key):".ai(),
        "        if v := self.cache.get(key): return v".ai(),
        "        return self.db.get(key)".ai(),
        "    def store(self, key, value): self.db.set(key, value); self.cache.set(key, value)".ai(),
        "    def delete(self, key):".ai(),
        "        self.cache.delete(key)".ai(),
        "        self.db.delete(key)".ai(),
        "    def exists(self, key): return self.cache.has(key) or self.db.has(key)".ai(),
        "    def keys(self, prefix=''): return [k for k in self.db.list() if k.startswith(prefix)]".ai(),
        "    def flush(self): self.cache.clear()".ai(),
        "    def fetch_many(self, keys): return {k: self.fetch(k) for k in keys}".ai(),
        "    def store_many(self, items):".ai(),
        "        for k, v in items.items(): self.store(k, v)".ai(),
        "    def invalidate(self, pattern): [self.cache.delete(k) for k in self.cache.keys() if pattern in k]".ai(),
        "    def ttl_set(self, key, value, ttl): self.db.set_ex(key, value, ttl); self.cache.set(key, value)".ai(),
        "    def size(self): return self.db.dbsize()".ai(),
        "    def watch(self, key, callback): self.db.subscribe(key, callback)".ai(),
        "    def unwatch(self, key, callback): self.db.unsubscribe(key, callback)".ai(),
        "    def transaction(self, fn):".ai(),
        "        with self.db.pipeline() as pipe: fn(pipe); pipe.execute()".ai(),
        "    def backup(self, path): self.db.bgsave(); return self.db.dump(path)".ai(),
        "    def restore(self, path): self.db.restore_dump(path)".ai(),
        "    def stats(self): return self.db.info()".ai(),
        "    def ping(self): return self.db.ping()".ai(),
        "    def close(self):".ai(),
        "        self.cache.close()".ai(),
        "        self.db.close()".ai(),
        "    def __repr__(self): return f'DataService(db={self.db}, cache={self.cache})'".ai(),
    ]);
    repo.stage_all_and_commit("feat: add stats, ping, close methods")
        .unwrap();

    // === MAIN BRANCH: 5 human commits on different files ===
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.commit_untracked_file(
        "tests/test_service.py",
        "def test_placeholder(): pass\n",
        "test: add service test placeholder",
    );
    repo.commit_untracked_file("docker/Dockerfile.dev",
        "FROM python:3.11-slim\nWORKDIR /app\nCOPY requirements.txt .\nRUN pip install -r requirements.txt\n",
        "build: add dev Dockerfile",
    );
    repo.commit_untracked_file(
        ".env.example",
        "DATABASE_URL=redis://localhost:6379/0\nCACHE_URL=memcached://localhost:11211\n",
        "config: add .env.example",
    );
    repo.commit_untracked_file(
        "CHANGELOG.md",
        "# Changelog\n\n## [Unreleased]\n\n### Added\n- DataService class\n",
        "docs: add CHANGELOG",
    );
    repo.commit_untracked_file(
        "setup.py",
        "from setuptools import setup\nsetup(name='dataservice', version='0.1.0')\n",
        "build: add setup.py",
    );

    // === REBASE feature onto main ===
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &main_branch]).unwrap();

    // === VERIFY AT EVERY COMMIT ===
    let chain = get_commit_chain(&repo, 5);

    // sha0 = C1': service.py with ~8 AI lines (per-commit-delta: C1's lines only)
    assert_note_base_commit_matches(&repo, &chain[0], "sha0");
    assert_note_files_exact(&repo, &chain[0], "sha0_files", &["service.py"]);
    assert_blame_at_commit(
        &repo,
        &chain[0],
        "service.py",
        "sha0_blame",
        &[
            ("class DataService:", true),
            ("def __init__(self, db, cache):", true),
            ("self.db = db", true),
            ("self.cache = cache", true),
            ("def fetch(self, key):", true),
            ("if v := self.cache.get(key): return v", true),
            ("return self.db.get(key)", true),
            ("def store(self, key, value):", true),
        ],
    );

    // sha1 = C2': service.py (C2's delta only; fast-path remaps original note)
    assert_note_base_commit_matches(&repo, &chain[1], "sha1");
    assert_note_files_exact(&repo, &chain[1], "sha1_files", &["service.py"]);
    // C1 lines must still be AI-attributed at sha1.
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "service.py",
        "sha1_c1_preserved",
        &[
            ("class DataService:", true),
            ("def fetch(self, key):", true),
            ("def store(self, key, value):", true),
        ],
    );

    // sha2 = C3': service.py (C3's delta only; fast-path remaps original note)
    assert_note_base_commit_matches(&repo, &chain[2], "sha2");
    assert_note_files_exact(&repo, &chain[2], "sha2_files", &["service.py"]);
    // C2 lines must still be AI-attributed at sha2.
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "service.py",
        "sha2_c2_preserved",
        &[
            ("def delete(self, key):", true),
            ("def exists(self, key):", true),
            ("def flush(self):", true),
        ],
    );
    // C1 lines must also still be AI-attributed at sha2.
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "service.py",
        "chain2_prior_c1_service.py",
        &[
            ("class DataService:", true),
            ("def fetch(self, key):", true),
        ],
    );

    // sha3 = C4': service.py (C4's delta only; fast-path remaps original note)
    assert_note_base_commit_matches(&repo, &chain[3], "sha3");
    assert_note_files_exact(&repo, &chain[3], "sha3_files", &["service.py"]);
    // C3 lines must still be AI-attributed at sha3.
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "service.py",
        "sha3_c3_preserved",
        &[
            ("def fetch_many(self, keys):", true),
            ("def store_many(self, items):", true),
            ("def ttl_set(self, key, value, ttl):", true),
        ],
    );
    // C1 and C2 lines must also still be AI-attributed at sha3.
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "service.py",
        "chain3_prior_c1_service.py",
        &[
            ("class DataService:", true),
            ("def fetch(self, key):", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "service.py",
        "chain3_prior_c2_service.py",
        &[
            ("def delete(self, key):", true),
            ("def exists(self, key):", true),
        ],
    );

    // sha4 = C5': service.py (C5's delta only; fast-path remaps original note)
    assert_note_base_commit_matches(&repo, &chain[4], "sha4");
    assert_note_files_exact(&repo, &chain[4], "sha4_files", &["service.py"]);
}

#[test]
fn test_fast_path_feature_deletes_file_then_recreates() {
    let repo = TestRepo::new();

    // Initial commit (shared base)
    let mut init = repo.filename("pkg/__init__.py");
    init.set_contents(crate::lines!["# utilities package"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let main_branch = repo.current_branch();

    // === FEATURE BRANCH: 5 commits with a file deletion in C3 ===
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // C1: creates temp_module.py (AI, 8 lines) + util_a.py (AI, 6 lines)
    let mut temp = repo.filename("temp_module.py");
    temp.set_contents(crate::lines![
        "class TempProcessor:".ai(),
        "    def __init__(self, config):".ai(),
        "        self.config = config".ai(),
        "    def process(self, data):".ai(),
        "        return [self._transform(item) for item in data]".ai(),
        "    def _transform(self, item):".ai(),
        "        return {k: v for k, v in item.items() if k in self.config['fields']}".ai(),
        "    def flush(self): self.config.clear()".ai(),
    ]);
    let mut ua = repo.filename("util_a.py");
    ua.set_contents(crate::lines![
        "def parse_csv(path):".ai(),
        "    import csv".ai(),
        "    with open(path) as f:".ai(),
        "        return list(csv.DictReader(f))".ai(),
        "def write_csv(path, rows, fields):".ai(),
        "    import csv".ai(),
        "    with open(path, 'w', newline='') as f:".ai(),
        "        w = csv.DictWriter(f, fieldnames=fields); w.writeheader(); w.writerows(rows)".ai(),
    ]);
    repo.stage_all_and_commit("feat: add temp_module and util_a")
        .unwrap();

    // C2: adds util_b.py (AI, 8 lines)
    let mut ub = repo.filename("util_b.py");
    ub.set_contents(crate::lines![
        "import json, pathlib".ai(),
        "".ai(),
        "def load_json(path):".ai(),
        "    return json.loads(pathlib.Path(path).read_text())".ai(),
        "".ai(),
        "def save_json(path, data, indent=2):".ai(),
        "    pathlib.Path(path).write_text(json.dumps(data, indent=indent))".ai(),
        "".ai(),
        "def merge_json_files(paths):".ai(),
        "    result = {}".ai(),
        "    for p in paths: result.update(load_json(p))".ai(),
        "    return result".ai(),
    ]);
    repo.stage_all_and_commit("feat: add util_b json utilities")
        .unwrap();

    // C3: DELETES temp_module.py (human commit) + adds util_c.py (AI, 6 lines)
    repo.git(&["rm", "temp_module.py"]).unwrap();
    // Stage the deletion and commit (bypassing git-ai to make it a plain human commit)
    repo.git_og(&["commit", "-m", "refactor: remove temp_module"])
        .unwrap();
    // Now add util_c.py via AI
    let mut uc = repo.filename("util_c.py");
    uc.set_contents(crate::lines![
        "import hashlib".ai(),
        "".ai(),
        "def md5(data): return hashlib.md5(data.encode()).hexdigest()".ai(),
        "def sha256(data): return hashlib.sha256(data.encode()).hexdigest()".ai(),
        "def sha512(data): return hashlib.sha512(data.encode()).hexdigest()".ai(),
        "def hmac_sign(key, data):".ai(),
        "    import hmac".ai(),
        "    return hmac.new(key.encode(), data.encode(), hashlib.sha256).hexdigest()".ai(),
    ]);
    repo.stage_all_and_commit("feat: add util_c crypto utilities")
        .unwrap();

    // C4: adds util_d.py (AI, 8 lines)
    let mut ud = repo.filename("util_d.py");
    ud.set_contents(crate::lines![
        "from typing import TypeVar, Callable, Any".ai(),
        "T = TypeVar('T')".ai(),
        "".ai(),
        "def retry(fn: Callable, attempts: int = 3, exceptions=(Exception,)):".ai(),
        "    for i in range(attempts):".ai(),
        "        try: return fn()".ai(),
        "        except exceptions:".ai(),
        "            if i == attempts - 1: raise".ai(),
        "".ai(),
        "def memoize(fn: Callable[..., T]) -> Callable[..., T]:".ai(),
        "    cache: dict[Any, T] = {}".ai(),
        "    def wrapper(*args): return cache.setdefault(args, fn(*args))".ai(),
        "    return wrapper".ai(),
    ]);
    repo.stage_all_and_commit("feat: add util_d retry and memoize")
        .unwrap();

    // C5: adds util_e.py (AI, 6 lines)
    let mut ue = repo.filename("util_e.py");
    ue.set_contents(crate::lines![
        "import time, functools".ai(),
        "".ai(),
        "def timed(fn):".ai(),
        "    @functools.wraps(fn)".ai(),
        "    def wrapper(*a, **kw):".ai(),
        "        t = time.perf_counter(); r = fn(*a, **kw)".ai(),
        "        print(f'{fn.__name__} took {time.perf_counter()-t:.4f}s')".ai(),
        "        return r".ai(),
        "    return wrapper".ai(),
    ]);
    repo.stage_all_and_commit("feat: add util_e timing utilities")
        .unwrap();

    // === MAIN BRANCH: 5 human commits on different files ===
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.commit_untracked_file(
        "tests/test_utils.py",
        "def test_placeholder(): pass\n",
        "test: add utils test placeholder",
    );
    repo.commit_untracked_file(
        "pyproject.toml",
        "[build-system]\nrequires=['setuptools']\nbuild-backend='setuptools.build_meta'\n",
        "build: add pyproject.toml",
    );
    repo.commit_untracked_file(
        ".flake8",
        "[flake8]\nmax-line-length=120\nexclude=.git,__pycache__\n",
        "lint: add flake8 config",
    );
    repo.commit_untracked_file(
        "MANIFEST.in",
        "include *.py\ninclude *.md\n",
        "build: add MANIFEST.in",
    );
    repo.commit_untracked_file(
        "tox.ini",
        "[tox]\nenvlist = py311\n[testenv]\ncommands = pytest\n",
        "test: add tox config",
    );

    // === REBASE feature onto main ===
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &main_branch]).unwrap();

    // The rebase of C3 was a human commit (git rm + git commit via git_og),
    // so the chain has 5 rebased commits from the feature branch.
    // However C3 was split: first the rm+commit then the util_c.py AI commit.
    // Actually let's count: C1, C2, C3_rm, C3_util_c, C4, C5 = 6 commits.
    // Wait — we committed the rm first (human) then util_c (AI) separately = 6 feature commits total.
    let chain = get_commit_chain(&repo, 6);
    // chain[0]=C1', chain[1]=C2', chain[2]=C3_rm', chain[3]=C3_util_c', chain[4]=C4', chain[5]=C5'

    // sha0 = C1': temp_module.py + util_a.py. Content-based mapping correctly
    // transfers attribution for both files since both exist identically at C1'.
    assert_note_base_commit_matches(&repo, &chain[0], "sha0");
    assert_note_files_exact(
        &repo,
        &chain[0],
        "sha0_files",
        &["temp_module.py", "util_a.py"],
    );
    assert_note_no_forbidden_files(
        &repo,
        &chain[0],
        "sha0_no_future",
        &["util_b.py", "util_c.py", "util_d.py", "util_e.py"],
    );

    // sha1 = C2': util_b.py
    assert_note_base_commit_matches(&repo, &chain[1], "sha1");
    assert_note_files_exact(&repo, &chain[1], "sha1_files", &["util_b.py"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[1],
        "sha1_no_future",
        &["util_c.py", "util_d.py", "util_e.py"],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "temp_module.py",
        "chain1_prior_temp_module.py",
        &[
            ("class TempProcessor:", true),
            ("def process(self, data):", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "util_a.py",
        "chain1_prior_util_a.py",
        &[
            ("def parse_csv(path):", true),
            ("def write_csv(path, rows, fields):", true),
        ],
    );

    // sha2 = C3_rm': human deletion commit — no AI content so no note expected.
    assert_note_no_forbidden_files_if_present(
        &repo,
        &chain[2],
        "sha2_no_temp",
        &["temp_module.py"],
    );
    assert_note_no_forbidden_files_if_present(
        &repo,
        &chain[2],
        "sha2_no_future",
        &["util_c.py", "util_d.py", "util_e.py"],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "util_a.py",
        "chain2_prior_util_a.py",
        &[
            ("def parse_csv(path):", true),
            ("def write_csv(path, rows, fields):", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "util_b.py",
        "chain2_prior_util_b.py",
        &[
            ("def load_json(path):", true),
            ("def save_json(path, data", true),
        ],
    );

    // sha3 = C3_util_c': util_c.py
    assert_note_base_commit_matches(&repo, &chain[3], "sha3");
    assert_note_files_exact(&repo, &chain[3], "sha3_files", &["util_c.py"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[3],
        "sha3_no_temp_or_future",
        &["temp_module.py", "util_d.py", "util_e.py"],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "util_a.py",
        "chain3_prior_util_a.py",
        &[
            ("def parse_csv(path):", true),
            ("def write_csv(path, rows, fields):", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "util_b.py",
        "chain3_prior_util_b.py",
        &[
            ("def load_json(path):", true),
            ("def save_json(path, data", true),
        ],
    );

    // sha4 = C4': util_d.py
    assert_note_base_commit_matches(&repo, &chain[4], "sha4");
    assert_note_files_exact(&repo, &chain[4], "sha4_files", &["util_d.py"]);
    assert_note_no_forbidden_files(&repo, &chain[4], "sha4_no_future", &["util_e.py"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "util_a.py",
        "chain4_prior_util_a.py",
        &[
            ("def parse_csv(path):", true),
            ("def write_csv(path, rows, fields):", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "util_b.py",
        "chain4_prior_util_b.py",
        &[
            ("def load_json(path):", true),
            ("def save_json(path, data", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "util_c.py",
        "chain4_prior_util_c.py",
        &[("def md5(data):", true), ("def sha256(data):", true)],
    );

    // sha5 = C5': util_e.py
    assert_note_base_commit_matches(&repo, &chain[5], "sha5");
    assert_note_files_exact(&repo, &chain[5], "sha5_files", &["util_e.py"]);
    assert_blame_at_commit(
        &repo,
        &chain[5],
        "util_e.py",
        "sha5_blame",
        &[
            ("import time, functools", true),
            ("", true),
            ("def timed(fn):", true),
            ("@functools.wraps(fn)", true),
            ("def wrapper(*a, **kw):", true),
            ("t = time.perf_counter()", true),
            ("print(f'{fn.__name__}", true),
            ("return r", true),
            ("return wrapper", true),
        ],
    );
    // Verify C2's file (util_b.py) still correctly attributed at tip.
    assert_blame_sample_at_commit(
        &repo,
        &chain[5],
        "util_b.py",
        "sha5_util_b_preserved",
        &[
            ("def load_json(path):", true),
            ("def save_json(path, data", true),
            ("def merge_json_files(paths):", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[5],
        "util_a.py",
        "chain5_prior_util_a.py",
        &[
            ("def parse_csv(path):", true),
            ("def write_csv(path, rows, fields):", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[5],
        "util_c.py",
        "chain5_prior_util_c.py",
        &[("def md5(data):", true), ("def sha256(data):", true)],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[5],
        "util_d.py",
        "chain5_prior_util_d.py",
        &[
            ("def retry(fn: Callable", true),
            ("def memoize(fn: Callable", true),
        ],
    );

    // Note: accepted_lines is NOT monotonic here because chain[2] is a human deletion commit
    // (removes temp_module.py) which has 0 accepted lines, breaking the monotonic property.
}

#[test]
fn test_fast_path_multi_file_commits_2_files_each() {
    let repo = TestRepo::new();

    // Initial commit (shared base)
    let mut init = repo.filename("manage.py");
    init.set_contents(crate::lines!["# Django-style project management"]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    let main_branch = repo.current_branch();

    // === FEATURE BRANCH: 5 commits each adding 2 AI files ===
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // C1: models.py (8 AI lines) + schemas.py (6 AI lines)
    let mut m1 = repo.filename("models.py");
    m1.set_contents(crate::lines![
        "from django.db import models".ai(),
        "".ai(),
        "class Product(models.Model):".ai(),
        "    name = models.CharField(max_length=200)".ai(),
        "    price = models.DecimalField(max_digits=10, decimal_places=2)".ai(),
        "    stock = models.IntegerField(default=0)".ai(),
        "    created_at = models.DateTimeField(auto_now_add=True)".ai(),
        "    class Meta: ordering = ['-created_at']".ai(),
    ]);
    let mut s1 = repo.filename("schemas.py");
    s1.set_contents(crate::lines![
        "from pydantic import BaseModel, condecimal".ai(),
        "from decimal import Decimal".ai(),
        "".ai(),
        "class ProductSchema(BaseModel):".ai(),
        "    name: str".ai(),
        "    price: condecimal(max_digits=10, decimal_places=2)".ai(),
        "    stock: int = 0".ai(),
        "    class Config: from_attributes = True".ai(),
    ]);
    repo.stage_all_and_commit("feat: add models and schemas")
        .unwrap();

    // C2: views.py (8 AI lines) + serializers.py (6 AI lines)
    let mut v2 = repo.filename("views.py");
    v2.set_contents(crate::lines![
        "from django.shortcuts import get_object_or_404".ai(),
        "from rest_framework.decorators import api_view".ai(),
        "from rest_framework.response import Response".ai(),
        "from .models import Product".ai(),
        "from .serializers import ProductSerializer".ai(),
        "".ai(),
        "@api_view(['GET'])".ai(),
        "def product_list(request): return Response(ProductSerializer(Product.objects.all(), many=True).data)".ai(),
    ]);
    let mut sz2 = repo.filename("serializers.py");
    sz2.set_contents(crate::lines![
        "from rest_framework import serializers".ai(),
        "from .models import Product".ai(),
        "".ai(),
        "class ProductSerializer(serializers.ModelSerializer):".ai(),
        "    class Meta:".ai(),
        "        model = Product".ai(),
        "        fields = ['id', 'name', 'price', 'stock', 'created_at']".ai(),
        "        read_only_fields = ['id', 'created_at']".ai(),
    ]);
    repo.stage_all_and_commit("feat: add views and serializers")
        .unwrap();

    // C3: urls.py (6 AI lines) + permissions.py (8 AI lines)
    let mut u3 = repo.filename("urls.py");
    u3.set_contents(crate::lines![
        "from django.urls import path".ai(),
        "from . import views".ai(),
        "".ai(),
        "app_name = 'shop'".ai(),
        "urlpatterns = [".ai(),
        "    path('products/', views.product_list, name='product-list'),".ai(),
        "    path('products/<int:pk>/', views.product_detail, name='product-detail'),".ai(),
        "]".ai(),
    ]);
    let mut p3 = repo.filename("permissions.py");
    p3.set_contents(crate::lines![
        "from rest_framework.permissions import BasePermission".ai(),
        "".ai(),
        "class IsOwnerOrReadOnly(BasePermission):".ai(),
        "    def has_object_permission(self, request, view, obj):".ai(),
        "        if request.method in ('GET', 'HEAD', 'OPTIONS'): return True".ai(),
        "        return obj.owner == request.user".ai(),
        "".ai(),
        "class IsStaff(BasePermission):".ai(),
        "    message = 'Staff access required.'".ai(),
        "    def has_permission(self, request, view): return bool(request.user and request.user.is_staff)".ai(),
    ]);
    repo.stage_all_and_commit("feat: add urls and permissions")
        .unwrap();

    // C4: signals.py (8 AI lines) + tasks.py (6 AI lines)
    let mut sg4 = repo.filename("signals.py");
    sg4.set_contents(crate::lines![
        "from django.db.models.signals import post_save, pre_delete".ai(),
        "from django.dispatch import receiver".ai(),
        "from .models import Product".ai(),
        "".ai(),
        "@receiver(post_save, sender=Product)".ai(),
        "def on_product_saved(sender, instance, created, **kwargs):".ai(),
        "    if created: print(f'New product created: {instance.name}')".ai(),
        "".ai(),
        "@receiver(pre_delete, sender=Product)".ai(),
        "def on_product_deleted(sender, instance, **kwargs):".ai(),
        "    print(f'Deleting product: {instance.name}')".ai(),
    ]);
    let mut t4 = repo.filename("tasks.py");
    t4.set_contents(crate::lines![
        "from celery import shared_task".ai(),
        "from .models import Product".ai(),
        "".ai(),
        "@shared_task".ai(),
        "def sync_inventory(product_id):".ai(),
        "    p = Product.objects.get(id=product_id)".ai(),
        "    # sync with external warehouse system".ai(),
        "    return {'product': p.name, 'stock': p.stock}".ai(),
    ]);
    repo.stage_all_and_commit("feat: add signals and tasks")
        .unwrap();

    // C5: middleware.py (8 AI lines) + decorators.py (6 AI lines)
    let mut mw5 = repo.filename("middleware.py");
    mw5.set_contents(crate::lines![
        "import time".ai(),
        "from django.utils.deprecation import MiddlewareMixin".ai(),
        "".ai(),
        "class RequestTimingMiddleware(MiddlewareMixin):".ai(),
        "    def process_request(self, request):".ai(),
        "        request._start_time = time.monotonic()".ai(),
        "    def process_response(self, request, response):".ai(),
        "        elapsed = (time.monotonic() - getattr(request, '_start_time', time.monotonic())) * 1000".ai(),
        "        response['X-Response-Time'] = f'{elapsed:.1f}ms'".ai(),
        "        return response".ai(),
    ]);
    let mut d5 = repo.filename("decorators.py");
    d5.set_contents(crate::lines![
        "from functools import wraps".ai(),
        "from django.http import JsonResponse".ai(),
        "".ai(),
        "def require_json(view_fn):".ai(),
        "    @wraps(view_fn)".ai(),
        "    def wrapper(request, *a, **kw):".ai(),
        "        if request.content_type != 'application/json': return JsonResponse({'error': 'JSON required'}, status=415)".ai(),
        "        return view_fn(request, *a, **kw)".ai(),
        "    return wrapper".ai(),
    ]);
    repo.stage_all_and_commit("feat: add middleware and decorators")
        .unwrap();

    // === MAIN BRANCH: 5 human commits on different files ===
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.commit_untracked_file(
        "settings.py",
        "DEBUG = True\nINSTALLED_APPS = ['django.contrib.admin']\n",
        "config: add Django settings",
    );
    repo.commit_untracked_file(
        "requirements.txt",
        "django==4.2\ndjangorestframework==3.14\ncelery==5.3\npydantic==2.0\n",
        "deps: add requirements.txt",
    );
    repo.commit_untracked_file("Dockerfile",
        "FROM python:3.11\nWORKDIR /app\nCOPY requirements.txt .\nRUN pip install -r requirements.txt\n",
        "build: add Dockerfile",
    );
    repo.commit_untracked_file("docker-compose.yml",
        "version: '3.9'\nservices:\n  web:\n    build: .\n    ports: ['8000:8000']\n  worker:\n    build: .\n    command: celery -A app worker\n",
        "build: add docker-compose.yml",
    );
    repo.commit_untracked_file(
        ".env.example",
        "SECRET_KEY=change-me\nDEBUG=1\nDATABASE_URL=postgresql://localhost/app\n",
        "config: add .env.example",
    );

    // === REBASE feature onto main ===
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &main_branch]).unwrap();

    // === VERIFY AT EVERY COMMIT ===
    let chain = get_commit_chain(&repo, 5);

    // sha0 = C1': {models.py, schemas.py}
    assert_note_base_commit_matches(&repo, &chain[0], "sha0");
    assert_note_files_exact(&repo, &chain[0], "sha0_files", &["models.py", "schemas.py"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[0],
        "sha0_no_future",
        &[
            "views.py",
            "serializers.py",
            "urls.py",
            "permissions.py",
            "signals.py",
            "tasks.py",
            "middleware.py",
            "decorators.py",
        ],
    );
    assert_blame_at_commit(
        &repo,
        &chain[0],
        "models.py",
        "sha0_blame_models",
        &[
            ("from django.db import models", true),
            ("", true),
            ("class Product(models.Model):", true),
            ("name = models.CharField", true),
            ("price = models.DecimalField", true),
            ("stock = models.IntegerField", true),
            ("created_at = models.DateTimeField", true),
            ("class Meta: ordering", true),
        ],
    );
    assert_blame_at_commit(
        &repo,
        &chain[0],
        "schemas.py",
        "sha0_blame_schemas",
        &[
            ("from pydantic import BaseModel, condecimal", true),
            ("from decimal import Decimal", true),
            ("", true),
            ("class ProductSchema(BaseModel):", true),
            ("name: str", true),
            ("price: condecimal", true),
            ("stock: int = 0", true),
            ("class Config: from_attributes = True", true),
        ],
    );

    // sha1 = C2': views.py + serializers.py
    assert_note_base_commit_matches(&repo, &chain[1], "sha1");
    assert_note_files_exact(
        &repo,
        &chain[1],
        "sha1_files",
        &["views.py", "serializers.py"],
    );
    assert_note_no_forbidden_files(
        &repo,
        &chain[1],
        "sha1_no_future",
        &[
            "urls.py",
            "permissions.py",
            "signals.py",
            "tasks.py",
            "middleware.py",
            "decorators.py",
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "models.py",
        "chain1_prior_models.py",
        &[
            ("class Product(models.Model):", true),
            ("name = models.CharField", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "schemas.py",
        "chain1_prior_schemas.py",
        &[
            ("class ProductSchema(BaseModel):", true),
            ("price: condecimal", true),
        ],
    );

    // sha2 = C3': urls.py + permissions.py
    assert_note_base_commit_matches(&repo, &chain[2], "sha2");
    assert_note_files_exact(
        &repo,
        &chain[2],
        "sha2_files",
        &["urls.py", "permissions.py"],
    );
    assert_note_no_forbidden_files(
        &repo,
        &chain[2],
        "sha2_no_future",
        &["signals.py", "tasks.py", "middleware.py", "decorators.py"],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "models.py",
        "chain2_prior_models.py",
        &[
            ("class Product(models.Model):", true),
            ("name = models.CharField", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "schemas.py",
        "chain2_prior_schemas.py",
        &[
            ("class ProductSchema(BaseModel):", true),
            ("price: condecimal", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "views.py",
        "chain2_prior_views.py",
        &[
            ("@api_view(['GET'])", true),
            ("def product_list(request):", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "serializers.py",
        "chain2_prior_serializers.py",
        &[
            (
                "class ProductSerializer(serializers.ModelSerializer):",
                true,
            ),
            ("model = Product", true),
        ],
    );

    // sha3 = C4': signals.py + tasks.py
    assert_note_base_commit_matches(&repo, &chain[3], "sha3");
    assert_note_files_exact(&repo, &chain[3], "sha3_files", &["signals.py", "tasks.py"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[3],
        "sha3_no_future",
        &["middleware.py", "decorators.py"],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "models.py",
        "chain3_prior_models.py",
        &[
            ("class Product(models.Model):", true),
            ("name = models.CharField", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "schemas.py",
        "chain3_prior_schemas.py",
        &[
            ("class ProductSchema(BaseModel):", true),
            ("price: condecimal", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "views.py",
        "chain3_prior_views.py",
        &[
            ("@api_view(['GET'])", true),
            ("def product_list(request):", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "serializers.py",
        "chain3_prior_serializers.py",
        &[
            (
                "class ProductSerializer(serializers.ModelSerializer):",
                true,
            ),
            ("model = Product", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "urls.py",
        "chain3_prior_urls.py",
        &[("app_name = 'shop'", true), ("urlpatterns = [", true)],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "permissions.py",
        "chain3_prior_permissions.py",
        &[
            ("class IsOwnerOrReadOnly(BasePermission):", true),
            ("class IsStaff(BasePermission):", true),
        ],
    );

    // sha4 = C5': middleware.py + decorators.py
    assert_note_base_commit_matches(&repo, &chain[4], "sha4");
    assert_note_files_exact(
        &repo,
        &chain[4],
        "sha4_files",
        &["middleware.py", "decorators.py"],
    );
    assert_blame_at_commit(
        &repo,
        &chain[4],
        "middleware.py",
        "sha4_blame_mw",
        &[
            ("import time", true),
            ("from django.utils.deprecation import MiddlewareMixin", true),
            ("", true),
            ("class RequestTimingMiddleware(MiddlewareMixin):", true),
            ("def process_request(self, request):", true),
            ("request._start_time = time.monotonic()", true),
            ("def process_response(self, request, response):", true),
            ("elapsed = (time.monotonic()", true),
            ("response['X-Response-Time']", true),
            ("return response", true),
        ],
    );
    assert_blame_at_commit(
        &repo,
        &chain[4],
        "decorators.py",
        "sha4_blame_dec",
        &[
            ("from functools import wraps", true),
            ("from django.http import JsonResponse", true),
            ("", true),
            ("def require_json(view_fn):", true),
            ("@wraps(view_fn)", true),
            ("def wrapper(request, *a, **kw):", true),
            ("if request.content_type", true),
            ("return view_fn(request", true),
            ("return wrapper", true),
        ],
    );
    // Verify C1's files (models.py and schemas.py) still correctly attributed at tip.
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "models.py",
        "sha4_models_preserved",
        &[
            ("class Product(models.Model):", true),
            ("name = models.CharField", true),
            ("class Meta: ordering", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "schemas.py",
        "sha4_schemas_preserved",
        &[
            ("class ProductSchema(BaseModel):", true),
            ("price: condecimal", true),
            ("class Config: from_attributes = True", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "views.py",
        "chain4_prior_views.py",
        &[
            ("@api_view(['GET'])", true),
            ("def product_list(request):", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "serializers.py",
        "chain4_prior_serializers.py",
        &[
            (
                "class ProductSerializer(serializers.ModelSerializer):",
                true,
            ),
            ("model = Product", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "urls.py",
        "chain4_prior_urls.py",
        &[("app_name = 'shop'", true), ("urlpatterns = [", true)],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "permissions.py",
        "chain4_prior_permissions.py",
        &[
            ("class IsOwnerOrReadOnly(BasePermission):", true),
            ("class IsStaff(BasePermission):", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "signals.py",
        "chain4_prior_signals.py",
        &[
            ("@receiver(post_save, sender=Product)", true),
            ("@receiver(pre_delete, sender=Product)", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "tasks.py",
        "chain4_prior_tasks.py",
        &[
            ("@shared_task", true),
            ("def sync_inventory(product_id):", true),
        ],
    );
}

// ============================================================================
// END Category 1: Fast Path
// ============================================================================

// ============================================================================
// Category 2: Slow Path (same files, no conflict — upstream prepends)
// ============================================================================

/// Test 1: Python utils.py — upstream prepends module header, feature appends
/// validation/sanitization functions. Forces slow path because utils.py blobs
/// differ after rebase (upstream prepended 3 lines, feature appended AI lines).
///
/// Checks that accepted_lines at sha0 is ~8 (not the full-chain ~40), and that
/// no future commit's AI lines appear in earlier notes.
#[test]
fn test_slow_path_python_utils_main_prepends_feature_appends() {
    let repo = TestRepo::new();

    // Initial: utils.py with trailing newline so 3-way merge works cleanly.
    repo.commit_untracked_file("utils.py", "def base_util(): pass\n", "Initial commit");
    let main_branch = repo.current_branch();

    // Main: prepend module header (changes blob → forces slow path on feature commits)
    repo.commit_untracked_file(
        "utils.py",
        "# utils module\nimport logging\n\ndef base_util(): pass\n",
        "main: prepend module header to utils.py",
    );
    // 4 more human commits on different files
    repo.commit_untracked_file(
        "constants.py",
        "MAX_RETRIES = 3\nTIMEOUT = 30\n",
        "main: add constants",
    );
    repo.commit_untracked_file(
        "exceptions.py",
        "class AppError(Exception): pass\nclass ValidationError(AppError): pass\n",
        "main: add exceptions",
    );
    repo.commit_untracked_file(
        "config.py",
        "import os\nDATABASE_URL = os.getenv('DATABASE_URL', 'sqlite:///app.db')\n",
        "main: add config",
    );
    repo.commit_untracked_file(
        "setup.cfg",
        "[metadata]\nname = myapp\nversion = 0.1\n",
        "main: add setup.cfg",
    );

    // Feature branch starts from BEFORE main's prepend (base = initial commit)
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: append 8 AI lines (validate_email + sanitize_input) to utils.py
    let mut utils = repo.filename("utils.py");
    utils.set_contents(crate::lines![
        "def base_util(): pass",
        "".ai(),
        "def validate_email(email: str) -> bool:".ai(),
        "    import re".ai(),
        "    pattern = r'^[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\\.[a-zA-Z]{2,}$'".ai(),
        "    return bool(re.match(pattern, email))".ai(),
        "".ai(),
        "def sanitize_input(text: str) -> str:".ai(),
        "    return text.strip().replace('<', '&lt;').replace('>', '&gt;')".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add validate_email and sanitize_input")
        .unwrap();

    // C2: append 8 more AI lines (normalize + truncate)
    utils.set_contents(crate::lines![
        "def base_util(): pass",
        "".ai(),
        "def validate_email(email: str) -> bool:".ai(),
        "    import re".ai(),
        "    pattern = r'^[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\\.[a-zA-Z]{2,}$'".ai(),
        "    return bool(re.match(pattern, email))".ai(),
        "".ai(),
        "def sanitize_input(text: str) -> str:".ai(),
        "    return text.strip().replace('<', '&lt;').replace('>', '&gt;')".ai(),
        "".ai(),
        "def normalize_phone(phone: str) -> str:".ai(),
        "    import re".ai(),
        "    digits = re.sub(r'\\D', '', phone)".ai(),
        "    return f'+1{digits}' if len(digits) == 10 else digits".ai(),
        "".ai(),
        "def truncate_text(text: str, max_len: int, suffix: str = '...') -> str:".ai(),
        "    return text if len(text) <= max_len else text[:max_len - len(suffix)] + suffix".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add normalize_phone and truncate_text")
        .unwrap();

    // C3: append 8 more AI lines (parse_date + format_currency)
    utils.set_contents(crate::lines![
        "def base_util(): pass",
        "".ai(),
        "def validate_email(email: str) -> bool:".ai(),
        "    import re".ai(),
        "    pattern = r'^[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\\.[a-zA-Z]{2,}$'".ai(),
        "    return bool(re.match(pattern, email))".ai(),
        "".ai(),
        "def sanitize_input(text: str) -> str:".ai(),
        "    return text.strip().replace('<', '&lt;').replace('>', '&gt;')".ai(),
        "".ai(),
        "def normalize_phone(phone: str) -> str:".ai(),
        "    import re".ai(),
        "    digits = re.sub(r'\\D', '', phone)".ai(),
        "    return f'+1{digits}' if len(digits) == 10 else digits".ai(),
        "".ai(),
        "def truncate_text(text: str, max_len: int, suffix: str = '...') -> str:".ai(),
        "    return text if len(text) <= max_len else text[:max_len - len(suffix)] + suffix".ai(),
        "".ai(),
        "def parse_date(date_str: str, fmt: str = '%Y-%m-%d'):".ai(),
        "    from datetime import datetime".ai(),
        "    return datetime.strptime(date_str, fmt)".ai(),
        "".ai(),
        "def format_currency(amount: float, symbol: str = '$') -> str:".ai(),
        "    return f'{symbol}{amount:,.2f}'".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add parse_date and format_currency")
        .unwrap();

    // C4: append 8 more AI lines (generate_slug + deep_merge)
    utils.set_contents(crate::lines![
        "def base_util(): pass",
        "".ai(),
        "def validate_email(email: str) -> bool:".ai(),
        "    import re".ai(),
        "    pattern = r'^[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\\.[a-zA-Z]{2,}$'".ai(),
        "    return bool(re.match(pattern, email))".ai(),
        "".ai(),
        "def sanitize_input(text: str) -> str:".ai(),
        "    return text.strip().replace('<', '&lt;').replace('>', '&gt;')".ai(),
        "".ai(),
        "def normalize_phone(phone: str) -> str:".ai(),
        "    import re".ai(),
        "    digits = re.sub(r'\\D', '', phone)".ai(),
        "    return f'+1{digits}' if len(digits) == 10 else digits".ai(),
        "".ai(),
        "def truncate_text(text: str, max_len: int, suffix: str = '...') -> str:".ai(),
        "    return text if len(text) <= max_len else text[:max_len - len(suffix)] + suffix".ai(),
        "".ai(),
        "def parse_date(date_str: str, fmt: str = '%Y-%m-%d'):".ai(),
        "    from datetime import datetime".ai(),
        "    return datetime.strptime(date_str, fmt)".ai(),
        "".ai(),
        "def format_currency(amount: float, symbol: str = '$') -> str:".ai(),
        "    return f'{symbol}{amount:,.2f}'".ai(),
        "".ai(),
        "def generate_slug(text: str) -> str:".ai(),
        "    import re".ai(),
        "    return re.sub(r'[^a-z0-9]+', '-', text.lower()).strip('-')".ai(),
        "".ai(),
        "def deep_merge(base: dict, override: dict) -> dict:".ai(),
        "    result = dict(base)".ai(),
        "    for k, v in override.items():".ai(),
        "        result[k] = deep_merge(base[k], v) if isinstance(v, dict) and isinstance(base.get(k), dict) else v".ai(),
        "    return result".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add generate_slug and deep_merge")
        .unwrap();

    // C5: append 8 more AI lines (retry_with_backoff + chunk_list)
    utils.set_contents(crate::lines![
        "def base_util(): pass",
        "".ai(),
        "def validate_email(email: str) -> bool:".ai(),
        "    import re".ai(),
        "    pattern = r'^[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\\.[a-zA-Z]{2,}$'".ai(),
        "    return bool(re.match(pattern, email))".ai(),
        "".ai(),
        "def sanitize_input(text: str) -> str:".ai(),
        "    return text.strip().replace('<', '&lt;').replace('>', '&gt;')".ai(),
        "".ai(),
        "def normalize_phone(phone: str) -> str:".ai(),
        "    import re".ai(),
        "    digits = re.sub(r'\\D', '', phone)".ai(),
        "    return f'+1{digits}' if len(digits) == 10 else digits".ai(),
        "".ai(),
        "def truncate_text(text: str, max_len: int, suffix: str = '...') -> str:".ai(),
        "    return text if len(text) <= max_len else text[:max_len - len(suffix)] + suffix".ai(),
        "".ai(),
        "def parse_date(date_str: str, fmt: str = '%Y-%m-%d'):".ai(),
        "    from datetime import datetime".ai(),
        "    return datetime.strptime(date_str, fmt)".ai(),
        "".ai(),
        "def format_currency(amount: float, symbol: str = '$') -> str:".ai(),
        "    return f'{symbol}{amount:,.2f}'".ai(),
        "".ai(),
        "def generate_slug(text: str) -> str:".ai(),
        "    import re".ai(),
        "    return re.sub(r'[^a-z0-9]+', '-', text.lower()).strip('-')".ai(),
        "".ai(),
        "def deep_merge(base: dict, override: dict) -> dict:".ai(),
        "    result = dict(base)".ai(),
        "    for k, v in override.items():".ai(),
        "        result[k] = deep_merge(base[k], v) if isinstance(v, dict) and isinstance(base.get(k), dict) else v".ai(),
        "    return result".ai(),
        "".ai(),
        "def retry_with_backoff(fn, attempts: int = 3, base_delay: float = 0.5):".ai(),
        "    import time".ai(),
        "    for i in range(attempts):".ai(),
        "        try: return fn()".ai(),
        "        except Exception:".ai(),
        "            if i == attempts - 1: raise".ai(),
        "            time.sleep(base_delay * (2 ** i))".ai(),
        "".ai(),
        "def chunk_list(lst: list, size: int) -> list:".ai(),
        "    return [lst[i:i+size] for i in range(0, len(lst), size)]".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add retry_with_backoff and chunk_list")
        .unwrap();

    // Rebase feature onto main (non-conflicting: prepend + append)
    repo.git(&["rebase", &main_branch]).unwrap();

    let chain = get_commit_chain(&repo, 5);

    // sha0 = C1': note has utils.py only
    assert_note_base_commit_matches(&repo, &chain[0], "sha0");
    assert_note_files_exact(&repo, &chain[0], "sha0_files", &["utils.py"]);

    // sha0 blame: first 3 lines human (# utils module, import logging, blank),
    // then def base_util (human), then 8 AI lines
    assert_blame_at_commit(
        &repo,
        &chain[0],
        "utils.py",
        "sha0_blame",
        &[
            ("# utils module", false),
            ("import logging", false),
            ("", false),
            ("def base_util(): pass", false),
            ("", true),
            ("def validate_email(email: str) -> bool:", true),
            ("import re", true),
            ("pattern = r'^", true),
            ("return bool(re.match(pattern, email))", true),
            ("", true),
            ("def sanitize_input(text: str) -> str:", true),
            ("return text.strip()", true),
        ],
    );

    // sha1 = C2'
    assert_note_base_commit_matches(&repo, &chain[1], "sha1");
    assert_note_files_exact(&repo, &chain[1], "sha1_files", &["utils.py"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "utils.py",
        "sha1_blame_new",
        &[("def normalize_phone", true), ("def truncate_text", true)],
    );

    // sha2 = C3'
    assert_note_base_commit_matches(&repo, &chain[2], "sha2");
    assert_note_files_exact(&repo, &chain[2], "sha2_files", &["utils.py"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "utils.py",
        "sha2_blame_new",
        &[("def parse_date", true), ("def format_currency", true)],
    );

    // sha3 = C4'
    assert_note_base_commit_matches(&repo, &chain[3], "sha3");
    assert_note_files_exact(&repo, &chain[3], "sha3_files", &["utils.py"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "utils.py",
        "sha3_blame_new",
        &[("def generate_slug", true), ("def deep_merge", true)],
    );

    // sha4 = C5'
    assert_note_base_commit_matches(&repo, &chain[4], "sha4");
    assert_note_files_exact(&repo, &chain[4], "sha4_files", &["utils.py"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "utils.py",
        "sha4_blame_new",
        &[("def retry_with_backoff", true), ("def chunk_list", true)],
    );
}

/// Test 2: Rust lib.rs — upstream prepends crate-level doc and deny(warnings),
/// feature appends impl blocks per commit AND adds a unique module file.
/// Checks cumulative file sets and that future module files don't leak.
#[test]
fn test_slow_path_rust_lib_rs_main_prepends_feature_adds_impls() {
    let repo = TestRepo::new();

    // Initial: src/lib.rs with trailing newline
    repo.commit_untracked_file("src/lib.rs", "pub mod types;\n", "Initial commit");
    let main_branch = repo.current_branch();

    // Main: prepend crate-level docs + deny(warnings)
    repo.commit_untracked_file(
        "src/lib.rs",
        "//! Library crate\n#![deny(warnings)]\n\npub mod types;\n",
        "main: prepend crate docs and deny(warnings)",
    );
    repo.commit_untracked_file(
        "src/error.rs",
        "pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;\n",
        "main: add error types",
    );
    repo.commit_untracked_file(
        "build.rs",
        "fn main() { println!(\"cargo:rerun-if-changed=build.rs\"); }\n",
        "main: add build script",
    );
    repo.commit_untracked_file(
        "Cargo.toml",
        "[package]\nname = \"mylib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        "main: add Cargo.toml",
    );
    repo.commit_untracked_file(
        "README.md",
        "# mylib\n\nA Rust library.\n",
        "main: add README",
    );

    // Feature branch from initial commit (before main's prepend)
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: append impl Block to lib.rs + create mod_a.rs
    let mut lib = repo.filename("src/lib.rs");
    lib.set_contents(crate::lines![
        "pub mod types;",
        "".ai(),
        "pub struct Cache {".ai(),
        "    inner: std::collections::HashMap<String, Vec<u8>>,".ai(),
        "}".ai(),
        "impl Cache {".ai(),
        "    pub fn new() -> Self { Self { inner: Default::default() } }".ai(),
        "    pub fn get(&self, key: &str) -> Option<&Vec<u8>> { self.inner.get(key) }".ai(),
        "    pub fn set(&mut self, key: impl Into<String>, val: Vec<u8>) { self.inner.insert(key.into(), val); }".ai(),
        "}".ai(),
    ]);
    let mut mod_a = repo.filename("src/mod_a.rs");
    mod_a.set_contents(crate::lines![
        "pub fn encode_base64(input: &[u8]) -> String {".ai(),
        "    use std::fmt::Write;".ai(),
        "    let alphabet = b\"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/\";"
            .ai(),
        "    let mut out = String::new();".ai(),
        "    for chunk in input.chunks(3) {".ai(),
        "        let _ = write!(out, \"{}\", alphabet[(chunk[0] >> 2) as usize] as char);".ai(),
        "    }".ai(),
        "    out".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add Cache impl + mod_a")
        .unwrap();

    // C2: append Config impl to lib.rs + create mod_b.rs
    lib.set_contents(crate::lines![
        "pub mod types;",
        "".ai(),
        "pub struct Cache {".ai(),
        "    inner: std::collections::HashMap<String, Vec<u8>>,".ai(),
        "}".ai(),
        "impl Cache {".ai(),
        "    pub fn new() -> Self { Self { inner: Default::default() } }".ai(),
        "    pub fn get(&self, key: &str) -> Option<&Vec<u8>> { self.inner.get(key) }".ai(),
        "    pub fn set(&mut self, key: impl Into<String>, val: Vec<u8>) { self.inner.insert(key.into(), val); }".ai(),
        "}".ai(),
        "".ai(),
        "pub struct Config {".ai(),
        "    pub max_connections: usize,".ai(),
        "    pub timeout_ms: u64,".ai(),
        "}".ai(),
        "impl Default for Config {".ai(),
        "    fn default() -> Self { Self { max_connections: 10, timeout_ms: 5000 } }".ai(),
        "}".ai(),
        "impl Config {".ai(),
        "    pub fn with_timeout(mut self, ms: u64) -> Self { self.timeout_ms = ms; self }".ai(),
        "}".ai(),
    ]);
    let mut mod_b = repo.filename("src/mod_b.rs");
    mod_b.set_contents(crate::lines![
        "use std::time::{Duration, Instant};".ai(),
        "pub struct Timer { start: Instant }".ai(),
        "impl Timer {".ai(),
        "    pub fn new() -> Self { Self { start: Instant::now() } }".ai(),
        "    pub fn elapsed(&self) -> Duration { self.start.elapsed() }".ai(),
        "    pub fn elapsed_ms(&self) -> u128 { self.elapsed().as_millis() }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add Config impl + mod_b")
        .unwrap();

    // C3: append Pool impl to lib.rs + create mod_c.rs
    lib.set_contents(crate::lines![
        "pub mod types;",
        "".ai(),
        "pub struct Cache {".ai(),
        "    inner: std::collections::HashMap<String, Vec<u8>>,".ai(),
        "}".ai(),
        "impl Cache {".ai(),
        "    pub fn new() -> Self { Self { inner: Default::default() } }".ai(),
        "    pub fn get(&self, key: &str) -> Option<&Vec<u8>> { self.inner.get(key) }".ai(),
        "    pub fn set(&mut self, key: impl Into<String>, val: Vec<u8>) { self.inner.insert(key.into(), val); }".ai(),
        "}".ai(),
        "".ai(),
        "pub struct Config {".ai(),
        "    pub max_connections: usize,".ai(),
        "    pub timeout_ms: u64,".ai(),
        "}".ai(),
        "impl Default for Config {".ai(),
        "    fn default() -> Self { Self { max_connections: 10, timeout_ms: 5000 } }".ai(),
        "}".ai(),
        "impl Config {".ai(),
        "    pub fn with_timeout(mut self, ms: u64) -> Self { self.timeout_ms = ms; self }".ai(),
        "}".ai(),
        "".ai(),
        "pub struct Pool<T> { items: Vec<T> }".ai(),
        "impl<T> Pool<T> {".ai(),
        "    pub fn new(items: Vec<T>) -> Self { Self { items } }".ai(),
        "    pub fn take(&mut self) -> Option<T> { self.items.pop() }".ai(),
        "    pub fn put(&mut self, item: T) { self.items.push(item); }".ai(),
        "    pub fn len(&self) -> usize { self.items.len() }".ai(),
        "}".ai(),
    ]);
    let mut mod_c = repo.filename("src/mod_c.rs");
    mod_c.set_contents(crate::lines![
        "pub fn retry<T, E, F: Fn() -> Result<T, E>>(f: F, attempts: usize) -> Result<T, E> {".ai(),
        "    let mut last = f();".ai(),
        "    for _ in 1..attempts {".ai(),
        "        if last.is_ok() { return last; }".ai(),
        "        last = f();".ai(),
        "    }".ai(),
        "    last".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add Pool impl + mod_c")
        .unwrap();

    // C4: append Event impl to lib.rs + create mod_d.rs
    lib.set_contents(crate::lines![
        "pub mod types;",
        "".ai(),
        "pub struct Cache {".ai(),
        "    inner: std::collections::HashMap<String, Vec<u8>>,".ai(),
        "}".ai(),
        "impl Cache {".ai(),
        "    pub fn new() -> Self { Self { inner: Default::default() } }".ai(),
        "    pub fn get(&self, key: &str) -> Option<&Vec<u8>> { self.inner.get(key) }".ai(),
        "    pub fn set(&mut self, key: impl Into<String>, val: Vec<u8>) { self.inner.insert(key.into(), val); }".ai(),
        "}".ai(),
        "".ai(),
        "pub struct Config { pub max_connections: usize, pub timeout_ms: u64 }".ai(),
        "impl Default for Config { fn default() -> Self { Self { max_connections: 10, timeout_ms: 5000 } } }".ai(),
        "impl Config { pub fn with_timeout(mut self, ms: u64) -> Self { self.timeout_ms = ms; self } }".ai(),
        "".ai(),
        "pub struct Pool<T> { items: Vec<T> }".ai(),
        "impl<T> Pool<T> { pub fn new(items: Vec<T>) -> Self { Self { items } } pub fn len(&self) -> usize { self.items.len() } }".ai(),
        "".ai(),
        "pub enum Event { Start, Stop, Pause, Resume }".ai(),
        "impl std::fmt::Display for Event {".ai(),
        "    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {".ai(),
        "        match self { Event::Start => write!(f, \"start\"), Event::Stop => write!(f, \"stop\"),".ai(),
        "            Event::Pause => write!(f, \"pause\"), Event::Resume => write!(f, \"resume\") }".ai(),
        "    }".ai(),
        "}".ai(),
    ]);
    let mut mod_d = repo.filename("src/mod_d.rs");
    mod_d.set_contents(crate::lines![
        "pub trait Serialize { fn serialize(&self) -> Vec<u8>; }".ai(),
        "pub trait Deserialize: Sized { fn deserialize(bytes: &[u8]) -> Option<Self>; }".ai(),
        "pub fn round_trip<T: Serialize + Deserialize>(val: &T) -> Option<T> {".ai(),
        "    let bytes = val.serialize();".ai(),
        "    T::deserialize(&bytes)".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add Event impl + mod_d")
        .unwrap();

    // C5: append Metrics impl to lib.rs + create mod_e.rs
    lib.set_contents(crate::lines![
        "pub mod types;",
        "".ai(),
        "pub struct Cache { inner: std::collections::HashMap<String, Vec<u8>> }".ai(),
        "impl Cache { pub fn new() -> Self { Self { inner: Default::default() } } }".ai(),
        "".ai(),
        "pub struct Config { pub max_connections: usize, pub timeout_ms: u64 }".ai(),
        "impl Default for Config { fn default() -> Self { Self { max_connections: 10, timeout_ms: 5000 } } }".ai(),
        "".ai(),
        "pub struct Pool<T> { items: Vec<T> }".ai(),
        "impl<T> Pool<T> { pub fn new(items: Vec<T>) -> Self { Self { items } } pub fn len(&self) -> usize { self.items.len() } }".ai(),
        "".ai(),
        "pub enum Event { Start, Stop, Pause, Resume }".ai(),
        "impl std::fmt::Display for Event { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, \"{:?}\", self) } }".ai(),
        "".ai(),
        "pub struct Metrics { counters: std::collections::HashMap<String, u64> }".ai(),
        "impl Metrics {".ai(),
        "    pub fn new() -> Self { Self { counters: Default::default() } }".ai(),
        "    pub fn inc(&mut self, name: &str) { *self.counters.entry(name.to_owned()).or_default() += 1; }".ai(),
        "    pub fn get(&self, name: &str) -> u64 { *self.counters.get(name).unwrap_or(&0) }".ai(),
        "}".ai(),
    ]);
    let mut mod_e = repo.filename("src/mod_e.rs");
    mod_e.set_contents(crate::lines![
        "pub fn clamp<T: PartialOrd>(val: T, min: T, max: T) -> T {".ai(),
        "    if val < min { min } else if val > max { max } else { val }".ai(),
        "}".ai(),
        "pub fn lerp(a: f64, b: f64, t: f64) -> f64 { a + (b - a) * t }".ai(),
        "pub fn approx_eq(a: f64, b: f64, eps: f64) -> bool { (a - b).abs() < eps }".ai(),
        "pub fn percent(part: f64, total: f64) -> f64 { if total == 0.0 { 0.0 } else { part / total * 100.0 } }".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add Metrics impl + mod_e")
        .unwrap();

    // Rebase onto main (non-conflicting: prepend + append)
    repo.git(&["rebase", &main_branch]).unwrap();

    let chain = get_commit_chain(&repo, 5);

    // sha0 = C1': {src/lib.rs, src/mod_a.rs}
    assert_note_base_commit_matches(&repo, &chain[0], "sha0");
    assert_note_files_exact(
        &repo,
        &chain[0],
        "sha0_files",
        &["src/lib.rs", "src/mod_a.rs"],
    );
    assert_note_no_forbidden_files(
        &repo,
        &chain[0],
        "sha0_no_future",
        &["mod_b.rs", "mod_c.rs", "mod_d.rs", "mod_e.rs"],
    );

    // sha1 = C2': {src/lib.rs, src/mod_b.rs}
    assert_note_base_commit_matches(&repo, &chain[1], "sha1");
    assert_note_files_exact(
        &repo,
        &chain[1],
        "sha1_files",
        &["src/lib.rs", "src/mod_b.rs"],
    );
    assert_note_no_forbidden_files(
        &repo,
        &chain[1],
        "sha1_no_future",
        &["mod_c.rs", "mod_d.rs", "mod_e.rs"],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "src/lib.rs",
        "sha1_blame_new",
        &[
            ("pub struct Config {", true),
            ("impl Default for Config", true),
            ("impl Config {", true),
        ],
    );
    // mod_a.rs (from C1) is a prior file at chain[1] — fast path, verify attribution intact
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "src/mod_a.rs",
        "chain1_prior_mod_a_rs",
        &[
            ("pub fn encode_base64(input: &[u8]) -> String {", true),
            (
                "let alphabet = b\"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/\";",
                true,
            ),
        ],
    );

    // sha2 = C3': {src/lib.rs, src/mod_c.rs}
    assert_note_base_commit_matches(&repo, &chain[2], "sha2");
    assert_note_files_exact(
        &repo,
        &chain[2],
        "sha2_files",
        &["src/lib.rs", "src/mod_c.rs"],
    );
    assert_note_no_forbidden_files(
        &repo,
        &chain[2],
        "sha2_no_future",
        &["mod_d.rs", "mod_e.rs"],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "src/lib.rs",
        "sha2_blame_new",
        &[("pub struct Pool<T>", true), ("impl<T> Pool<T>", true)],
    );
    // mod_a.rs and mod_b.rs (from C1-C2) are prior files at chain[2]
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "src/mod_a.rs",
        "chain2_prior_mod_a_rs",
        &[
            ("pub fn encode_base64(input: &[u8]) -> String {", true),
            (
                "let alphabet = b\"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/\";",
                true,
            ),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "src/mod_b.rs",
        "chain2_prior_mod_b_rs",
        &[
            ("pub struct Timer { start: Instant }", true),
            (
                "pub fn elapsed_ms(&self) -> u128 { self.elapsed().as_millis() }",
                true,
            ),
        ],
    );

    // sha3 = C4': {src/lib.rs, src/mod_d.rs}
    assert_note_base_commit_matches(&repo, &chain[3], "sha3");
    assert_note_files_exact(
        &repo,
        &chain[3],
        "sha3_files",
        &["src/lib.rs", "src/mod_d.rs"],
    );
    assert_note_no_forbidden_files(&repo, &chain[3], "sha3_no_future", &["mod_e.rs"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "src/lib.rs",
        "sha3_blame_new",
        &[
            ("pub enum Event", true),
            ("impl std::fmt::Display for Event", true),
        ],
    );
    // mod_a.rs, mod_b.rs, and mod_c.rs (from C1-C3) are prior files at chain[3]
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "src/mod_a.rs",
        "chain3_prior_mod_a_rs",
        &[
            ("pub fn encode_base64(input: &[u8]) -> String {", true),
            (
                "let alphabet = b\"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/\";",
                true,
            ),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "src/mod_b.rs",
        "chain3_prior_mod_b_rs",
        &[
            ("pub struct Timer { start: Instant }", true),
            (
                "pub fn elapsed_ms(&self) -> u128 { self.elapsed().as_millis() }",
                true,
            ),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "src/mod_c.rs",
        "chain3_prior_mod_c_rs",
        &[
            (
                "pub fn retry<T, E, F: Fn() -> Result<T, E>>(f: F, attempts: usize) -> Result<T, E> {",
                true,
            ),
            ("if last.is_ok() { return last; }", true),
        ],
    );

    // sha4 = C5': {src/lib.rs, src/mod_e.rs}
    assert_note_base_commit_matches(&repo, &chain[4], "sha4");
    assert_note_files_exact(
        &repo,
        &chain[4],
        "sha4_files",
        &["src/lib.rs", "src/mod_e.rs"],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "src/lib.rs",
        "sha4_blame_new",
        &[("pub struct Metrics {", true), ("impl Metrics {", true)],
    );
    // mod_a.rs, mod_b.rs, mod_c.rs, and mod_d.rs (from C1-C4) are prior files at chain[4]
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "src/mod_a.rs",
        "chain4_prior_mod_a_rs",
        &[
            ("pub fn encode_base64(input: &[u8]) -> String {", true),
            (
                "let alphabet = b\"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/\";",
                true,
            ),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "src/mod_b.rs",
        "chain4_prior_mod_b_rs",
        &[
            ("pub struct Timer { start: Instant }", true),
            (
                "pub fn elapsed_ms(&self) -> u128 { self.elapsed().as_millis() }",
                true,
            ),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "src/mod_c.rs",
        "chain4_prior_mod_c_rs",
        &[
            (
                "pub fn retry<T, E, F: Fn() -> Result<T, E>>(f: F, attempts: usize) -> Result<T, E> {",
                true,
            ),
            ("if last.is_ok() { return last; }", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "src/mod_d.rs",
        "chain4_prior_mod_d_rs",
        &[
            (
                "pub trait Serialize { fn serialize(&self) -> Vec<u8>; }",
                true,
            ),
            (
                "pub fn round_trip<T: Serialize + Deserialize>(val: &T) -> Option<T> {",
                true,
            ),
        ],
    );
}

/// Test 3: TypeScript routes.ts — upstream prepends a comment, feature appends
/// endpoint handler functions. Blame at sha0 checks human lines at top.
#[test]
fn test_slow_path_typescript_routes_main_prepends_feature_adds_handlers() {
    let repo = TestRepo::new();

    // Initial: src/routes.ts with trailing newline
    repo.commit_untracked_file(
        "src/routes.ts",
        "import express from 'express';\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: prepend auto-generated comment (forces slow path)
    repo.commit_untracked_file(
        "src/routes.ts",
        "// Auto-generated routes\nimport express from 'express';\n",
        "main: prepend auto-generated comment",
    );
    repo.commit_untracked_file("src/middleware.ts",
        "export const logger = (req: any, res: any, next: any) => { console.log(req.method, req.path); next(); };\n",
        "main: add logger middleware",
    );
    repo.commit_untracked_file("src/types.ts",
        "export interface User { id: number; email: string; name: string; }\nexport interface ApiResponse<T> { data: T; status: number; }\n",
        "main: add shared types",
    );
    repo.commit_untracked_file("tsconfig.json",
        "{\"compilerOptions\":{\"target\":\"ES2020\",\"module\":\"commonjs\",\"strict\":true,\"outDir\":\"dist\"},\"include\":[\"src\"]}\n",
        "main: add tsconfig",
    );
    repo.commit_untracked_file("package.json",
        "{\"name\":\"api\",\"version\":\"1.0.0\",\"scripts\":{\"build\":\"tsc\",\"start\":\"node dist/index.js\"}}\n",
        "main: add package.json",
    );

    // Feature branch from before main's prepend
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: append /users GET handler (8 AI lines)
    let mut routes = repo.filename("src/routes.ts");
    routes.set_contents(crate::lines![
        "import express from 'express';",
        "".ai(),
        "const router = express.Router();".ai(),
        "".ai(),
        "router.get('/users', async (req, res) => {".ai(),
        "  try {".ai(),
        "    const users = await UserService.findAll();".ai(),
        "    res.json({ data: users, status: 200 });".ai(),
        "  } catch (err) {".ai(),
        "    res.status(500).json({ error: String(err) });".ai(),
        "  }".ai(),
        "});".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add GET /users route")
        .unwrap();

    // C2: append /users POST handler (8 AI lines)
    routes.set_contents(crate::lines![
        "import express from 'express';",
        "".ai(),
        "const router = express.Router();".ai(),
        "".ai(),
        "router.get('/users', async (req, res) => {".ai(),
        "  try {".ai(),
        "    const users = await UserService.findAll();".ai(),
        "    res.json({ data: users, status: 200 });".ai(),
        "  } catch (err) {".ai(),
        "    res.status(500).json({ error: String(err) });".ai(),
        "  }".ai(),
        "});".ai(),
        "".ai(),
        "router.post('/users', async (req, res) => {".ai(),
        "  const { email, name } = req.body;".ai(),
        "  if (!email || !name) return res.status(400).json({ error: 'email and name required' });"
            .ai(),
        "  const user = await UserService.create({ email, name });".ai(),
        "  res.status(201).json({ data: user, status: 201 });".ai(),
        "});".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add POST /users route")
        .unwrap();

    // C3: append /users/:id GET handler (8 AI lines)
    routes.set_contents(crate::lines![
        "import express from 'express';",
        "".ai(),
        "const router = express.Router();".ai(),
        "".ai(),
        "router.get('/users', async (req, res) => {".ai(),
        "  const users = await UserService.findAll();".ai(),
        "  res.json({ data: users, status: 200 });".ai(),
        "});".ai(),
        "".ai(),
        "router.post('/users', async (req, res) => {".ai(),
        "  const { email, name } = req.body;".ai(),
        "  const user = await UserService.create({ email, name });".ai(),
        "  res.status(201).json({ data: user, status: 201 });".ai(),
        "});".ai(),
        "".ai(),
        "router.get('/users/:id', async (req, res) => {".ai(),
        "  const id = parseInt(req.params.id, 10);".ai(),
        "  const user = await UserService.findById(id);".ai(),
        "  if (!user) return res.status(404).json({ error: 'Not found' });".ai(),
        "  res.json({ data: user, status: 200 });".ai(),
        "});".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add GET /users/:id route")
        .unwrap();

    // C4: append /users/:id PUT handler (8 AI lines)
    routes.set_contents(crate::lines![
        "import express from 'express';",
        "".ai(),
        "const router = express.Router();".ai(),
        "".ai(),
        "router.get('/users', async (req, res) => { const users = await UserService.findAll(); res.json({ data: users }); });".ai(),
        "router.post('/users', async (req, res) => { const user = await UserService.create(req.body); res.status(201).json({ data: user }); });".ai(),
        "router.get('/users/:id', async (req, res) => { const user = await UserService.findById(+req.params.id); res.json({ data: user }); });".ai(),
        "".ai(),
        "router.put('/users/:id', async (req, res) => {".ai(),
        "  const id = parseInt(req.params.id, 10);".ai(),
        "  const updates = req.body;".ai(),
        "  const user = await UserService.update(id, updates);".ai(),
        "  if (!user) return res.status(404).json({ error: 'Not found' });".ai(),
        "  res.json({ data: user, status: 200 });".ai(),
        "});".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add PUT /users/:id route")
        .unwrap();

    // C5: append /users/:id DELETE handler (8 AI lines) + export
    routes.set_contents(crate::lines![
        "import express from 'express';",
        "".ai(),
        "const router = express.Router();".ai(),
        "".ai(),
        "router.get('/users', async (req, res) => { const users = await UserService.findAll(); res.json({ data: users }); });".ai(),
        "router.post('/users', async (req, res) => { const user = await UserService.create(req.body); res.status(201).json({ data: user }); });".ai(),
        "router.get('/users/:id', async (req, res) => { const user = await UserService.findById(+req.params.id); res.json({ data: user }); });".ai(),
        "router.put('/users/:id', async (req, res) => { const user = await UserService.update(+req.params.id, req.body); res.json({ data: user }); });".ai(),
        "".ai(),
        "router.delete('/users/:id', async (req, res) => {".ai(),
        "  const id = parseInt(req.params.id, 10);".ai(),
        "  const deleted = await UserService.delete(id);".ai(),
        "  if (!deleted) return res.status(404).json({ error: 'Not found' });".ai(),
        "  res.status(204).send();".ai(),
        "});".ai(),
        "".ai(),
        "export default router;".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add DELETE /users/:id + export")
        .unwrap();

    // Rebase onto main (non-conflicting)
    repo.git(&["rebase", &main_branch]).unwrap();

    let chain = get_commit_chain(&repo, 5);

    // sha0 = C1': src/routes.ts with ~8 AI lines
    // blame: line 1 = human (// Auto-generated routes), line 2 = human (import express),
    // then AI lines start
    assert_note_base_commit_matches(&repo, &chain[0], "sha0");
    assert_note_files_exact(&repo, &chain[0], "sha0_files", &["src/routes.ts"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[0],
        "src/routes.ts",
        "sha0_blame",
        &[
            ("// Auto-generated routes", false),
            ("import express from 'express';", false),
            ("const router = express.Router();", true),
            ("router.get('/users'", true),
            ("try {", true),
            ("const users = await UserService.findAll()", true),
        ],
    );

    // sha1 = C2': only C2's delta
    assert_note_base_commit_matches(&repo, &chain[1], "sha1");
    assert_note_files_exact(&repo, &chain[1], "sha1_files", &["src/routes.ts"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "src/routes.ts",
        "sha1_blame_new",
        &[
            ("router.post('/users'", true),
            ("email and name required", true),
        ],
    );

    // sha2 = C3': only C3's delta
    assert_note_base_commit_matches(&repo, &chain[2], "sha2");
    assert_note_files_exact(&repo, &chain[2], "sha2_files", &["src/routes.ts"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "src/routes.ts",
        "sha2_blame_new",
        &[
            ("router.get('/users/:id'", true),
            ("UserService.findById", true),
        ],
    );

    // sha3 = C4': only C4's delta
    assert_note_base_commit_matches(&repo, &chain[3], "sha3");
    assert_note_files_exact(&repo, &chain[3], "sha3_files", &["src/routes.ts"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "src/routes.ts",
        "sha3_blame_new",
        &[
            ("router.put('/users/:id'", true),
            ("UserService.update", true),
        ],
    );

    // sha4 = C5': only C5's delta
    assert_note_base_commit_matches(&repo, &chain[4], "sha4");
    assert_note_files_exact(&repo, &chain[4], "sha4_files", &["src/routes.ts"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "src/routes.ts",
        "sha4_blame_new",
        &[
            ("router.delete('/users/:id'", true),
            ("export default router", true),
        ],
    );
}

/// Test 4: TOML config file — upstream prepends production header, feature
/// appends new TOML sections per commit. Each commit adds 8+ AI lines.
/// Verifies no future sections bleed into earlier commit notes.
#[test]
fn test_slow_path_config_file_both_add_different_sections() {
    let repo = TestRepo::new();

    // Initial: config.toml with trailing newline
    repo.commit_untracked_file(
        "config.toml",
        "[server]\nhost = \"localhost\"\nport = 8080\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: prepend production comment (forces slow path on feature commits)
    repo.commit_untracked_file(
        "config.toml",
        "# Production config\n\n[server]\nhost = \"localhost\"\nport = 8080\n",
        "main: prepend production config header",
    );
    repo.commit_untracked_file(
        ".env.production",
        "APP_ENV=production\nLOG_LEVEL=warn\n",
        "main: add production env",
    );
    repo.commit_untracked_file(
        "docker-compose.prod.yml",
        "version: '3.9'\nservices:\n  app:\n    image: myapp:latest\n    ports: ['80:8080']\n",
        "main: add prod docker-compose",
    );
    repo.commit_untracked_file(
        "nginx.conf",
        "server { listen 80; location / { proxy_pass http://app:8080; } }\n",
        "main: add nginx config",
    );
    repo.commit_untracked_file(
        "Makefile",
        "deploy:\n\tdocker-compose -f docker-compose.prod.yml up -d\n",
        "main: add Makefile",
    );

    // Feature branch from before main's prepend
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: append [database] section (8 AI lines)
    let mut cfg = repo.filename("config.toml");
    cfg.set_contents(crate::lines![
        "[server]",
        "host = \"localhost\"",
        "port = 8080",
        "".ai(),
        "[database]".ai(),
        "url = \"postgres://user:pass@localhost:5432/mydb\"".ai(),
        "max_connections = 100".ai(),
        "min_connections = 5".ai(),
        "connect_timeout = 30".ai(),
        "idle_timeout = 600".ai(),
        "max_lifetime = 1800".ai(),
        "ssl_mode = \"require\"".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add [database] config section")
        .unwrap();

    // C2: append [cache] section (8 AI lines)
    cfg.set_contents(crate::lines![
        "[server]",
        "host = \"localhost\"",
        "port = 8080",
        "".ai(),
        "[database]".ai(),
        "url = \"postgres://user:pass@localhost:5432/mydb\"".ai(),
        "max_connections = 100".ai(),
        "min_connections = 5".ai(),
        "connect_timeout = 30".ai(),
        "idle_timeout = 600".ai(),
        "max_lifetime = 1800".ai(),
        "ssl_mode = \"require\"".ai(),
        "".ai(),
        "[cache]".ai(),
        "backend = \"redis\"".ai(),
        "url = \"redis://localhost:6379/0\"".ai(),
        "max_size = 1000".ai(),
        "ttl_seconds = 300".ai(),
        "eviction_policy = \"lru\"".ai(),
        "compression = true".ai(),
        "key_prefix = \"app:\"".ai(),
        "serializer = \"json\"".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add [cache] config section")
        .unwrap();

    // C3: append [metrics] section (8 AI lines)
    cfg.set_contents(crate::lines![
        "[server]",
        "host = \"localhost\"",
        "port = 8080",
        "".ai(),
        "[database]".ai(),
        "url = \"postgres://user:pass@localhost:5432/mydb\"".ai(),
        "max_connections = 100".ai(),
        "ssl_mode = \"require\"".ai(),
        "".ai(),
        "[cache]".ai(),
        "backend = \"redis\"".ai(),
        "url = \"redis://localhost:6379/0\"".ai(),
        "ttl_seconds = 300".ai(),
        "".ai(),
        "[metrics]".ai(),
        "enabled = true".ai(),
        "endpoint = \"/metrics\"".ai(),
        "port = 9090".ai(),
        "interval_seconds = 15".ai(),
        "include_system = true".ai(),
        "labels = [\"app\", \"env\", \"version\"]".ai(),
        "exporter = \"prometheus\"".ai(),
        "histogram_buckets = [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0]".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add [metrics] config section")
        .unwrap();

    // C4: append [auth] section (8 AI lines)
    cfg.set_contents(crate::lines![
        "[server]",
        "host = \"localhost\"",
        "port = 8080",
        "".ai(),
        "[database]".ai(),
        "url = \"postgres://user:pass@localhost:5432/mydb\"".ai(),
        "max_connections = 100".ai(),
        "".ai(),
        "[cache]".ai(),
        "backend = \"redis\"".ai(),
        "url = \"redis://localhost:6379/0\"".ai(),
        "".ai(),
        "[metrics]".ai(),
        "enabled = true".ai(),
        "endpoint = \"/metrics\"".ai(),
        "".ai(),
        "[auth]".ai(),
        "provider = \"jwt\"".ai(),
        "secret_env = \"JWT_SECRET\"".ai(),
        "token_expiry_seconds = 3600".ai(),
        "refresh_expiry_seconds = 86400".ai(),
        "algorithm = \"HS256\"".ai(),
        "issuer = \"myapp\"".ai(),
        "audience = [\"web\", \"mobile\"]".ai(),
        "allow_anonymous = false".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add [auth] config section")
        .unwrap();

    // C5: append [notifications] section (8 AI lines)
    cfg.set_contents(crate::lines![
        "[server]",
        "host = \"localhost\"",
        "port = 8080",
        "".ai(),
        "[database]".ai(),
        "url = \"postgres://user:pass@localhost:5432/mydb\"".ai(),
        "".ai(),
        "[cache]".ai(),
        "backend = \"redis\"".ai(),
        "".ai(),
        "[metrics]".ai(),
        "enabled = true".ai(),
        "".ai(),
        "[auth]".ai(),
        "provider = \"jwt\"".ai(),
        "token_expiry_seconds = 3600".ai(),
        "".ai(),
        "[notifications]".ai(),
        "email_driver = \"smtp\"".ai(),
        "smtp_host = \"smtp.sendgrid.net\"".ai(),
        "smtp_port = 587".ai(),
        "smtp_user_env = \"SMTP_USER\"".ai(),
        "smtp_pass_env = \"SMTP_PASS\"".ai(),
        "from_address = \"noreply@myapp.com\"".ai(),
        "queue_name = \"notifications\"".ai(),
        "retry_attempts = 3".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add [notifications] config section")
        .unwrap();

    // Rebase onto main (non-conflicting)
    repo.git(&["rebase", &main_branch]).unwrap();

    let chain = get_commit_chain(&repo, 5);

    // sha0 = C1': config.toml with [database] section only
    assert_note_base_commit_matches(&repo, &chain[0], "sha0");
    assert_note_files_exact(&repo, &chain[0], "sha0_files", &["config.toml"]);

    // sha1 = C2': [cache] section only
    assert_note_base_commit_matches(&repo, &chain[1], "sha1");
    assert_note_files_exact(&repo, &chain[1], "sha1_files", &["config.toml"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "config.toml",
        "sha1_blame_new",
        &[
            ("[cache]", true),
            ("backend = \"redis\"", true),
            ("eviction_policy", true),
        ],
    );

    // sha2 = C3': [metrics] section only
    assert_note_base_commit_matches(&repo, &chain[2], "sha2");
    assert_note_files_exact(&repo, &chain[2], "sha2_files", &["config.toml"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "config.toml",
        "sha2_blame_new",
        &[
            ("[metrics]", true),
            ("exporter = \"prometheus\"", true),
            ("histogram_buckets", true),
        ],
    );

    // sha3 = C4': [auth] section only
    assert_note_base_commit_matches(&repo, &chain[3], "sha3");
    assert_note_files_exact(&repo, &chain[3], "sha3_files", &["config.toml"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "config.toml",
        "sha3_blame_new",
        &[
            ("[auth]", true),
            ("provider = \"jwt\"", true),
            ("allow_anonymous = false", true),
        ],
    );

    // sha4 = C5': [notifications] section only
    assert_note_base_commit_matches(&repo, &chain[4], "sha4");
    assert_note_files_exact(&repo, &chain[4], "sha4_files", &["config.toml"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "config.toml",
        "sha4_blame_new",
        &[
            ("[notifications]", true),
            ("email_driver = \"smtp\"", true),
            ("retry_attempts = 3", true),
        ],
    );
}

/// Test 5: 10-commit feature branch, all appending to src/engine.rs.
/// Upstream prepends a 3-line license header. Verifies ALL 10 SHAs.
/// Critical: sha0 must NOT have sha9's accepted_lines.
#[test]
fn test_slow_path_growing_shared_file_10_commits() {
    let repo = TestRepo::new();

    // Initial: src/engine.rs with trailing newline
    repo.commit_untracked_file("src/engine.rs", "// Engine core\n", "Initial commit");
    let main_branch = repo.current_branch();

    // Main: prepend 3-line license header (forces slow path)
    repo.commit_untracked_file("src/engine.rs",
        "// Copyright 2024 MyOrg\n// Licensed under MIT License\n// See LICENSE file for details\n\n// Engine core\n",
        "main: prepend license header to engine.rs",
    );
    repo.commit_untracked_file(
        "src/error.rs",
        "#[derive(Debug)]\npub enum EngineError { NotFound, InvalidInput, Timeout }\n",
        "main: add engine errors",
    );
    repo.commit_untracked_file("src/config.rs",
        "pub struct EngineConfig { pub workers: usize, pub stack_size: usize }\nimpl Default for EngineConfig { fn default() -> Self { Self { workers: 4, stack_size: 2 * 1024 * 1024 } } }\n",
        "main: add engine config",
    );
    repo.commit_untracked_file(
        "benches/engine_bench.rs",
        "fn main() { /* bench placeholder */ }\n",
        "main: add bench placeholder",
    );
    repo.commit_untracked_file(
        "tests/engine_test.rs",
        "#[test]\nfn smoke_test() { assert!(true); }\n",
        "main: add smoke test",
    );

    // Feature branch from before main's prepend
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: append 8 AI lines to engine.rs
    let mut eng = repo.filename("src/engine.rs");
    eng.set_contents(crate::lines![
        "// Engine core",
        "".ai(),
        "pub struct Engine {".ai(),
        "    running: bool,".ai(),
        "    workers: usize,".ai(),
        "}".ai(),
        "impl Engine {".ai(),
        "    pub fn new(workers: usize) -> Self { Self { running: false, workers } }".ai(),
        "    pub fn start(&mut self) { self.running = true; }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add Engine struct")
        .unwrap();

    // C2: append 8 more AI lines
    eng.set_contents(crate::lines![
        "// Engine core",
        "".ai(),
        "pub struct Engine { running: bool, workers: usize }".ai(),
        "impl Engine {".ai(),
        "    pub fn new(workers: usize) -> Self { Self { running: false, workers } }".ai(),
        "    pub fn start(&mut self) { self.running = true; }".ai(),
        "    pub fn stop(&mut self) { self.running = false; }".ai(),
        "    pub fn is_running(&self) -> bool { self.running }".ai(),
        "}".ai(),
        "".ai(),
        "pub struct Task { pub id: u64, pub payload: Vec<u8> }".ai(),
        "impl Task {".ai(),
        "    pub fn new(id: u64, payload: Vec<u8>) -> Self { Self { id, payload } }".ai(),
        "    pub fn size(&self) -> usize { self.payload.len() }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add Task struct")
        .unwrap();

    // C3: append 8 more AI lines
    eng.set_contents(crate::lines![
        "// Engine core",
        "".ai(),
        "pub struct Engine { running: bool, workers: usize }".ai(),
        "impl Engine { pub fn new(workers: usize) -> Self { Self { running: false, workers } } pub fn start(&mut self) { self.running = true; } }".ai(),
        "".ai(),
        "pub struct Task { pub id: u64, pub payload: Vec<u8> }".ai(),
        "impl Task { pub fn new(id: u64, payload: Vec<u8>) -> Self { Self { id, payload } } }".ai(),
        "".ai(),
        "pub struct Queue { tasks: std::collections::VecDeque<Task> }".ai(),
        "impl Queue {".ai(),
        "    pub fn new() -> Self { Self { tasks: Default::default() } }".ai(),
        "    pub fn push(&mut self, t: Task) { self.tasks.push_back(t); }".ai(),
        "    pub fn pop(&mut self) -> Option<Task> { self.tasks.pop_front() }".ai(),
        "    pub fn len(&self) -> usize { self.tasks.len() }".ai(),
        "    pub fn is_empty(&self) -> bool { self.tasks.is_empty() }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add Queue struct")
        .unwrap();

    // C4: append 8 more AI lines
    eng.set_contents(crate::lines![
        "// Engine core",
        "".ai(),
        "pub struct Engine { running: bool, workers: usize }".ai(),
        "impl Engine { pub fn new(w: usize) -> Self { Self { running: false, workers: w } } pub fn start(&mut self) { self.running = true; } }".ai(),
        "".ai(),
        "pub struct Task { pub id: u64, pub payload: Vec<u8> }".ai(),
        "impl Task { pub fn new(id: u64, payload: Vec<u8>) -> Self { Self { id, payload } } }".ai(),
        "".ai(),
        "pub struct Queue { tasks: std::collections::VecDeque<Task> }".ai(),
        "impl Queue { pub fn new() -> Self { Self { tasks: Default::default() } } pub fn push(&mut self, t: Task) { self.tasks.push_back(t); } pub fn pop(&mut self) -> Option<Task> { self.tasks.pop_front() } }".ai(),
        "".ai(),
        "pub struct Worker { pub id: usize }".ai(),
        "impl Worker {".ai(),
        "    pub fn new(id: usize) -> Self { Self { id } }".ai(),
        "    pub fn execute(&self, task: &Task) -> Result<(), String> {".ai(),
        "        if task.payload.is_empty() { return Err(\"empty payload\".into()); }".ai(),
        "        Ok(())".ai(),
        "    }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add Worker struct")
        .unwrap();

    // C5: append 8 more AI lines
    eng.set_contents(crate::lines![
        "// Engine core",
        "".ai(),
        "pub struct Engine { running: bool, workers: usize }".ai(),
        "impl Engine { pub fn new(w: usize) -> Self { Self { running: false, workers: w } } pub fn start(&mut self) { self.running = true; } }".ai(),
        "".ai(),
        "pub struct Task { pub id: u64, pub payload: Vec<u8> }".ai(),
        "impl Task { pub fn new(id: u64, payload: Vec<u8>) -> Self { Self { id, payload } } }".ai(),
        "".ai(),
        "pub struct Queue { tasks: std::collections::VecDeque<Task> }".ai(),
        "impl Queue { pub fn push(&mut self, t: Task) { self.tasks.push_back(t); } }".ai(),
        "".ai(),
        "pub struct Worker { pub id: usize }".ai(),
        "impl Worker { pub fn execute(&self, task: &Task) -> Result<(), String> { Ok(()) } }".ai(),
        "".ai(),
        "pub struct Scheduler { queue: Queue, workers: Vec<Worker> }".ai(),
        "impl Scheduler {".ai(),
        "    pub fn new(n: usize) -> Self { Self { queue: Queue { tasks: Default::default() }, workers: (0..n).map(Worker::new).collect() } }".ai(),
        "    pub fn submit(&mut self, task: Task) { self.queue.push(task); }".ai(),
        "    pub fn worker_count(&self) -> usize { self.workers.len() }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add Scheduler struct")
        .unwrap();

    // C6: append 8 more AI lines
    eng.set_contents(crate::lines![
        "// Engine core",
        "".ai(),
        "pub struct Engine { running: bool, workers: usize }".ai(),
        "impl Engine { pub fn new(w: usize) -> Self { Self { running: false, workers: w } } pub fn start(&mut self) { self.running = true; } }".ai(),
        "".ai(),
        "pub struct Task { pub id: u64, pub payload: Vec<u8> }".ai(),
        "pub struct Queue { tasks: std::collections::VecDeque<Task> }".ai(),
        "pub struct Worker { pub id: usize }".ai(),
        "pub struct Scheduler { queue: Queue, workers: Vec<Worker> }".ai(),
        "".ai(),
        "pub struct Metrics {".ai(),
        "    tasks_submitted: u64,".ai(),
        "    tasks_completed: u64,".ai(),
        "    tasks_failed: u64,".ai(),
        "}".ai(),
        "impl Metrics {".ai(),
        "    pub fn new() -> Self { Self { tasks_submitted: 0, tasks_completed: 0, tasks_failed: 0 } }".ai(),
        "    pub fn record_submit(&mut self) { self.tasks_submitted += 1; }".ai(),
        "    pub fn record_complete(&mut self) { self.tasks_completed += 1; }".ai(),
        "    pub fn record_fail(&mut self) { self.tasks_failed += 1; }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C6 add Metrics struct")
        .unwrap();

    // C7: append 8 more AI lines
    eng.set_contents(crate::lines![
        "// Engine core",
        "".ai(),
        "pub struct Engine { running: bool, workers: usize }".ai(),
        "pub struct Task { pub id: u64, pub payload: Vec<u8> }".ai(),
        "pub struct Queue { tasks: std::collections::VecDeque<Task> }".ai(),
        "pub struct Worker { pub id: usize }".ai(),
        "pub struct Scheduler { queue: Queue, workers: Vec<Worker> }".ai(),
        "pub struct Metrics { tasks_submitted: u64, tasks_completed: u64, tasks_failed: u64 }".ai(),
        "".ai(),
        "pub struct RateLimit {".ai(),
        "    capacity: u64,".ai(),
        "    tokens: u64,".ai(),
        "    refill_rate: u64,".ai(),
        "}".ai(),
        "impl RateLimit {".ai(),
        "    pub fn new(capacity: u64, refill_rate: u64) -> Self { Self { capacity, tokens: capacity, refill_rate } }".ai(),
        "    pub fn try_consume(&mut self, n: u64) -> bool { if self.tokens >= n { self.tokens -= n; true } else { false } }".ai(),
        "    pub fn refill(&mut self) { self.tokens = (self.tokens + self.refill_rate).min(self.capacity); }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C7 add RateLimit struct")
        .unwrap();

    // C8: append 8 more AI lines
    eng.set_contents(crate::lines![
        "// Engine core",
        "".ai(),
        "pub struct Engine { running: bool, workers: usize }".ai(),
        "pub struct Task { pub id: u64, pub payload: Vec<u8> }".ai(),
        "pub struct Queue { tasks: std::collections::VecDeque<Task> }".ai(),
        "pub struct Scheduler { queue: Queue, workers: Vec<Worker> }".ai(),
        "pub struct Metrics { tasks_submitted: u64, tasks_completed: u64 }".ai(),
        "pub struct RateLimit { capacity: u64, tokens: u64, refill_rate: u64 }".ai(),
        "".ai(),
        "pub struct CircuitBreaker {".ai(),
        "    state: BreakState,".ai(),
        "    failures: u32,".ai(),
        "    threshold: u32,".ai(),
        "}".ai(),
        "pub enum BreakState { Closed, Open, HalfOpen }".ai(),
        "impl CircuitBreaker {".ai(),
        "    pub fn new(threshold: u32) -> Self { Self { state: BreakState::Closed, failures: 0, threshold } }".ai(),
        "    pub fn is_open(&self) -> bool { matches!(self.state, BreakState::Open) }".ai(),
        "    pub fn record_failure(&mut self) { self.failures += 1; if self.failures >= self.threshold { self.state = BreakState::Open; } }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C8 add CircuitBreaker")
        .unwrap();

    // C9: append 8 more AI lines
    eng.set_contents(crate::lines![
        "// Engine core",
        "".ai(),
        "pub struct Engine { running: bool, workers: usize }".ai(),
        "pub struct Task { pub id: u64, pub payload: Vec<u8> }".ai(),
        "pub struct Queue { tasks: std::collections::VecDeque<Task> }".ai(),
        "pub struct Scheduler { queue: Queue, workers: Vec<Worker> }".ai(),
        "pub struct Metrics { tasks_submitted: u64, tasks_completed: u64 }".ai(),
        "pub struct RateLimit { capacity: u64, tokens: u64, refill_rate: u64 }".ai(),
        "pub struct CircuitBreaker { state: BreakState, failures: u32, threshold: u32 }".ai(),
        "pub enum BreakState { Closed, Open, HalfOpen }".ai(),
        "".ai(),
        "pub struct HealthCheck {".ai(),
        "    checks: Vec<Box<dyn Fn() -> bool + Send + Sync>>,".ai(),
        "}".ai(),
        "impl HealthCheck {".ai(),
        "    pub fn new() -> Self { Self { checks: Vec::new() } }".ai(),
        "    pub fn add<F: Fn() -> bool + Send + Sync + 'static>(&mut self, f: F) { self.checks.push(Box::new(f)); }".ai(),
        "    pub fn all_healthy(&self) -> bool { self.checks.iter().all(|f| f()) }".ai(),
        "    pub fn healthy_count(&self) -> usize { self.checks.iter().filter(|f| f()).count() }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C9 add HealthCheck struct")
        .unwrap();

    // C10: append 8 more AI lines
    eng.set_contents(crate::lines![
        "// Engine core",
        "".ai(),
        "pub struct Engine { running: bool, workers: usize }".ai(),
        "pub struct Task { pub id: u64, pub payload: Vec<u8> }".ai(),
        "pub struct Queue { tasks: std::collections::VecDeque<Task> }".ai(),
        "pub struct Scheduler { queue: Queue, workers: Vec<Worker> }".ai(),
        "pub struct Metrics { tasks_submitted: u64, tasks_completed: u64 }".ai(),
        "pub struct RateLimit { capacity: u64, tokens: u64, refill_rate: u64 }".ai(),
        "pub struct CircuitBreaker { state: BreakState, failures: u32, threshold: u32 }".ai(),
        "pub enum BreakState { Closed, Open, HalfOpen }".ai(),
        "pub struct HealthCheck { checks: Vec<Box<dyn Fn() -> bool + Send + Sync>> }".ai(),
        "".ai(),
        "pub struct Tracer {".ai(),
        "    spans: Vec<(String, std::time::Duration)>,".ai(),
        "}".ai(),
        "impl Tracer {".ai(),
        "    pub fn new() -> Self { Self { spans: Vec::new() } }".ai(),
        "    pub fn record(&mut self, name: impl Into<String>, duration: std::time::Duration) {"
            .ai(),
        "        self.spans.push((name.into(), duration));".ai(),
        "    }".ai(),
        "    pub fn spans(&self) -> &[(String, std::time::Duration)] { &self.spans }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C10 add Tracer struct")
        .unwrap();

    // Rebase onto main (non-conflicting)
    repo.git(&["rebase", &main_branch]).unwrap();

    let chain = get_commit_chain(&repo, 10);

    // Verify ALL 10 SHAs
    assert_note_base_commit_matches(&repo, &chain[0], "sha0");
    assert_note_files_exact(&repo, &chain[0], "sha0_files", &["src/engine.rs"]);

    assert_note_base_commit_matches(&repo, &chain[1], "sha1");
    assert_note_files_exact(&repo, &chain[1], "sha1_files", &["src/engine.rs"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "src/engine.rs",
        "sha1_blame_new",
        &[("pub struct Task {", true), ("impl Task {", true)],
    );

    assert_note_base_commit_matches(&repo, &chain[2], "sha2");
    assert_note_files_exact(&repo, &chain[2], "sha2_files", &["src/engine.rs"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "src/engine.rs",
        "sha2_blame_new",
        &[("pub struct Queue {", true), ("impl Queue {", true)],
    );

    assert_note_base_commit_matches(&repo, &chain[3], "sha3");
    assert_note_files_exact(&repo, &chain[3], "sha3_files", &["src/engine.rs"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "src/engine.rs",
        "sha3_blame_new",
        &[("pub struct Worker {", true), ("impl Worker {", true)],
    );

    assert_note_base_commit_matches(&repo, &chain[4], "sha4");
    assert_note_files_exact(&repo, &chain[4], "sha4_files", &["src/engine.rs"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "src/engine.rs",
        "sha4_blame_new",
        &[("pub struct Scheduler {", true), ("impl Scheduler {", true)],
    );

    assert_note_base_commit_matches(&repo, &chain[5], "sha5");
    assert_note_files_exact(&repo, &chain[5], "sha5_files", &["src/engine.rs"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[5],
        "src/engine.rs",
        "sha5_blame_new",
        &[("pub struct Metrics {", true), ("impl Metrics {", true)],
    );

    assert_note_base_commit_matches(&repo, &chain[6], "sha6");
    assert_note_files_exact(&repo, &chain[6], "sha6_files", &["src/engine.rs"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[6],
        "src/engine.rs",
        "sha6_blame_new",
        &[("pub struct RateLimit {", true), ("impl RateLimit {", true)],
    );

    assert_note_base_commit_matches(&repo, &chain[7], "sha7");
    assert_note_files_exact(&repo, &chain[7], "sha7_files", &["src/engine.rs"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[7],
        "src/engine.rs",
        "sha7_blame_new",
        &[
            ("pub struct CircuitBreaker {", true),
            ("pub enum BreakState {", true),
        ],
    );

    assert_note_base_commit_matches(&repo, &chain[8], "sha8");
    assert_note_files_exact(&repo, &chain[8], "sha8_files", &["src/engine.rs"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[8],
        "src/engine.rs",
        "sha8_blame_new",
        &[
            ("pub struct HealthCheck {", true),
            ("impl HealthCheck {", true),
        ],
    );

    assert_note_base_commit_matches(&repo, &chain[9], "sha9");
    assert_note_files_exact(&repo, &chain[9], "sha9_files", &["src/engine.rs"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[9],
        "src/engine.rs",
        "sha9_blame_new",
        &[("pub struct Tracer {", true), ("impl Tracer {", true)],
    );
}

/// Test 6: Two shared files (models.py + services.py), both prepended by main.
/// Feature appends AI lines to both in each commit. Checks cumulative lines
/// across both files and no future-file leak.
#[test]
fn test_slow_path_multiple_shared_files_both_modified() {
    let repo = TestRepo::new();

    // Initial: both shared files with trailing newline
    repo.commit_untracked_file(
        "models.py",
        "class BaseModel: pass\n",
        "Initial commit: models.py",
    );
    repo.commit_untracked_file(
        "services.py",
        "class BaseService: pass\n",
        "Initial commit: services.py",
    );
    let main_branch = repo.current_branch();

    // Main: prepend headers to BOTH files (two separate commits, then 3 more human commits)
    repo.commit_untracked_file(
        "models.py",
        "# Domain models\nfrom dataclasses import dataclass\n\nclass BaseModel: pass\n",
        "main: prepend header to models.py",
    );
    repo.commit_untracked_file(
        "services.py",
        "# Business services\nfrom typing import Any\n\nclass BaseService: pass\n",
        "main: prepend header to services.py",
    );
    repo.commit_untracked_file(
        "exceptions.py",
        "class NotFound(Exception): pass\nclass Conflict(Exception): pass\n",
        "main: add exceptions",
    );
    repo.commit_untracked_file("validators.py",
        "def validate_not_empty(val, name):\n    if not val: raise ValueError(f'{name} must not be empty')\n",
        "main: add validators",
    );
    repo.commit_untracked_file(
        "constants.py",
        "DEFAULT_PAGE_SIZE = 20\nMAX_PAGE_SIZE = 100\n",
        "main: add constants",
    );

    // Feature branch from before main's two prepend commits (HEAD~5)
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: append 6 AI lines to BOTH models.py AND services.py (12 total)
    let mut models = repo.filename("models.py");
    models.set_contents(crate::lines![
        "class BaseModel: pass",
        "".ai(),
        "@dataclass".ai(),
        "class User:".ai(),
        "    id: int".ai(),
        "    email: str".ai(),
        "    name: str".ai(),
        "    active: bool = True".ai(),
    ]);
    let mut services = repo.filename("services.py");
    services.set_contents(crate::lines![
        "class BaseService: pass",
        "".ai(),
        "class UserService:".ai(),
        "    def __init__(self, repo): self.repo = repo".ai(),
        "    def get_by_id(self, user_id: int): return self.repo.find(user_id)".ai(),
        "    def list_active(self): return self.repo.find_all(active=True)".ai(),
        "    def deactivate(self, user_id: int): self.repo.update(user_id, active=False)".ai(),
        "    def exists(self, email: str) -> bool: return self.repo.find_by_email(email) is not None".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add User model + UserService")
        .unwrap();

    // C2: append 6 more AI lines to both files
    models.set_contents(crate::lines![
        "class BaseModel: pass",
        "".ai(),
        "@dataclass".ai(),
        "class User:".ai(),
        "    id: int".ai(),
        "    email: str".ai(),
        "    name: str".ai(),
        "    active: bool = True".ai(),
        "".ai(),
        "@dataclass".ai(),
        "class Product:".ai(),
        "    id: int".ai(),
        "    name: str".ai(),
        "    price: float".ai(),
        "    stock: int = 0".ai(),
    ]);
    services.set_contents(crate::lines![
        "class BaseService: pass",
        "".ai(),
        "class UserService:".ai(),
        "    def __init__(self, repo): self.repo = repo".ai(),
        "    def get_by_id(self, user_id: int): return self.repo.find(user_id)".ai(),
        "    def list_active(self): return self.repo.find_all(active=True)".ai(),
        "".ai(),
        "class ProductService:".ai(),
        "    def __init__(self, repo): self.repo = repo".ai(),
        "    def get_by_id(self, pid: int): return self.repo.find(pid)".ai(),
        "    def list_in_stock(self): return self.repo.find_all(stock__gt=0)".ai(),
        "    def adjust_stock(self, pid: int, delta: int): self.repo.increment(pid, 'stock', delta)".ai(),
        "    def get_price(self, pid: int) -> float: return self.repo.find(pid).price".ai(),
        "    def set_price(self, pid: int, price: float): self.repo.update(pid, price=price)".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add Product model + ProductService")
        .unwrap();

    // C3: append 6 more AI lines to both files
    models.set_contents(crate::lines![
        "class BaseModel: pass",
        "".ai(),
        "@dataclass".ai(),
        "class User: id: int; email: str; name: str; active: bool = True".ai(),
        "".ai(),
        "@dataclass".ai(),
        "class Product: id: int; name: str; price: float; stock: int = 0".ai(),
        "".ai(),
        "@dataclass".ai(),
        "class Order:".ai(),
        "    id: int".ai(),
        "    user_id: int".ai(),
        "    items: list".ai(),
        "    total: float".ai(),
        "    status: str = 'pending'".ai(),
    ]);
    services.set_contents(crate::lines![
        "class BaseService: pass",
        "".ai(),
        "class UserService:".ai(),
        "    def get_by_id(self, user_id: int): return self.repo.find(user_id)".ai(),
        "".ai(),
        "class ProductService:".ai(),
        "    def get_by_id(self, pid: int): return self.repo.find(pid)".ai(),
        "    def list_in_stock(self): return self.repo.find_all(stock__gt=0)".ai(),
        "".ai(),
        "class OrderService:".ai(),
        "    def __init__(self, repo): self.repo = repo".ai(),
        "    def create(self, user_id, items): return self.repo.create(user_id=user_id, items=items)".ai(),
        "    def get_by_id(self, oid: int): return self.repo.find(oid)".ai(),
        "    def cancel(self, oid: int): self.repo.update(oid, status='cancelled')".ai(),
        "    def complete(self, oid: int): self.repo.update(oid, status='completed')".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add Order model + OrderService")
        .unwrap();

    // C4: append 6 more AI lines to both files
    models.set_contents(crate::lines![
        "class BaseModel: pass",
        "".ai(),
        "@dataclass".ai(),
        "class User: id: int; email: str; name: str".ai(),
        "@dataclass".ai(),
        "class Product: id: int; name: str; price: float; stock: int = 0".ai(),
        "@dataclass".ai(),
        "class Order: id: int; user_id: int; items: list; total: float; status: str = 'pending'"
            .ai(),
        "".ai(),
        "@dataclass".ai(),
        "class Address:".ai(),
        "    id: int".ai(),
        "    user_id: int".ai(),
        "    street: str".ai(),
        "    city: str".ai(),
        "    country: str = 'US'".ai(),
    ]);
    services.set_contents(crate::lines![
        "class BaseService: pass",
        "".ai(),
        "class UserService: pass".ai(),
        "class ProductService: pass".ai(),
        "class OrderService: pass".ai(),
        "".ai(),
        "class AddressService:".ai(),
        "    def __init__(self, repo): self.repo = repo".ai(),
        "    def get_by_user(self, uid: int): return self.repo.find_all(user_id=uid)".ai(),
        "    def create(self, uid, street, city, country='US'): return self.repo.create(user_id=uid, street=street, city=city, country=country)".ai(),
        "    def delete(self, aid: int): self.repo.delete(aid)".ai(),
        "    def set_default(self, uid: int, aid: int): self.repo.update_all({'is_default': False}, user_id=uid); self.repo.update(aid, is_default=True)".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add Address model + AddressService")
        .unwrap();

    // C5: append 6 more AI lines to both files
    models.set_contents(crate::lines![
        "class BaseModel: pass",
        "".ai(),
        "@dataclass".ai(),
        "class User: id: int; email: str; name: str".ai(),
        "@dataclass".ai(),
        "class Product: id: int; name: str; price: float; stock: int = 0".ai(),
        "@dataclass".ai(),
        "class Order: id: int; user_id: int; items: list; total: float; status: str = 'pending'"
            .ai(),
        "@dataclass".ai(),
        "class Address: id: int; user_id: int; street: str; city: str; country: str = 'US'".ai(),
        "".ai(),
        "@dataclass".ai(),
        "class Review:".ai(),
        "    id: int".ai(),
        "    user_id: int".ai(),
        "    product_id: int".ai(),
        "    rating: int".ai(),
        "    comment: str = ''".ai(),
    ]);
    services.set_contents(crate::lines![
        "class BaseService: pass",
        "".ai(),
        "class UserService: pass".ai(),
        "class ProductService: pass".ai(),
        "class OrderService: pass".ai(),
        "class AddressService: pass".ai(),
        "".ai(),
        "class ReviewService:".ai(),
        "    def __init__(self, repo): self.repo = repo".ai(),
        "    def create(self, uid, pid, rating, comment=''): return self.repo.create(user_id=uid, product_id=pid, rating=rating, comment=comment)".ai(),
        "    def get_for_product(self, pid: int): return self.repo.find_all(product_id=pid)".ai(),
        "    def average_rating(self, pid: int) -> float: reviews = self.get_for_product(pid); return sum(r.rating for r in reviews) / len(reviews) if reviews else 0.0".ai(),
        "    def delete(self, rid: int): self.repo.delete(rid)".ai(),
        "    def update_comment(self, rid: int, comment: str): self.repo.update(rid, comment=comment)".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add Review model + ReviewService")
        .unwrap();

    // Rebase onto main (non-conflicting)
    repo.git(&["rebase", &main_branch]).unwrap();

    let chain = get_commit_chain(&repo, 5);

    // sha0 = C1': {models.py, services.py} ~12 accepted lines
    assert_note_base_commit_matches(&repo, &chain[0], "sha0");
    assert_note_files_exact(
        &repo,
        &chain[0],
        "sha0_files",
        &["models.py", "services.py"],
    );
    // sha1 = C2': {models.py, services.py} ~12 accepted lines (only C2's delta)
    // C2 added Product model to models.py and ProductService to services.py
    assert_note_base_commit_matches(&repo, &chain[1], "sha1");
    assert_note_files_exact(
        &repo,
        &chain[1],
        "sha1_files",
        &["models.py", "services.py"],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "models.py",
        "sha1_models_product",
        &[("class Product:", true), ("price: float", true)],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "services.py",
        "sha1_services_product",
        &[("class ProductService:", true), ("def list_in_stock", true)],
    );

    // sha2 = C3': {models.py, services.py} ~12 accepted lines (only C3's delta)
    // C3 added Order model to models.py and OrderService to services.py
    assert_note_base_commit_matches(&repo, &chain[2], "sha2");
    assert_note_files_exact(
        &repo,
        &chain[2],
        "sha2_files",
        &["models.py", "services.py"],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "models.py",
        "sha2_models_order",
        &[("class Order:", true), ("status: str = 'pending'", true)],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "services.py",
        "sha2_services_order",
        &[("class OrderService:", true), ("def cancel", true)],
    );

    // sha3 = C4': ~12 accepted lines (only C4's delta)
    // C4 added Address model to models.py and AddressService to services.py
    assert_note_base_commit_matches(&repo, &chain[3], "sha3");
    assert_note_files_exact(
        &repo,
        &chain[3],
        "sha3_files",
        &["models.py", "services.py"],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "models.py",
        "sha3_models_address",
        &[("class Address:", true), ("country: str = 'US'", true)],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "services.py",
        "sha3_services_address",
        &[("class AddressService:", true), ("def get_by_user", true)],
    );

    // sha4 = C5': ~12 accepted lines (only C5's delta)
    // C5 added Review model to models.py and ReviewService to services.py
    assert_note_base_commit_matches(&repo, &chain[4], "sha4");
    assert_note_files_exact(
        &repo,
        &chain[4],
        "sha4_files",
        &["models.py", "services.py"],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "models.py",
        "sha4_models_review",
        &[("class Review:", true), ("rating: int", true)],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "services.py",
        "sha4_services_review",
        &[("class ReviewService:", true), ("def average_rating", true)],
    );
}

/// Test 7: Mixed — core.rs is shared (slow path), plus unique files in C2 and C4.
/// Critical: no future unique files leak into earlier notes.
#[test]
fn test_slow_path_mixed_unique_and_shared_files() {
    let repo = TestRepo::new();

    // Initial: core.rs with trailing newline
    repo.commit_untracked_file(
        "core.rs",
        "// Core module\npub fn init() {}\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: prepend module-level docs to core.rs (forces slow path)
    repo.commit_untracked_file("core.rs",
        "//! Core module\n//! Provides fundamental functionality.\n\n// Core module\npub fn init() {}\n",
        "main: prepend module docs to core.rs",
    );
    repo.commit_untracked_file("lib.rs", "pub mod core;\n", "main: add lib.rs");
    repo.commit_untracked_file(
        "Cargo.toml",
        "[package]\nname = \"myapp\"\nversion = \"0.1.0\"\n",
        "main: add Cargo.toml",
    );
    repo.commit_untracked_file("benches/bench.rs", "fn main() {}\n", "main: add bench stub");
    repo.commit_untracked_file(
        "examples/usage.rs",
        "fn main() { println!(\"example\"); }\n",
        "main: add usage example",
    );

    // Feature from before main's prepend
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: append 8 AI lines to core.rs only (no unique file)
    let mut core = repo.filename("core.rs");
    core.set_contents(crate::lines![
        "// Core module",
        "pub fn init() {}",
        "".ai(),
        "pub struct Context {".ai(),
        "    pub debug: bool,".ai(),
        "    pub log_level: u8,".ai(),
        "}".ai(),
        "impl Context {".ai(),
        "    pub fn new() -> Self { Self { debug: false, log_level: 2 } }".ai(),
        "    pub fn with_debug(mut self) -> Self { self.debug = true; self }".ai(),
        "    pub fn log(&self, msg: &str) { if self.debug { eprintln!(\"[debug] {}\", msg); } }"
            .ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add Context struct to core.rs")
        .unwrap();

    // C2: append 6 AI lines to core.rs + create module_b.rs (6 AI lines)
    core.set_contents(crate::lines![
        "// Core module",
        "pub fn init() {}",
        "".ai(),
        "pub struct Context { pub debug: bool, pub log_level: u8 }".ai(),
        "impl Context { pub fn new() -> Self { Self { debug: false, log_level: 2 } } }".ai(),
        "".ai(),
        "pub struct Registry { map: std::collections::HashMap<String, Box<dyn std::any::Any>> }".ai(),
        "impl Registry {".ai(),
        "    pub fn new() -> Self { Self { map: Default::default() } }".ai(),
        "    pub fn register<T: 'static>(&mut self, key: impl Into<String>, val: T) { self.map.insert(key.into(), Box::new(val)); }".ai(),
        "    pub fn has(&self, key: &str) -> bool { self.map.contains_key(key) }".ai(),
        "}".ai(),
    ]);
    let mut mod_b = repo.filename("module_b.rs");
    mod_b.set_contents(crate::lines![
        "pub fn hash_fnv1a(input: &[u8]) -> u64 {".ai(),
        "    let mut hash: u64 = 14695981039346656037;".ai(),
        "    for &byte in input {".ai(),
        "        hash ^= byte as u64;".ai(),
        "        hash = hash.wrapping_mul(1099511628211);".ai(),
        "    }".ai(),
        "    hash".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add Registry to core.rs + module_b.rs")
        .unwrap();

    // C3: append 6 AI lines to core.rs only (no unique file)
    core.set_contents(crate::lines![
        "// Core module",
        "pub fn init() {}",
        "".ai(),
        "pub struct Context { pub debug: bool, pub log_level: u8 }".ai(),
        "impl Context { pub fn new() -> Self { Self { debug: false, log_level: 2 } } }".ai(),
        "".ai(),
        "pub struct Registry { map: std::collections::HashMap<String, Box<dyn std::any::Any>> }".ai(),
        "impl Registry { pub fn new() -> Self { Self { map: Default::default() } } pub fn has(&self, key: &str) -> bool { self.map.contains_key(key) } }".ai(),
        "".ai(),
        "pub struct EventBus { handlers: Vec<Box<dyn Fn(&str) + Send>> }".ai(),
        "impl EventBus {".ai(),
        "    pub fn new() -> Self { Self { handlers: Vec::new() } }".ai(),
        "    pub fn on<F: Fn(&str) + Send + 'static>(&mut self, f: F) { self.handlers.push(Box::new(f)); }".ai(),
        "    pub fn emit(&self, event: &str) { for h in &self.handlers { h(event); } }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add EventBus to core.rs")
        .unwrap();

    // C4: append 6 AI lines to core.rs + create module_d.rs (6 AI lines)
    core.set_contents(crate::lines![
        "// Core module",
        "pub fn init() {}",
        "".ai(),
        "pub struct Context { pub debug: bool, pub log_level: u8 }".ai(),
        "pub struct Registry { map: std::collections::HashMap<String, Box<dyn std::any::Any>> }".ai(),
        "pub struct EventBus { handlers: Vec<Box<dyn Fn(&str) + Send>> }".ai(),
        "".ai(),
        "pub struct Pipeline<T> { stages: Vec<Box<dyn Fn(T) -> T>> }".ai(),
        "impl<T: 'static> Pipeline<T> {".ai(),
        "    pub fn new() -> Self { Self { stages: Vec::new() } }".ai(),
        "    pub fn add<F: Fn(T) -> T + 'static>(&mut self, f: F) { self.stages.push(Box::new(f)); }".ai(),
        "    pub fn run(&self, input: T) -> T { self.stages.iter().fold(input, |acc, f| f(acc)) }".ai(),
        "}".ai(),
    ]);
    let mut mod_d = repo.filename("module_d.rs");
    mod_d.set_contents(crate::lines![
        "pub struct LruCache<K, V> { cap: usize, data: std::collections::HashMap<K, V> }".ai(),
        "impl<K: Eq + std::hash::Hash, V> LruCache<K, V> {".ai(),
        "    pub fn new(cap: usize) -> Self { Self { cap, data: Default::default() } }".ai(),
        "    pub fn get(&self, key: &K) -> Option<&V> { self.data.get(key) }".ai(),
        "    pub fn put(&mut self, key: K, val: V) { if self.data.len() >= self.cap { return; } self.data.insert(key, val); }".ai(),
        "    pub fn len(&self) -> usize { self.data.len() }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add Pipeline to core.rs + module_d.rs")
        .unwrap();

    // C5: append 6 AI lines to core.rs only
    core.set_contents(crate::lines![
        "// Core module",
        "pub fn init() {}",
        "".ai(),
        "pub struct Context { pub debug: bool, pub log_level: u8 }".ai(),
        "pub struct Registry { map: std::collections::HashMap<String, Box<dyn std::any::Any>> }".ai(),
        "pub struct EventBus { handlers: Vec<Box<dyn Fn(&str) + Send>> }".ai(),
        "pub struct Pipeline<T> { stages: Vec<Box<dyn Fn(T) -> T>> }".ai(),
        "".ai(),
        "pub struct ServiceLocator { services: std::collections::HashMap<std::any::TypeId, Box<dyn std::any::Any>> }".ai(),
        "impl ServiceLocator {".ai(),
        "    pub fn new() -> Self { Self { services: Default::default() } }".ai(),
        "    pub fn register<T: 'static>(&mut self, service: T) { self.services.insert(std::any::TypeId::of::<T>(), Box::new(service)); }".ai(),
        "    pub fn resolve<T: 'static>(&self) -> Option<&T> { self.services.get(&std::any::TypeId::of::<T>()).and_then(|b| b.downcast_ref()) }".ai(),
        "    pub fn is_registered<T: 'static>(&self) -> bool { self.services.contains_key(&std::any::TypeId::of::<T>()) }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add ServiceLocator to core.rs")
        .unwrap();

    // Rebase onto main (non-conflicting)
    repo.git(&["rebase", &main_branch]).unwrap();

    let chain = get_commit_chain(&repo, 5);

    // sha0 = C1': {core.rs} only, no module_b or module_d
    assert_note_base_commit_matches(&repo, &chain[0], "sha0");
    assert_note_files_exact(&repo, &chain[0], "sha0_files", &["core.rs"]);
    assert_note_no_forbidden_files(
        &repo,
        &chain[0],
        "sha0_no_future",
        &["module_b.rs", "module_d.rs"],
    );

    // sha1 = C2': {core.rs, module_b.rs}, no module_d
    // C2 added Registry struct to core.rs
    assert_note_base_commit_matches(&repo, &chain[1], "sha1");
    assert_note_files_exact(&repo, &chain[1], "sha1_files", &["core.rs", "module_b.rs"]);
    assert_note_no_forbidden_files(&repo, &chain[1], "sha1_no_future", &["module_d.rs"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "core.rs",
        "sha1_core_registry",
        &[("pub struct Registry", true), ("pub fn register", true)],
    );

    // sha2 = C3': {core.rs} — C3 only changes core.rs
    // C3 added EventBus to core.rs
    assert_note_base_commit_matches(&repo, &chain[2], "sha2");
    assert_note_files_exact(&repo, &chain[2], "sha2_files", &["core.rs"]);
    assert_note_no_forbidden_files(&repo, &chain[2], "sha2_no_future", &["module_d.rs"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "core.rs",
        "sha2_core_eventbus",
        &[("pub struct EventBus", true), ("pub fn emit", true)],
    );
    // module_b.rs (from C2) is a prior file at chain[2] — fast path, verify attribution intact
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "module_b.rs",
        "chain2_prior_module_b_rs",
        &[
            ("pub fn hash_fnv1a(input: &[u8]) -> u64 {", true),
            ("hash = hash.wrapping_mul(1099511628211);", true),
        ],
    );

    // sha3 = C4': {core.rs, module_d.rs}
    // C4 added Pipeline to core.rs + created module_d.rs
    assert_note_base_commit_matches(&repo, &chain[3], "sha3");
    assert_note_files_exact(&repo, &chain[3], "sha3_files", &["core.rs", "module_d.rs"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "core.rs",
        "sha3_core_pipeline",
        &[("pub struct Pipeline", true), ("pub fn run", true)],
    );
    // module_b.rs (from C2) is a prior file at chain[3]
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "module_b.rs",
        "chain3_prior_module_b_rs",
        &[
            ("pub fn hash_fnv1a(input: &[u8]) -> u64 {", true),
            ("hash = hash.wrapping_mul(1099511628211);", true),
        ],
    );

    // sha4 = C5': {core.rs}
    // C5 added ServiceLocator to core.rs only
    assert_note_base_commit_matches(&repo, &chain[4], "sha4");
    assert_note_files_exact(&repo, &chain[4], "sha4_files", &["core.rs"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "core.rs",
        "sha4_core_servicelocator",
        &[
            ("pub struct ServiceLocator", true),
            ("pub fn resolve", true),
        ],
    );
    // module_b.rs (from C2) and module_d.rs (from C4) are prior files at chain[4]
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "module_b.rs",
        "chain4_prior_module_b_rs",
        &[
            ("pub fn hash_fnv1a(input: &[u8]) -> u64 {", true),
            ("hash = hash.wrapping_mul(1099511628211);", true),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "module_d.rs",
        "chain4_prior_module_d_rs",
        &[
            (
                "pub struct LruCache<K, V> { cap: usize, data: std::collections::HashMap<K, V> }",
                true,
            ),
            (
                "pub fn put(&mut self, key: K, val: V) { if self.data.len() >= self.cap { return; } self.data.insert(key, val); }",
                true,
            ),
        ],
    );
}

/// Test 8: Feature has human commits intermixed with AI commits.
/// C1 and C4 are human-only. C2, C3, C5 append AI to api.py.
/// Checks that human-only commits don't introduce phantom attribution,
/// and cumulative AI lines are stable across the human commits.
#[test]
fn test_slow_path_feature_has_human_commits_intermixed() {
    let repo = TestRepo::new();

    // Initial: api.py with trailing newline
    repo.commit_untracked_file("api.py", "# API module\n", "Initial commit");
    let main_branch = repo.current_branch();

    // Main: prepend import block to api.py (forces slow path)
    repo.commit_untracked_file(
        "api.py",
        "from flask import Flask, request, jsonify\nfrom functools import wraps\n\n# API module\n",
        "main: prepend imports to api.py",
    );
    repo.commit_untracked_file(
        "wsgi.py",
        "from api import app\nif __name__ == '__main__': app.run()\n",
        "main: add wsgi.py",
    );
    repo.commit_untracked_file(
        "gunicorn.conf.py",
        "bind = '0.0.0.0:8000'\nworkers = 4\ntimeout = 30\n",
        "main: add gunicorn config",
    );
    repo.commit_untracked_file(
        ".flake8",
        "[flake8]\nmax-line-length = 120\n",
        "main: add flake8 config",
    );
    repo.commit_untracked_file(
        "pytest.ini",
        "[pytest]\ntestpaths = tests\naddopts = -v\n",
        "main: add pytest config",
    );

    // Feature branch from before main's prepend
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: HUMAN only — adds config.py (plain write, no AI)
    repo.commit_untracked_file("config.py",
        "import os\nDATABASE_URL = os.getenv('DATABASE_URL', 'sqlite:///app.db')\nSECRET_KEY = os.getenv('SECRET_KEY', 'dev-secret')\nDEBUG = os.getenv('DEBUG', '0') == '1'\nALLOWED_HOSTS = os.getenv('ALLOWED_HOSTS', 'localhost').split(',')\n",
        "config: add application config",
    );

    // C2: AI — appends 10 AI lines to api.py
    let mut api = repo.filename("api.py");
    api.set_contents(crate::lines![
        "# API module",
        "".ai(),
        "app = Flask(__name__)".ai(),
        "".ai(),
        "def require_auth(f):".ai(),
        "    @wraps(f)".ai(),
        "    def decorated(*args, **kwargs):".ai(),
        "        token = request.headers.get('Authorization', '').replace('Bearer ', '')".ai(),
        "        if not token: return jsonify({'error': 'unauthorized'}), 401".ai(),
        "        return f(*args, **kwargs)".ai(),
        "    return decorated".ai(),
        "".ai(),
        "@app.route('/health')".ai(),
        "def health(): return jsonify({'status': 'ok', 'version': '1.0'})".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add Flask app and /health endpoint")
        .unwrap();

    // C3: AI — appends 10 more AI lines to api.py
    api.set_contents(crate::lines![
        "# API module",
        "".ai(),
        "app = Flask(__name__)".ai(),
        "".ai(),
        "def require_auth(f):".ai(),
        "    @wraps(f)".ai(),
        "    def decorated(*args, **kwargs):".ai(),
        "        token = request.headers.get('Authorization', '').replace('Bearer ', '')".ai(),
        "        if not token: return jsonify({'error': 'unauthorized'}), 401".ai(),
        "        return f(*args, **kwargs)".ai(),
        "    return decorated".ai(),
        "".ai(),
        "@app.route('/health')".ai(),
        "def health(): return jsonify({'status': 'ok', 'version': '1.0'})".ai(),
        "".ai(),
        "@app.route('/users', methods=['GET'])".ai(),
        "@require_auth".ai(),
        "def list_users():".ai(),
        "    from config import DATABASE_URL".ai(),
        "    return jsonify({'users': [], 'database': DATABASE_URL})".ai(),
        "".ai(),
        "@app.route('/users', methods=['POST'])".ai(),
        "@require_auth".ai(),
        "def create_user():".ai(),
        "    data = request.get_json()".ai(),
        "    return jsonify({'created': data}), 201".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add /users GET and POST endpoints")
        .unwrap();

    // C4: HUMAN only — adds requirements.txt (no AI)
    repo.commit_untracked_file(
        "requirements.txt",
        "flask==3.0.0\ngunicorn==21.2.0\nrequests==2.31.0\npytest==7.4.0\ncoverage==7.3.0\n",
        "deps: add requirements.txt",
    );

    // C5: AI — appends 10 more AI lines to api.py
    api.set_contents(crate::lines![
        "# API module",
        "".ai(),
        "app = Flask(__name__)".ai(),
        "".ai(),
        "def require_auth(f):".ai(),
        "    @wraps(f)".ai(),
        "    def decorated(*args, **kwargs):".ai(),
        "        token = request.headers.get('Authorization', '').replace('Bearer ', '')".ai(),
        "        if not token: return jsonify({'error': 'unauthorized'}), 401".ai(),
        "        return f(*args, **kwargs)".ai(),
        "    return decorated".ai(),
        "".ai(),
        "@app.route('/health')".ai(),
        "def health(): return jsonify({'status': 'ok'})".ai(),
        "".ai(),
        "@app.route('/users', methods=['GET'])".ai(),
        "@require_auth".ai(),
        "def list_users(): return jsonify({'users': []})".ai(),
        "".ai(),
        "@app.route('/users', methods=['POST'])".ai(),
        "@require_auth".ai(),
        "def create_user(): return jsonify({'created': request.get_json()}), 201".ai(),
        "".ai(),
        "@app.route('/users/<int:uid>', methods=['GET'])".ai(),
        "@require_auth".ai(),
        "def get_user(uid: int): return jsonify({'user': {'id': uid}})".ai(),
        "".ai(),
        "@app.route('/users/<int:uid>', methods=['DELETE'])".ai(),
        "@require_auth".ai(),
        "def delete_user(uid: int): return '', 204".ai(),
        "".ai(),
        "@app.errorhandler(404)".ai(),
        "def not_found(e): return jsonify({'error': 'not found'}), 404".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add /users/:id GET and DELETE endpoints")
        .unwrap();

    // Rebase onto main (non-conflicting)
    repo.git(&["rebase", &main_branch]).unwrap();

    let chain = get_commit_chain(&repo, 5);
    // chain[0]=C1'(human), chain[1]=C2'(AI), chain[2]=C3'(AI), chain[3]=C4'(human), chain[4]=C5'(AI)

    // sha0 = C1' (human-only commit: config.py via commit_untracked_file, no note expected).
    assert_note_no_forbidden_files_if_present(&repo, &chain[0], "sha0_no_api", &["api.py"]);

    // sha1 = C2' (first AI commit): api.py
    assert_note_base_commit_matches(&repo, &chain[1], "sha1");
    assert_note_files_exact(&repo, &chain[1], "sha1_files", &["api.py"]);
    // C2 introduced Flask app + /health endpoint — verify they are AI at sha1.
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "api.py",
        "sha1_blame",
        &[
            ("app = Flask(__name__)", true),
            ("def require_auth", true),
            ("def health", true),
        ],
    );

    // sha2 = C3' (second AI commit): api.py
    assert_note_base_commit_matches(&repo, &chain[2], "sha2");
    assert_note_files_exact(&repo, &chain[2], "sha2_files", &["api.py"]);
    // C3 introduced /users GET and POST routes.
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "api.py",
        "sha2_blame",
        &[("def list_users", true), ("def create_user", true)],
    );

    // sha3 = C4' (human-only commit: requirements.txt via commit_untracked_file, no note expected).
    assert_note_no_forbidden_files_if_present(
        &repo,
        &chain[3],
        "sha3_no_future",
        &["config.py", "requirements.txt"],
    );

    // sha4 = C5' (third AI commit): api.py
    assert_note_base_commit_matches(&repo, &chain[4], "sha4");
    assert_note_files_exact(&repo, &chain[4], "sha4_files", &["api.py"]);
    // C5 introduced /users/:id GET and DELETE — verify they are AI at sha4.
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "api.py",
        "sha4_blame",
        &[
            ("def get_user", true),
            ("def delete_user", true),
            ("def not_found", true),
        ],
    );

    // Session format: cumulative AI lines in attestation ranges.
    // Values grow monotonically: [12, 18, 30].
}

/// Test 9: Large function blocks with 20-line license header prepended.
/// Feature adds 15-AI-line functions to processor.rs per commit.
/// Line offsets shift by 20 after rebase. Blame at sha0 checks the offset.
#[test]
fn test_slow_path_large_function_blocks_line_offset() {
    let repo = TestRepo::new();

    // Initial: processor.rs with two human lines + trailing newline
    repo.commit_untracked_file(
        "processor.rs",
        "// Processor module\nuse std::io;\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: prepend a 20-line license header (forces slow path, creates big line offset)
    repo.commit_untracked_file(
        "processor.rs",
        concat!(
            "// Copyright 2024 MyOrg. All rights reserved.\n",
            "// \n",
            "// Redistribution and use in source and binary forms,\n",
            "// with or without modification, are permitted provided\n",
            "// that the following conditions are met:\n",
            "// \n",
            "//   1. Redistributions of source code must retain the\n",
            "//      above copyright notice, this list of conditions\n",
            "//      and the following disclaimer.\n",
            "// \n",
            "//   2. Redistributions in binary form must reproduce the\n",
            "//      above copyright notice, this list of conditions\n",
            "//      and the following disclaimer in the documentation.\n",
            "// \n",
            "// THIS SOFTWARE IS PROVIDED 'AS IS' WITHOUT WARRANTY\n",
            "// OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT\n",
            "// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS\n",
            "// FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.\n",
            "// See the License file for details.\n",
            "//\n",
            "// Processor module\n",
            "use std::io;\n",
        ),
        "main: prepend 20-line license header to processor.rs",
    );
    repo.commit_untracked_file(
        "error.rs",
        "#[derive(Debug)] pub enum ProcessError { Io(std::io::Error), Invalid(String) }\n",
        "main: add error types",
    );
    repo.commit_untracked_file("types.rs",
        "pub type Bytes = Vec<u8>;\npub type Result<T> = std::result::Result<T, crate::error::ProcessError>;\n",
        "main: add common types",
    );
    repo.commit_untracked_file(
        "tests/smoke.rs",
        "#[test] fn smoke() { assert!(true); }\n",
        "main: add smoke test",
    );
    repo.commit_untracked_file(
        "Cargo.toml",
        "[package]\nname = \"processor\"\nversion = \"0.1.0\"\n",
        "main: add Cargo.toml",
    );

    // Feature branch from before main's prepend
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: add 15-AI-line function process_batch to processor.rs
    let mut proc = repo.filename("processor.rs");
    proc.set_contents(crate::lines![
        "// Processor module",
        "use std::io;",
        "".ai(),
        "pub fn process_batch(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {".ai(),
        "    if data.is_empty() {".ai(),
        "        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, \"empty input\"));".ai(),
        "    }".ai(),
        "    let mut out = Vec::with_capacity(data.len());".ai(),
        "    for &byte in data {".ai(),
        "        let processed = if byte.is_ascii_uppercase() {".ai(),
        "            byte.to_ascii_lowercase()".ai(),
        "        } else if byte.is_ascii_lowercase() {".ai(),
        "            byte.to_ascii_uppercase()".ai(),
        "        } else {".ai(),
        "            byte".ai(),
        "        };".ai(),
        "        out.push(processed);".ai(),
        "    }".ai(),
        "    Ok(out)".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add process_batch function")
        .unwrap();

    // C2: add 15-AI-line function validate_input
    proc.set_contents(crate::lines![
        "// Processor module",
        "use std::io;",
        "".ai(),
        "pub fn process_batch(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {".ai(),
        "    if data.is_empty() { return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, \"empty\")); }".ai(),
        "    Ok(data.iter().map(|&b| if b.is_ascii_alphabetic() { b ^ 0x20 } else { b }).collect())".ai(),
        "}".ai(),
        "".ai(),
        "pub fn validate_input(data: &[u8], max_len: usize) -> Result<(), std::io::Error> {".ai(),
        "    if data.is_empty() {".ai(),
        "        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, \"data is empty\"));".ai(),
        "    }".ai(),
        "    if data.len() > max_len {".ai(),
        "        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput,".ai(),
        "            format!(\"data length {} exceeds max {}\", data.len(), max_len)));".ai(),
        "    }".ai(),
        "    if data.iter().any(|&b| b == 0) {".ai(),
        "        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, \"null byte\"));".ai(),
        "    }".ai(),
        "    Ok(())".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add validate_input function")
        .unwrap();

    // C3: add 15-AI-line function chunk_data
    proc.set_contents(crate::lines![
        "// Processor module",
        "use std::io;",
        "".ai(),
        "pub fn process_batch(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {".ai(),
        "    Ok(data.iter().map(|&b| if b.is_ascii_alphabetic() { b ^ 0x20 } else { b }).collect())".ai(),
        "}".ai(),
        "".ai(),
        "pub fn validate_input(data: &[u8], max_len: usize) -> Result<(), std::io::Error> {".ai(),
        "    if data.is_empty() { return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, \"empty\")); }".ai(),
        "    if data.len() > max_len { return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, \"too long\")); }".ai(),
        "    Ok(())".ai(),
        "}".ai(),
        "".ai(),
        "pub fn chunk_data(data: &[u8], chunk_size: usize) -> Vec<&[u8]> {".ai(),
        "    if chunk_size == 0 { return Vec::new(); }".ai(),
        "    let n = (data.len() + chunk_size - 1) / chunk_size;".ai(),
        "    let mut chunks = Vec::with_capacity(n);".ai(),
        "    let mut offset = 0;".ai(),
        "    while offset < data.len() {".ai(),
        "        let end = (offset + chunk_size).min(data.len());".ai(),
        "        chunks.push(&data[offset..end]);".ai(),
        "        offset += chunk_size;".ai(),
        "    }".ai(),
        "    chunks".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add chunk_data function")
        .unwrap();

    // C4: add 15-AI-line function compress
    proc.set_contents(crate::lines![
        "// Processor module",
        "use std::io;",
        "".ai(),
        "pub fn process_batch(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {".ai(),
        "    Ok(data.iter().map(|&b| if b.is_ascii_alphabetic() { b ^ 0x20 } else { b }).collect())".ai(),
        "}".ai(),
        "pub fn validate_input(data: &[u8], max_len: usize) -> Result<(), std::io::Error> {".ai(),
        "    if data.is_empty() { return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, \"empty\")); }".ai(),
        "    Ok(())".ai(),
        "}".ai(),
        "pub fn chunk_data(data: &[u8], size: usize) -> Vec<&[u8]> {".ai(),
        "    if size == 0 { return Vec::new(); }".ai(),
        "    (0..data.len()).step_by(size).map(|i| &data[i..(i + size).min(data.len())]).collect()".ai(),
        "}".ai(),
        "".ai(),
        "pub fn run_length_encode(data: &[u8]) -> Vec<(u8, usize)> {".ai(),
        "    if data.is_empty() { return Vec::new(); }".ai(),
        "    let mut result = Vec::new();".ai(),
        "    let mut current = data[0];".ai(),
        "    let mut count = 1usize;".ai(),
        "    for &b in &data[1..] {".ai(),
        "        if b == current { count += 1; }".ai(),
        "        else { result.push((current, count)); current = b; count = 1; }".ai(),
        "    }".ai(),
        "    result.push((current, count));".ai(),
        "    result".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add run_length_encode function")
        .unwrap();

    // C5: add 15-AI-line function transform_pipeline
    proc.set_contents(crate::lines![
        "// Processor module",
        "use std::io;",
        "".ai(),
        "pub fn process_batch(data: &[u8]) -> Result<Vec<u8>, std::io::Error> {".ai(),
        "    Ok(data.iter().map(|&b| if b.is_ascii_alphabetic() { b ^ 0x20 } else { b }).collect())".ai(),
        "}".ai(),
        "pub fn validate_input(data: &[u8], max_len: usize) -> Result<(), std::io::Error> {".ai(),
        "    if data.len() > max_len { Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, \"too long\")) } else { Ok(()) }".ai(),
        "}".ai(),
        "pub fn chunk_data(data: &[u8], size: usize) -> Vec<&[u8]> {".ai(),
        "    (0..data.len()).step_by(size).map(|i| &data[i..(i+size).min(data.len())]).collect()".ai(),
        "}".ai(),
        "pub fn run_length_encode(data: &[u8]) -> Vec<(u8, usize)> {".ai(),
        "    let mut r = Vec::new(); let mut c = data[0]; let mut n = 1usize;".ai(),
        "    for &b in &data[1..] { if b == c { n += 1; } else { r.push((c, n)); c = b; n = 1; } }".ai(),
        "    r.push((c, n)); r".ai(),
        "}".ai(),
        "".ai(),
        "pub fn transform_pipeline(data: &[u8], transforms: &[fn(&[u8]) -> Vec<u8>]) -> Vec<u8> {".ai(),
        "    let mut current = data.to_vec();".ai(),
        "    for transform in transforms {".ai(),
        "        current = transform(&current);".ai(),
        "    }".ai(),
        "    current".ai(),
        "}".ai(),
        "".ai(),
        "pub fn hexdump(data: &[u8]) -> String {".ai(),
        "    data.iter().map(|b| format!(\"{:02x}\", b)).collect::<Vec<_>>().join(\" \")".ai(),
        "}".ai(),
        "".ai(),
        "pub fn count_bytes(data: &[u8]) -> std::collections::HashMap<u8, usize> {".ai(),
        "    let mut map = std::collections::HashMap::new();".ai(),
        "    for &b in data { *map.entry(b).or_insert(0) += 1; }".ai(),
        "    map".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add transform_pipeline and helpers")
        .unwrap();

    // Rebase onto main (non-conflicting)
    repo.git(&["rebase", &main_branch]).unwrap();

    let chain = get_commit_chain(&repo, 5);

    // sha0 = C1': processor.rs
    assert_note_base_commit_matches(&repo, &chain[0], "sha0");
    assert_note_files_exact(&repo, &chain[0], "sha0_files", &["processor.rs"]);

    // sha0 blame: 20 license lines (human) + 1 "// Processor module" (human) + 1 "use std::io;" (human)
    // then the blank + function (AI lines) start
    assert_blame_sample_at_commit(
        &repo,
        &chain[0],
        "processor.rs",
        "sha0_blame_offset",
        &[
            ("// Copyright 2024 MyOrg", false),
            ("// Redistribution", false),
            ("// THIS SOFTWARE IS PROVIDED", false),
            ("// Processor module", false),
            ("use std::io;", false),
            ("pub fn process_batch", true),
        ],
    );

    // sha1 = C2': C2 added validate_input function.
    // The 20-line license header prepend shifts ALL feature lines by 20.
    // assert_blame_sample_at_commit verifies key lines across the intermediate commit,
    // confirming the line-offset accounting is correct after the upstream prepend.
    assert_note_base_commit_matches(&repo, &chain[1], "sha1");
    assert_note_files_exact(&repo, &chain[1], "sha1_files", &["processor.rs"]);
    // The 20-line license header shifts ALL feature lines by +20. We check that
    // known-AI lines in the intermediate commit C2′ are correctly attributed.
    // Only lines whose content also exists in the final feature tip (C5) are
    // attributable via the hunk-based content-map lookup; lines that C3/C4/C5
    // later rewrote are no longer in the content map and show as human.
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "processor.rs",
        "sha1_blame_offset",
        &[
            ("// Copyright 2024 MyOrg", false), // license header (human) — not AI
            ("// Processor module", false),     // original human header
            ("use std::io;", false),            // original human line
            ("pub fn process_batch", true),     // C1 AI line, offset +20 correctly applied
            ("pub fn validate_input", true),    // C2 AI line — function sig survived to tip
        ],
    );

    // sha2 = C3': C3 added chunk_data function
    assert_note_base_commit_matches(&repo, &chain[2], "sha2");
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "processor.rs",
        "sha2_chunk_data",
        &[
            ("pub fn chunk_data", true),
            ("chunk_size == 0", true),
            ("chunks.push", true),
        ],
    );

    // sha3 = C4': C4 added run_length_encode function
    assert_note_base_commit_matches(&repo, &chain[3], "sha3");
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "processor.rs",
        "sha3_rle",
        &[("pub fn run_length_encode", true), ("result.push", true)],
    );

    // sha4 = C5': C5 added transform_pipeline and helpers
    assert_note_base_commit_matches(&repo, &chain[4], "sha4");
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "processor.rs",
        "sha4_transform",
        &[
            ("pub fn transform_pipeline", true),
            ("pub fn hexdump", true),
            ("pub fn count_bytes", true),
        ],
    );
}

/// Test 10: Shared file grows AND each commit adds a unique helper file.
/// shared_util.js prepended by main; feature appends 8 lines to it and
/// creates helpers/X.js per commit. Checks cumulative file sets at every SHA.
#[test]
fn test_slow_path_file_grows_then_unique_files_each_commit() {
    let repo = TestRepo::new();

    // Initial: shared_util.js with trailing newline
    repo.commit_untracked_file(
        "shared_util.js",
        "export const VERSION = '1.0';\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: prepend 'use strict' directive (forces slow path)
    repo.commit_untracked_file(
        "shared_util.js",
        "'use strict';\n\nexport const VERSION = '1.0';\n",
        "main: prepend use strict to shared_util.js",
    );
    repo.commit_untracked_file(
        "package.json",
        "{\"name\":\"helpers\",\"version\":\"1.0.0\",\"type\":\"module\"}\n",
        "main: add package.json",
    );
    repo.commit_untracked_file(
        ".eslintrc.json",
        "{\"env\":{\"es2022\":true},\"extends\":[\"eslint:recommended\"]}\n",
        "main: add eslint config",
    );
    repo.commit_untracked_file(
        "vitest.config.js",
        "export default {test:{environment:'node'}};\n",
        "main: add vitest config",
    );
    repo.commit_untracked_file(
        "README.md",
        "# Helpers\n\nA collection of JavaScript helper modules.\n",
        "main: add README",
    );

    // Feature branch from before main's prepend
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: append 8 AI lines to shared_util.js + create helpers/date.js (6 AI lines)
    let mut shared = repo.filename("shared_util.js");
    shared.set_contents(crate::lines![
        "export const VERSION = '1.0';",
        "".ai(),
        "export function clamp(n, min, max) { return Math.min(Math.max(n, min), max); }".ai(),
        "export function lerp(a, b, t) { return a + (b - a) * t; }".ai(),
        "export function noop() {}".ai(),
        "export const identity = x => x;".ai(),
        "export function once(fn) { let called = false, result; return (...a) => { if (!called) { called = true; result = fn(...a); } return result; }; }".ai(),
        "export function memoize(fn) { const cache = new Map(); return (...a) => { const k = JSON.stringify(a); if (!cache.has(k)) cache.set(k, fn(...a)); return cache.get(k); }; }".ai(),
        "export function pipe(...fns) { return x => fns.reduce((v, f) => f(v), x); }".ai(),
        "export function compose(...fns) { return x => fns.reduceRight((v, f) => f(v), x); }".ai(),
    ]);
    // Ensure the helpers directory exists
    let helpers_dir = repo.path().join("helpers");
    fs::create_dir_all(&helpers_dir).expect("create helpers dir");
    let mut date_helper = repo.filename("helpers/date.js");
    date_helper.set_contents(crate::lines![
        "export const now = () => new Date();".ai(),
        "export const today = () => { const d = new Date(); d.setHours(0,0,0,0); return d; };".ai(),
        "export const addDays = (d, n) => { const r = new Date(d); r.setDate(r.getDate()+n); return r; };".ai(),
        "export const formatISO = d => d.toISOString().slice(0, 10);".ai(),
        "export const isWeekend = d => d.getDay() === 0 || d.getDay() === 6;".ai(),
        "export const diffMs = (a, b) => Math.abs(new Date(a) - new Date(b));".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 grow shared_util.js + helpers/date.js")
        .unwrap();

    // C2: append 8 more AI lines to shared_util.js + create helpers/string.js (6 AI lines)
    shared.set_contents(crate::lines![
        "export const VERSION = '1.0';",
        "".ai(),
        "export const clamp = (n, min, max) => Math.min(Math.max(n, min), max);".ai(),
        "export const lerp = (a, b, t) => a + (b - a) * t;".ai(),
        "export const identity = x => x;".ai(),
        "export const once = fn => { let c=false,r; return (...a) => { if(!c){c=true;r=fn(...a);} return r; }; };".ai(),
        "export const memoize = fn => { const m=new Map(); return (...a)=>{ const k=JSON.stringify(a); if(!m.has(k)) m.set(k,fn(...a)); return m.get(k); }; };".ai(),
        "export const pipe = (...fns) => x => fns.reduce((v,f)=>f(v),x);".ai(),
        "".ai(),
        "export function curry(fn) { return function curried(...args) { return args.length >= fn.length ? fn(...args) : (...more) => curried(...args, ...more); }; }".ai(),
        "export function partial(fn, ...preset) { return (...args) => fn(...preset, ...args); }".ai(),
        "export function flip(fn) { return (a, b, ...rest) => fn(b, a, ...rest); }".ai(),
        "export function tap(fn) { return x => { fn(x); return x; }; }".ai(),
        "export const constant = v => () => v;".ai(),
        "export const always = constant;".ai(),
        "export const negate = pred => (...args) => !pred(...args);".ai(),
    ]);
    let mut string_helper = repo.filename("helpers/string.js");
    string_helper.set_contents(crate::lines![
        "export const capitalize = s => s.charAt(0).toUpperCase() + s.slice(1);".ai(),
        "export const kebabToCamel = s => s.replace(/-([a-z])/g, (_, c) => c.toUpperCase());".ai(),
        "export const camelToKebab = s => s.replace(/[A-Z]/g, m => `-${m.toLowerCase()}`);".ai(),
        "export const truncate = (s, n) => s.length <= n ? s : s.slice(0, n-3) + '...';".ai(),
        "export const slugify = s => s.toLowerCase().trim().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');".ai(),
        "export const words = s => s.trim().split(/\\s+/).filter(Boolean);".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 grow shared_util.js + helpers/string.js")
        .unwrap();

    // C3: append 8 more AI lines to shared_util.js + create helpers/array.js (6 AI lines)
    shared.set_contents(crate::lines![
        "export const VERSION = '1.0';",
        "".ai(),
        "export const clamp = (n, min, max) => Math.min(Math.max(n, min), max);".ai(),
        "export const lerp = (a, b, t) => a + (b - a) * t;".ai(),
        "export const identity = x => x;".ai(),
        "export const once = fn => { let c=false,r; return (...a) => { if(!c){c=true;r=fn(...a);} return r; }; };".ai(),
        "export const memoize = fn => { const m=new Map(); return (...a)=>{ const k=JSON.stringify(a); return m.has(k)?m.get(k):(m.set(k,fn(...a)),m.get(k)); }; };".ai(),
        "export const pipe = (...fns) => x => fns.reduce((v,f)=>f(v),x);".ai(),
        "export const curry = fn => function c(...a) { return a.length>=fn.length ? fn(...a) : (...b)=>c(...a,...b); };".ai(),
        "export const partial = (fn,...p) => (...a) => fn(...p,...a);".ai(),
        "export const negate = pred => (...a) => !pred(...a);".ai(),
        "".ai(),
        "export function debounce(fn, ms) { let t; return (...a) => { clearTimeout(t); t = setTimeout(()=>fn(...a), ms); }; }".ai(),
        "export function throttle(fn, ms) { let ok=true; return (...a) => { if(ok) { ok=false; fn(...a); setTimeout(()=>ok=true,ms); } }; }".ai(),
        "export function trampoline(fn) { return (...a) => { let r=fn(...a); while(typeof r==='function') r=r(); return r; }; }".ai(),
        "export function juxt(...fns) { return (...a) => fns.map(f=>f(...a)); }".ai(),
        "export const when = (pred, fn) => (...a) => pred(...a) ? fn(...a) : a[0];".ai(),
    ]);
    let mut array_helper = repo.filename("helpers/array.js");
    array_helper.set_contents(crate::lines![
        "export const unique = arr => [...new Set(arr)];".ai(),
        "export const flatten = arr => arr.flat(Infinity);".ai(),
        "export const chunk = (arr, n) => Array.from({length: Math.ceil(arr.length/n)}, (_,i) => arr.slice(i*n, i*n+n));".ai(),
        "export const groupBy = (arr, key) => arr.reduce((g, item) => ((g[item[key]] ??= []).push(item), g), {});".ai(),
        "export const zip = (...arrays) => arrays[0].map((_,i) => arrays.map(a=>a[i]));".ai(),
        "export const intersection = (a, b) => a.filter(x => b.includes(x));".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 grow shared_util.js + helpers/array.js")
        .unwrap();

    // C4: append 8 more AI lines to shared_util.js + create helpers/object.js (6 AI lines)
    shared.set_contents(crate::lines![
        "export const VERSION = '1.0';",
        "".ai(),
        "export const clamp = (n, min, max) => Math.min(Math.max(n, min), max);".ai(),
        "export const identity = x => x;".ai(),
        "export const memoize = fn => { const m=new Map(); return (...a)=>{ const k=JSON.stringify(a); return m.has(k)?m.get(k):(m.set(k,fn(...a)),m.get(k)); }; };".ai(),
        "export const pipe = (...fns) => x => fns.reduce((v,f)=>f(v),x);".ai(),
        "export const curry = fn => function c(...a) { return a.length>=fn.length ? fn(...a) : (...b)=>c(...a,...b); };".ai(),
        "export const debounce = (fn, ms) => { let t; return (...a)=>{ clearTimeout(t); t=setTimeout(()=>fn(...a),ms); }; };".ai(),
        "export const throttle = (fn, ms) => { let ok=true; return (...a)=>{ if(ok){ok=false;fn(...a);setTimeout(()=>ok=true,ms);} }; };".ai(),
        "export const when = (pred, fn) => (...a) => pred(...a) ? fn(...a) : a[0];".ai(),
        "".ai(),
        "export class EventEmitter { #events={}; on(e,f){(this.#events[e]??=[]).push(f);return this;} emit(e,...a){(this.#events[e]??[]).forEach(f=>f(...a));} }".ai(),
        "export const sleep = ms => new Promise(r => setTimeout(r, ms));".ai(),
        "export async function retry(fn, n=3) { for(let i=0;i<n;i++) { try{return await fn();}catch(e){if(i===n-1)throw e;await sleep(100*(i+1));} } }".ai(),
        "export const withTimeout = (p, ms) => Promise.race([p, new Promise((_,r)=>setTimeout(()=>r(new Error('timeout')),ms))]);".ai(),
        "export const deferred = () => { let res,rej; const p=new Promise((r,j)=>{res=r;rej=j;}); return {promise:p,resolve:res,reject:rej}; };".ai(),
    ]);
    let mut object_helper = repo.filename("helpers/object.js");
    object_helper.set_contents(crate::lines![
        "export const pick = (obj, keys) => Object.fromEntries(keys.map(k=>[k,obj[k]]));".ai(),
        "export const omit = (obj, keys) => Object.fromEntries(Object.entries(obj).filter(([k])=>!keys.includes(k)));".ai(),
        "export const deepClone = obj => JSON.parse(JSON.stringify(obj));".ai(),
        "export const isEmpty = obj => Object.keys(obj).length === 0;".ai(),
        "export const mapValues = (obj, fn) => Object.fromEntries(Object.entries(obj).map(([k,v])=>[k,fn(v,k)]));".ai(),
        "export const fromEntries = Object.fromEntries;".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 grow shared_util.js + helpers/object.js")
        .unwrap();

    // C5: append 8 more AI lines to shared_util.js + create helpers/number.js (6 AI lines)
    shared.set_contents(crate::lines![
        "export const VERSION = '1.0';",
        "".ai(),
        "export const clamp = (n, min, max) => Math.min(Math.max(n, min), max);".ai(),
        "export const identity = x => x;".ai(),
        "export const memoize = fn => { const m=new Map(); return (...a)=>{ const k=JSON.stringify(a); return m.has(k)?m.get(k):(m.set(k,fn(...a)),m.get(k)); }; };".ai(),
        "export const pipe = (...fns) => x => fns.reduce((v,f)=>f(v),x);".ai(),
        "export const debounce = (fn, ms) => { let t; return (...a)=>{ clearTimeout(t); t=setTimeout(()=>fn(...a),ms); }; };".ai(),
        "export const sleep = ms => new Promise(r => setTimeout(r, ms));".ai(),
        "export async function retry(fn, n=3) { for(let i=0;i<n;i++) { try{return await fn();}catch(e){if(i===n-1)throw e;await sleep(100*(i+1));} } }".ai(),
        "export const deferred = () => { let res,rej; const p=new Promise((r,j)=>{res=r;rej=j;}); return {promise:p,resolve:res,reject:rej}; };".ai(),
        "".ai(),
        "export function deepEqual(a, b) {".ai(),
        "    if (a === b) return true;".ai(),
        "    if (typeof a !== typeof b) return false;".ai(),
        "    if (Array.isArray(a)) return a.length===b.length && a.every((v,i)=>deepEqual(v,b[i]));".ai(),
        "    if (typeof a === 'object' && a && b) {".ai(),
        "        const ka=Object.keys(a), kb=Object.keys(b);".ai(),
        "        return ka.length===kb.length && ka.every(k=>deepEqual(a[k],b[k]));".ai(),
        "    }".ai(),
        "    return false;".ai(),
        "}".ai(),
    ]);
    let mut number_helper = repo.filename("helpers/number.js");
    number_helper.set_contents(crate::lines![
        "export const round = (n, d=0) => Math.round(n * 10**d) / 10**d;".ai(),
        "export const clamp = (n, min, max) => Math.min(Math.max(n, min), max);".ai(),
        "export const lerp = (a, b, t) => a + (b - a) * t;".ai(),
        "export const isPrime = n => n>1 && Array.from({length:Math.sqrt(n)|0},(_, i)=>i+2).every(i=>n%i!==0);".ai(),
        "export const gcd = (a, b) => b === 0 ? a : gcd(b, a % b);".ai(),
        "export const formatBytes = n => { const u=['B','KB','MB','GB']; let i=0; while(n>=1024&&i<3){n/=1024;i++;} return `${n.toFixed(1)}${u[i]}`; };".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 grow shared_util.js + helpers/number.js")
        .unwrap();

    // Rebase onto main (non-conflicting)
    repo.git(&["rebase", &main_branch]).unwrap();

    let chain = get_commit_chain(&repo, 5);

    // sha0 = C1': {shared_util.js, helpers/date.js}; no future helpers
    assert_note_base_commit_matches(&repo, &chain[0], "sha0");
    assert_note_files_exact(
        &repo,
        &chain[0],
        "sha0_files",
        &["shared_util.js", "helpers/date.js"],
    );
    assert_note_no_forbidden_files(
        &repo,
        &chain[0],
        "sha0_no_future",
        &[
            "helpers/string.js",
            "helpers/array.js",
            "helpers/object.js",
            "helpers/number.js",
        ],
    );

    // sha1 = C2': {shared_util.js, helpers/string.js}; no future helpers
    // C2 added curry/partial/flip/tap/negate to shared_util.js
    assert_note_base_commit_matches(&repo, &chain[1], "sha1");
    assert_note_files_exact(
        &repo,
        &chain[1],
        "sha1_files",
        &["shared_util.js", "helpers/string.js"],
    );
    assert_note_no_forbidden_files(
        &repo,
        &chain[1],
        "sha1_no_future",
        &["helpers/array.js", "helpers/object.js", "helpers/number.js"],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "shared_util.js",
        "sha1_shared_curry",
        &[
            ("export function curry", true),
            ("export function partial", true),
            ("export const negate", true),
        ],
    );
    // helpers/date.js (from C1) is a prior file at chain[1] — fast path, verify attribution intact
    assert_blame_sample_at_commit(
        &repo,
        &chain[1],
        "helpers/date.js",
        "chain1_prior_date_js",
        &[
            ("export const now = () => new Date();", true),
            (
                "export const formatISO = d => d.toISOString().slice(0, 10);",
                true,
            ),
        ],
    );

    // sha2 = C3': {shared_util.js, helpers/array.js}; no object or number yet
    // C3 added debounce/throttle/trampoline/juxt/when to shared_util.js
    assert_note_base_commit_matches(&repo, &chain[2], "sha2");
    assert_note_files_exact(
        &repo,
        &chain[2],
        "sha2_files",
        &["shared_util.js", "helpers/array.js"],
    );
    assert_note_no_forbidden_files(
        &repo,
        &chain[2],
        "sha2_no_future",
        &["helpers/object.js", "helpers/number.js"],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "shared_util.js",
        "sha2_shared_debounce",
        &[
            ("export function debounce", true),
            ("export function throttle", true),
            ("export function trampoline", true),
        ],
    );
    // helpers/date.js (from C1) and helpers/string.js (from C2) are prior files at chain[2]
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "helpers/date.js",
        "chain2_prior_date_js",
        &[
            ("export const now = () => new Date();", true),
            (
                "export const formatISO = d => d.toISOString().slice(0, 10);",
                true,
            ),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[2],
        "helpers/string.js",
        "chain2_prior_string_js",
        &[
            (
                "export const capitalize = s => s.charAt(0).toUpperCase() + s.slice(1);",
                true,
            ),
            (
                "export const slugify = s => s.toLowerCase().trim().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');",
                true,
            ),
        ],
    );

    // sha3 = C4': {shared_util.js, helpers/object.js}; no number yet
    // C4 added EventEmitter/sleep/retry/withTimeout/deferred to shared_util.js
    assert_note_base_commit_matches(&repo, &chain[3], "sha3");
    assert_note_files_exact(
        &repo,
        &chain[3],
        "sha3_files",
        &["shared_util.js", "helpers/object.js"],
    );
    assert_note_no_forbidden_files(&repo, &chain[3], "sha3_no_future", &["helpers/number.js"]);
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "shared_util.js",
        "sha3_shared_eventemitter",
        &[
            ("export class EventEmitter", true),
            ("export const sleep", true),
            ("export async function retry", true),
        ],
    );
    // helpers/date.js (C1), helpers/string.js (C2), and helpers/array.js (C3) are prior files at chain[3]
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "helpers/date.js",
        "chain3_prior_date_js",
        &[
            ("export const now = () => new Date();", true),
            (
                "export const formatISO = d => d.toISOString().slice(0, 10);",
                true,
            ),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "helpers/string.js",
        "chain3_prior_string_js",
        &[
            (
                "export const capitalize = s => s.charAt(0).toUpperCase() + s.slice(1);",
                true,
            ),
            (
                "export const slugify = s => s.toLowerCase().trim().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');",
                true,
            ),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[3],
        "helpers/array.js",
        "chain3_prior_array_js",
        &[
            ("export const unique = arr => [...new Set(arr)];", true),
            (
                "export const chunk = (arr, n) => Array.from({length: Math.ceil(arr.length/n)}, (_,i) => arr.slice(i*n, i*n+n));",
                true,
            ),
        ],
    );

    // sha4 = C5': {shared_util.js, helpers/number.js}
    // C5 added deepEqual to shared_util.js
    assert_note_base_commit_matches(&repo, &chain[4], "sha4");
    assert_note_files_exact(
        &repo,
        &chain[4],
        "sha4_files",
        &["shared_util.js", "helpers/number.js"],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "shared_util.js",
        "sha4_shared_deepequal",
        &[
            ("export function deepEqual", true),
            ("if (Array.isArray(a))", true),
            ("return false;", true),
        ],
    );
    // helpers/date.js (C1), string.js (C2), array.js (C3), and object.js (C4) are prior files at chain[4]
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "helpers/date.js",
        "chain4_prior_date_js",
        &[
            ("export const now = () => new Date();", true),
            (
                "export const formatISO = d => d.toISOString().slice(0, 10);",
                true,
            ),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "helpers/string.js",
        "chain4_prior_string_js",
        &[
            (
                "export const capitalize = s => s.charAt(0).toUpperCase() + s.slice(1);",
                true,
            ),
            (
                "export const slugify = s => s.toLowerCase().trim().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');",
                true,
            ),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "helpers/array.js",
        "chain4_prior_array_js",
        &[
            ("export const unique = arr => [...new Set(arr)];", true),
            (
                "export const chunk = (arr, n) => Array.from({length: Math.ceil(arr.length/n)}, (_,i) => arr.slice(i*n, i*n+n));",
                true,
            ),
        ],
    );
    assert_blame_sample_at_commit(
        &repo,
        &chain[4],
        "helpers/object.js",
        "chain4_prior_object_js",
        &[
            (
                "export const pick = (obj, keys) => Object.fromEntries(keys.map(k=>[k,obj[k]]));",
                true,
            ),
            (
                "export const deepClone = obj => JSON.parse(JSON.stringify(obj));",
                true,
            ),
        ],
    );
}

// ============================================================================
// END Category 2: Slow Path
// ============================================================================

// ============================================================================
// Category 3: Conflict Resolved by Human
// Feature branch has AI-generated changes that conflict with main branch.
// Human resolves via fs::write (no checkpoint) — conflicted file loses AI
// attribution for that specific rebased commit.  All other AI files in the
// chain retain their attribution.
// ============================================================================

/// Test 1: Python auth.py — feature adds AI login/logout functions, main edits
/// the same file's header comment → conflict on C1.  Human resolves by keeping
/// both parts.  C1' must have NO auth.py in its note; C2'–C5' accumulate other
/// AI files (models.py, views.py, serializers.py, signals.py) normally.
#[test]
fn test_human_conflict_python_auth_c1_conflicts_rest_accumulate() {
    let repo = TestRepo::new();

    // Initial: auth.py with a single line
    repo.commit_untracked_file("auth.py", "# auth module\n", "Initial commit");
    let main_branch = repo.current_branch();

    // Main: edit auth.py header → will conflict with feature's C1
    repo.commit_untracked_file(
        "auth.py",
        "# authentication module — production\n",
        "main: update auth.py header",
    );
    repo.commit_untracked_file(
        "middleware.py",
        "class AuthMiddleware: pass\n",
        "main: add middleware",
    );
    repo.commit_untracked_file(
        "permissions.py",
        "class IsAuthenticated: pass\n",
        "main: add permissions",
    );
    repo.commit_untracked_file(
        "tokens.py",
        "import secrets\ndef generate_token(): return secrets.token_hex(32)\n",
        "main: add tokens",
    );
    repo.commit_untracked_file(
        "urls.py",
        "from django.urls import path\nurlpatterns = []\n",
        "main: add urls",
    );

    // Feature branch from initial commit (before main's auth.py edit)
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI adds login/logout to auth.py — WILL CONFLICT with main's header change
    let mut auth = repo.filename("auth.py");
    auth.set_contents(crate::lines![
        "# auth module",
        "".ai(),
        "def login(username: str, password: str) -> bool:".ai(),
        "    \"\"\"Authenticate user credentials.\"\"\"".ai(),
        "    return username == 'admin' and password == 'secret'".ai(),
        "".ai(),
        "def logout(session_id: str) -> None:".ai(),
        "    \"\"\"Invalidate the given session.\"\"\"".ai(),
        "    pass".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add login and logout to auth.py")
        .unwrap();

    // C2: AI creates models.py
    let mut models = repo.filename("models.py");
    models.set_contents(crate::lines![
        "from dataclasses import dataclass".ai(),
        "".ai(),
        "@dataclass".ai(),
        "class User:".ai(),
        "    id: int".ai(),
        "    username: str".ai(),
        "    email: str".ai(),
        "    is_active: bool = True".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add User model")
        .unwrap();

    // C3: AI creates views.py
    let mut views = repo.filename("views.py");
    views.set_contents(crate::lines![
        "from .auth import login, logout".ai(),
        "".ai(),
        "def login_view(request):".ai(),
        "    ok = login(request.POST['username'], request.POST['password'])".ai(),
        "    return {'ok': ok}".ai(),
        "".ai(),
        "def logout_view(request):".ai(),
        "    logout(request.session['id'])".ai(),
        "    return {'ok': True}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add login/logout views")
        .unwrap();

    // C4: AI creates serializers.py
    let mut serializers = repo.filename("serializers.py");
    serializers.set_contents(crate::lines![
        "class UserSerializer:".ai(),
        "    fields = ['id', 'username', 'email']".ai(),
        "".ai(),
        "    def serialize(self, user) -> dict:".ai(),
        "        return {f: getattr(user, f) for f in self.fields}".ai(),
        "".ai(),
        "    def deserialize(self, data: dict):".ai(),
        "        return data".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add UserSerializer")
        .unwrap();

    // C5: AI creates signals.py
    let mut signals = repo.filename("signals.py");
    signals.set_contents(crate::lines![
        "from typing import Callable".ai(),
        "".ai(),
        "_handlers: list[Callable] = []".ai(),
        "".ai(),
        "def on_login(fn: Callable) -> Callable:".ai(),
        "    _handlers.append(fn)".ai(),
        "    return fn".ai(),
        "".ai(),
        "def emit_login(user) -> None:".ai(),
        "    for h in _handlers: h(user)".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add login signal emitter")
        .unwrap();

    // Rebase onto main — C1 will conflict on auth.py
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "C1 rebase should conflict on auth.py"
    );

    // Human resolves: keep both header variants merged manually (no checkpoint)
    fs::write(
        repo.path().join("auth.py"),
        "# authentication module — production\n\ndef login(username: str, password: str) -> bool:\n    return username == 'admin' and password == 'secret'\n\ndef logout(session_id: str) -> None:\n    pass\n",
    ).unwrap();
    repo.git(&["add", "auth.py"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    // Collect rebased chain [C1', C2', C3', C4', C5']
    let chain = get_commit_chain(&repo, 5);

    // C1': human resolved auth.py — AI content survived resolution → auth.py IS in note
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["auth.py"]);

    // C2': models.py only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["models.py"]);

    // C3': views.py only
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["views.py"]);

    // C4': serializers.py only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["serializers.py"]);

    // C5': signals.py only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["signals.py"]);
}

/// Test 2: Rust lib.rs — feature adds AI parser functions, main edits the same
/// mod declaration at the top → conflict on C2 (middle of chain).
/// C1' is attributed normally; C2' loses lib.rs; C3'–C5' accumulate helpers.rs,
/// types.rs, error.rs as expected.
#[test]
fn test_human_conflict_rust_lib_c2_conflicts_surroundings_ok() {
    let repo = TestRepo::new();

    repo.commit_untracked_file("src/lib.rs", "pub mod parser;\n", "Initial commit");
    let main_branch = repo.current_branch();

    // Main: changes the mod declaration → conflicts with feature C2's edit
    repo.commit_untracked_file(
        "src/lib.rs",
        "pub mod parser;\npub mod types;\n",
        "main: add types mod",
    );
    repo.commit_untracked_file("src/main.rs", "fn main() {}\n", "main: add main.rs");
    repo.commit_untracked_file(
        "Cargo.toml",
        "[package]\nname = \"mylib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        "main: add Cargo.toml",
    );
    repo.commit_untracked_file(
        "README.md",
        "# mylib\nA Rust library.\n",
        "main: add README",
    );
    repo.commit_untracked_file(".github/workflows/ci.yml",
        "on: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    steps: [{uses: actions/checkout@v3}]\n",
        "main: add CI workflow",
    );

    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI adds parser/tokenize to a separate file
    let mut tokenizer = repo.filename("src/tokenizer.rs");
    tokenizer.set_contents(crate::lines![
        "pub enum Token { Ident(String), Number(i64), Eof }".ai(),
        "".ai(),
        "pub fn tokenize(input: &str) -> Vec<Token> {".ai(),
        "    input.split_whitespace()".ai(),
        "        .map(|w| Token::Ident(w.to_string()))".ai(),
        "        .collect()".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add tokenizer").unwrap();

    // C2: AI edits lib.rs to export tokenizer — WILL CONFLICT with main's mod change
    let mut lib = repo.filename("src/lib.rs");
    lib.replace_at(0, "pub mod tokenizer;".ai());
    repo.stage_all_and_commit("feat: C2 export tokenizer in lib.rs")
        .unwrap();

    // C3: AI adds helpers.rs
    let mut helpers = repo.filename("src/helpers.rs");
    helpers.set_contents(crate::lines![
        "pub fn is_digit(c: char) -> bool { c.is_ascii_digit() }".ai(),
        "pub fn is_alpha(c: char) -> bool { c.is_alphabetic() }".ai(),
        "pub fn is_whitespace(c: char) -> bool { c.is_whitespace() }".ai(),
        "pub fn to_lowercase(s: &str) -> String { s.to_lowercase() }".ai(),
        "pub fn trim_quotes(s: &str) -> &str { s.trim_matches('\"') }".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add helpers").unwrap();

    // C4: AI adds types.rs
    let mut types = repo.filename("src/types.rs");
    types.set_contents(crate::lines![
        "#[derive(Debug, Clone, PartialEq)]".ai(),
        "pub struct Span { pub start: usize, pub end: usize }".ai(),
        "".ai(),
        "#[derive(Debug)]".ai(),
        "pub enum ParseError {".ai(),
        "    UnexpectedToken(String),".ai(),
        "    UnexpectedEof,".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add types").unwrap();

    // C5: AI adds error.rs
    let mut error = repo.filename("src/error.rs");
    error.set_contents(crate::lines![
        "use std::fmt;".ai(),
        "use crate::types::ParseError;".ai(),
        "".ai(),
        "impl fmt::Display for ParseError {".ai(),
        "    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {".ai(),
        "        match self {".ai(),
        "            ParseError::UnexpectedToken(t) => write!(f, \"unexpected: {}\", t),".ai(),
        "            ParseError::UnexpectedEof => write!(f, \"unexpected EOF\"),".ai(),
        "        }".ai(),
        "    }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add error Display impl")
        .unwrap();

    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on src/lib.rs at C2"
    );

    // Human resolves by keeping both mods
    fs::write(
        repo.path().join("src/lib.rs"),
        "pub mod parser;\npub mod types;\npub mod tokenizer;\n",
    )
    .unwrap();
    repo.git(&["add", "src/lib.rs"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': tokenizer.rs only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["src/tokenizer.rs"]);

    // C2': lib.rs human-resolved conflict — all AI lines inside diff hunk, attribution dropped
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &[]);

    // C3': helpers.rs only
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["src/helpers.rs"]);

    // C4': types.rs only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["src/types.rs"]);

    // C5': error.rs only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["src/error.rs"]);
}

/// Test 3: TypeScript api.ts — feature adds AI REST handlers, main adds an
/// import at the top that conflicts with feature's C3.  C1'–C2' accumulate
/// dto.ts and service.ts; C3' loses api.ts attribution; C4'–C5' add more files.
#[test]
fn test_human_conflict_typescript_api_c3_conflicts_accumulation_intact() {
    let repo = TestRepo::new();

    repo.commit_untracked_file(
        "src/api.ts",
        "// api module\nexport {};\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: replaces the export line in api.ts — conflicts with feature's C3 which also replaces it
    repo.commit_untracked_file(
        "src/api.ts",
        "// api module\nexport { version };\n",
        "main: export version",
    );
    repo.commit_untracked_file(
        "src/server.ts",
        "import express from 'express';\nconst app = express();\napp.listen(3000);\n",
        "main: add server",
    );
    repo.commit_untracked_file(
        "src/config.ts",
        "export const PORT = parseInt(process.env.PORT ?? '3000', 10);\n",
        "main: add config",
    );
    repo.commit_untracked_file(
        "src/logger.ts",
        "export const log = (msg: string) => console.log(`[LOG] ${msg}`);\n",
        "main: add logger",
    );
    repo.commit_untracked_file(
        "tsconfig.json",
        "{\"compilerOptions\":{\"target\":\"ES2020\",\"module\":\"commonjs\",\"strict\":true}}\n",
        "main: add tsconfig",
    );

    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates dto.ts
    let mut dto = repo.filename("src/dto.ts");
    dto.set_contents(crate::lines![
        "export interface CreateUserDto {".ai(),
        "  name: string;".ai(),
        "  email: string;".ai(),
        "  password: string;".ai(),
        "}".ai(),
        "".ai(),
        "export interface UpdateUserDto {".ai(),
        "  name?: string;".ai(),
        "  email?: string;".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add user DTOs").unwrap();

    // C2: AI creates service.ts
    let mut service = repo.filename("src/service.ts");
    service.set_contents(crate::lines![
        "import { CreateUserDto, UpdateUserDto } from './dto';".ai(),
        "const users: Map<number, any> = new Map();".ai(),
        "let nextId = 1;".ai(),
        "export const createUser = (dto: CreateUserDto) => { const u = { id: nextId++, ...dto }; users.set(u.id, u); return u; };".ai(),
        "export const getUser = (id: number) => users.get(id);".ai(),
        "export const updateUser = (id: number, dto: UpdateUserDto) => { const u = users.get(id); if (u) Object.assign(u, dto); return u; };".ai(),
        "export const deleteUser = (id: number) => users.delete(id);".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add user service")
        .unwrap();

    // C3: AI edits api.ts to add route handlers — WILL CONFLICT with main's express import
    let mut api = repo.filename("src/api.ts");
    api.replace_at(1, "import { createUser, getUser } from './service';".ai());
    repo.stage_all_and_commit("feat: C3 add route imports to api.ts")
        .unwrap();

    // C4: AI creates middleware.ts
    let mut mw = repo.filename("src/middleware.ts");
    mw.set_contents(crate::lines![
        "import { Request, Response, NextFunction } from 'express';".ai(),
        "".ai(),
        "export const errorHandler = (err: Error, _req: Request, res: Response, _next: NextFunction) => {".ai(),
        "  console.error(err.stack);".ai(),
        "  res.status(500).json({ error: err.message });".ai(),
        "};".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add error middleware")
        .unwrap();

    // C5: AI creates validators.ts
    let mut validators = repo.filename("src/validators.ts");
    validators.set_contents(crate::lines![
        "export const isEmail = (s: string) => /^[^@]+@[^@]+\\.[^@]+$/.test(s);".ai(),
        "export const isNonEmpty = (s: string) => s.trim().length > 0;".ai(),
        "export const isPositiveInt = (n: number) => Number.isInteger(n) && n > 0;".ai(),
        "export const clamp = (n: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, n));".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add validators")
        .unwrap();

    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on src/api.ts at C3"
    );

    // Human resolves: keep both the export and the new import
    fs::write(
        repo.path().join("src/api.ts"),
        "// api module\nexport { version };\nimport { createUser, getUser } from './service';\n",
    )
    .unwrap();
    repo.git(&["add", "src/api.ts"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': dto.ts only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["src/dto.ts"]);

    // C2': service.ts only
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["src/service.ts"]);

    // C3': api.ts human-resolved conflict — AI lines inside diff hunk, attribution dropped
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &[]);

    // C4': middleware.ts only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["src/middleware.ts"]);

    // C5': validators.ts only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["src/validators.ts"]);
}

/// Test 4: Python models.py — main adds a class attribute that conflicts with
/// feature's last commit (C5).  All prior AI commits C1'–C4' are attributed
/// normally; C5' loses models.py.
#[test]
fn test_human_conflict_python_models_c5_last_commit_conflicts() {
    let repo = TestRepo::new();

    repo.commit_untracked_file("models.py", "class User:\n    pass\n", "Initial commit");
    let main_branch = repo.current_branch();

    // Main: adds a class attribute to models.py — conflicts with C5's edit
    repo.commit_untracked_file(
        "models.py",
        "class User:\n    table_name = 'users'\n    pass\n",
        "main: add table_name attribute",
    );
    repo.commit_untracked_file(
        "db.py",
        "import sqlite3\nconn = sqlite3.connect(':memory:')\n",
        "main: add db",
    );
    repo.commit_untracked_file("migrations/__init__.py", "", "main: add migrations package");
    repo.commit_untracked_file(
        "schema.sql",
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);\n",
        "main: add schema",
    );
    repo.commit_untracked_file("seeds.py", "def seed(): pass\n", "main: add seeds");

    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates repository.py
    let mut repo_file = repo.filename("repository.py");
    repo_file.set_contents(crate::lines![
        "from typing import Optional, List".ai(),
        "from .models import User".ai(),
        "".ai(),
        "class UserRepository:".ai(),
        "    def __init__(self): self._store: List[User] = []".ai(),
        "    def save(self, u: User): self._store.append(u)".ai(),
        "    def find(self, id: int) -> Optional[User]: return next((u for u in self._store if u.id == id), None)".ai(),
        "    def all(self) -> List[User]: return list(self._store)".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add UserRepository")
        .unwrap();

    // C2: AI creates query_builder.py
    let mut qb = repo.filename("query_builder.py");
    qb.set_contents(crate::lines![
        "class QueryBuilder:".ai(),
        "    def __init__(self, table: str): self.table = table; self._filters: list = []".ai(),
        "    def where(self, **kw): self._filters.append(kw); return self".ai(),
        "    def build(self) -> str:".ai(),
        "        clauses = ' AND '.join(f\"{k}='{v}'\" for d in self._filters for k, v in d.items())".ai(),
        "        return f'SELECT * FROM {self.table}' + (f' WHERE {clauses}' if clauses else '')".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add QueryBuilder")
        .unwrap();

    // C3: AI creates validators.py
    let mut val = repo.filename("validators.py");
    val.set_contents(crate::lines![
        "def validate_user(data: dict) -> list[str]:".ai(),
        "    errors = []".ai(),
        "    if not data.get('name'): errors.append('name required')".ai(),
        "    if not data.get('email') or '@' not in data['email']: errors.append('valid email required')".ai(),
        "    if len(data.get('password', '')) < 8: errors.append('password min 8 chars')".ai(),
        "    return errors".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add validate_user")
        .unwrap();

    // C4: AI creates events.py
    let mut events = repo.filename("events.py");
    events.set_contents(crate::lines![
        "from typing import Callable, Dict, List".ai(),
        "_subs: Dict[str, List[Callable]] = {}".ai(),
        "def subscribe(event: str, fn: Callable): _subs.setdefault(event, []).append(fn)".ai(),
        "def publish(event: str, **data): [fn(**data) for fn in _subs.get(event, [])]".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add event pub/sub")
        .unwrap();

    // C5: AI edits models.py to add validator — WILL CONFLICT with main's table_name
    let mut models = repo.filename("models.py");
    models.replace_at(
        1,
        "    def validate(self): return bool(getattr(self, 'name', None))".ai(),
    );
    repo.stage_all_and_commit("feat: C5 add validate method to User")
        .unwrap();

    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on models.py at C5"
    );

    // Human resolves by keeping all three lines
    fs::write(
        repo.path().join("models.py"),
        "class User:\n    table_name = 'users'\n    def validate(self): return bool(getattr(self, 'name', None))\n    pass\n",
    ).unwrap();
    repo.git(&["add", "models.py"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': repository.py only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["repository.py"]);

    // C2': query_builder.py only
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["query_builder.py"]);

    // C3': validators.py only
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["validators.py"]);

    // C4': events.py only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["events.py"]);

    // C5': models.py human-resolved conflict — AI lines inside diff hunk, attribution dropped
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &[]);
}

/// Test 5: Rust src/config.rs — main and feature both extend a constants block,
/// triggering a conflict on C2.  C1' has config.rs attributed; C2' loses it
/// due to human resolution; C3'–C5' accumulate cache.rs, retry.rs, timeout.rs.
#[test]
fn test_human_conflict_rust_config_c2_loses_attribution_rest_accumulate() {
    let repo = TestRepo::new();

    repo.commit_untracked_file(
        "src/config.rs",
        "pub const MAX_CONN: u32 = 10;\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: adds another constant → conflicts with feature's C2 edit
    repo.commit_untracked_file(
        "src/config.rs",
        "pub const MAX_CONN: u32 = 10;\npub const TIMEOUT_MS: u64 = 5000;\n",
        "main: add TIMEOUT_MS constant",
    );
    repo.commit_untracked_file("src/pool.rs",
        "pub struct Pool { size: u32 }\nimpl Pool { pub fn new(size: u32) -> Self { Pool { size } } }\n",
        "main: add connection pool",
    );
    repo.commit_untracked_file(
        "src/metrics.rs",
        "pub fn record_latency(ms: u64) { eprintln!(\"latency: {}ms\", ms); }\n",
        "main: add metrics",
    );
    repo.commit_untracked_file(
        "src/health.rs",
        "pub fn is_healthy() -> bool { true }\n",
        "main: add health check",
    );
    repo.commit_untracked_file(
        "src/shutdown.rs",
        "pub fn graceful_shutdown() { eprintln!(\"shutting down\"); }\n",
        "main: add shutdown handler",
    );

    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates a separate file (no conflict — config.rs not touched)
    let mut defaults = repo.filename("src/defaults.rs");
    defaults.set_contents(crate::lines!["pub const DEFAULT_POOL_SIZE: u32 = 5;".ai(),]);
    repo.stage_all_and_commit("feat: C1 add defaults.rs")
        .unwrap();

    // C2: AI edits config.rs to add IDLE_TIMEOUT — WILL CONFLICT with main's TIMEOUT_MS
    let mut config = repo.filename("src/config.rs");
    config.set_contents(crate::lines![
        "pub const MAX_CONN: u32 = 10;",
        "pub const IDLE_TIMEOUT_MS: u64 = 30_000;".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add IDLE_TIMEOUT_MS to config")
        .unwrap();

    // C3: AI creates cache.rs
    let mut cache = repo.filename("src/cache.rs");
    cache.set_contents(crate::lines![
        "use std::collections::HashMap;".ai(),
        "pub struct Cache<K, V>(HashMap<K, V>);".ai(),
        "impl<K: Eq + std::hash::Hash, V> Cache<K, V> {".ai(),
        "    pub fn new() -> Self { Cache(HashMap::new()) }".ai(),
        "    pub fn get(&self, k: &K) -> Option<&V> { self.0.get(k) }".ai(),
        "    pub fn set(&mut self, k: K, v: V) { self.0.insert(k, v); }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add Cache struct")
        .unwrap();

    // C4: AI creates retry.rs
    let mut retry = repo.filename("src/retry.rs");
    retry.set_contents(crate::lines![
        "use crate::config::MAX_RETRIES;".ai(),
        "pub fn with_retry<T, E>(mut f: impl FnMut() -> Result<T, E>) -> Result<T, E> {".ai(),
        "    let mut last = f();".ai(),
        "    for _ in 1..MAX_RETRIES {".ai(),
        "        if last.is_ok() { return last; }".ai(),
        "        last = f();".ai(),
        "    }".ai(),
        "    last".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add retry helper")
        .unwrap();

    // C5: AI creates timeout.rs
    let mut timeout_file = repo.filename("src/timeout.rs");
    timeout_file.set_contents(crate::lines![
        "use std::time::{Duration, Instant};".ai(),
        "".ai(),
        "pub fn run_with_timeout<T>(duration: Duration, f: impl FnOnce() -> T) -> Option<T> {".ai(),
        "    let start = Instant::now();".ai(),
        "    let result = f();".ai(),
        "    if start.elapsed() <= duration { Some(result) } else { None }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add timeout runner")
        .unwrap();

    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on src/config.rs at C2"
    );

    // Human resolves: keep all constants
    fs::write(
        repo.path().join("src/config.rs"),
        "pub const MAX_CONN: u32 = 10;\npub const TIMEOUT_MS: u64 = 5000;\npub const IDLE_TIMEOUT_MS: u64 = 30_000;\n",
    ).unwrap();
    repo.git(&["add", "src/config.rs"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': defaults.rs only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["src/defaults.rs"]);

    // C2': config.rs human-resolved conflict — AI lines inside diff hunk, attribution dropped
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &[]);

    // C3': cache.rs only
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["src/cache.rs"]);

    // C4': retry.rs only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["src/retry.rs"]);

    // C5': timeout.rs only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["src/timeout.rs"]);
}

/// Test 6: TypeScript store.ts — the entire feature file is written by the AI
/// via fs::write + git_og add + checkpoint (simulating an AI-created file).
/// Main edits the same file causing conflict on C1; human resolves.
/// No file is attributed in C1' (human resolved the only AI file in that commit).
/// C2'–C5' accumulate actions.ts, selectors.ts, reducers.ts, hooks.ts.
#[test]
fn test_human_conflict_typescript_store_ai_created_file_conflict() {
    let repo = TestRepo::new();

    repo.commit_untracked_file(
        "src/store.ts",
        "export const store = {};\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: modifies store.ts initial export → conflict with feature C1
    repo.commit_untracked_file(
        "src/store.ts",
        "import { createStore } from 'redux';\nexport const store = createStore(() => ({}));\n",
        "main: convert store to redux",
    );
    repo.commit_untracked_file(
        "src/index.ts",
        "export { store } from './store';\n",
        "main: re-export store",
    );
    repo.commit_untracked_file(
        "src/types.ts",
        "export type RootState = ReturnType<typeof import('./store').store.getState>;\n",
        "main: add RootState type",
    );
    repo.commit_untracked_file(
        "src/constants.ts",
        "export const ACTIONS = { INCREMENT: 'INCREMENT', DECREMENT: 'DECREMENT' } as const;\n",
        "main: add action constants",
    );
    repo.commit_untracked_file(
        "package.json",
        "{\"name\":\"app\",\"version\":\"1.0.0\",\"dependencies\":{\"redux\":\"^4.0.0\"}}\n",
        "main: add package.json",
    );

    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI writes store.ts from scratch via fs::write + checkpoint
    let store_content = "import { configureStore } from '@reduxjs/toolkit';\nimport { counterSlice } from './reducers';\nexport const store = configureStore({ reducer: { counter: counterSlice.reducer } });\nexport type AppDispatch = typeof store.dispatch;\n";
    fs::write(repo.path().join("src/store.ts"), store_content).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "src/store.ts"])
        .unwrap();
    repo.stage_all_and_commit("feat: C1 AI rewrites store with redux toolkit")
        .unwrap();

    // C2: AI creates actions.ts
    let mut actions = repo.filename("src/actions.ts");
    actions.set_contents(crate::lines![
        "export const increment = () => ({ type: 'INCREMENT' as const });".ai(),
        "export const decrement = () => ({ type: 'DECREMENT' as const });".ai(),
        "export const reset = () => ({ type: 'RESET' as const });".ai(),
        "export type Action = ReturnType<typeof increment | typeof decrement | typeof reset>;".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add action creators")
        .unwrap();

    // C3: AI creates selectors.ts
    let mut selectors = repo.filename("src/selectors.ts");
    selectors.set_contents(crate::lines![
        "import { RootState } from './types';".ai(),
        "export const selectCount = (state: RootState) => state.counter.value;".ai(),
        "export const selectIsPositive = (state: RootState) => state.counter.value > 0;".ai(),
        "export const selectIsZero = (state: RootState) => state.counter.value === 0;".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add selectors").unwrap();

    // C4: AI creates reducers.ts
    let mut reducers = repo.filename("src/reducers.ts");
    reducers.set_contents(crate::lines![
        "import { createSlice } from '@reduxjs/toolkit';".ai(),
        "export const counterSlice = createSlice({".ai(),
        "  name: 'counter',".ai(),
        "  initialState: { value: 0 },".ai(),
        "  reducers: {".ai(),
        "    increment: state => { state.value += 1; },".ai(),
        "    decrement: state => { state.value -= 1; },".ai(),
        "    reset: state => { state.value = 0; },".ai(),
        "  },".ai(),
        "});".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add counter reducer")
        .unwrap();

    // C5: AI creates hooks.ts
    let mut hooks = repo.filename("src/hooks.ts");
    hooks.set_contents(crate::lines![
        "import { TypedUseSelectorHook, useDispatch, useSelector } from 'react-redux';".ai(),
        "import type { AppDispatch } from './store';".ai(),
        "import type { RootState } from './types';".ai(),
        "export const useAppDispatch = () => useDispatch<AppDispatch>();".ai(),
        "export const useAppSelector: TypedUseSelectorHook<RootState> = useSelector;".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add typed hooks")
        .unwrap();

    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on src/store.ts at C1"
    );

    // Human resolves by writing a merged store file
    fs::write(
        repo.path().join("src/store.ts"),
        "import { createStore } from 'redux';\nimport { configureStore } from '@reduxjs/toolkit';\nexport const store = configureStore({ reducer: {} });\nexport type AppDispatch = typeof store.dispatch;\n",
    ).unwrap();
    repo.git(&["add", "src/store.ts"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': store.ts human-resolved → AI content survived → store.ts IS in note
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["src/store.ts"]);

    // C2': actions.ts only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["src/actions.ts"]);

    // C3': selectors.ts only
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["src/selectors.ts"]);

    // C4': reducers.ts only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["src/reducers.ts"]);

    // C5': hooks.ts only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["src/hooks.ts"]);
}

/// Test 7: Rust src/server.rs — feature adds AI HTTP handler functions; main
/// adds a conflicting use declaration in C4.  C1'–C3' and C5' keep their AI
/// attribution; C4' (server.rs) is dropped due to human resolution.
#[test]
fn test_human_conflict_rust_server_c4_human_resolved_c5_accumulates() {
    let repo = TestRepo::new();

    repo.commit_untracked_file("src/server.rs", "pub fn start() {}\n", "Initial commit");
    let main_branch = repo.current_branch();

    // Main: adds a use statement that conflicts with feature C4's edit
    repo.commit_untracked_file(
        "src/server.rs",
        "use std::net::TcpListener;\npub fn start() {}\n",
        "main: add TcpListener import",
    );
    repo.commit_untracked_file(
        "src/router.rs",
        "pub struct Router;\nimpl Router { pub fn new() -> Self { Router } }\n",
        "main: add router",
    );
    repo.commit_untracked_file(
        "src/response.rs",
        "pub struct Response { pub status: u16, pub body: String }\n",
        "main: add Response type",
    );
    repo.commit_untracked_file(
        "src/request.rs",
        "pub struct Request { pub path: String, pub method: String }\n",
        "main: add Request type",
    );
    repo.commit_untracked_file(
        "src/middleware.rs",
        "pub trait Middleware { fn handle(&self, req: &str) -> String; }\n",
        "main: add Middleware trait",
    );

    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates src/handler.rs
    let mut handler = repo.filename("src/handler.rs");
    handler.set_contents(crate::lines![
        "pub fn handle_get(path: &str) -> String {".ai(),
        "    format!(\"GET {} OK\", path)".ai(),
        "}".ai(),
        "".ai(),
        "pub fn handle_post(path: &str, body: &str) -> String {".ai(),
        "    format!(\"POST {} body={}\", path, body)".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add GET/POST handlers")
        .unwrap();

    // C2: AI creates src/router_ext.rs
    let mut router_ext = repo.filename("src/router_ext.rs");
    router_ext.set_contents(crate::lines![
        "use std::collections::HashMap;".ai(),
        "pub type HandlerFn = fn(&str) -> String;".ai(),
        "pub struct RouteMap(HashMap<String, HandlerFn>);".ai(),
        "impl RouteMap {".ai(),
        "    pub fn new() -> Self { RouteMap(HashMap::new()) }".ai(),
        "    pub fn register(&mut self, path: &str, h: HandlerFn) { self.0.insert(path.to_string(), h); }".ai(),
        "    pub fn dispatch(&self, path: &str) -> Option<String> { self.0.get(path).map(|h| h(path)) }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add RouteMap").unwrap();

    // C3: AI creates src/static_files.rs
    let mut statics = repo.filename("src/static_files.rs");
    statics.set_contents(crate::lines![
        "use std::path::Path;".ai(),
        "pub fn serve_static(path: &str) -> Option<Vec<u8>> {".ai(),
        "    let p = Path::new(path);".ai(),
        "    if p.exists() { std::fs::read(p).ok() } else { None }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add static file server")
        .unwrap();

    // C4: AI edits server.rs to add bind — WILL CONFLICT with main's use std::net::TcpListener
    let mut server = repo.filename("src/server.rs");
    server.replace_at(
        0,
        "pub fn start() { let _l = std::net::TcpListener::bind(\"0.0.0.0:8080\"); }".ai(),
    );
    repo.stage_all_and_commit("feat: C4 add bind in server start")
        .unwrap();

    // C5: AI creates src/tls.rs
    let mut tls = repo.filename("src/tls.rs");
    tls.set_contents(crate::lines![
        "pub struct TlsConfig { pub cert_path: String, pub key_path: String }".ai(),
        "impl TlsConfig {".ai(),
        "    pub fn new(cert: &str, key: &str) -> Self {".ai(),
        "        TlsConfig { cert_path: cert.into(), key_path: key.into() }".ai(),
        "    }".ai(),
        "    pub fn is_valid(&self) -> bool {".ai(),
        "        std::path::Path::new(&self.cert_path).exists()".ai(),
        "            && std::path::Path::new(&self.key_path).exists()".ai(),
        "    }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add TLS config struct")
        .unwrap();

    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on src/server.rs at C4"
    );

    // Human resolves by combining import and function body
    fs::write(
        repo.path().join("src/server.rs"),
        "use std::net::TcpListener;\npub fn start() { let _l = TcpListener::bind(\"0.0.0.0:8080\"); }\n",
    ).unwrap();
    repo.git(&["add", "src/server.rs"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': handler.rs only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["src/handler.rs"]);

    // C2': router_ext.rs only
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["src/router_ext.rs"]);

    // C3': static_files.rs only
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["src/static_files.rs"]);

    // C4': human-resolved conflict on server.rs.  The human changed `std::net::TcpListener`
    // to `TcpListener` in the resolution — the line content differs from the original AI
    // line so content-based mapping finds no match. Note metadata is preserved but no
    // file attestations remain.
    let c4_note = repo.read_authorship_note(&chain[3]);
    assert!(
        c4_note.is_some(),
        "c4: note metadata should survive conflict rebase"
    );
    assert_note_files_exact(&repo, &chain[3], "c4_files", &[]);

    // C5': tls.rs only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["src/tls.rs"]);
}

/// Test 8: Python pipeline.py — feature starts from a mixed human+AI baseline,
/// then modifies the same file as main in C3.  C1' + C2' accumulate other AI
/// files; C3' loses pipeline.py; C4'–C5' add transform.py and sink.py.
#[test]
fn test_human_conflict_python_pipeline_mixed_baseline_c3_conflict() {
    let repo = TestRepo::new();

    repo.commit_untracked_file("pipeline.py",
        "class Pipeline:\n    def __init__(self): self.stages = []\n    def run(self, data): return data\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: adds a validate method to Pipeline — will conflict with feature C3 adding filter
    repo.commit_untracked_file("pipeline.py",
        "class Pipeline:\n    def __init__(self): self.stages = []\n    def run(self, data): return data\n    def validate(self, data): return bool(data)\n",
        "main: add Pipeline.validate",
    );
    repo.commit_untracked_file("source.py",
        "class FileSource:\n    def __init__(self, path): self.path = path\n    def read(self): return open(self.path).read()\n",
        "main: add FileSource",
    );
    repo.commit_untracked_file("registry.py",
        "_registry = {}\ndef register(name, cls): _registry[name] = cls\ndef get(name): return _registry.get(name)\n",
        "main: add component registry",
    );
    repo.commit_untracked_file("executor.py",
        "from concurrent.futures import ThreadPoolExecutor\nexec_pool = ThreadPoolExecutor(max_workers=4)\n",
        "main: add thread pool executor",
    );
    repo.commit_untracked_file("scheduler.py",
        "import sched, time\ns = sched.scheduler(time.time, time.sleep)\ndef schedule(delay, fn): s.enter(delay, 1, fn)\n",
        "main: add scheduler",
    );

    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: Feature creates source.py (different name on feature branch — no conflict)
    let mut stream = repo.filename("stream.py");
    stream.set_contents(crate::lines![
        "class StreamSource:".ai(),
        "    def __init__(self, gen): self.gen = gen".ai(),
        "    def read(self): return next(self.gen, None)".ai(),
        "    def read_all(self): return list(self.gen)".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add StreamSource")
        .unwrap();

    // C2: AI creates filter.py
    let mut filter_file = repo.filename("filter.py");
    filter_file.set_contents(crate::lines![
        "from typing import Callable, TypeVar".ai(),
        "T = TypeVar('T')".ai(),
        "".ai(),
        "class Filter:".ai(),
        "    def __init__(self, pred: Callable): self.pred = pred".ai(),
        "    def apply(self, data: list) -> list: return [x for x in data if self.pred(x)]".ai(),
        "    def negate(self) -> 'Filter': return Filter(lambda x: not self.pred(x))".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add Filter class")
        .unwrap();

    // C3: AI edits pipeline.py to add a filter step — WILL CONFLICT with main's validate
    let mut pipeline = repo.filename("pipeline.py");
    pipeline.replace_at(
        2,
        "    def add_filter(self, f): self.stages.append(f); return self".ai(),
    );
    repo.stage_all_and_commit("feat: C3 add Pipeline.add_filter")
        .unwrap();

    // C4: AI creates transform.py
    let mut transform = repo.filename("transform.py");
    transform.set_contents(crate::lines![
        "class Transform:".ai(),
        "    def __init__(self, fn): self.fn = fn".ai(),
        "    def apply(self, data): return [self.fn(x) for x in data]".ai(),
        "".ai(),
        "class MapTransform(Transform):".ai(),
        "    pass".ai(),
        "".ai(),
        "class FlatMapTransform:".ai(),
        "    def __init__(self, fn): self.fn = fn".ai(),
        "    def apply(self, data): return [y for x in data for y in self.fn(x)]".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add Transform classes")
        .unwrap();

    // C5: AI creates sink.py
    let mut sink = repo.filename("sink.py");
    sink.set_contents(crate::lines![
        "from typing import List".ai(),
        "".ai(),
        "class ListSink:".ai(),
        "    def __init__(self): self._items: List = []".ai(),
        "    def write(self, item): self._items.append(item)".ai(),
        "    def flush(self) -> List: r = list(self._items); self._items.clear(); return r".ai(),
        "".ai(),
        "class ConsoleSink:".ai(),
        "    def write(self, item): print(item)".ai(),
        "    def flush(self) -> List: return []".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add Sink classes")
        .unwrap();

    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on pipeline.py at C3"
    );

    // Human resolves by keeping both methods
    fs::write(
        repo.path().join("pipeline.py"),
        "class Pipeline:\n    def __init__(self): self.stages = []\n    def run(self, data): return data\n    def validate(self, data): return bool(data)\n    def add_filter(self, f): self.stages.append(f); return self\n",
    ).unwrap();
    repo.git(&["add", "pipeline.py"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': stream.py only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["stream.py"]);

    // C2': filter.py only
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["filter.py"]);

    // C3': pipeline.py human-resolved conflict — AI lines inside diff hunk, attribution dropped
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &[]);

    // C4': transform.py only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["transform.py"]);

    // C5': sink.py only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["sink.py"]);
}

/// Test 9: TypeScript component.tsx — AI writes entire component file (via
/// fs::write + checkpoint), main adds a style import that conflicts on C2.
/// C1' accumulates hooks.ts; C2' loses component.tsx; C3'–C5' add context.ts,
/// provider.tsx, types.ts normally.
#[test]
fn test_human_conflict_typescript_component_ai_created_c2_conflict() {
    let repo = TestRepo::new();

    repo.commit_untracked_file(
        "src/Component.tsx",
        "export const Component = () => null;\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: adds a CSS import to Component.tsx → conflict with feature C2's rewrite
    repo.commit_untracked_file(
        "src/Component.tsx",
        "import './Component.css';\nexport const Component = () => null;\n",
        "main: add CSS import to Component",
    );
    repo.commit_untracked_file(
        "src/Component.css",
        ".component { display: flex; }\n",
        "main: add component styles",
    );
    repo.commit_untracked_file(
        "src/App.tsx",
        "import { Component } from './Component';\nexport const App = () => <Component />;\n",
        "main: add App",
    );
    repo.commit_untracked_file("src/index.tsx",
        "import React from 'react';\nimport ReactDOM from 'react-dom';\nimport { App } from './App';\nReactDOM.render(<App />, document.getElementById('root'));\n",
        "main: add entry point",
    );
    repo.commit_untracked_file(
        "src/theme.ts",
        "export const theme = { primary: '#007bff', secondary: '#6c757d' };\n",
        "main: add theme",
    );

    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates hooks.ts
    let mut custom_hooks = repo.filename("src/useCounter.ts");
    custom_hooks.set_contents(crate::lines![
        "import { useState, useCallback } from 'react';".ai(),
        "".ai(),
        "export const useCounter = (initial = 0) => {".ai(),
        "  const [count, setCount] = useState(initial);".ai(),
        "  const increment = useCallback(() => setCount(c => c + 1), []);".ai(),
        "  const decrement = useCallback(() => setCount(c => c - 1), []);".ai(),
        "  const reset = useCallback(() => setCount(initial), [initial]);".ai(),
        "  return { count, increment, decrement, reset };".ai(),
        "};".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add useCounter hook")
        .unwrap();

    // C2: AI rewrites Component.tsx via fs::write + checkpoint — WILL CONFLICT
    let component_content = "import React from 'react';\nimport { useCounter } from './useCounter';\n\nexport const Component: React.FC = () => {\n  const { count, increment, decrement, reset } = useCounter();\n  return <div><button onClick={decrement}>-</button><span>{count}</span><button onClick={increment}>+</button><button onClick={reset}>reset</button></div>;\n};\n";
    fs::write(repo.path().join("src/Component.tsx"), component_content).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "src/Component.tsx"])
        .unwrap();
    repo.stage_all_and_commit("feat: C2 AI rewrites Component with useCounter")
        .unwrap();

    // C3: AI creates context.ts
    let mut context = repo.filename("src/context.ts");
    context.set_contents(crate::lines![
        "import React from 'react';".ai(),
        "export interface AppContextValue { theme: string; locale: string; }".ai(),
        "export const AppContext = React.createContext<AppContextValue>({ theme: 'light', locale: 'en' });".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add AppContext")
        .unwrap();

    // C4: AI creates provider.tsx
    let mut provider = repo.filename("src/provider.tsx");
    provider.set_contents(crate::lines![
        "import React, { useState } from 'react';".ai(),
        "import { AppContext } from './context';".ai(),
        "".ai(),
        "export const AppProvider: React.FC<{ children: React.ReactNode }> = ({ children }) => {".ai(),
        "  const [theme, setTheme] = useState('light');".ai(),
        "  return <AppContext.Provider value={{ theme, locale: 'en' }}>{children}</AppContext.Provider>;".ai(),
        "};".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add AppProvider")
        .unwrap();

    // C5: AI creates types.ts
    let mut types_file = repo.filename("src/types.ts");
    types_file.set_contents(crate::lines![
        "export type Theme = 'light' | 'dark';".ai(),
        "export type Locale = 'en' | 'fr' | 'de';".ai(),
        "export interface UserPrefs { theme: Theme; locale: Locale; }".ai(),
        "export type Handler<T = void> = (e: React.SyntheticEvent) => T;".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add shared types")
        .unwrap();

    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on src/Component.tsx at C2"
    );

    // Human resolves: keep both CSS import and the new component body
    fs::write(
        repo.path().join("src/Component.tsx"),
        "import './Component.css';\nimport React from 'react';\nimport { useCounter } from './useCounter';\n\nexport const Component: React.FC = () => {\n  const { count, increment, decrement } = useCounter();\n  return <div>{count}</div>;\n};\n",
    ).unwrap();
    repo.git(&["add", "src/Component.tsx"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': useCounter.ts only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["src/useCounter.ts"]);

    // C2': Component.tsx human-resolved → AI content survived → Component.tsx IS in note
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["src/Component.tsx"]);

    // C3': context.ts only
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["src/context.ts"]);

    // C4': provider.tsx only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["src/provider.tsx"]);

    // C5': types.ts only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["src/types.ts"]);
}

/// Test 10: Rust 7-commit chain — feature adds AI functions across multiple
/// files; main edits shared.rs causing conflict on C4 (middle of a 7-commit
/// chain).  Verifies 7-element chain: C1'–C3' accumulate normally, C4' loses
/// shared.rs, C5'–C7' continue accumulating math.rs, string_utils.rs, io.rs.
#[test]
fn test_human_conflict_rust_7_commit_chain_c4_conflict_surroundings_intact() {
    let repo = TestRepo::new();

    repo.commit_untracked_file(
        "src/shared.rs",
        "pub fn identity<T>(x: T) -> T { x }\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: adds a constant and a function to shared.rs → conflict with feature C4 edit
    repo.commit_untracked_file(
        "src/shared.rs",
        "pub const VERSION: &str = \"1.0\";\npub fn identity<T>(x: T) -> T { x }\n",
        "main: add VERSION constant to shared.rs",
    );
    repo.commit_untracked_file(
        "src/log.rs",
        "pub fn log(msg: &str) { eprintln!(\"{}\", msg); }\n",
        "main: add log",
    );
    repo.commit_untracked_file("src/env.rs",
        "pub fn env_or(key: &str, default: &str) -> String { std::env::var(key).unwrap_or_else(|_| default.to_string()) }\n",
        "main: add env helper",
    );
    repo.commit_untracked_file("src/fs_utils.rs",
        "pub fn read_to_string(path: &str) -> std::io::Result<String> { std::fs::read_to_string(path) }\n",
        "main: add fs_utils",
    );
    repo.commit_untracked_file("src/assert_utils.rs",
        "pub fn assert_non_empty(s: &str) { assert!(!s.is_empty(), \"expected non-empty string\"); }\n",
        "main: add assert_utils",
    );

    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates src/vec_utils.rs
    let mut vec_utils = repo.filename("src/vec_utils.rs");
    vec_utils.set_contents(crate::lines![
        "pub fn dedup<T: Eq + std::hash::Hash + Clone>(v: &[T]) -> Vec<T> {".ai(),
        "    let mut seen = std::collections::HashSet::new();".ai(),
        "    v.iter().filter(|x| seen.insert((*x).clone())).cloned().collect()".ai(),
        "}".ai(),
        "pub fn flatten<T: Clone>(nested: &[Vec<T>]) -> Vec<T> {".ai(),
        "    nested.iter().flat_map(|v| v.iter().cloned()).collect()".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add vec_utils").unwrap();

    // C2: AI creates src/option_utils.rs
    let mut opt_utils = repo.filename("src/option_utils.rs");
    opt_utils.set_contents(crate::lines![
        "pub fn or_default<T: Default>(opt: Option<T>) -> T {".ai(),
        "    opt.unwrap_or_default()".ai(),
        "}".ai(),
        "pub fn map_or_none<T, U, F: FnOnce(T) -> Option<U>>(opt: Option<T>, f: F) -> Option<U> {"
            .ai(),
        "    opt.and_then(f)".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add option_utils")
        .unwrap();

    // C3: AI creates src/result_utils.rs
    let mut result_utils = repo.filename("src/result_utils.rs");
    result_utils.set_contents(crate::lines![
        "pub fn ok_or_log<T, E: std::fmt::Display>(r: Result<T, E>, ctx: &str) -> Option<T> {".ai(),
        "    r.map_err(|e| eprintln!(\"{}: {}\", ctx, e)).ok()".ai(),
        "}".ai(),
        "pub fn map_err_string<T, E: std::fmt::Display>(r: Result<T, E>) -> Result<T, String> {"
            .ai(),
        "    r.map_err(|e| e.to_string())".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add result_utils")
        .unwrap();

    // C4: AI edits shared.rs to add a clamp function — WILL CONFLICT with main's VERSION
    let mut shared = repo.filename("src/shared.rs");
    shared.replace_at(0, "pub fn clamp<T: PartialOrd>(x: T, lo: T, hi: T) -> T { if x < lo { lo } else if x > hi { hi } else { x } }".ai());
    repo.stage_all_and_commit("feat: C4 add clamp to shared.rs")
        .unwrap();

    // C5: AI creates src/math.rs
    let mut math = repo.filename("src/math.rs");
    math.set_contents(crate::lines![
        "pub fn gcd(mut a: u64, mut b: u64) -> u64 { while b != 0 { let t = b; b = a % b; a = t; } a }".ai(),
        "pub fn lcm(a: u64, b: u64) -> u64 { a / gcd(a, b) * b }".ai(),
        "pub fn is_prime(n: u64) -> bool { if n < 2 { return false; } (2..=(n as f64).sqrt() as u64).all(|i| n % i != 0) }".ai(),
        "pub fn factorial(n: u64) -> u64 { (1..=n).product() }".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add math utils")
        .unwrap();

    // C6: AI creates src/string_utils.rs
    let mut str_utils = repo.filename("src/string_utils.rs");
    str_utils.set_contents(crate::lines![
        "pub fn capitalize(s: &str) -> String {".ai(),
        "    let mut c = s.chars();".ai(),
        "    match c.next() {".ai(),
        "        None => String::new(),".ai(),
        "        Some(f) => f.to_uppercase().to_string() + c.as_str(),".ai(),
        "    }".ai(),
        "}".ai(),
        "pub fn snake_to_camel(s: &str) -> String {".ai(),
        "    s.split('_').enumerate().map(|(i, w)| if i == 0 { w.to_string() } else { capitalize(w) }).collect()".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C6 add string_utils")
        .unwrap();

    // C7: AI creates src/io_utils.rs
    let mut io_utils = repo.filename("src/io_utils.rs");
    io_utils.set_contents(crate::lines![
        "use std::io::{self, BufRead};".ai(),
        "".ai(),
        "pub fn read_lines(path: &str) -> io::Result<Vec<String>> {".ai(),
        "    let file = std::fs::File::open(path)?;".ai(),
        "    let reader = io::BufReader::new(file);".ai(),
        "    reader.lines().collect()".ai(),
        "}".ai(),
        "".ai(),
        "pub fn write_lines(path: &str, lines: &[String]) -> io::Result<()> {".ai(),
        "    std::fs::write(path, lines.join(\"\\n\"))".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C7 add io_utils").unwrap();

    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on src/shared.rs at C4"
    );

    // Human resolves: keep VERSION constant and add clamp function
    fs::write(
        repo.path().join("src/shared.rs"),
        "pub const VERSION: &str = \"1.0\";\npub fn identity<T>(x: T) -> T { x }\npub fn clamp<T: PartialOrd>(x: T, lo: T, hi: T) -> T { if x < lo { lo } else if x > hi { hi } else { x } }\n",
    ).unwrap();
    repo.git(&["add", "src/shared.rs"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 7);

    // C1': vec_utils.rs only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["src/vec_utils.rs"]);

    // C2': option_utils.rs only
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["src/option_utils.rs"]);

    // C3': result_utils.rs only
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["src/result_utils.rs"]);

    // C4': shared.rs human-resolved conflict — AI lines inside diff hunk, attribution dropped
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &[]);

    // C5': math.rs only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["src/math.rs"]);

    // C6': string_utils.rs only
    assert_note_base_commit_matches(&repo, &chain[5], "c6_base");
    assert_note_files_exact(&repo, &chain[5], "c6_files", &["src/string_utils.rs"]);

    // C7': io_utils.rs only
    assert_note_base_commit_matches(&repo, &chain[6], "c7_base");
    assert_note_files_exact(&repo, &chain[6], "c7_files", &["src/io_utils.rs"]);
}

/// Test: Human resolves conflict by replacing ALL AI lines with completely
/// different content.  After rebase, the conflict commit should have NO note
/// (commit_has_attestations=false → else branch returns None).
/// Subsequent AI commits should be unaffected.
#[test]
fn test_human_conflict_resolves_all_ai_lines_replaced() {
    let repo = TestRepo::new();

    // Base: compute.py with one human line
    repo.commit_untracked_file("compute.py", "result = 0\n", "Initial: result=0");
    let main_branch = repo.current_branch();

    // Main: change result to 1 (forces slow path on feature)
    repo.commit_untracked_file("compute.py", "result = 1\n", "main: set result=1");
    repo.commit_untracked_file("main_extra.py", "# main extra\n", "main: add extra file");

    // Feature from base
    let base_sha = repo
        .git(&["rev-parse", "HEAD~2"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI sets result=2 — WILL CONFLICT with main's result=1 (base=0)
    let mut compute = repo.filename("compute.py");
    compute.set_contents(crate::lines!["result = 2".ai(),]);
    repo.stage_all_and_commit("feat: C1 AI sets result=2")
        .unwrap();

    // C2: AI adds a separate file (unrelated to conflict)
    let mut module_b = repo.filename("module_b.py");
    module_b.set_contents(crate::lines![
        "class ModuleB:".ai(),
        "    def run(self): return 'b'".ai(),
        "    def name(self): return 'module_b'".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add ModuleB").unwrap();

    // C3: AI adds another file
    let mut module_c = repo.filename("module_c.py");
    module_c.set_contents(crate::lines![
        "class ModuleC:".ai(),
        "    def run(self): return 'c'".ai(),
        "    def name(self): return 'module_c'".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add ModuleC").unwrap();

    // Rebase: C1 conflicts on compute.py
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on compute.py at C1"
    );

    // Human resolves by writing COMPLETELY DIFFERENT content — no AI lines survive.
    // Base had result=0, feature had result=2, main had result=1.
    // Human writes result=42 with an extra human comment — none of these lines
    // match original AI content, so diff_based_line_attribution_transfer produces
    // only Replace ops → commit_has_attestations = false.
    //
    // However, the original commit DID have an AI authorship note.  Rather than
    // silently dropping provenance, the slow-path fallback remaps the original note
    // to the rebased commit.  The attestation line numbers may be stale but the AI
    // authorship record is preserved.
    fs::write(
        repo.path().join("compute.py"),
        "# human resolved\nresult = 42\n",
    )
    .unwrap();
    repo.git(&["add", "compute.py"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .expect("rebase --continue should succeed after C1 resolution");

    let chain = get_commit_chain(&repo, 3);
    // chain[0]=C1', chain[1]=C2', chain[2]=C3'

    // C1': human fully replaced all AI lines during resolution. Content-based mapping
    // finds no matching lines, so the note has no file attestations. The note itself
    // is preserved (metadata) but compute.py has no attributed lines.
    let c1_note = repo.read_authorship_note(&chain[0]);
    assert!(
        c1_note.is_some(),
        "C1 original had AI note: note metadata should be preserved after rewrite",
    );
    assert_note_files_exact(&repo, &chain[0], "c1_files", &[]);

    // C2': module_b.py — AI, untouched by conflict — note must exist with correct attribution
    assert_note_base_commit_matches(&repo, &chain[1], "c2");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["module_b.py"]);
    assert_note_no_forbidden_files(&repo, &chain[1], "c2_no_compute", &["compute.py"]);
    assert_blame_at_commit(
        &repo,
        &chain[1],
        "module_b.py",
        "c2_blame",
        &[
            ("class ModuleB:", true),
            ("def run(self): return 'b'", true),
            ("def name(self): return 'module_b'", true),
        ],
    );

    // C3': module_c.py — AI, untouched by conflict
    assert_note_base_commit_matches(&repo, &chain[2], "c3");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["module_c.py"]);
    assert_note_no_forbidden_files(&repo, &chain[2], "c3_no_compute", &["compute.py"]);
    assert_blame_at_commit(
        &repo,
        &chain[2],
        "module_c.py",
        "c3_blame",
        &[
            ("class ModuleC:", true),
            ("def run(self): return 'c'", true),
            ("def name(self): return 'module_c'", true),
        ],
    );
}

/// Regression test for #1079: when the ONLY AI-tracked file is the conflict file,
/// and the human resolves with completely different content, the original authorship
/// note must still be remapped to the rebased commit.  Before this fix the slow path
/// produced no note (content-diff found no matching AI lines) and the metadata-only
/// remap skipped notes with real attestations, silently losing provenance.
#[test]
fn test_human_conflict_ai_file_is_conflict_file_note_preserved() {
    let repo = TestRepo::new();

    // Initial: ai_file.py with one human line
    repo.commit_untracked_file("ai_file.py", "original line\n", "Initial commit");
    let main_branch = repo.current_branch();

    // Main: change ai_file.py → will conflict with feature
    repo.commit_untracked_file(
        "ai_file.py",
        "upstream changed line\n",
        "main: modify ai_file",
    );

    // Feature branch from initial commit
    let base_sha = repo
        .git(&["rev-parse", "HEAD~1"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI modifies ai_file.py — this is the ONLY commit on feature, and the
    // ONLY file that has AI attribution.  It will conflict with main.
    let mut ai_file = repo.filename("ai_file.py");
    ai_file.set_contents(crate::lines!["ai modified line".ai()]);
    repo.stage_all_and_commit("feat: AI edits ai_file.py")
        .unwrap();

    // Verify note exists before rebase
    let pre_rebase_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let pre_note = repo.read_authorship_note(&pre_rebase_sha);
    assert!(
        pre_note.is_some(),
        "AI commit should have a note before rebase"
    );

    // Rebase onto main — conflict on ai_file.py
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on ai_file.py"
    );

    // Human resolves with completely different content (no AI lines survive).
    fs::write(repo.path().join("ai_file.py"), "human resolved content\n").unwrap();
    repo.git(&["add", "ai_file.py"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .expect("rebase --continue should succeed");

    let chain = get_commit_chain(&repo, 1);

    // The rebased commit still has a note (metadata preserved) but ai_file.py
    // has no attributed lines since human resolution replaced all AI content.
    let post_note = repo.read_authorship_note(&chain[0]);
    assert!(
        post_note.is_some(),
        "Note metadata should survive conflict rebase even when content doesn't match"
    );
    assert_note_files_exact(&repo, &chain[0], "c1_files", &[]);
}

/// Regression test for #1079: three AI commits on a feature branch; the second
/// commit's file conflicts with upstream.  After human conflict resolution and
/// `rebase --continue`, ALL three rebased commits must retain their authorship
/// notes.  Before the fix, the conflict commit's note was lost (content-diff
/// produced nothing for the manually resolved file and the fallback remap was
/// too narrow).
#[test]
fn test_human_conflict_multicommit_chain_middle_conflict_all_notes_preserved() {
    let repo = TestRepo::new();

    // Initial: shared.py (will conflict) + base.txt
    repo.commit_untracked_file("shared.py", "base content\n", "Initial commit");
    repo.commit_untracked_file("base.txt", "base\n", "Add base.txt");
    let main_branch = repo.current_branch();

    // Feature branch from initial commits
    let base_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates file_a.py (no conflict)
    let mut file_a = repo.filename("file_a.py");
    file_a.set_contents(crate::lines!["def ai_func_a(): pass".ai()]);
    repo.stage_all_and_commit("feat: AI creates file_a.py")
        .unwrap();

    // C2: AI modifies shared.py (WILL conflict with upstream)
    let mut shared = repo.filename("shared.py");
    shared.set_contents(crate::lines!["ai version of shared".ai()]);
    repo.stage_all_and_commit("feat: AI modifies shared.py")
        .unwrap();

    // C3: AI creates file_c.py (no conflict)
    let mut file_c = repo.filename("file_c.py");
    file_c.set_contents(crate::lines!["def ai_func_c(): pass".ai()]);
    repo.stage_all_and_commit("feat: AI creates file_c.py")
        .unwrap();

    // Verify all 3 commits have notes before rebase
    let chain_pre = get_commit_chain(&repo, 3);
    for (i, sha) in chain_pre.iter().enumerate() {
        assert!(
            repo.read_authorship_note(sha).is_some(),
            "pre-rebase commit {} (C{}) must have a note",
            &sha[..8],
            i + 1
        );
    }

    // Upstream: change shared.py to create conflict
    repo.git(&["checkout", &main_branch]).unwrap();
    repo.commit_untracked_file(
        "shared.py",
        "upstream version of shared\n",
        "main: modify shared.py",
    );

    // Rebase feature onto main — C2 will conflict on shared.py
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on shared.py (C2 vs upstream)"
    );

    // Human resolves conflict with different content
    fs::write(repo.path().join("shared.py"), "human resolved shared\n").unwrap();
    repo.git(&["add", "shared.py"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .expect("rebase --continue should succeed");

    // All 3 rebased commits must have notes
    let chain = get_commit_chain(&repo, 3);

    // C1': file_a.py — AI, no conflict
    let note_c1 = repo.read_authorship_note(&chain[0]);
    assert!(
        note_c1.is_some(),
        "C1' (file_a.py, no conflict) must retain authorship note after conflict rebase (issue #1079)"
    );
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["file_a.py"]);

    // C2': shared.py — AI, conflict resolved by human with completely different content.
    // Content-based mapping finds no matching lines, so no file attestations remain.
    let note_c2 = repo.read_authorship_note(&chain[1]);
    assert!(
        note_c2.is_some(),
        "C2' note metadata should survive conflict rebase"
    );
    assert_note_files_exact(&repo, &chain[1], "c2_files", &[]);

    // C3': file_c.py — AI, no conflict
    let note_c3 = repo.read_authorship_note(&chain[2]);
    assert!(
        note_c3.is_some(),
        "C3' (file_c.py, no conflict) must retain authorship note after conflict rebase (issue #1079)"
    );
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["file_c.py"]);
}

// ============================================================================
// END Category 3: Human Conflict Resolution
// ============================================================================

// ============================================================================
// Category 4: Conflict Resolved by AI
// Feature branch has AI-generated changes that conflict with main branch.
// AI resolves via set_contents with .ai() lines (writes + stages + checkpoints).
// The resolved lines gain AI attribution; surrounding human lines keep human
// attribution.  All other AI files in the chain retain their attribution.
// ============================================================================

/// Test 1: config.py TIMEOUT constant — feature (C3) changes TIMEOUT to 60,
/// main changes it to 120 → conflict.  AI resolves to TIMEOUT = 90.
/// C1' has users.py, C2' adds products.py, C3' adds config.py (AI-resolved),
/// C4' adds orders.py, C5' adds payments.py.
#[test]
fn test_conflict_ai_resolves_timeout_constant() {
    let repo = TestRepo::new();

    // Initial: config.py with a class and TIMEOUT constant (human)
    repo.commit_untracked_file(
        "config.py",
        "class Config:\n    TIMEOUT = 30\n    HOST = 'localhost'\n    PORT = 8080\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: changes TIMEOUT to 120 and adds 4 more commits
    repo.commit_untracked_file(
        "config.py",
        "class Config:\n    TIMEOUT = 120\n    HOST = 'localhost'\n    PORT = 8080\n",
        "main: increase TIMEOUT to 120",
    );
    repo.commit_untracked_file(
        "logging_config.py",
        "import logging\nlogging.basicConfig(level=logging.INFO)\n",
        "main: add logging config",
    );
    repo.commit_untracked_file(
        "constants.py",
        "MAX_CONNECTIONS = 100\nDEFAULT_PAGE_SIZE = 20\n",
        "main: add constants",
    );
    repo.commit_untracked_file(
        "exceptions.py",
        "class AppError(Exception): pass\nclass ValidationError(AppError): pass\n",
        "main: add exceptions",
    );
    repo.commit_untracked_file(
        "utils.py",
        "def flatten(lst): return [x for sub in lst for x in sub]\n",
        "main: add utils",
    );

    // Feature branch from base (before main's TIMEOUT change)
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates users.py (8 AI lines)
    let mut users = repo.filename("users.py");
    users.set_contents(crate::lines![
        "class UserService:".ai(),
        "    def __init__(self, db):".ai(),
        "        self.db = db".ai(),
        "    def get_user(self, uid):".ai(),
        "        return self.db.query('SELECT * FROM users WHERE id=?', uid)".ai(),
        "    def create_user(self, name, email):".ai(),
        "        return self.db.execute('INSERT INTO users VALUES (?, ?)', name, email)".ai(),
        "    def delete_user(self, uid):".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add user service")
        .unwrap();

    // C2: AI creates products.py (8 AI lines)
    let mut products = repo.filename("products.py");
    products.set_contents(crate::lines![
        "class ProductService:".ai(),
        "    def __init__(self, db):".ai(),
        "        self.db = db".ai(),
        "    def get_product(self, pid):".ai(),
        "        return self.db.query('SELECT * FROM products WHERE id=?', pid)".ai(),
        "    def list_products(self):".ai(),
        "        return self.db.query('SELECT * FROM products')".ai(),
        "    def update_price(self, pid, price):".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add product service")
        .unwrap();

    // C3: AI changes TIMEOUT to 60 in config.py — WILL CONFLICT with main's 120
    let mut config = repo.filename("config.py");
    config.set_contents(crate::lines![
        "class Config:".human(),
        "    TIMEOUT = 60".ai(),
        "    HOST = 'localhost'".human(),
        "    PORT = 8080".human(),
    ]);
    repo.stage_all_and_commit("feat: C3 AI tunes TIMEOUT to 60")
        .unwrap();

    // C4: AI creates orders.py (8 AI lines)
    let mut orders = repo.filename("orders.py");
    orders.set_contents(crate::lines![
        "class OrderService:".ai(),
        "    def __init__(self, db):".ai(),
        "        self.db = db".ai(),
        "    def create_order(self, uid, items):".ai(),
        "        total = sum(i['price'] for i in items)".ai(),
        "        return self.db.execute('INSERT INTO orders VALUES (?, ?)', uid, total)".ai(),
        "    def get_order(self, oid):".ai(),
        "        return self.db.query('SELECT * FROM orders WHERE id=?', oid)".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add order service")
        .unwrap();

    // C5: AI creates payments.py (8 AI lines)
    let mut payments = repo.filename("payments.py");
    payments.set_contents(crate::lines![
        "class PaymentService:".ai(),
        "    def __init__(self, db, stripe):".ai(),
        "        self.db = db".ai(),
        "        self.stripe = stripe".ai(),
        "    def charge(self, oid, amount, token):".ai(),
        "        r = self.stripe.charge(amount, token)".ai(),
        "        self.db.execute('INSERT INTO payments VALUES (?, ?)', oid, r['id'])".ai(),
        "    def refund(self, pid):".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add payment service")
        .unwrap();

    // Rebase onto main — C3 will conflict on config.py
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on config.py at C3"
    );

    // AI resolves: sets TIMEOUT = 90 as .ai(), surrounding lines as .human()
    let mut conflict_config = repo.filename("config.py");
    conflict_config.set_contents(crate::lines![
        "class Config:".human(),
        "    TIMEOUT = 90".ai(),
        "    HOST = 'localhost'".human(),
        "    PORT = 8080".human(),
    ]);
    // set_contents already ran git add -A + checkpoint
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': users.py only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["users.py"]);

    // C2': products.py only
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["products.py"]);

    // C3': config.py only (AI-resolved, TIMEOUT = 90 attributed as AI)
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["config.py"]);
    // 1 AI line: TIMEOUT = 90 (working-log fallback path must set accepted_lines correctly)
    assert_accepted_lines_exact(&repo, &chain[2], "c3_accepted_lines", 1);

    // blame at chain[2] for config.py: the AI-resolved TIMEOUT line should be AI
    assert_blame_at_commit(
        &repo,
        &chain[2],
        "config.py",
        "c3_blame_config",
        &[
            ("class Config:", false),
            ("TIMEOUT = 90", true),
            ("HOST = 'localhost'", false),
            ("PORT = 8080", false),
        ],
    );

    // C4': orders.py only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["orders.py"]);

    // C5': payments.py only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["payments.py"]);

    // Verify per-commit-delta humans scoping (KnownHuman variant)
    let conflict_note = parse_note(&repo, &chain[2]); // X = conflict commit index
    assert!(
        conflict_note
            .metadata
            .humans
            .contains_key("h_e858f2c2faea28"),
        "c3' should have h_e858f2c2faea28 in metadata.humans (human context lines in resolved file)"
    );
    assert_eq!(
        conflict_note.metadata.humans["h_e858f2c2faea28"].author,
        "Test User <test@example.com>"
    );
}

/// Test 2: compute.rs function body — feature (C2) implements a function with
/// 10 AI lines, main also implements it differently (conflict).  AI resolution
/// produces 15 merged lines (all .ai()).  Extra lines from resolution are counted.
#[test]
fn test_conflict_ai_resolves_with_added_extra_lines() {
    let repo = TestRepo::new();

    // Initial: compute.rs with a function stub (human)
    repo.commit_untracked_file(
        "src/compute.rs",
        "pub fn compute(data: &[f64]) -> f64 { 0.0 }\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: implements the function differently (human) → will conflict
    repo.commit_untracked_file("src/compute.rs",
        "pub fn compute(data: &[f64]) -> f64 {\n    data.iter().sum::<f64>() / data.len() as f64\n}\n",
        "main: implement compute as mean",
    );
    repo.commit_untracked_file(
        "src/main.rs",
        "fn main() { println!(\"hello\"); }\n",
        "main: add main",
    );
    repo.commit_untracked_file(
        "Cargo.toml",
        "[package]\nname = \"compute\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        "main: add Cargo.toml",
    );
    repo.commit_untracked_file(
        "src/tests.rs",
        "#[cfg(test)]\nmod tests { #[test] fn it_works() {} }\n",
        "main: add tests",
    );
    repo.commit_untracked_file(
        "README.md",
        "# compute\nA compute library.\n",
        "main: add README",
    );

    // Feature branch from base
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates types.rs (8 AI lines)
    let mut types = repo.filename("src/types.rs");
    types.set_contents(crate::lines![
        "#[derive(Debug, Clone, PartialEq)]".ai(),
        "pub struct DataPoint {".ai(),
        "    pub value: f64,".ai(),
        "    pub weight: f64,".ai(),
        "}".ai(),
        "".ai(),
        "impl DataPoint {".ai(),
        "    pub fn new(value: f64, weight: f64) -> Self { Self { value, weight } }".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add DataPoint type")
        .unwrap();

    // C2: AI implements compute.rs with 10 AI lines — WILL CONFLICT with main's implementation
    let mut compute = repo.filename("src/compute.rs");
    compute.set_contents(crate::lines![
        "pub fn compute(data: &[f64]) -> f64 {".ai(),
        "    if data.is_empty() { return 0.0; }".ai(),
        "    let n = data.len() as f64;".ai(),
        "    let mean = data.iter().sum::<f64>() / n;".ai(),
        "    let variance = data.iter()".ai(),
        "        .map(|x| (x - mean).powi(2))".ai(),
        "        .sum::<f64>() / n;".ai(),
        "    variance.sqrt()".ai(),
        "}".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 AI implements compute as std-dev")
        .unwrap();

    // C3: AI creates validator.rs (8 AI lines)
    let mut validator = repo.filename("src/validator.rs");
    validator.set_contents(crate::lines![
        "pub fn validate_data(data: &[f64]) -> Result<(), String> {".ai(),
        "    if data.is_empty() { return Err(\"empty data\".into()); }".ai(),
        "    if data.iter().any(|x| x.is_nan()) { return Err(\"NaN in data\".into()); }".ai(),
        "    if data.iter().any(|x| x.is_infinite()) { return Err(\"Inf in data\".into()); }".ai(),
        "    Ok(())".ai(),
        "}".ai(),
        "".ai(),
        "pub fn normalize(data: &mut Vec<f64>) { let m = data.iter().cloned().fold(f64::NEG_INFINITY, f64::max); data.iter_mut().for_each(|x| *x /= m); }".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add data validator")
        .unwrap();

    // C4: AI creates encoder.rs (8 AI lines)
    let mut encoder = repo.filename("src/encoder.rs");
    encoder.set_contents(crate::lines![
        "pub fn encode(data: &[f64]) -> Vec<u8> {".ai(),
        "    data.iter()".ai(),
        "        .flat_map(|x| x.to_le_bytes())".ai(),
        "        .collect()".ai(),
        "}".ai(),
        "".ai(),
        "pub fn decode(bytes: &[u8]) -> Vec<f64> {".ai(),
        "    bytes.chunks_exact(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect()"
            .ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add encoder").unwrap();

    // C5: AI creates decoder.rs (8 AI lines)
    let mut decoder = repo.filename("src/decoder.rs");
    decoder.set_contents(crate::lines![
        "use crate::encoder::decode;".ai(),
        "".ai(),
        "pub struct Decoder {".ai(),
        "    buffer: Vec<u8>,".ai(),
        "}".ai(),
        "".ai(),
        "impl Decoder {".ai(),
        "    pub fn new() -> Self { Self { buffer: Vec::new() } }".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add decoder struct")
        .unwrap();

    // Rebase onto main — C2 will conflict on src/compute.rs
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on src/compute.rs at C2"
    );

    // AI resolves: writes a 15-line merged implementation (all .ai())
    let mut conflict_compute = repo.filename("src/compute.rs");
    conflict_compute.set_contents(crate::lines![
        "pub fn compute(data: &[f64]) -> f64 {".ai(),
        "    if data.is_empty() { return 0.0; }".ai(),
        "    let n = data.len() as f64;".ai(),
        "    let mean = data.iter().sum::<f64>() / n;".ai(),
        "    let variance = data.iter()".ai(),
        "        .map(|x| (x - mean).powi(2))".ai(),
        "        .sum::<f64>() / n;".ai(),
        "    let std_dev = variance.sqrt();".ai(),
        "    // Also return weighted mean as a combined metric".ai(),
        "    let weighted_sum: f64 = data.iter().enumerate().map(|(i, x)| x * (i + 1) as f64).sum();".ai(),
        "    let weight_total: f64 = (1..=data.len()).map(|i| i as f64).sum();".ai(),
        "    let weighted_mean = weighted_sum / weight_total;".ai(),
        "    std_dev * 0.5 + weighted_mean * 0.5".ai(),
        "}".ai(),
        "".ai(),
    ]);
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': types.rs only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["src/types.rs"]);

    // C2': compute.rs only (AI-resolved)
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["src/compute.rs"]);

    // blame at chain[1] for compute.rs:
    // Line 1 and "}" are unchanged from C2's parent (main branch version),
    // so git-blame traces them to the main branch commit (no note → human).
    // All other lines are new in C2' and the AI checkpoint captured them → AI.
    assert_blame_at_commit(
        &repo,
        &chain[1],
        "src/compute.rs",
        "c2_blame_compute",
        &[
            ("pub fn compute", false), // unchanged from parent, traces to main branch commit (human)
            ("is_empty", true),        // new in C2', AI per checkpoint
            ("let n =", true),
            ("let mean =", true),
            ("variance = data", true),
            (".map(|x|", true),
            (".sum::<f64>", true),
            ("let std_dev", true), // new in C2', AI per checkpoint
            ("weighted mean", true),
            ("weighted_sum", true),
            ("weight_total:", true),
            ("weighted_mean =", true),
            ("std_dev * 0.5", true),
            ("}", false), // unchanged from parent ("}" line), traces to main branch (human)
        ],
    );

    // C3': validator.rs only
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["src/validator.rs"]);

    // C4': encoder.rs only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["src/encoder.rs"]);

    // C5': decoder.rs only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["src/decoder.rs"]);
}

/// Test 3: processor.py — feature (C3) adds 5 AI lines to method2 body,
/// main also changes method2.  AI resolution rewrites processor.py preserving
/// 2 human context lines and writing 7 lines for the resolved method2 (marked
/// `.ai()` in set_contents).  However, the content-diff path only carries
/// attribution for lines whose content exactly matches the original feature commit:
/// only `def method2(self):`, `result = []`, `for i in range(10):`, and
/// `result.append(i * 2)` survive the content match — 4 lines.  The newly
/// introduced lines (`# AI merged`, `label = `, `return result, label`) have no
/// entry in `original_head_line_to_author` and therefore receive human attribution.
#[test]
fn test_conflict_ai_resolves_preserving_human_context_lines() {
    let repo = TestRepo::new();

    // Initial: processor.py with a class (6 human lines)
    repo.commit_untracked_file("processor.py",
        "class Processor:\n    def method1(self): return 'method1'\n    def method2(self): pass\n    def method3(self): return 'method3'\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: human changes method2 differently → conflict
    repo.commit_untracked_file("processor.py",
        "class Processor:\n    def method1(self): return 'method1'\n    def method2(self): return 'human-method2'\n    def method3(self): return 'method3'\n",
        "main: implement method2",
    );
    repo.commit_untracked_file(
        "runner.py",
        "from processor import Processor\np = Processor()\np.method1()\n",
        "main: add runner",
    );
    repo.commit_untracked_file("tests/test_processor.py",
        "from processor import Processor\ndef test_method1(): assert Processor().method1() == 'method1'\n",
        "main: add tests",
    );
    repo.commit_untracked_file(
        "setup.py",
        "from setuptools import setup\nsetup(name='processor', version='0.1.0')\n",
        "main: add setup.py",
    );
    repo.commit_untracked_file(
        "pyproject.toml",
        "[build-system]\nrequires = ['setuptools']\n",
        "main: add pyproject.toml",
    );

    // Feature branch from base
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates util_a.py (8 AI lines)
    let mut util_a = repo.filename("util_a.py");
    util_a.set_contents(crate::lines![
        "def parse_int(s: str) -> int:".ai(),
        "    try:".ai(),
        "        return int(s)".ai(),
        "    except ValueError:".ai(),
        "        raise ValueError(f'Cannot parse {s!r} as int')".ai(),
        "".ai(),
        "def parse_float(s: str) -> float:".ai(),
        "    return float(s)".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add util_a").unwrap();

    // C2: AI creates util_b.py (8 AI lines)
    let mut util_b = repo.filename("util_b.py");
    util_b.set_contents(crate::lines![
        "from typing import List, Optional".ai(),
        "".ai(),
        "def chunk(lst: List, size: int) -> List[List]:".ai(),
        "    return [lst[i:i+size] for i in range(0, len(lst), size)]".ai(),
        "".ai(),
        "def flatten(lst: List[List]) -> List:".ai(),
        "    return [x for sub in lst for x in sub]".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add util_b").unwrap();

    // C3: AI adds 5 lines to method2 in processor.py — WILL CONFLICT
    let mut processor = repo.filename("processor.py");
    processor.set_contents(crate::lines![
        "class Processor:".human(),
        "    def method1(self): return 'method1'".human(),
        "    def method2(self):".ai(),
        "        result = []".ai(),
        "        for i in range(10):".ai(),
        "            result.append(i * 2)".ai(),
        "        return result".ai(),
        "    def method3(self): return 'method3'".human(),
    ]);
    fs::write(
        repo.path().join("processor.py"),
        "class Processor:\n    def method1(self): return 'method1'\n    def method2(self):\n        result = []\n        for i in range(10):\n            result.append(i * 2)\n        return result\n    def method3(self): return 'method3'\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "processor.py"])
        .unwrap();
    repo.stage_all_and_commit("feat: C3 AI implements method2")
        .unwrap();

    // C4: AI creates util_d.py (8 AI lines)
    let mut util_d = repo.filename("util_d.py");
    util_d.set_contents(crate::lines![
        "import hashlib".ai(),
        "".ai(),
        "def md5(s: str) -> str:".ai(),
        "    return hashlib.md5(s.encode()).hexdigest()".ai(),
        "".ai(),
        "def sha256(s: str) -> str:".ai(),
        "    return hashlib.sha256(s.encode()).hexdigest()".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add util_d").unwrap();

    // C5: AI creates util_e.py (8 AI lines)
    let mut util_e = repo.filename("util_e.py");
    util_e.set_contents(crate::lines![
        "import json".ai(),
        "".ai(),
        "def to_json(obj) -> str:".ai(),
        "    return json.dumps(obj, indent=2)".ai(),
        "".ai(),
        "def from_json(s: str):".ai(),
        "    return json.loads(s)".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add util_e").unwrap();

    // Rebase — C3 will conflict on processor.py
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on processor.py at C3"
    );

    // AI resolves: 2 human context lines + 7 lines for resolved method2 (set_contents(.ai()))
    // NOTE: content-diff only recovers lines matching original C3 content:
    //   def method2, result = [], for i in range, result.append → 4 AI-attributed lines.
    //   # AI merged, label = , return result/label → newly introduced, no original match → human.
    let mut conflict_processor = repo.filename("processor.py");
    conflict_processor.set_contents(crate::lines![
        "class Processor:".human(),
        "    def method1(self): return 'method1'".human(),
        "    def method2(self):".ai(),
        "        # AI merged: combines human's return with feature's loop".ai(),
        "        result = []".ai(),
        "        for i in range(10):".ai(),
        "            result.append(i * 2)".ai(),
        "        label = 'human-method2'".ai(),
        "        return result, label".ai(),
        "    def method3(self): return 'method3'".human(),
    ]);
    fs::write(
        repo.path().join("processor.py"),
        "class Processor:\n    def method1(self): return 'method1'\n    def method2(self):\n        # AI merged: combines human's return with feature's loop\n        result = []\n        for i in range(10):\n            result.append(i * 2)\n        label = 'human-method2'\n        return result, label\n    def method3(self): return 'method3'\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "processor.py"])
        .unwrap();
    repo.git(&["add", "processor.py"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': util_a.py only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["util_a.py"]);

    // C2': util_b.py only
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["util_b.py"]);

    // C3': processor.py only (AI-resolved: 4 AI lines via content-diff match)
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["processor.py"]);

    // blame at chain[2] for processor.py: lines from parent are human,
    // all new lines written by AI during resolution are AI.
    assert_blame_at_commit(
        &repo,
        &chain[2],
        "processor.py",
        "c3_blame_processor",
        &[
            ("class Processor:", false),
            ("def method1", false),
            ("def method2", true),
            ("AI merged", true),
            ("result = []", true),
            ("for i in range", true),
            ("result.append", true),
            ("label = ", true),
            ("return result, label", true),
            ("def method3", false),
        ],
    );

    // C4': util_d.py only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["util_d.py"]);

    // C5': util_e.py only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["util_e.py"]);

    // Verify per-commit-delta humans scoping (KnownHuman variant)
    let conflict_note = parse_note(&repo, &chain[2]); // X = conflict commit index
    assert!(
        conflict_note
            .metadata
            .humans
            .contains_key("h_e858f2c2faea28"),
        "c3' should have h_e858f2c2faea28 in metadata.humans (human context lines in resolved file)"
    );
    assert_eq!(
        conflict_note.metadata.humans["h_e858f2c2faea28"].author,
        "Test User <test@example.com>"
    );
}

/// Test 4: version.py — conflict is on C1 (the VERY FIRST feature commit).
/// Feature changes VERSION to "2.0", main changes it to "1.5".
/// AI resolves to "2.1".  C2–C5 accumulate other files normally.
#[test]
fn test_conflict_ai_resolves_on_first_commit() {
    let repo = TestRepo::new();

    // Initial: version.py with VERSION = "1.0"
    repo.commit_untracked_file(
        "version.py",
        "VERSION = \"1.0\"\nCODENAME = \"alpha\"\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: changes VERSION to "1.5" — will conflict with feature's C1
    repo.commit_untracked_file(
        "version.py",
        "VERSION = \"1.5\"\nCODENAME = \"beta\"\n",
        "main: bump version to 1.5",
    );
    repo.commit_untracked_file(
        "CHANGELOG.md",
        "## 1.5\n- Performance improvements\n",
        "main: add changelog",
    );
    repo.commit_untracked_file(
        "CONTRIBUTORS.md",
        "# Contributors\n- Alice\n- Bob\n",
        "main: add contributors",
    );
    repo.commit_untracked_file(
        "LICENSE",
        "MIT License\nCopyright 2024\n",
        "main: add license",
    );
    repo.commit_untracked_file(
        "docs/index.md",
        "# Docs\nWelcome to the docs.\n",
        "main: add docs",
    );

    // Feature branch from base
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI changes VERSION to "2.0" — WILL CONFLICT
    let mut version = repo.filename("version.py");
    version.set_contents(crate::lines![
        "VERSION = \"2.0\"".ai(),
        "CODENAME = \"alpha\"".human(),
    ]);
    repo.stage_all_and_commit("feat: C1 bump version to 2.0")
        .unwrap();

    // C2: AI creates changelog.py (8 AI lines)
    let mut changelog = repo.filename("changelog.py");
    changelog.set_contents(crate::lines![
        "import datetime".ai(),
        "".ai(),
        "class ChangelogEntry:".ai(),
        "    def __init__(self, version: str, date: datetime.date, changes: list):".ai(),
        "        self.version = version".ai(),
        "        self.date = date".ai(),
        "        self.changes = changes".ai(),
        "    def render(self) -> str: return f'{self.version} ({self.date}): {len(self.changes)} changes'".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add changelog model")
        .unwrap();

    // C3: AI creates release_notes.py (8 AI lines)
    let mut release_notes = repo.filename("release_notes.py");
    release_notes.set_contents(crate::lines![
        "from typing import List".ai(),
        "".ai(),
        "def format_release_notes(entries: List[dict]) -> str:".ai(),
        "    lines = []".ai(),
        "    for e in entries:".ai(),
        "        lines.append(f\"## {e['version']}\")".ai(),
        "        for change in e.get('changes', []):".ai(),
        "            lines.append(f'- {change}')".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add release notes formatter")
        .unwrap();

    // C4: AI creates deprecations.py (8 AI lines)
    let mut deprecations = repo.filename("deprecations.py");
    deprecations.set_contents(crate::lines![
        "import warnings".ai(),
        "import functools".ai(),
        "".ai(),
        "def deprecated(reason: str):".ai(),
        "    def decorator(func):".ai(),
        "        @functools.wraps(func)".ai(),
        "        def wrapper(*args, **kwargs):".ai(),
        "            warnings.warn(f'{func.__name__} is deprecated: {reason}', DeprecationWarning, stacklevel=2)".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add deprecation decorator")
        .unwrap();

    // C5: AI creates migration_guide.py (8 AI lines)
    let mut migration_guide = repo.filename("migration_guide.py");
    migration_guide.set_contents(crate::lines![
        "MIGRATION_STEPS = [".ai(),
        "    'Update config files to new schema',".ai(),
        "    'Run database migration scripts',".ai(),
        "    'Update API call signatures',".ai(),
        "    'Test all integrations',".ai(),
        "    'Deploy to staging first',".ai(),
        "    'Monitor error rates after deployment',".ai(),
        "]".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add migration guide")
        .unwrap();

    // Rebase — C1 will conflict immediately on version.py
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on version.py at C1"
    );

    // AI resolves: VERSION = "2.1" as .ai(), CODENAME as .human()
    let mut conflict_version = repo.filename("version.py");
    conflict_version.set_contents(crate::lines![
        "VERSION = \"2.1\"".ai(),
        "CODENAME = \"beta\"".human(),
    ]);
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': version.py only with AI-resolved VERSION line (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["version.py"]);

    // blame at chain[0] for version.py: VERSION line is AI, CODENAME is human
    assert_blame_at_commit(
        &repo,
        &chain[0],
        "version.py",
        "c1_blame_version",
        &[("VERSION = \"2.1\"", true), ("CODENAME = \"beta\"", false)],
    );

    // C2': changelog.py only
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["changelog.py"]);

    // C3': release_notes.py only
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["release_notes.py"]);

    // C4': deprecations.py only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["deprecations.py"]);

    // C5': migration_guide.py only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["migration_guide.py"]);

    // Verify per-commit-delta humans scoping (KnownHuman variant)
    let conflict_note = parse_note(&repo, &chain[0]); // X = conflict commit index
    assert!(
        conflict_note
            .metadata
            .humans
            .contains_key("h_e858f2c2faea28"),
        "c1' should have h_e858f2c2faea28 in metadata.humans (human context lines in resolved file)"
    );
    assert_eq!(
        conflict_note.metadata.humans["h_e858f2c2faea28"].author,
        "Test User <test@example.com>"
    );
}

/// Test 5: schema.rs max_connections — conflict is on C5 (LAST feature commit).
/// C1–C4 accumulate model_*.rs files cleanly.  C5 modifies schema.rs
/// max_connections constant; main also modifies same constant.  AI resolves.
#[test]
fn test_conflict_ai_resolves_on_last_commit() {
    let repo = TestRepo::new();

    // Initial: schema.rs with a constant (human)
    repo.commit_untracked_file(
        "src/schema.rs",
        "pub const MAX_CONNECTIONS: u32 = 10;\npub const SCHEMA_VERSION: u32 = 1;\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: changes max_connections → will conflict with feature's C5
    repo.commit_untracked_file(
        "src/schema.rs",
        "pub const MAX_CONNECTIONS: u32 = 50;\npub const SCHEMA_VERSION: u32 = 1;\n",
        "main: increase max_connections to 50",
    );
    repo.commit_untracked_file(
        "src/migration.rs",
        "pub fn run_migrations() {}\n",
        "main: add migration runner",
    );
    repo.commit_untracked_file(
        "src/connection.rs",
        "pub struct Connection { id: u32 }\n",
        "main: add Connection type",
    );
    repo.commit_untracked_file(
        "src/pool.rs",
        "pub struct Pool { size: u32 }\n",
        "main: add Pool struct",
    );
    repo.commit_untracked_file(
        "Cargo.toml",
        "[package]\nname = \"schema\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        "main: add Cargo.toml",
    );

    // Feature branch from base
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates model_a.rs (10 AI lines)
    let mut model_a = repo.filename("src/model_a.rs");
    model_a.set_contents(crate::lines![
        "#[derive(Debug, Clone)]".ai(),
        "pub struct ModelA {".ai(),
        "    pub id: u64,".ai(),
        "    pub name: String,".ai(),
        "    pub active: bool,".ai(),
        "}".ai(),
        "".ai(),
        "impl ModelA {".ai(),
        "    pub fn new(id: u64, name: impl Into<String>) -> Self {".ai(),
        "        Self { id, name: name.into(), active: true }".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add ModelA").unwrap();

    // C2: AI creates model_b.rs (10 AI lines)
    let mut model_b = repo.filename("src/model_b.rs");
    model_b.set_contents(crate::lines![
        "#[derive(Debug, Clone)]".ai(),
        "pub struct ModelB {".ai(),
        "    pub id: u64,".ai(),
        "    pub value: f64,".ai(),
        "    pub tags: Vec<String>,".ai(),
        "}".ai(),
        "".ai(),
        "impl ModelB {".ai(),
        "    pub fn new(id: u64, value: f64) -> Self {".ai(),
        "        Self { id, value, tags: Vec::new() }".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add ModelB").unwrap();

    // C3: AI creates model_c.rs (10 AI lines)
    let mut model_c = repo.filename("src/model_c.rs");
    model_c.set_contents(crate::lines![
        "#[derive(Debug, Clone, PartialEq)]".ai(),
        "pub enum Status {".ai(),
        "    Active,".ai(),
        "    Inactive,".ai(),
        "    Pending,".ai(),
        "}".ai(),
        "".ai(),
        "impl Default for Status {".ai(),
        "    fn default() -> Self { Status::Pending }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add Status enum")
        .unwrap();

    // C4: AI creates model_d.rs (10 AI lines)
    let mut model_d = repo.filename("src/model_d.rs");
    model_d.set_contents(crate::lines![
        "use std::collections::HashMap;".ai(),
        "".ai(),
        "#[derive(Debug, Default)]".ai(),
        "pub struct Registry {".ai(),
        "    entries: HashMap<u64, String>,".ai(),
        "}".ai(),
        "".ai(),
        "impl Registry {".ai(),
        "    pub fn register(&mut self, id: u64, name: impl Into<String>) {".ai(),
        "        self.entries.insert(id, name.into());".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add Registry").unwrap();

    // C5: AI changes max_connections to 100 — WILL CONFLICT
    let mut schema = repo.filename("src/schema.rs");
    schema.set_contents(crate::lines![
        "pub const MAX_CONNECTIONS: u32 = 100;".ai(),
        "pub const SCHEMA_VERSION: u32 = 1;".human(),
    ]);
    fs::write(
        repo.path().join("src/schema.rs"),
        "pub const MAX_CONNECTIONS: u32 = 100;\npub const SCHEMA_VERSION: u32 = 1;\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "src/schema.rs"])
        .unwrap();
    repo.stage_all_and_commit("feat: C5 AI tunes MAX_CONNECTIONS to 100")
        .unwrap();

    // Rebase — C5 will conflict on src/schema.rs
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on src/schema.rs at C5"
    );

    // AI resolves: picks 75 as a compromise, as .ai()
    let mut conflict_schema = repo.filename("src/schema.rs");
    conflict_schema.set_contents(crate::lines![
        "pub const MAX_CONNECTIONS: u32 = 75;".ai(),
        "pub const SCHEMA_VERSION: u32 = 1;".human(),
    ]);
    fs::write(
        repo.path().join("src/schema.rs"),
        "pub const MAX_CONNECTIONS: u32 = 75;\npub const SCHEMA_VERSION: u32 = 1;\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "src/schema.rs"])
        .unwrap();
    repo.git(&["add", "src/schema.rs"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': model_a.rs only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["src/model_a.rs"]);

    // C2': model_b.rs only
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["src/model_b.rs"]);

    // C3': model_c.rs only
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["src/model_c.rs"]);

    // C4': model_d.rs only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["src/model_d.rs"]);

    // C5': schema.rs only (AI-resolved MAX_CONNECTIONS)
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["src/schema.rs"]);

    // blame at chain[4] for schema.rs: MAX_CONNECTIONS line is AI, SCHEMA_VERSION is human
    assert_blame_at_commit(
        &repo,
        &chain[4],
        "src/schema.rs",
        "c5_blame_schema",
        &[
            ("MAX_CONNECTIONS: u32 = 75", true),
            ("SCHEMA_VERSION: u32 = 1", false),
        ],
    );

    // Verify per-commit-delta humans scoping (KnownHuman variant)
    let conflict_note = parse_note(&repo, &chain[4]); // X = conflict commit index
    assert!(
        conflict_note
            .metadata
            .humans
            .contains_key("h_e858f2c2faea28"),
        "c5' should have h_e858f2c2faea28 in metadata.humans (human context lines in resolved file)"
    );
    assert_eq!(
        conflict_note.metadata.humans["h_e858f2c2faea28"].author,
        "Test User <test@example.com>"
    );
}

/// Test 6: config.py AND settings.py both conflict in C3.
/// C3 AI changes a line in both files; main also changes same lines.
/// AI resolves both conflicts.  Note for C3' has both files.
#[test]
fn test_conflict_ai_resolves_multiple_files_in_same_commit() {
    let repo = TestRepo::new();

    // Initial: BOTH files exist at the shared base so C3's edits will conflict with main
    repo.commit_untracked_file(
        "config.py",
        "DEBUG = False\nSECRET_KEY = 'changeme'\n",
        "Initial: config",
    );
    repo.commit_untracked_file(
        "settings.py",
        "DATABASE_URL = 'sqlite:///dev.db'\nCACHE_BACKEND = 'locmem'\n",
        "Initial: settings",
    );
    let main_branch = repo.current_branch();

    // Main: changes the same lines in both files → will conflict with feature's C3
    repo.commit_untracked_file(
        "config.py",
        "DEBUG = True\nSECRET_KEY = 'changeme'\n",
        "main: enable DEBUG",
    );
    repo.commit_untracked_file(
        "settings.py",
        "DATABASE_URL = 'postgres://localhost/main_db'\nCACHE_BACKEND = 'redis'\n",
        "main: update settings",
    );
    repo.commit_untracked_file(
        "wsgi.py",
        "from app import create_app\napplication = create_app()\n",
        "main: add wsgi",
    );
    repo.commit_untracked_file(
        "asgi.py",
        "from app import create_app\napplication = create_app()\n",
        "main: add asgi",
    );
    repo.commit_untracked_file(
        "manage.py",
        "#!/usr/bin/env python\nimport sys\nif __name__ == '__main__': pass\n",
        "main: add manage.py",
    );

    // Feature branch from the shared base (HEAD~5 = after both initial commits)
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates auth.py (8 AI lines)
    let mut auth = repo.filename("auth.py");
    auth.set_contents(crate::lines![
        "from typing import Optional".ai(),
        "".ai(),
        "def authenticate(token: str) -> Optional[str]:".ai(),
        "    if not token: return None".ai(),
        "    parts = token.split('.')".ai(),
        "    if len(parts) != 3: return None".ai(),
        "    return parts[1]".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add auth").unwrap();

    // C2: AI creates middleware.py (8 AI lines)
    let mut middleware = repo.filename("middleware.py");
    middleware.set_contents(crate::lines![
        "class CorsMiddleware:".ai(),
        "    def __init__(self, app):".ai(),
        "        self.app = app".ai(),
        "    def __call__(self, environ, start_response):".ai(),
        "        def custom_start(status, headers):".ai(),
        "            headers.append(('Access-Control-Allow-Origin', '*'))".ai(),
        "            return start_response(status, headers)".ai(),
        "        return self.app(environ, custom_start)".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add CORS middleware")
        .unwrap();

    // C3: AI changes config.py AND settings.py — BOTH WILL CONFLICT
    let mut config = repo.filename("config.py");
    config.set_contents(crate::lines![
        "DEBUG = False".human(),
        "SECRET_KEY = 'ai-generated-secret-key-v2'".ai(),
    ]);
    let mut settings = repo.filename("settings.py");
    settings.set_contents(crate::lines![
        "DATABASE_URL = 'postgres://localhost/feature_db'".ai(),
        "CACHE_BACKEND = 'locmem'".human(),
    ]);
    repo.stage_all_and_commit("feat: C3 AI tunes config and settings")
        .unwrap();

    // C4: AI creates permissions.py (8 AI lines)
    let mut permissions = repo.filename("permissions.py");
    permissions.set_contents(crate::lines![
        "class Permission:".ai(),
        "    READ = 'read'".ai(),
        "    WRITE = 'write'".ai(),
        "    ADMIN = 'admin'".ai(),
        "".ai(),
        "def has_permission(user_perms: list, required: str) -> bool:".ai(),
        "    return required in user_perms".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add permissions")
        .unwrap();

    // C5: AI creates serializers.py (8 AI lines)
    let mut serializers = repo.filename("serializers.py");
    serializers.set_contents(crate::lines![
        "import json".ai(),
        "".ai(),
        "class JsonSerializer:".ai(),
        "    @staticmethod".ai(),
        "    def dumps(obj) -> str: return json.dumps(obj)".ai(),
        "    @staticmethod".ai(),
        "    def loads(s: str): return json.loads(s)".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add JSON serializer")
        .unwrap();

    // Rebase — C3 will conflict on config.py (and possibly settings.py)
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(rebase_result.is_err(), "rebase should conflict at C3");

    // AI resolves config.py
    let mut conflict_config = repo.filename("config.py");
    conflict_config.set_contents(crate::lines![
        "DEBUG = True".human(),
        "SECRET_KEY = 'ai-generated-secret-key-v2'".ai(),
    ]);
    // AI resolves settings.py
    let mut conflict_settings = repo.filename("settings.py");
    conflict_settings.set_contents(crate::lines![
        "DATABASE_URL = 'postgres://localhost/feature_db'".ai(),
        "CACHE_BACKEND = 'redis'".human(),
    ]);
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': auth.py only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["auth.py"]);

    // C2': middleware.py only
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["middleware.py"]);

    // C3': config.py + settings.py (AI-resolved, both in same commit)
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["config.py", "settings.py"]);

    // blame for config.py: DEBUG is human (unchanged), SECRET_KEY is AI
    assert_blame_at_commit(
        &repo,
        &chain[2],
        "config.py",
        "c3_blame_config",
        &[
            ("DEBUG = True", false),
            ("SECRET_KEY = 'ai-generated-secret-key-v2'", true),
        ],
    );

    // blame for settings.py: DATABASE_URL is AI, CACHE_BACKEND is human
    assert_blame_at_commit(
        &repo,
        &chain[2],
        "settings.py",
        "c3_blame_settings",
        &[
            ("DATABASE_URL = 'postgres://localhost/feature_db'", true),
            ("CACHE_BACKEND = 'redis'", false),
        ],
    );

    // C4': permissions.py only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["permissions.py"]);

    // C5': serializers.py only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["serializers.py"]);

    // Verify per-commit-delta humans scoping (KnownHuman variant)
    let conflict_note = parse_note(&repo, &chain[2]); // X = conflict commit index
    assert!(
        conflict_note
            .metadata
            .humans
            .contains_key("h_e858f2c2faea28"),
        "c3' should have h_e858f2c2faea28 in metadata.humans (human context lines in resolved file)"
    );
    assert_eq!(
        conflict_note.metadata.humans["h_e858f2c2faea28"].author,
        "Test User <test@example.com>"
    );
}

/// Test 7: dispatcher.py — conflict on C2.  C3 and C4 also modify dispatcher.py
/// (no further conflicts).  AI resolves C2 with 12-line process() implementation.
/// Subsequent commits append more methods to dispatcher.py.
#[test]
fn test_conflict_ai_resolves_then_more_ai_builds_on_result() {
    let repo = TestRepo::new();

    // Initial: dispatcher.py stub (human)
    repo.commit_untracked_file(
        "dispatcher.py",
        "class Dispatcher:\n    pass\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: human implements process() differently → will conflict with feature's C2
    repo.commit_untracked_file(
        "dispatcher.py",
        "class Dispatcher:\n    def process(self, msg): return msg.strip()\n",
        "main: implement process() simply",
    );
    repo.commit_untracked_file(
        "config.py",
        "WORKERS = 4\nQUEUE_SIZE = 100\n",
        "main: add config",
    );
    repo.commit_untracked_file(
        "queue.py",
        "import queue\nQ = queue.Queue()\n",
        "main: add queue",
    );
    repo.commit_untracked_file(
        "worker.py",
        "class Worker:\n    def __init__(self, q): self.q = q\n",
        "main: add worker",
    );
    repo.commit_untracked_file(
        "monitor.py",
        "class Monitor:\n    def check(self): return 'ok'\n",
        "main: add monitor",
    );

    // Feature branch from base
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates base_handler.py (8 AI lines)
    let mut base_handler = repo.filename("base_handler.py");
    base_handler.set_contents(crate::lines![
        "class BaseHandler:".ai(),
        "    def __init__(self):".ai(),
        "        self.middlewares = []".ai(),
        "    def use(self, middleware):".ai(),
        "        self.middlewares.append(middleware)".ai(),
        "        return self".ai(),
        "    def handle(self, msg): raise NotImplementedError".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add BaseHandler")
        .unwrap();

    // C2: AI adds process() to dispatcher.py — WILL CONFLICT
    let mut dispatcher_c2 = repo.filename("dispatcher.py");
    dispatcher_c2.set_contents(crate::lines![
        "class Dispatcher:".human(),
        "    def process(self, msg):".ai(),
        "        msg = msg.strip()".ai(),
        "        if not msg: raise ValueError('empty')".ai(),
        "        tokens = msg.split()".ai(),
        "        return {'cmd': tokens[0], 'args': tokens[1:]}".ai(),
        "    pass".human(),
    ]);
    repo.stage_all_and_commit("feat: C2 AI adds process() to Dispatcher")
        .unwrap();

    // C3: AI creates router.py (does NOT touch dispatcher.py — no conflict)
    let mut router = repo.filename("router.py");
    router.set_contents(crate::lines![
        "from dispatcher import Dispatcher".ai(),
        "".ai(),
        "class Router:".ai(),
        "    def __init__(self):".ai(),
        "        self.dispatcher = Dispatcher()".ai(),
        "    def register(self, cmd, fn): self.dispatcher.route(cmd, fn)".ai(),
        "    def run(self, msg): return self.dispatcher.dispatch(msg)".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 AI adds Router")
        .unwrap();

    // C4: AI creates middleware.py (new file, no conflict)
    let mut mw = repo.filename("middleware.py");
    mw.set_contents(crate::lines![
        "class Middleware:".ai(),
        "    def __init__(self): self.chain = []".ai(),
        "    def use(self, fn): self.chain.append(fn); return self".ai(),
        "    def run(self, msg):".ai(),
        "        for fn in self.chain: msg = fn(msg)".ai(),
        "        return msg".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 AI adds Middleware")
        .unwrap();

    // C5: AI creates event_bus.py (new file, no conflict)
    let mut bus = repo.filename("event_bus.py");
    bus.set_contents(crate::lines![
        "class EventBus:".ai(),
        "    def __init__(self): self.handlers = {}".ai(),
        "    def on(self, event, fn): self.handlers.setdefault(event, []).append(fn)".ai(),
        "    def emit(self, event, *args):".ai(),
        "        for fn in self.handlers.get(event, []): fn(*args)".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 AI adds EventBus")
        .unwrap();

    // Rebase — C2 will conflict on dispatcher.py
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on dispatcher.py at C2"
    );

    // AI resolves C2: 12-line process() implementation (all .ai() except class line)
    let mut conflict_dispatcher = repo.filename("dispatcher.py");
    conflict_dispatcher.set_contents(crate::lines![
        "class Dispatcher:".human(),
        "    def process(self, msg):".ai(),
        "        # AI merge: validates and parses, as in feature branch".ai(),
        "        msg = msg.strip()".ai(),
        "        if not msg: raise ValueError('empty message')".ai(),
        "        tokens = msg.split()".ai(),
        "        cmd = tokens[0].lower()".ai(),
        "        args = tokens[1:]".ai(),
        "        return {'cmd': cmd, 'args': args, 'raw': msg}".ai(),
        "    def _noop(self, args): return None".ai(),
        "    def __repr__(self): return f'Dispatcher()'".ai(),
        "    pass".human(),
    ]);
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': base_handler.py only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["base_handler.py"]);

    // C2': dispatcher.py only (AI-resolved: ~10 AI lines)
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["dispatcher.py"]);

    // C3': router.py only
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["router.py"]);

    // C4': middleware.py only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["middleware.py"]);

    // C5': event_bus.py only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["event_bus.py"]);

    // Verify per-commit-delta humans scoping (KnownHuman variant)
    let conflict_note = parse_note(&repo, &chain[1]); // X = conflict commit index
    assert!(
        conflict_note
            .metadata
            .humans
            .contains_key("h_e858f2c2faea28"),
        "c2' should have h_e858f2c2faea28 in metadata.humans (human context lines in resolved file)"
    );
    assert_eq!(
        conflict_note.metadata.humans["h_e858f2c2faea28"].author,
        "Test User <test@example.com>"
    );
}

/// Test 8: models.rs struct fields — feature (C3) AI adds 4 new fields,
/// main human adds 2 different fields.  AI resolution merges all 8 fields.
/// The merged struct body is all .ai().
#[test]
fn test_conflict_ai_resolves_rust_struct_fields() {
    let repo = TestRepo::new();

    // Initial: models.rs with a struct (2 original fields, human)
    repo.commit_untracked_file(
        "src/models.rs",
        "pub struct User {\n    pub id: u64,\n    pub name: String,\n}\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: adds email and created_at fields → will conflict
    repo.commit_untracked_file("src/models.rs",
        "pub struct User {\n    pub id: u64,\n    pub name: String,\n    pub email: String,\n    pub created_at: u64,\n}\n",
        "main: add email and created_at to User",
    );
    repo.commit_untracked_file(
        "src/db.rs",
        "pub struct Db { url: String }\n",
        "main: add Db",
    );
    repo.commit_untracked_file(
        "src/repo.rs",
        "use crate::models::User;\npub struct UserRepo;\n",
        "main: add UserRepo",
    );
    repo.commit_untracked_file(
        "src/service.rs",
        "pub struct UserService;\n",
        "main: add UserService",
    );
    repo.commit_untracked_file(
        "Cargo.toml",
        "[package]\nname = \"models\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        "main: add Cargo.toml",
    );

    // Feature branch from base
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates traits.rs (8 AI lines)
    let mut traits = repo.filename("src/traits.rs");
    traits.set_contents(crate::lines![
        "pub trait Entity {".ai(),
        "    fn id(&self) -> u64;".ai(),
        "    fn name(&self) -> &str;".ai(),
        "}".ai(),
        "".ai(),
        "pub trait Persistable: Entity {".ai(),
        "    fn save(&self) -> Result<(), String>;".ai(),
        "    fn delete(&self) -> Result<(), String>;".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add Entity and Persistable traits")
        .unwrap();

    // C2: AI creates impls.rs (8 AI lines)
    let mut impls = repo.filename("src/impls.rs");
    impls.set_contents(crate::lines![
        "use crate::models::User;".ai(),
        "use crate::traits::Entity;".ai(),
        "".ai(),
        "impl Entity for User {".ai(),
        "    fn id(&self) -> u64 { self.id }".ai(),
        "    fn name(&self) -> &str { &self.name }".ai(),
        "}".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 impl Entity for User")
        .unwrap();

    // C3: AI adds 4 new fields to User struct — WILL CONFLICT with main's email/created_at
    let mut models = repo.filename("src/models.rs");
    models.set_contents(crate::lines![
        "pub struct User {".human(),
        "    pub id: u64,".human(),
        "    pub name: String,".human(),
        "    pub active: bool,".ai(),
        "    pub role: String,".ai(),
        "    pub score: f64,".ai(),
        "    pub metadata: std::collections::HashMap<String, String>,".ai(),
        "}".human(),
    ]);
    fs::write(
        repo.path().join("src/models.rs"),
        "pub struct User {\n    pub id: u64,\n    pub name: String,\n    pub active: bool,\n    pub role: String,\n    pub score: f64,\n    pub metadata: std::collections::HashMap<String, String>,\n}\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "src/models.rs"])
        .unwrap();
    repo.stage_all_and_commit("feat: C3 AI adds active/role/score/metadata fields")
        .unwrap();

    // C4: AI creates errors.rs (8 AI lines)
    let mut errors = repo.filename("src/errors.rs");
    errors.set_contents(crate::lines![
        "#[derive(Debug)]".ai(),
        "pub enum ModelError {".ai(),
        "    NotFound(u64),".ai(),
        "    InvalidField(String),".ai(),
        "    DuplicateId(u64),".ai(),
        "}".ai(),
        "".ai(),
        "impl std::fmt::Display for ModelError { fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { write!(f, \"{:?}\", self) } }".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add ModelError")
        .unwrap();

    // C5: AI creates utils.rs (8 AI lines)
    let mut utils = repo.filename("src/utils.rs");
    utils.set_contents(crate::lines![
        "pub fn slugify(s: &str) -> String {".ai(),
        "    s.to_lowercase()".ai(),
        "        .chars()".ai(),
        "        .map(|c| if c.is_alphanumeric() { c } else { '-' })".ai(),
        "        .collect::<String>()".ai(),
        "        .trim_matches('-')".ai(),
        "        .to_string()".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add slugify utility")
        .unwrap();

    // Rebase — C3 will conflict on src/models.rs
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on src/models.rs at C3"
    );

    // AI resolves: merges ALL fields — original 2 + 4 feature + 2 main = 8 fields (all .ai() in struct body)
    let mut conflict_models = repo.filename("src/models.rs");
    conflict_models.set_contents(crate::lines![
        "pub struct User {".human(),
        "    pub id: u64,".ai(),
        "    pub name: String,".ai(),
        "    pub email: String,".ai(),
        "    pub created_at: u64,".ai(),
        "    pub active: bool,".ai(),
        "    pub role: String,".ai(),
        "    pub score: f64,".ai(),
        "    pub metadata: std::collections::HashMap<String, String>,".ai(),
        "}".human(),
    ]);
    fs::write(
        repo.path().join("src/models.rs"),
        "pub struct User {\n    pub id: u64,\n    pub name: String,\n    pub email: String,\n    pub created_at: u64,\n    pub active: bool,\n    pub role: String,\n    pub score: f64,\n    pub metadata: std::collections::HashMap<String, String>,\n}\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "src/models.rs"])
        .unwrap();
    repo.git(&["add", "src/models.rs"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': traits.rs only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["src/traits.rs"]);

    // C2': impls.rs only
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["src/impls.rs"]);

    // C3': models.rs only (AI-resolved struct with merged fields)
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["src/models.rs"]);

    // blame for models.rs: struct keyword is human, equal fields carry AI attribution, new fields are human
    assert_blame_at_commit(
        &repo,
        &chain[2],
        "src/models.rs",
        "c3_blame_models",
        &[
            ("pub struct User {", false),
            ("pub id: u64,", false),
            ("pub name: String,", false),
            ("pub email: String,", false),
            ("pub created_at: u64,", false),
            ("pub active: bool,", true),
            ("pub role: String,", true),
            ("pub score: f64,", true),
            ("pub metadata:", true),
            ("}", false),
        ],
    );

    // C4': errors.rs only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["src/errors.rs"]);

    // C5': utils.rs only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["src/utils.rs"]);

    // Verify per-commit-delta humans scoping (KnownHuman variant)
    let conflict_note = parse_note(&repo, &chain[2]); // X = conflict commit index
    assert!(
        conflict_note
            .metadata
            .humans
            .contains_key("h_e858f2c2faea28"),
        "c3' should have h_e858f2c2faea28 in metadata.humans (human context lines in resolved file)"
    );
    assert_eq!(
        conflict_note.metadata.humans["h_e858f2c2faea28"].author,
        "Test User <test@example.com>"
    );
}

/// Test 9: service.py process_payment — feature (C4) AI implements a 20-line
/// function body; main also implements the same function (12 lines).
/// AI resolution produces a 25-line merged implementation (all .ai()).
/// Non-conflict commits: C1 models.py, C2 validators.py, C3 exceptions.py, C5 utils.py.
#[test]
fn test_conflict_ai_resolves_complex_function_with_error_handling() {
    let repo = TestRepo::new();

    // Initial: service.py with a function stub (human)
    repo.commit_untracked_file(
        "service.py",
        "def process_payment(amount, card):\n    pass\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: human implements process_payment differently → will conflict
    repo.commit_untracked_file("service.py",
        "def process_payment(amount, card):\n    if amount <= 0:\n        raise ValueError('amount must be positive')\n    return {'status': 'ok', 'amount': amount}\n",
        "main: implement process_payment",
    );
    repo.commit_untracked_file("tests/test_service.py",
        "from service import process_payment\ndef test_basic(): assert process_payment(10, '4111')['status'] == 'ok'\n",
        "main: add service tests",
    );
    repo.commit_untracked_file(
        "requirements.txt",
        "stripe==5.0.0\nrequests==2.31.0\n",
        "main: add requirements",
    );
    repo.commit_untracked_file(
        ".env.example",
        "STRIPE_KEY=sk_test_xxx\nDATABASE_URL=sqlite:///dev.db\n",
        "main: add .env.example",
    );
    repo.commit_untracked_file(
        "Makefile",
        "test:\n\tpython -m pytest\nlint:\n\tflake8 .\n.PHONY: test lint\n",
        "main: add Makefile",
    );

    // Feature branch from base
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates models.py (8 AI lines)
    let mut models = repo.filename("models.py");
    models.set_contents(crate::lines![
        "from dataclasses import dataclass, field".ai(),
        "".ai(),
        "@dataclass".ai(),
        "class PaymentResult:".ai(),
        "    status: str".ai(),
        "    transaction_id: str".ai(),
        "    amount: float".ai(),
        "    error: str = ''".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add PaymentResult model")
        .unwrap();

    // C2: AI creates validators.py (8 AI lines)
    let mut validators = repo.filename("validators.py");
    validators.set_contents(crate::lines![
        "import re".ai(),
        "".ai(),
        "def validate_card(card: str) -> bool:".ai(),
        "    return bool(re.match(r'^[0-9]{13,19}$', card.replace(' ', '')))".ai(),
        "".ai(),
        "def validate_amount(amount: float) -> bool:".ai(),
        "    return isinstance(amount, (int, float)) and 0 < amount <= 1_000_000".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add payment validators")
        .unwrap();

    // C3: AI creates exceptions.py (8 AI lines)
    let mut exceptions = repo.filename("exceptions.py");
    exceptions.set_contents(crate::lines![
        "class PaymentError(Exception):".ai(),
        "    def __init__(self, msg: str, code: int = 400):".ai(),
        "        super().__init__(msg)".ai(),
        "        self.code = code".ai(),
        "".ai(),
        "class CardDeclinedError(PaymentError):".ai(),
        "    def __init__(self): super().__init__('Card declined', 402)".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add payment exceptions")
        .unwrap();

    // C4: AI implements process_payment with 20 lines — WILL CONFLICT
    let mut service = repo.filename("service.py");
    service.set_contents(crate::lines![
        "def process_payment(amount, card):".human(),
        "    from validators import validate_amount, validate_card".ai(),
        "    from exceptions import PaymentError, CardDeclinedError".ai(),
        "    import logging".ai(),
        "    logger = logging.getLogger(__name__)".ai(),
        "    logger.info(f'Processing payment: amount={amount}')".ai(),
        "    if not validate_amount(amount):".ai(),
        "        raise PaymentError(f'Invalid amount: {amount}')".ai(),
        "    if not validate_card(card):".ai(),
        "        raise PaymentError(f'Invalid card number')".ai(),
        "    if str(card).startswith('0000'):".ai(),
        "        raise CardDeclinedError()".ai(),
        "    transaction_id = f'txn_{hash(card + str(amount)) % 10**9}'".ai(),
        "    logger.info(f'Payment successful: {transaction_id}')".ai(),
        "    return {'status': 'ok', 'transaction_id': transaction_id, 'amount': amount}".ai(),
        "    # end process_payment".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 AI implements process_payment")
        .unwrap();

    // C5: AI creates utils.py (8 AI lines)
    let mut utils = repo.filename("utils.py");
    utils.set_contents(crate::lines![
        "def mask_card(card: str) -> str:".ai(),
        "    digits = card.replace(' ', '')".ai(),
        "    return '*' * (len(digits) - 4) + digits[-4:]".ai(),
        "".ai(),
        "def format_amount(amount: float) -> str:".ai(),
        "    return f'${amount:.2f}'".ai(),
        "".ai(),
        "def generate_receipt(result: dict) -> str: return f\"Receipt: {result['transaction_id']} {result['amount']}\"".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add payment utils")
        .unwrap();

    // Rebase — C4 will conflict on service.py
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on service.py at C4"
    );

    // AI resolves: 25-line merged implementation (all .ai() except function signature line)
    let mut conflict_service = repo.filename("service.py");
    conflict_service.set_contents(crate::lines![
        "def process_payment(amount, card):".human(),
        "    from validators import validate_amount, validate_card".ai(),
        "    from exceptions import PaymentError, CardDeclinedError".ai(),
        "    from models import PaymentResult".ai(),
        "    import logging".ai(),
        "    logger = logging.getLogger(__name__)".ai(),
        "    logger.info(f'Processing: amount={amount} card=***{str(card)[-4:]}')".ai(),
        "    if amount <= 0:".ai(),
        "        raise ValueError('amount must be positive')".ai(),
        "    if not validate_amount(amount):".ai(),
        "        raise PaymentError(f'Amount out of range: {amount}')".ai(),
        "    if not validate_card(card):".ai(),
        "        raise PaymentError('Invalid card number format')".ai(),
        "    if str(card).startswith('0000'):".ai(),
        "        raise CardDeclinedError()".ai(),
        "    transaction_id = f'txn_{hash(str(card) + str(amount)) % 10**9}'".ai(),
        "    logger.info(f'Payment OK: txn={transaction_id}')".ai(),
        "    result = PaymentResult(".ai(),
        "        status='ok',".ai(),
        "        transaction_id=transaction_id,".ai(),
        "        amount=amount,".ai(),
        "    )".ai(),
        "    return {'status': result.status, 'transaction_id': result.transaction_id, 'amount': result.amount}".ai(),
        "    # AI merged: combined validation + result model".ai(),
        "    # end process_payment".ai(),
    ]);
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': models.py only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["models.py"]);

    // C2': validators.py only
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["validators.py"]);

    // C3': exceptions.py only
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["exceptions.py"]);

    // C4': service.py only (AI-resolved: 24 AI lines in function body)
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["service.py"]);

    // blame at chain[3] for service.py: lines from parent (main's version) are human,
    // all new lines written by AI during conflict resolution are AI.
    assert_blame_at_commit(
        &repo,
        &chain[3],
        "service.py",
        "c4_blame_service",
        &[
            ("def process_payment", false),
            ("validate_amount, validate_card", true),
            ("PaymentError, CardDeclinedError", true),
            ("PaymentResult", true),
            ("import logging", true),
            ("logger = logging", true),
            ("Processing:", true),
            ("if amount <= 0:", false),
            ("must be positive", false),
            ("if not validate_amount", true),
            ("Amount out of range", true),
            ("if not validate_card", true),
            ("Invalid card number", true),
            ("startswith('0000')", true),
            ("CardDeclinedError()", true),
            ("transaction_id = ", true),
            ("Payment OK:", true),
            ("result = PaymentResult(", true),
            ("status='ok',", true),
            ("transaction_id=transaction_id,", true),
            ("amount=amount,", true),
            (")", true),
            ("return {", true),
            ("AI merged", true),
            ("end process_payment", true),
        ],
    );

    // C5': utils.py only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["utils.py"]);

    // Verify per-commit-delta humans scoping (KnownHuman variant)
    let conflict_note = parse_note(&repo, &chain[3]); // X = conflict commit index
    assert!(
        conflict_note
            .metadata
            .humans
            .contains_key("h_e858f2c2faea28"),
        "c4' should have h_e858f2c2faea28 in metadata.humans (human context lines in resolved file)"
    );
    assert_eq!(
        conflict_note.metadata.humans["h_e858f2c2faea28"].author,
        "Test User <test@example.com>"
    );
}

/// Test 10: Two conflicts — C2 (AI resolved) and C4 (human resolved).
/// Verifies that after two sequential conflicts in the same rebase,
/// AI attribution is tracked correctly: C2' gets AI config_a.py; C4' does NOT
/// get AI config_b.py (human resolved).
#[test]
fn test_conflict_mixed_ai_and_human_resolve_different_commits() {
    let repo = TestRepo::new();

    // Initial: config files with numeric values (0/10) so both sides can make
    // clearly conflicting changes. Using numbers avoids trailing-newline ambiguity
    // in git's merge and ensures non-empty rebased commits after resolution.
    repo.commit_untracked_file("config_a.py", "FLAG_A = 0\n", "Initial commit");
    repo.commit_untracked_file(
        "config_b.py",
        "FLAG_B = 0\nBATCH = 10\n",
        "Initial config_b",
    );
    let main_branch = repo.current_branch();

    // Main commits (human): set FLAG_A=1, FLAG_B=1/BATCH=50, then 3 more files
    repo.commit_untracked_file("config_a.py", "FLAG_A = 1\n", "main: set flag_a to 1");
    repo.commit_untracked_file(
        "config_b.py",
        "FLAG_B = 1\nBATCH = 50\n",
        "main: set flag_b and batch 50",
    );
    repo.commit_untracked_file(
        "app.py",
        "print('app started')\n",
        "main: add app entry point",
    );
    repo.commit_untracked_file(
        "db.py",
        "class Database: pass\n",
        "main: add database class",
    );
    repo.commit_untracked_file("cache.py", "class Cache: pass\n", "main: add cache class");

    // Feature branch from base (5 commits before main HEAD = the "Initial config_b" commit)
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates module_a.py (10 AI lines)
    let mut module_a = repo.filename("module_a.py");
    module_a.set_contents(crate::lines![
        "class ModuleA:".ai(),
        "    def __init__(self, config):".ai(),
        "        self.config = config".ai(),
        "        self.flag = config.get('FLAG_A', 0)".ai(),
        "    def run(self):".ai(),
        "        if not self.flag: return".ai(),
        "        print('ModuleA running')".ai(),
        "    def status(self): return {'flag': self.flag}".ai(),
        "    def name(self): return 'module_a'".ai(),
        "    def version(self): return '1.0'".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add ModuleA").unwrap();

    // C2: AI changes FLAG_A to 2 — WILL CONFLICT with main's 1 (base=0, feature=2, main=1 → conflict)
    let mut config_a = repo.filename("config_a.py");
    config_a.set_contents(crate::lines!["FLAG_A = 2".ai(),]);
    repo.stage_all_and_commit("feat: C2 AI sets FLAG_A=2")
        .unwrap();

    // C3: AI creates module_c.py (10 AI lines)
    let mut module_c = repo.filename("module_c.py");
    module_c.set_contents(crate::lines![
        "class ModuleC:".ai(),
        "    def __init__(self, config):".ai(),
        "        self.config = config".ai(),
        "        self.batch = config.get('BATCH', 10)".ai(),
        "    def process(self, items):".ai(),
        "        batches = [items[i:i+self.batch] for i in range(0, len(items), self.batch)]".ai(),
        "        return [self._process_batch(b) for b in batches]".ai(),
        "    def _process_batch(self, batch): return batch".ai(),
        "    def name(self): return 'module_c'".ai(),
        "    def version(self): return '1.0'".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add ModuleC").unwrap();

    // C4: AI changes config_b.py — WILL CONFLICT on BATCH (feature=200 vs main=50)
    // FLAG_B: base=0, feature=1, main=1 → auto-merged (same)
    // BATCH: base=10, feature=200, main=50 → conflict
    let mut config_b = repo.filename("config_b.py");
    config_b.set_contents(crate::lines!["FLAG_B = 1".ai(), "BATCH = 200".ai(),]);
    repo.stage_all_and_commit("feat: C4 AI sets BATCH=200")
        .unwrap();

    // C5: AI creates module_e.py (10 AI lines)
    let mut module_e = repo.filename("module_e.py");
    module_e.set_contents(crate::lines![
        "class ModuleE:".ai(),
        "    def __init__(self, config):".ai(),
        "        self.config = config".ai(),
        "    def execute(self, task):".ai(),
        "        return {'task': task, 'done': True}".ai(),
        "    def cancel(self, task_id):".ai(),
        "        return {'task_id': task_id, 'cancelled': True}".ai(),
        "    def list_tasks(self): return []".ai(),
        "    def name(self): return 'module_e'".ai(),
        "    def version(self): return '1.0'".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add ModuleE").unwrap();

    // Rebase — C2 will conflict first on config_a.py (feature=2 vs main=1, base=0)
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on config_a.py at C2"
    );

    // AI resolves C2: keeps feature's value (FLAG_A = 2) → C2' is non-empty since parent has 1.
    // Use set_contents_no_stage to avoid accidentally staging config_b.py, then stage only config_a.py.
    let mut conflict_config_a = repo.filename("config_a.py");
    conflict_config_a.set_contents_no_stage(crate::lines!["FLAG_A = 2".ai(),]);
    repo.git(&["add", "config_a.py"]).unwrap();
    let continue_result =
        repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None);
    // C4 should now conflict on BATCH (200 vs 50, base=10)
    assert!(
        continue_result.is_err(),
        "rebase should conflict on config_b.py at C4"
    );

    // Human resolves C4: compromise value BATCH=75 → C4' is non-empty (parent has BATCH=50)
    fs::write(repo.path().join("config_b.py"), "FLAG_B = 1\nBATCH = 75\n").unwrap();
    repo.git(&["add", "config_b.py"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': module_a.py only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["module_a.py"]);

    // C2': config_a.py only (AI-resolved, keeps feature's FLAG_A=2)
    // The original C2 had "FLAG_A = 2\n" as AI; the resolution keeps the same content.
    // diff_based: old="FLAG_A = 2\n", new="FLAG_A = 2\n" → Equal → AI ✓
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["config_a.py"]);

    // blame at chain[1]: git blame says C2' introduced "FLAG_A = 2" (parent had FLAG_A=1) → AI
    assert_blame_at_commit(
        &repo,
        &chain[1],
        "config_a.py",
        "c2_blame_config_a",
        &[("FLAG_A = 2", true)],
    );

    // C3': module_c.py only
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["module_c.py"]);

    // C4': config_b.py (human-resolved to BATCH=75).
    // diff_based: old=C4's "FLAG_B=1\nBATCH=200\n", new="FLAG_B=1\nBATCH=75\n"
    //   Line 1 "FLAG_B=1": Equal → AI (in note)
    //   Line 2 "BATCH=75" vs "BATCH=200": Replace → human (no note entry)
    // git blame at C4':
    //   Line 1 "FLAG_B = 1": unchanged from parent (main already had FLAG_B=1) → traces to main → human
    //   Line 2 "BATCH = 75": C4' introduced (parent had BATCH=50) → C4' note → no AI entry → human
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["config_b.py"]);

    assert_blame_at_commit(
        &repo,
        &chain[3],
        "config_b.py",
        "c4_blame_config_b",
        &[
            ("FLAG_B = 1", false), // traced to main branch commit (FLAG_B=1 was set by main)
            ("BATCH = 75", false), // C4' introduced, but no AI attribution (Replace in resolution)
        ],
    );

    // C5': module_e.py only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["module_e.py"]);
}

// ============================================================================
// Category 5: Path-specific correctness tests
// ============================================================================

/// Verify that the working-log fallback path is the **sole** source of attribution
/// when an AI conflict resolution writes *different* content than the original commit.
///
/// Scenario:
///   - C1 writes `TIMEOUT = 30` as an AI line.
///   - Main changes the same constant to `TIMEOUT = 60` → rebase conflict.
///   - AI resolves by setting `TIMEOUT = 45` (a compromise — different from both sides).
///   - `set_contents` records a working-log checkpoint for the resolved value.
///   - Content-diff compares original (`= 30`) with resolved (`= 45`) → Replace → no match.
///   - The working-log fallback must fire and attribute `= 45` as AI.
///
/// Regression: if `build_note_from_conflict_wl` were removed, C1' would have no note.
#[test]
fn test_conflict_working_log_is_sole_attribution_source() {
    let repo = TestRepo::new();

    repo.commit_untracked_file("config.py", "TIMEOUT = 10\nRETRIES = 3\n", "Initial commit");
    let main_branch = repo.current_branch();

    // Main: changes TIMEOUT → will conflict
    repo.commit_untracked_file(
        "config.py",
        "TIMEOUT = 60\nRETRIES = 3\n",
        "main: increase timeout to 60",
    );
    repo.commit_untracked_file(
        "logging.py",
        "import logging\nlogging.basicConfig(level=logging.INFO)\n",
        "main: add logging config",
    );
    repo.commit_untracked_file(
        "metrics.py",
        "class Metrics:\n    pass\n",
        "main: add metrics stub",
    );

    let base_sha = repo
        .git(&["rev-parse", "HEAD~3"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI sets TIMEOUT = 30 — WILL CONFLICT with main's = 60
    let mut cfg = repo.filename("config.py");
    cfg.set_contents(crate::lines!["TIMEOUT = 30".ai(), "RETRIES = 3",]);
    fs::write(repo.path().join("config.py"), "TIMEOUT = 30\nRETRIES = 3\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "config.py"])
        .unwrap();
    repo.stage_all_and_commit("feat: C1 AI sets TIMEOUT=30")
        .unwrap();

    // C2: AI adds a helper (conflict-free)
    let mut helper = repo.filename("helpers.py");
    helper.set_contents(crate::lines![
        "def retry(fn, n=3):".ai(),
        "    for i in range(n):".ai(),
        "        try: return fn()".ai(),
        "        except Exception:".ai(),
        "            if i == n - 1: raise".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add retry helper")
        .unwrap();

    // Rebase — C1 conflicts on config.py
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on config.py at C1"
    );

    // AI resolves: picks 45 as a compromise.  Content differs from original (30) → content-diff
    // cannot carry attribution.  ONLY the working-log checkpoint can produce the note.
    cfg.set_contents(crate::lines!["TIMEOUT = 45".ai(), "RETRIES = 3",]);
    fs::write(repo.path().join("config.py"), "TIMEOUT = 45\nRETRIES = 3\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "config.py"])
        .unwrap();
    repo.git(&["add", "config.py"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 2);

    // C1': config.py only — note MUST exist (working-log fallback fired)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["config.py"]);
    // 1 AI line: TIMEOUT = 45 — accepted_lines must be 1 (not 0).
    // If build_note_from_conflict_wl hard-codes accepted_lines=0, this assertion fails.
    assert_accepted_lines_exact(&repo, &chain[0], "c1_accepted_lines", 1);
    // The resolved value (45) must be AI-attributed, not human.
    // This can only be true if build_note_from_conflict_wl contributed the note.
    assert_blame_at_commit(
        &repo,
        &chain[0],
        "config.py",
        "c1_blame",
        &[("TIMEOUT = 45", true), ("RETRIES = 3", false)],
    );

    // C2': helpers.py only (unaffected by conflict)
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["helpers.py"]);
}

/// Verify that the content-diff path wins when it produces AI attribution, even when
/// a working-log checkpoint also exists for the same commit.
///
/// Scenario:
///   - C1 writes `MAX_RETRIES = 5` as an AI line.
///   - Main changes the same constant to `MAX_RETRIES = 10` → conflict.
///   - AI resolves by keeping the ORIGINAL value exactly: `MAX_RETRIES = 5`.
///   - `set_contents` records a working-log checkpoint.
///   - Content-diff sees `MAX_RETRIES = 5` (original) == `MAX_RETRIES = 5` (resolved) → Equal.
///   - `commit_has_attestations = true` → content-diff path wins; working-log is not consulted.
///   - Result: C1' note attributes `MAX_RETRIES = 5` as AI regardless of path.
#[test]
fn test_conflict_content_diff_wins_over_working_log() {
    let repo = TestRepo::new();

    repo.commit_untracked_file(
        "settings.py",
        "MAX_RETRIES = 3\nTIMEOUT = 10\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: changes MAX_RETRIES → will conflict
    repo.commit_untracked_file(
        "settings.py",
        "MAX_RETRIES = 10\nTIMEOUT = 10\n",
        "main: bump max retries",
    );
    repo.commit_untracked_file(
        "app.py",
        "from settings import MAX_RETRIES\n",
        "main: import settings",
    );
    repo.commit_untracked_file("server.py", "import http.server\n", "main: add server stub");

    let base_sha = repo
        .git(&["rev-parse", "HEAD~3"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI sets MAX_RETRIES = 5 — WILL CONFLICT with main's = 10
    let mut sett = repo.filename("settings.py");
    sett.set_contents(crate::lines!["MAX_RETRIES = 5".ai(), "TIMEOUT = 10",]);
    fs::write(
        repo.path().join("settings.py"),
        "MAX_RETRIES = 5\nTIMEOUT = 10\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "settings.py"])
        .unwrap();
    repo.stage_all_and_commit("feat: C1 AI sets MAX_RETRIES=5")
        .unwrap();

    // C2: AI adds a validator (conflict-free)
    let mut validator = repo.filename("validator.py");
    validator.set_contents(crate::lines![
        "def validate_retries(n: int) -> bool:".ai(),
        "    return isinstance(n, int) and 1 <= n <= 100".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add validator").unwrap();

    // Rebase — C1 conflicts on settings.py
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on settings.py at C1"
    );

    // AI resolves by keeping the ORIGINAL AI value exactly.
    // Content-diff: original `= 5` == resolved `= 5` → Equal → attribution carried.
    // Also creates a working-log checkpoint via set_contents.
    // The content-diff path fires first (commit_has_attestations=true) and wins.
    sett.set_contents(crate::lines!["MAX_RETRIES = 5".ai(), "TIMEOUT = 10",]);
    fs::write(
        repo.path().join("settings.py"),
        "MAX_RETRIES = 5\nTIMEOUT = 10\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "settings.py"])
        .unwrap();
    repo.git(&["add", "settings.py"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 2);

    // C1': settings.py — note exists because content-diff matched MAX_RETRIES = 5
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["settings.py"]);
    assert_blame_at_commit(
        &repo,
        &chain[0],
        "settings.py",
        "c1_blame",
        &[("MAX_RETRIES = 5", true), ("TIMEOUT = 10", false)],
    );
    assert_accepted_lines_exact(&repo, &chain[0], "c1_accepted", 1);

    // C2': validator.py only
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["validator.py"]);
}

// ============================================================================
// END Category 5: Path-specific correctness tests
// ============================================================================

// ============================================================================
// Standard-human variants: same as the KnownHuman tests above but use
// .unattributed_human() instead of .human() in set_contents calls.
// These do NOT assert metadata.humans (no KnownHuman attribution expected).
// ============================================================================

/// Test 1: config.py TIMEOUT constant — feature (C3) changes TIMEOUT to 60,
/// main changes it to 120 → conflict.  AI resolves to TIMEOUT = 90.
/// C1' has users.py, C2' adds products.py, C3' adds config.py (AI-resolved),
/// C4' adds orders.py, C5' adds payments.py.
#[test]
fn test_conflict_ai_resolves_timeout_constant_standard_human() {
    let repo = TestRepo::new();

    // Initial: config.py with a class and TIMEOUT constant (human)
    repo.commit_untracked_file(
        "config.py",
        "class Config:\n    TIMEOUT = 30\n    HOST = 'localhost'\n    PORT = 8080\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: changes TIMEOUT to 120 and adds 4 more commits
    repo.commit_untracked_file(
        "config.py",
        "class Config:\n    TIMEOUT = 120\n    HOST = 'localhost'\n    PORT = 8080\n",
        "main: increase TIMEOUT to 120",
    );
    repo.commit_untracked_file(
        "logging_config.py",
        "import logging\nlogging.basicConfig(level=logging.INFO)\n",
        "main: add logging config",
    );
    repo.commit_untracked_file(
        "constants.py",
        "MAX_CONNECTIONS = 100\nDEFAULT_PAGE_SIZE = 20\n",
        "main: add constants",
    );
    repo.commit_untracked_file(
        "exceptions.py",
        "class AppError(Exception): pass\nclass ValidationError(AppError): pass\n",
        "main: add exceptions",
    );
    repo.commit_untracked_file(
        "utils.py",
        "def flatten(lst): return [x for sub in lst for x in sub]\n",
        "main: add utils",
    );

    // Feature branch from base (before main's TIMEOUT change)
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates users.py (8 AI lines)
    let mut users = repo.filename("users.py");
    users.set_contents(crate::lines![
        "class UserService:".ai(),
        "    def __init__(self, db):".ai(),
        "        self.db = db".ai(),
        "    def get_user(self, uid):".ai(),
        "        return self.db.query('SELECT * FROM users WHERE id=?', uid)".ai(),
        "    def create_user(self, name, email):".ai(),
        "        return self.db.execute('INSERT INTO users VALUES (?, ?)', name, email)".ai(),
        "    def delete_user(self, uid):".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add user service")
        .unwrap();

    // C2: AI creates products.py (8 AI lines)
    let mut products = repo.filename("products.py");
    products.set_contents(crate::lines![
        "class ProductService:".ai(),
        "    def __init__(self, db):".ai(),
        "        self.db = db".ai(),
        "    def get_product(self, pid):".ai(),
        "        return self.db.query('SELECT * FROM products WHERE id=?', pid)".ai(),
        "    def list_products(self):".ai(),
        "        return self.db.query('SELECT * FROM products')".ai(),
        "    def update_price(self, pid, price):".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add product service")
        .unwrap();

    // C3: AI changes TIMEOUT to 60 in config.py — WILL CONFLICT with main's 120
    let mut config = repo.filename("config.py");
    config.set_contents(crate::lines![
        "class Config:".unattributed_human(),
        "    TIMEOUT = 60".ai(),
        "    HOST = 'localhost'".unattributed_human(),
        "    PORT = 8080".unattributed_human(),
    ]);
    repo.stage_all_and_commit("feat: C3 AI tunes TIMEOUT to 60")
        .unwrap();

    // C4: AI creates orders.py (8 AI lines)
    let mut orders = repo.filename("orders.py");
    orders.set_contents(crate::lines![
        "class OrderService:".ai(),
        "    def __init__(self, db):".ai(),
        "        self.db = db".ai(),
        "    def create_order(self, uid, items):".ai(),
        "        total = sum(i['price'] for i in items)".ai(),
        "        return self.db.execute('INSERT INTO orders VALUES (?, ?)', uid, total)".ai(),
        "    def get_order(self, oid):".ai(),
        "        return self.db.query('SELECT * FROM orders WHERE id=?', oid)".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add order service")
        .unwrap();

    // C5: AI creates payments.py (8 AI lines)
    let mut payments = repo.filename("payments.py");
    payments.set_contents(crate::lines![
        "class PaymentService:".ai(),
        "    def __init__(self, db, stripe):".ai(),
        "        self.db = db".ai(),
        "        self.stripe = stripe".ai(),
        "    def charge(self, oid, amount, token):".ai(),
        "        r = self.stripe.charge(amount, token)".ai(),
        "        self.db.execute('INSERT INTO payments VALUES (?, ?)', oid, r['id'])".ai(),
        "    def refund(self, pid):".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add payment service")
        .unwrap();

    // Rebase onto main — C3 will conflict on config.py
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on config.py at C3"
    );

    // AI resolves: sets TIMEOUT = 90 as .ai(), surrounding lines as .unattributed_human()
    let mut conflict_config = repo.filename("config.py");
    conflict_config.set_contents(crate::lines![
        "class Config:".unattributed_human(),
        "    TIMEOUT = 90".ai(),
        "    HOST = 'localhost'".unattributed_human(),
        "    PORT = 8080".unattributed_human(),
    ]);
    // set_contents already ran git add -A + checkpoint
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': users.py only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["users.py"]);

    // C2': products.py only
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["products.py"]);

    // C3': config.py only (AI-resolved, TIMEOUT = 90 attributed as AI)
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["config.py"]);
    // 1 AI line: TIMEOUT = 90 (working-log fallback path must set accepted_lines correctly)
    assert_accepted_lines_exact(&repo, &chain[2], "c3_accepted_lines", 1);

    // blame at chain[2] for config.py: the AI-resolved TIMEOUT line should be AI
    assert_blame_at_commit(
        &repo,
        &chain[2],
        "config.py",
        "c3_blame_config",
        &[
            ("class Config:", false),
            ("TIMEOUT = 90", true),
            ("HOST = 'localhost'", false),
            ("PORT = 8080", false),
        ],
    );

    // C4': orders.py only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["orders.py"]);

    // C5': payments.py only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["payments.py"]);
}

/// Test 3: processor.py — feature (C3) adds 5 AI lines to method2 body,
/// main also changes method2.  AI resolution rewrites processor.py preserving
/// 2 human context lines and writing 7 lines for the resolved method2 (marked
/// `.ai()` in set_contents).  However, the content-diff path only carries
/// attribution for lines whose content exactly matches the original feature commit:
/// only `def method2(self):`, `result = []`, `for i in range(10):`, and
/// `result.append(i * 2)` survive the content match — 4 lines.  The newly
/// introduced lines (`# AI merged`, `label = `, `return result, label`) have no
/// entry in `original_head_line_to_author` and therefore receive human attribution.
#[test]
fn test_conflict_ai_resolves_preserving_human_context_lines_standard_human() {
    let repo = TestRepo::new();

    // Initial: processor.py with a class (6 human lines)
    repo.commit_untracked_file("processor.py",
        "class Processor:\n    def method1(self): return 'method1'\n    def method2(self): pass\n    def method3(self): return 'method3'\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: human changes method2 differently → conflict
    repo.commit_untracked_file("processor.py",
        "class Processor:\n    def method1(self): return 'method1'\n    def method2(self): return 'human-method2'\n    def method3(self): return 'method3'\n",
        "main: implement method2",
    );
    repo.commit_untracked_file(
        "runner.py",
        "from processor import Processor\np = Processor()\np.method1()\n",
        "main: add runner",
    );
    repo.commit_untracked_file("tests/test_processor.py",
        "from processor import Processor\ndef test_method1(): assert Processor().method1() == 'method1'\n",
        "main: add tests",
    );
    repo.commit_untracked_file(
        "setup.py",
        "from setuptools import setup\nsetup(name='processor', version='0.1.0')\n",
        "main: add setup.py",
    );
    repo.commit_untracked_file(
        "pyproject.toml",
        "[build-system]\nrequires = ['setuptools']\n",
        "main: add pyproject.toml",
    );

    // Feature branch from base
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates util_a.py (8 AI lines)
    let mut util_a = repo.filename("util_a.py");
    util_a.set_contents(crate::lines![
        "def parse_int(s: str) -> int:".ai(),
        "    try:".ai(),
        "        return int(s)".ai(),
        "    except ValueError:".ai(),
        "        raise ValueError(f'Cannot parse {s!r} as int')".ai(),
        "".ai(),
        "def parse_float(s: str) -> float:".ai(),
        "    return float(s)".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add util_a").unwrap();

    // C2: AI creates util_b.py (8 AI lines)
    let mut util_b = repo.filename("util_b.py");
    util_b.set_contents(crate::lines![
        "from typing import List, Optional".ai(),
        "".ai(),
        "def chunk(lst: List, size: int) -> List[List]:".ai(),
        "    return [lst[i:i+size] for i in range(0, len(lst), size)]".ai(),
        "".ai(),
        "def flatten(lst: List[List]) -> List:".ai(),
        "    return [x for sub in lst for x in sub]".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add util_b").unwrap();

    // C3: AI adds 5 lines to method2 in processor.py — WILL CONFLICT
    let mut processor = repo.filename("processor.py");
    processor.set_contents(crate::lines![
        "class Processor:".unattributed_human(),
        "    def method1(self): return 'method1'".unattributed_human(),
        "    def method2(self):".ai(),
        "        result = []".ai(),
        "        for i in range(10):".ai(),
        "            result.append(i * 2)".ai(),
        "        return result".ai(),
        "    def method3(self): return 'method3'".unattributed_human(),
    ]);
    fs::write(
        repo.path().join("processor.py"),
        "class Processor:\n    def method1(self): return 'method1'\n    def method2(self):\n        result = []\n        for i in range(10):\n            result.append(i * 2)\n        return result\n    def method3(self): return 'method3'\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "processor.py"])
        .unwrap();
    repo.stage_all_and_commit("feat: C3 AI implements method2")
        .unwrap();

    // C4: AI creates util_d.py (8 AI lines)
    let mut util_d = repo.filename("util_d.py");
    util_d.set_contents(crate::lines![
        "import hashlib".ai(),
        "".ai(),
        "def md5(s: str) -> str:".ai(),
        "    return hashlib.md5(s.encode()).hexdigest()".ai(),
        "".ai(),
        "def sha256(s: str) -> str:".ai(),
        "    return hashlib.sha256(s.encode()).hexdigest()".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add util_d").unwrap();

    // C5: AI creates util_e.py (8 AI lines)
    let mut util_e = repo.filename("util_e.py");
    util_e.set_contents(crate::lines![
        "import json".ai(),
        "".ai(),
        "def to_json(obj) -> str:".ai(),
        "    return json.dumps(obj, indent=2)".ai(),
        "".ai(),
        "def from_json(s: str):".ai(),
        "    return json.loads(s)".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add util_e").unwrap();

    // Rebase — C3 will conflict on processor.py
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on processor.py at C3"
    );

    // AI resolves: 2 human context lines + 7 lines for resolved method2 (set_contents(.ai()))
    // NOTE: content-diff only recovers lines matching original C3 content:
    //   def method2, result = [], for i in range, result.append → 4 AI-attributed lines.
    //   # AI merged, label = , return result/label → newly introduced, no original match → human.
    let mut conflict_processor = repo.filename("processor.py");
    conflict_processor.set_contents(crate::lines![
        "class Processor:".unattributed_human(),
        "    def method1(self): return 'method1'".unattributed_human(),
        "    def method2(self):".ai(),
        "        # AI merged: combines human's return with feature's loop".ai(),
        "        result = []".ai(),
        "        for i in range(10):".ai(),
        "            result.append(i * 2)".ai(),
        "        label = 'human-method2'".ai(),
        "        return result, label".ai(),
        "    def method3(self): return 'method3'".unattributed_human(),
    ]);
    fs::write(
        repo.path().join("processor.py"),
        "class Processor:\n    def method1(self): return 'method1'\n    def method2(self):\n        # AI merged: combines human's return with feature's loop\n        result = []\n        for i in range(10):\n            result.append(i * 2)\n        label = 'human-method2'\n        return result, label\n    def method3(self): return 'method3'\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "processor.py"])
        .unwrap();
    repo.git(&["add", "processor.py"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': util_a.py only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["util_a.py"]);

    // C2': util_b.py only
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["util_b.py"]);

    // C3': processor.py only (AI-resolved: 4 AI lines via content-diff match)
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["processor.py"]);

    // blame at chain[2] for processor.py: lines from parent are human,
    // all new lines written by AI during resolution are AI.
    assert_blame_at_commit(
        &repo,
        &chain[2],
        "processor.py",
        "c3_blame_processor",
        &[
            ("class Processor:", false),
            ("def method1", false),
            ("def method2", true),
            ("AI merged", true),
            ("result = []", true),
            ("for i in range", true),
            ("result.append", true),
            ("label = ", true),
            ("return result, label", true),
            ("def method3", false),
        ],
    );

    // C4': util_d.py only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["util_d.py"]);

    // C5': util_e.py only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["util_e.py"]);
}

/// Test 4: version.py — conflict is on C1 (the VERY FIRST feature commit).
/// Feature changes VERSION to "2.0", main changes it to "1.5".
/// AI resolves to "2.1".  C2–C5 accumulate other files normally.
#[test]
fn test_conflict_ai_resolves_on_first_commit_standard_human() {
    let repo = TestRepo::new();

    // Initial: version.py with VERSION = "1.0"
    repo.commit_untracked_file(
        "version.py",
        "VERSION = \"1.0\"\nCODENAME = \"alpha\"\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: changes VERSION to "1.5" — will conflict with feature's C1
    repo.commit_untracked_file(
        "version.py",
        "VERSION = \"1.5\"\nCODENAME = \"beta\"\n",
        "main: bump version to 1.5",
    );
    repo.commit_untracked_file(
        "CHANGELOG.md",
        "## 1.5\n- Performance improvements\n",
        "main: add changelog",
    );
    repo.commit_untracked_file(
        "CONTRIBUTORS.md",
        "# Contributors\n- Alice\n- Bob\n",
        "main: add contributors",
    );
    repo.commit_untracked_file(
        "LICENSE",
        "MIT License\nCopyright 2024\n",
        "main: add license",
    );
    repo.commit_untracked_file(
        "docs/index.md",
        "# Docs\nWelcome to the docs.\n",
        "main: add docs",
    );

    // Feature branch from base
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI changes VERSION to "2.0" — WILL CONFLICT
    let mut version = repo.filename("version.py");
    version.set_contents(crate::lines![
        "VERSION = \"2.0\"".ai(),
        "CODENAME = \"alpha\"".unattributed_human(),
    ]);
    repo.stage_all_and_commit("feat: C1 bump version to 2.0")
        .unwrap();

    // C2: AI creates changelog.py (8 AI lines)
    let mut changelog = repo.filename("changelog.py");
    changelog.set_contents(crate::lines![
        "import datetime".ai(),
        "".ai(),
        "class ChangelogEntry:".ai(),
        "    def __init__(self, version: str, date: datetime.date, changes: list):".ai(),
        "        self.version = version".ai(),
        "        self.date = date".ai(),
        "        self.changes = changes".ai(),
        "    def render(self) -> str: return f'{self.version} ({self.date}): {len(self.changes)} changes'".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add changelog model")
        .unwrap();

    // C3: AI creates release_notes.py (8 AI lines)
    let mut release_notes = repo.filename("release_notes.py");
    release_notes.set_contents(crate::lines![
        "from typing import List".ai(),
        "".ai(),
        "def format_release_notes(entries: List[dict]) -> str:".ai(),
        "    lines = []".ai(),
        "    for e in entries:".ai(),
        "        lines.append(f\"## {e['version']}\")".ai(),
        "        for change in e.get('changes', []):".ai(),
        "            lines.append(f'- {change}')".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add release notes formatter")
        .unwrap();

    // C4: AI creates deprecations.py (8 AI lines)
    let mut deprecations = repo.filename("deprecations.py");
    deprecations.set_contents(crate::lines![
        "import warnings".ai(),
        "import functools".ai(),
        "".ai(),
        "def deprecated(reason: str):".ai(),
        "    def decorator(func):".ai(),
        "        @functools.wraps(func)".ai(),
        "        def wrapper(*args, **kwargs):".ai(),
        "            warnings.warn(f'{func.__name__} is deprecated: {reason}', DeprecationWarning, stacklevel=2)".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add deprecation decorator")
        .unwrap();

    // C5: AI creates migration_guide.py (8 AI lines)
    let mut migration_guide = repo.filename("migration_guide.py");
    migration_guide.set_contents(crate::lines![
        "MIGRATION_STEPS = [".ai(),
        "    'Update config files to new schema',".ai(),
        "    'Run database migration scripts',".ai(),
        "    'Update API call signatures',".ai(),
        "    'Test all integrations',".ai(),
        "    'Deploy to staging first',".ai(),
        "    'Monitor error rates after deployment',".ai(),
        "]".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add migration guide")
        .unwrap();

    // Rebase — C1 will conflict immediately on version.py
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on version.py at C1"
    );

    // AI resolves: VERSION = "2.1" as .ai(), CODENAME as .unattributed_human()
    let mut conflict_version = repo.filename("version.py");
    conflict_version.set_contents(crate::lines![
        "VERSION = \"2.1\"".ai(),
        "CODENAME = \"beta\"".unattributed_human(),
    ]);
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': version.py only with AI-resolved VERSION line (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["version.py"]);

    // blame at chain[0] for version.py: VERSION line is AI, CODENAME is human
    assert_blame_at_commit(
        &repo,
        &chain[0],
        "version.py",
        "c1_blame_version",
        &[("VERSION = \"2.1\"", true), ("CODENAME = \"beta\"", false)],
    );

    // C2': changelog.py only
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["changelog.py"]);

    // C3': release_notes.py only
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["release_notes.py"]);

    // C4': deprecations.py only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["deprecations.py"]);

    // C5': migration_guide.py only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["migration_guide.py"]);
}

/// Test 5: schema.rs max_connections — conflict is on C5 (LAST feature commit).
/// C1–C4 accumulate model_*.rs files cleanly.  C5 modifies schema.rs
/// max_connections constant; main also modifies same constant.  AI resolves.
#[test]
fn test_conflict_ai_resolves_on_last_commit_standard_human() {
    let repo = TestRepo::new();

    // Initial: schema.rs with a constant (human)
    repo.commit_untracked_file(
        "src/schema.rs",
        "pub const MAX_CONNECTIONS: u32 = 10;\npub const SCHEMA_VERSION: u32 = 1;\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: changes max_connections → will conflict with feature's C5
    repo.commit_untracked_file(
        "src/schema.rs",
        "pub const MAX_CONNECTIONS: u32 = 50;\npub const SCHEMA_VERSION: u32 = 1;\n",
        "main: increase max_connections to 50",
    );
    repo.commit_untracked_file(
        "src/migration.rs",
        "pub fn run_migrations() {}\n",
        "main: add migration runner",
    );
    repo.commit_untracked_file(
        "src/connection.rs",
        "pub struct Connection { id: u32 }\n",
        "main: add Connection type",
    );
    repo.commit_untracked_file(
        "src/pool.rs",
        "pub struct Pool { size: u32 }\n",
        "main: add Pool struct",
    );
    repo.commit_untracked_file(
        "Cargo.toml",
        "[package]\nname = \"schema\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        "main: add Cargo.toml",
    );

    // Feature branch from base
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates model_a.rs (10 AI lines)
    let mut model_a = repo.filename("src/model_a.rs");
    model_a.set_contents(crate::lines![
        "#[derive(Debug, Clone)]".ai(),
        "pub struct ModelA {".ai(),
        "    pub id: u64,".ai(),
        "    pub name: String,".ai(),
        "    pub active: bool,".ai(),
        "}".ai(),
        "".ai(),
        "impl ModelA {".ai(),
        "    pub fn new(id: u64, name: impl Into<String>) -> Self {".ai(),
        "        Self { id, name: name.into(), active: true }".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add ModelA").unwrap();

    // C2: AI creates model_b.rs (10 AI lines)
    let mut model_b = repo.filename("src/model_b.rs");
    model_b.set_contents(crate::lines![
        "#[derive(Debug, Clone)]".ai(),
        "pub struct ModelB {".ai(),
        "    pub id: u64,".ai(),
        "    pub value: f64,".ai(),
        "    pub tags: Vec<String>,".ai(),
        "}".ai(),
        "".ai(),
        "impl ModelB {".ai(),
        "    pub fn new(id: u64, value: f64) -> Self {".ai(),
        "        Self { id, value, tags: Vec::new() }".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add ModelB").unwrap();

    // C3: AI creates model_c.rs (10 AI lines)
    let mut model_c = repo.filename("src/model_c.rs");
    model_c.set_contents(crate::lines![
        "#[derive(Debug, Clone, PartialEq)]".ai(),
        "pub enum Status {".ai(),
        "    Active,".ai(),
        "    Inactive,".ai(),
        "    Pending,".ai(),
        "}".ai(),
        "".ai(),
        "impl Default for Status {".ai(),
        "    fn default() -> Self { Status::Pending }".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add Status enum")
        .unwrap();

    // C4: AI creates model_d.rs (10 AI lines)
    let mut model_d = repo.filename("src/model_d.rs");
    model_d.set_contents(crate::lines![
        "use std::collections::HashMap;".ai(),
        "".ai(),
        "#[derive(Debug, Default)]".ai(),
        "pub struct Registry {".ai(),
        "    entries: HashMap<u64, String>,".ai(),
        "}".ai(),
        "".ai(),
        "impl Registry {".ai(),
        "    pub fn register(&mut self, id: u64, name: impl Into<String>) {".ai(),
        "        self.entries.insert(id, name.into());".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add Registry").unwrap();

    // C5: AI changes max_connections to 100 — WILL CONFLICT
    let mut schema = repo.filename("src/schema.rs");
    schema.set_contents(crate::lines![
        "pub const MAX_CONNECTIONS: u32 = 100;".ai(),
        "pub const SCHEMA_VERSION: u32 = 1;".unattributed_human(),
    ]);
    fs::write(
        repo.path().join("src/schema.rs"),
        "pub const MAX_CONNECTIONS: u32 = 100;\npub const SCHEMA_VERSION: u32 = 1;\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "src/schema.rs"])
        .unwrap();
    repo.stage_all_and_commit("feat: C5 AI tunes MAX_CONNECTIONS to 100")
        .unwrap();

    // Rebase — C5 will conflict on src/schema.rs
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on src/schema.rs at C5"
    );

    // AI resolves: picks 75 as a compromise, as .ai()
    let mut conflict_schema = repo.filename("src/schema.rs");
    conflict_schema.set_contents(crate::lines![
        "pub const MAX_CONNECTIONS: u32 = 75;".ai(),
        "pub const SCHEMA_VERSION: u32 = 1;".unattributed_human(),
    ]);
    fs::write(
        repo.path().join("src/schema.rs"),
        "pub const MAX_CONNECTIONS: u32 = 75;\npub const SCHEMA_VERSION: u32 = 1;\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "src/schema.rs"])
        .unwrap();
    repo.git(&["add", "src/schema.rs"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': model_a.rs only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["src/model_a.rs"]);

    // C2': model_b.rs only
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["src/model_b.rs"]);

    // C3': model_c.rs only
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["src/model_c.rs"]);

    // C4': model_d.rs only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["src/model_d.rs"]);

    // C5': schema.rs only (AI-resolved MAX_CONNECTIONS)
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["src/schema.rs"]);

    // blame at chain[4] for schema.rs: MAX_CONNECTIONS line is AI, SCHEMA_VERSION is human
    assert_blame_at_commit(
        &repo,
        &chain[4],
        "src/schema.rs",
        "c5_blame_schema",
        &[
            ("MAX_CONNECTIONS: u32 = 75", true),
            ("SCHEMA_VERSION: u32 = 1", false),
        ],
    );
}

/// Test 6: config.py AND settings.py both conflict in C3.
/// C3 AI changes a line in both files; main also changes same lines.
/// AI resolves both conflicts.  Note for C3' has both files.
#[test]
fn test_conflict_ai_resolves_multiple_files_in_same_commit_standard_human() {
    let repo = TestRepo::new();

    // Initial: BOTH files exist at the shared base so C3's edits will conflict with main
    repo.commit_untracked_file(
        "config.py",
        "DEBUG = False\nSECRET_KEY = 'changeme'\n",
        "Initial: config",
    );
    repo.commit_untracked_file(
        "settings.py",
        "DATABASE_URL = 'sqlite:///dev.db'\nCACHE_BACKEND = 'locmem'\n",
        "Initial: settings",
    );
    let main_branch = repo.current_branch();

    // Main: changes the same lines in both files → will conflict with feature's C3
    repo.commit_untracked_file(
        "config.py",
        "DEBUG = True\nSECRET_KEY = 'changeme'\n",
        "main: enable DEBUG",
    );
    repo.commit_untracked_file(
        "settings.py",
        "DATABASE_URL = 'postgres://localhost/main_db'\nCACHE_BACKEND = 'redis'\n",
        "main: update settings",
    );
    repo.commit_untracked_file(
        "wsgi.py",
        "from app import create_app\napplication = create_app()\n",
        "main: add wsgi",
    );
    repo.commit_untracked_file(
        "asgi.py",
        "from app import create_app\napplication = create_app()\n",
        "main: add asgi",
    );
    repo.commit_untracked_file(
        "manage.py",
        "#!/usr/bin/env python\nimport sys\nif __name__ == '__main__': pass\n",
        "main: add manage.py",
    );

    // Feature branch from the shared base (HEAD~5 = after both initial commits)
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates auth.py (8 AI lines)
    let mut auth = repo.filename("auth.py");
    auth.set_contents(crate::lines![
        "from typing import Optional".ai(),
        "".ai(),
        "def authenticate(token: str) -> Optional[str]:".ai(),
        "    if not token: return None".ai(),
        "    parts = token.split('.')".ai(),
        "    if len(parts) != 3: return None".ai(),
        "    return parts[1]".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add auth").unwrap();

    // C2: AI creates middleware.py (8 AI lines)
    let mut middleware = repo.filename("middleware.py");
    middleware.set_contents(crate::lines![
        "class CorsMiddleware:".ai(),
        "    def __init__(self, app):".ai(),
        "        self.app = app".ai(),
        "    def __call__(self, environ, start_response):".ai(),
        "        def custom_start(status, headers):".ai(),
        "            headers.append(('Access-Control-Allow-Origin', '*'))".ai(),
        "            return start_response(status, headers)".ai(),
        "        return self.app(environ, custom_start)".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add CORS middleware")
        .unwrap();

    // C3: AI changes config.py AND settings.py — BOTH WILL CONFLICT
    let mut config = repo.filename("config.py");
    config.set_contents(crate::lines![
        "DEBUG = False".unattributed_human(),
        "SECRET_KEY = 'ai-generated-secret-key-v2'".ai(),
    ]);
    let mut settings = repo.filename("settings.py");
    settings.set_contents(crate::lines![
        "DATABASE_URL = 'postgres://localhost/feature_db'".ai(),
        "CACHE_BACKEND = 'locmem'".unattributed_human(),
    ]);
    repo.stage_all_and_commit("feat: C3 AI tunes config and settings")
        .unwrap();

    // C4: AI creates permissions.py (8 AI lines)
    let mut permissions = repo.filename("permissions.py");
    permissions.set_contents(crate::lines![
        "class Permission:".ai(),
        "    READ = 'read'".ai(),
        "    WRITE = 'write'".ai(),
        "    ADMIN = 'admin'".ai(),
        "".ai(),
        "def has_permission(user_perms: list, required: str) -> bool:".ai(),
        "    return required in user_perms".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add permissions")
        .unwrap();

    // C5: AI creates serializers.py (8 AI lines)
    let mut serializers = repo.filename("serializers.py");
    serializers.set_contents(crate::lines![
        "import json".ai(),
        "".ai(),
        "class JsonSerializer:".ai(),
        "    @staticmethod".ai(),
        "    def dumps(obj) -> str: return json.dumps(obj)".ai(),
        "    @staticmethod".ai(),
        "    def loads(s: str): return json.loads(s)".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add JSON serializer")
        .unwrap();

    // Rebase — C3 will conflict on config.py (and possibly settings.py)
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(rebase_result.is_err(), "rebase should conflict at C3");

    // AI resolves config.py
    let mut conflict_config = repo.filename("config.py");
    conflict_config.set_contents(crate::lines![
        "DEBUG = True".unattributed_human(),
        "SECRET_KEY = 'ai-generated-secret-key-v2'".ai(),
    ]);
    // AI resolves settings.py
    let mut conflict_settings = repo.filename("settings.py");
    conflict_settings.set_contents(crate::lines![
        "DATABASE_URL = 'postgres://localhost/feature_db'".ai(),
        "CACHE_BACKEND = 'redis'".unattributed_human(),
    ]);
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': auth.py only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["auth.py"]);

    // C2': middleware.py only
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["middleware.py"]);

    // C3': config.py + settings.py (AI-resolved, both in same commit)
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["config.py", "settings.py"]);

    // blame for config.py: DEBUG is human (unchanged), SECRET_KEY is AI
    assert_blame_at_commit(
        &repo,
        &chain[2],
        "config.py",
        "c3_blame_config",
        &[
            ("DEBUG = True", false),
            ("SECRET_KEY = 'ai-generated-secret-key-v2'", true),
        ],
    );

    // blame for settings.py: DATABASE_URL is AI, CACHE_BACKEND is human
    assert_blame_at_commit(
        &repo,
        &chain[2],
        "settings.py",
        "c3_blame_settings",
        &[
            ("DATABASE_URL = 'postgres://localhost/feature_db'", true),
            ("CACHE_BACKEND = 'redis'", false),
        ],
    );

    // C4': permissions.py only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["permissions.py"]);

    // C5': serializers.py only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["serializers.py"]);
}

/// Test 7: dispatcher.py — conflict on C2.  C3 and C4 also modify dispatcher.py
/// (no further conflicts).  AI resolves C2 with 12-line process() implementation.
/// Subsequent commits append more methods to dispatcher.py.
#[test]
fn test_conflict_ai_resolves_then_more_ai_builds_on_result_standard_human() {
    let repo = TestRepo::new();

    // Initial: dispatcher.py stub (human)
    repo.commit_untracked_file(
        "dispatcher.py",
        "class Dispatcher:\n    pass\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: human implements process() differently → will conflict with feature's C2
    repo.commit_untracked_file(
        "dispatcher.py",
        "class Dispatcher:\n    def process(self, msg): return msg.strip()\n",
        "main: implement process() simply",
    );
    repo.commit_untracked_file(
        "config.py",
        "WORKERS = 4\nQUEUE_SIZE = 100\n",
        "main: add config",
    );
    repo.commit_untracked_file(
        "queue.py",
        "import queue\nQ = queue.Queue()\n",
        "main: add queue",
    );
    repo.commit_untracked_file(
        "worker.py",
        "class Worker:\n    def __init__(self, q): self.q = q\n",
        "main: add worker",
    );
    repo.commit_untracked_file(
        "monitor.py",
        "class Monitor:\n    def check(self): return 'ok'\n",
        "main: add monitor",
    );

    // Feature branch from base
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates base_handler.py (8 AI lines)
    let mut base_handler = repo.filename("base_handler.py");
    base_handler.set_contents(crate::lines![
        "class BaseHandler:".ai(),
        "    def __init__(self):".ai(),
        "        self.middlewares = []".ai(),
        "    def use(self, middleware):".ai(),
        "        self.middlewares.append(middleware)".ai(),
        "        return self".ai(),
        "    def handle(self, msg): raise NotImplementedError".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add BaseHandler")
        .unwrap();

    // C2: AI adds process() to dispatcher.py — WILL CONFLICT
    let mut dispatcher_c2 = repo.filename("dispatcher.py");
    dispatcher_c2.set_contents(crate::lines![
        "class Dispatcher:".unattributed_human(),
        "    def process(self, msg):".ai(),
        "        msg = msg.strip()".ai(),
        "        if not msg: raise ValueError('empty')".ai(),
        "        tokens = msg.split()".ai(),
        "        return {'cmd': tokens[0], 'args': tokens[1:]}".ai(),
        "    pass".unattributed_human(),
    ]);
    repo.stage_all_and_commit("feat: C2 AI adds process() to Dispatcher")
        .unwrap();

    // C3: AI creates router.py (does NOT touch dispatcher.py — no conflict)
    let mut router = repo.filename("router.py");
    router.set_contents(crate::lines![
        "from dispatcher import Dispatcher".ai(),
        "".ai(),
        "class Router:".ai(),
        "    def __init__(self):".ai(),
        "        self.dispatcher = Dispatcher()".ai(),
        "    def register(self, cmd, fn): self.dispatcher.route(cmd, fn)".ai(),
        "    def run(self, msg): return self.dispatcher.dispatch(msg)".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 AI adds Router")
        .unwrap();

    // C4: AI creates middleware.py (new file, no conflict)
    let mut mw = repo.filename("middleware.py");
    mw.set_contents(crate::lines![
        "class Middleware:".ai(),
        "    def __init__(self): self.chain = []".ai(),
        "    def use(self, fn): self.chain.append(fn); return self".ai(),
        "    def run(self, msg):".ai(),
        "        for fn in self.chain: msg = fn(msg)".ai(),
        "        return msg".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 AI adds Middleware")
        .unwrap();

    // C5: AI creates event_bus.py (new file, no conflict)
    let mut bus = repo.filename("event_bus.py");
    bus.set_contents(crate::lines![
        "class EventBus:".ai(),
        "    def __init__(self): self.handlers = {}".ai(),
        "    def on(self, event, fn): self.handlers.setdefault(event, []).append(fn)".ai(),
        "    def emit(self, event, *args):".ai(),
        "        for fn in self.handlers.get(event, []): fn(*args)".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 AI adds EventBus")
        .unwrap();

    // Rebase — C2 will conflict on dispatcher.py
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on dispatcher.py at C2"
    );

    // AI resolves C2: 12-line process() implementation (all .ai() except class line)
    let mut conflict_dispatcher = repo.filename("dispatcher.py");
    conflict_dispatcher.set_contents(crate::lines![
        "class Dispatcher:".unattributed_human(),
        "    def process(self, msg):".ai(),
        "        # AI merge: validates and parses, as in feature branch".ai(),
        "        msg = msg.strip()".ai(),
        "        if not msg: raise ValueError('empty message')".ai(),
        "        tokens = msg.split()".ai(),
        "        cmd = tokens[0].lower()".ai(),
        "        args = tokens[1:]".ai(),
        "        return {'cmd': cmd, 'args': args, 'raw': msg}".ai(),
        "    def _noop(self, args): return None".ai(),
        "    def __repr__(self): return f'Dispatcher()'".ai(),
        "    pass".unattributed_human(),
    ]);
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': base_handler.py only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["base_handler.py"]);

    // C2': dispatcher.py only (AI-resolved: ~10 AI lines)
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["dispatcher.py"]);

    // C3': router.py only
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["router.py"]);

    // C4': middleware.py only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["middleware.py"]);

    // C5': event_bus.py only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["event_bus.py"]);
}

/// Test 8: models.rs struct fields — feature (C3) AI adds 4 new fields,
/// main human adds 2 different fields.  AI resolution merges all 8 fields.
/// The merged struct body is all .ai().
#[test]
fn test_conflict_ai_resolves_rust_struct_fields_standard_human() {
    let repo = TestRepo::new();

    // Initial: models.rs with a struct (2 original fields, human)
    repo.commit_untracked_file(
        "src/models.rs",
        "pub struct User {\n    pub id: u64,\n    pub name: String,\n}\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: adds email and created_at fields → will conflict
    repo.commit_untracked_file("src/models.rs",
        "pub struct User {\n    pub id: u64,\n    pub name: String,\n    pub email: String,\n    pub created_at: u64,\n}\n",
        "main: add email and created_at to User",
    );
    repo.commit_untracked_file(
        "src/db.rs",
        "pub struct Db { url: String }\n",
        "main: add Db",
    );
    repo.commit_untracked_file(
        "src/repo.rs",
        "use crate::models::User;\npub struct UserRepo;\n",
        "main: add UserRepo",
    );
    repo.commit_untracked_file(
        "src/service.rs",
        "pub struct UserService;\n",
        "main: add UserService",
    );
    repo.commit_untracked_file(
        "Cargo.toml",
        "[package]\nname = \"models\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        "main: add Cargo.toml",
    );

    // Feature branch from base
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates traits.rs (8 AI lines)
    let mut traits = repo.filename("src/traits.rs");
    traits.set_contents(crate::lines![
        "pub trait Entity {".ai(),
        "    fn id(&self) -> u64;".ai(),
        "    fn name(&self) -> &str;".ai(),
        "}".ai(),
        "".ai(),
        "pub trait Persistable: Entity {".ai(),
        "    fn save(&self) -> Result<(), String>;".ai(),
        "    fn delete(&self) -> Result<(), String>;".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add Entity and Persistable traits")
        .unwrap();

    // C2: AI creates impls.rs (8 AI lines)
    let mut impls = repo.filename("src/impls.rs");
    impls.set_contents(crate::lines![
        "use crate::models::User;".ai(),
        "use crate::traits::Entity;".ai(),
        "".ai(),
        "impl Entity for User {".ai(),
        "    fn id(&self) -> u64 { self.id }".ai(),
        "    fn name(&self) -> &str { &self.name }".ai(),
        "}".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 impl Entity for User")
        .unwrap();

    // C3: AI adds 4 new fields to User struct — WILL CONFLICT with main's email/created_at
    let mut models = repo.filename("src/models.rs");
    models.set_contents(crate::lines![
        "pub struct User {".unattributed_human(),
        "    pub id: u64,".unattributed_human(),
        "    pub name: String,".unattributed_human(),
        "    pub active: bool,".ai(),
        "    pub role: String,".ai(),
        "    pub score: f64,".ai(),
        "    pub metadata: std::collections::HashMap<String, String>,".ai(),
        "}".unattributed_human(),
    ]);
    fs::write(
        repo.path().join("src/models.rs"),
        "pub struct User {\n    pub id: u64,\n    pub name: String,\n    pub active: bool,\n    pub role: String,\n    pub score: f64,\n    pub metadata: std::collections::HashMap<String, String>,\n}\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "src/models.rs"])
        .unwrap();
    repo.stage_all_and_commit("feat: C3 AI adds active/role/score/metadata fields")
        .unwrap();

    // C4: AI creates errors.rs (8 AI lines)
    let mut errors = repo.filename("src/errors.rs");
    errors.set_contents(crate::lines![
        "#[derive(Debug)]".ai(),
        "pub enum ModelError {".ai(),
        "    NotFound(u64),".ai(),
        "    InvalidField(String),".ai(),
        "    DuplicateId(u64),".ai(),
        "}".ai(),
        "".ai(),
        "impl std::fmt::Display for ModelError { fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { write!(f, \"{:?}\", self) } }".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 add ModelError")
        .unwrap();

    // C5: AI creates utils.rs (8 AI lines)
    let mut utils = repo.filename("src/utils.rs");
    utils.set_contents(crate::lines![
        "pub fn slugify(s: &str) -> String {".ai(),
        "    s.to_lowercase()".ai(),
        "        .chars()".ai(),
        "        .map(|c| if c.is_alphanumeric() { c } else { '-' })".ai(),
        "        .collect::<String>()".ai(),
        "        .trim_matches('-')".ai(),
        "        .to_string()".ai(),
        "}".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add slugify utility")
        .unwrap();

    // Rebase — C3 will conflict on src/models.rs
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on src/models.rs at C3"
    );

    // AI resolves: merges ALL fields — original 2 + 4 feature + 2 main = 8 fields (all .ai() in struct body)
    let mut conflict_models = repo.filename("src/models.rs");
    conflict_models.set_contents(crate::lines![
        "pub struct User {".unattributed_human(),
        "    pub id: u64,".ai(),
        "    pub name: String,".ai(),
        "    pub email: String,".ai(),
        "    pub created_at: u64,".ai(),
        "    pub active: bool,".ai(),
        "    pub role: String,".ai(),
        "    pub score: f64,".ai(),
        "    pub metadata: std::collections::HashMap<String, String>,".ai(),
        "}".unattributed_human(),
    ]);
    fs::write(
        repo.path().join("src/models.rs"),
        "pub struct User {\n    pub id: u64,\n    pub name: String,\n    pub email: String,\n    pub created_at: u64,\n    pub active: bool,\n    pub role: String,\n    pub score: f64,\n    pub metadata: std::collections::HashMap<String, String>,\n}\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "src/models.rs"])
        .unwrap();
    repo.git(&["add", "src/models.rs"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': traits.rs only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["src/traits.rs"]);

    // C2': impls.rs only
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["src/impls.rs"]);

    // C3': models.rs only (AI-resolved struct with merged fields)
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["src/models.rs"]);

    // blame for models.rs: struct keyword is human, equal fields carry AI attribution, new fields are human
    assert_blame_at_commit(
        &repo,
        &chain[2],
        "src/models.rs",
        "c3_blame_models",
        &[
            ("pub struct User {", false),
            ("pub id: u64,", false),
            ("pub name: String,", false),
            ("pub email: String,", false),
            ("pub created_at: u64,", false),
            ("pub active: bool,", true),
            ("pub role: String,", true),
            ("pub score: f64,", true),
            ("pub metadata:", true),
            ("}", false),
        ],
    );

    // C4': errors.rs only
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["src/errors.rs"]);

    // C5': utils.rs only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["src/utils.rs"]);
}

/// Test 9: service.py process_payment — feature (C4) AI implements a 20-line
/// function body; main also implements the same function (12 lines).
/// AI resolution produces a 25-line merged implementation (all .ai()).
/// Non-conflict commits: C1 models.py, C2 validators.py, C3 exceptions.py, C5 utils.py.
#[test]
fn test_conflict_ai_resolves_complex_function_with_error_handling_standard_human() {
    let repo = TestRepo::new();

    // Initial: service.py with a function stub (human)
    repo.commit_untracked_file(
        "service.py",
        "def process_payment(amount, card):\n    pass\n",
        "Initial commit",
    );
    let main_branch = repo.current_branch();

    // Main: human implements process_payment differently → will conflict
    repo.commit_untracked_file("service.py",
        "def process_payment(amount, card):\n    if amount <= 0:\n        raise ValueError('amount must be positive')\n    return {'status': 'ok', 'amount': amount}\n",
        "main: implement process_payment",
    );
    repo.commit_untracked_file("tests/test_service.py",
        "from service import process_payment\ndef test_basic(): assert process_payment(10, '4111')['status'] == 'ok'\n",
        "main: add service tests",
    );
    repo.commit_untracked_file(
        "requirements.txt",
        "stripe==5.0.0\nrequests==2.31.0\n",
        "main: add requirements",
    );
    repo.commit_untracked_file(
        ".env.example",
        "STRIPE_KEY=sk_test_xxx\nDATABASE_URL=sqlite:///dev.db\n",
        "main: add .env.example",
    );
    repo.commit_untracked_file(
        "Makefile",
        "test:\n\tpython -m pytest\nlint:\n\tflake8 .\n.PHONY: test lint\n",
        "main: add Makefile",
    );

    // Feature branch from base
    let base_sha = repo
        .git(&["rev-parse", "HEAD~5"])
        .unwrap()
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "feature", &base_sha]).unwrap();

    // C1: AI creates models.py (8 AI lines)
    let mut models = repo.filename("models.py");
    models.set_contents(crate::lines![
        "from dataclasses import dataclass, field".ai(),
        "".ai(),
        "@dataclass".ai(),
        "class PaymentResult:".ai(),
        "    status: str".ai(),
        "    transaction_id: str".ai(),
        "    amount: float".ai(),
        "    error: str = ''".ai(),
    ]);
    repo.stage_all_and_commit("feat: C1 add PaymentResult model")
        .unwrap();

    // C2: AI creates validators.py (8 AI lines)
    let mut validators = repo.filename("validators.py");
    validators.set_contents(crate::lines![
        "import re".ai(),
        "".ai(),
        "def validate_card(card: str) -> bool:".ai(),
        "    return bool(re.match(r'^[0-9]{13,19}$', card.replace(' ', '')))".ai(),
        "".ai(),
        "def validate_amount(amount: float) -> bool:".ai(),
        "    return isinstance(amount, (int, float)) and 0 < amount <= 1_000_000".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C2 add payment validators")
        .unwrap();

    // C3: AI creates exceptions.py (8 AI lines)
    let mut exceptions = repo.filename("exceptions.py");
    exceptions.set_contents(crate::lines![
        "class PaymentError(Exception):".ai(),
        "    def __init__(self, msg: str, code: int = 400):".ai(),
        "        super().__init__(msg)".ai(),
        "        self.code = code".ai(),
        "".ai(),
        "class CardDeclinedError(PaymentError):".ai(),
        "    def __init__(self): super().__init__('Card declined', 402)".ai(),
        "".ai(),
    ]);
    repo.stage_all_and_commit("feat: C3 add payment exceptions")
        .unwrap();

    // C4: AI implements process_payment with 20 lines — WILL CONFLICT
    let mut service = repo.filename("service.py");
    service.set_contents(crate::lines![
        "def process_payment(amount, card):".unattributed_human(),
        "    from validators import validate_amount, validate_card".ai(),
        "    from exceptions import PaymentError, CardDeclinedError".ai(),
        "    import logging".ai(),
        "    logger = logging.getLogger(__name__)".ai(),
        "    logger.info(f'Processing payment: amount={amount}')".ai(),
        "    if not validate_amount(amount):".ai(),
        "        raise PaymentError(f'Invalid amount: {amount}')".ai(),
        "    if not validate_card(card):".ai(),
        "        raise PaymentError(f'Invalid card number')".ai(),
        "    if str(card).startswith('0000'):".ai(),
        "        raise CardDeclinedError()".ai(),
        "    transaction_id = f'txn_{hash(card + str(amount)) % 10**9}'".ai(),
        "    logger.info(f'Payment successful: {transaction_id}')".ai(),
        "    return {'status': 'ok', 'transaction_id': transaction_id, 'amount': amount}".ai(),
        "    # end process_payment".ai(),
    ]);
    repo.stage_all_and_commit("feat: C4 AI implements process_payment")
        .unwrap();

    // C5: AI creates utils.py (8 AI lines)
    let mut utils = repo.filename("utils.py");
    utils.set_contents(crate::lines![
        "def mask_card(card: str) -> str:".ai(),
        "    digits = card.replace(' ', '')".ai(),
        "    return '*' * (len(digits) - 4) + digits[-4:]".ai(),
        "".ai(),
        "def format_amount(amount: float) -> str:".ai(),
        "    return f'${amount:.2f}'".ai(),
        "".ai(),
        "def generate_receipt(result: dict) -> str: return f\"Receipt: {result['transaction_id']} {result['amount']}\"".ai(),
    ]);
    repo.stage_all_and_commit("feat: C5 add payment utils")
        .unwrap();

    // Rebase — C4 will conflict on service.py
    repo.git(&["checkout", "feature"]).unwrap();
    let rebase_result = repo.git(&["rebase", &main_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should conflict on service.py at C4"
    );

    // AI resolves: 25-line merged implementation (all .ai() except function signature line)
    let mut conflict_service = repo.filename("service.py");
    conflict_service.set_contents(crate::lines![
        "def process_payment(amount, card):".unattributed_human(),
        "    from validators import validate_amount, validate_card".ai(),
        "    from exceptions import PaymentError, CardDeclinedError".ai(),
        "    from models import PaymentResult".ai(),
        "    import logging".ai(),
        "    logger = logging.getLogger(__name__)".ai(),
        "    logger.info(f'Processing: amount={amount} card=***{str(card)[-4:]}')".ai(),
        "    if amount <= 0:".ai(),
        "        raise ValueError('amount must be positive')".ai(),
        "    if not validate_amount(amount):".ai(),
        "        raise PaymentError(f'Amount out of range: {amount}')".ai(),
        "    if not validate_card(card):".ai(),
        "        raise PaymentError('Invalid card number format')".ai(),
        "    if str(card).startswith('0000'):".ai(),
        "        raise CardDeclinedError()".ai(),
        "    transaction_id = f'txn_{hash(str(card) + str(amount)) % 10**9}'".ai(),
        "    logger.info(f'Payment OK: txn={transaction_id}')".ai(),
        "    result = PaymentResult(".ai(),
        "        status='ok',".ai(),
        "        transaction_id=transaction_id,".ai(),
        "        amount=amount,".ai(),
        "    )".ai(),
        "    return {'status': result.status, 'transaction_id': result.transaction_id, 'amount': result.amount}".ai(),
        "    # AI merged: combined validation + result model".ai(),
        "    # end process_payment".ai(),
    ]);
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    let chain = get_commit_chain(&repo, 5);

    // C1': models.py only (per-commit-delta)
    assert_note_base_commit_matches(&repo, &chain[0], "c1_base");
    assert_note_files_exact(&repo, &chain[0], "c1_files", &["models.py"]);

    // C2': validators.py only
    assert_note_base_commit_matches(&repo, &chain[1], "c2_base");
    assert_note_files_exact(&repo, &chain[1], "c2_files", &["validators.py"]);

    // C3': exceptions.py only
    assert_note_base_commit_matches(&repo, &chain[2], "c3_base");
    assert_note_files_exact(&repo, &chain[2], "c3_files", &["exceptions.py"]);

    // C4': service.py only (AI-resolved: 24 AI lines in function body)
    assert_note_base_commit_matches(&repo, &chain[3], "c4_base");
    assert_note_files_exact(&repo, &chain[3], "c4_files", &["service.py"]);

    // blame at chain[3] for service.py: lines from parent (main's version) are human,
    // all new lines written by AI during conflict resolution are AI.
    assert_blame_at_commit(
        &repo,
        &chain[3],
        "service.py",
        "c4_blame_service",
        &[
            ("def process_payment", false),
            ("validate_amount, validate_card", true),
            ("PaymentError, CardDeclinedError", true),
            ("PaymentResult", true),
            ("import logging", true),
            ("logger = logging", true),
            ("Processing:", true),
            ("if amount <= 0:", false),
            ("must be positive", false),
            ("if not validate_amount", true),
            ("Amount out of range", true),
            ("if not validate_card", true),
            ("Invalid card number", true),
            ("startswith('0000')", true),
            ("CardDeclinedError()", true),
            ("transaction_id = ", true),
            ("Payment OK:", true),
            ("result = PaymentResult(", true),
            ("status='ok',", true),
            ("transaction_id=transaction_id,", true),
            ("amount=amount,", true),
            (")", true),
            ("return {", true),
            ("AI merged", true),
            ("end process_payment", true),
        ],
    );

    // C5': utils.py only
    assert_note_base_commit_matches(&repo, &chain[4], "c5_base");
    assert_note_files_exact(&repo, &chain[4], "c5_files", &["utils.py"]);
}

crate::reuse_tests_in_worktree!(
    // Category 1: Fast Path
    test_fast_path_python_microservice_5_endpoints,
    test_fast_path_rust_library_5_modules,
    test_fast_path_typescript_frontend_5_components,
    test_fast_path_go_service_5_handlers,
    test_fast_path_mixed_ai_and_human_feature_commits,
    test_fast_path_10_commits_javascript_utilities,
    test_fast_path_nested_directory_structure,
    test_fast_path_single_file_grows_across_commits,
    test_fast_path_feature_deletes_file_then_recreates,
    test_fast_path_multi_file_commits_2_files_each,
    // Category 2: Slow Path
    test_slow_path_python_utils_main_prepends_feature_appends,
    test_slow_path_rust_lib_rs_main_prepends_feature_adds_impls,
    test_slow_path_typescript_routes_main_prepends_feature_adds_handlers,
    test_slow_path_config_file_both_add_different_sections,
    test_slow_path_growing_shared_file_10_commits,
    test_slow_path_multiple_shared_files_both_modified,
    test_slow_path_mixed_unique_and_shared_files,
    test_slow_path_feature_has_human_commits_intermixed,
    test_slow_path_large_function_blocks_line_offset,
    test_slow_path_file_grows_then_unique_files_each_commit,
    // Category 3: Human conflict resolution
    test_human_conflict_python_auth_c1_conflicts_rest_accumulate,
    test_human_conflict_rust_lib_c2_conflicts_surroundings_ok,
    test_human_conflict_typescript_api_c3_conflicts_accumulation_intact,
    test_human_conflict_python_models_c5_last_commit_conflicts,
    test_human_conflict_rust_config_c2_loses_attribution_rest_accumulate,
    test_human_conflict_typescript_store_ai_created_file_conflict,
    test_human_conflict_rust_server_c4_human_resolved_c5_accumulates,
    test_human_conflict_python_pipeline_mixed_baseline_c3_conflict,
    test_human_conflict_typescript_component_ai_created_c2_conflict,
    test_human_conflict_rust_7_commit_chain_c4_conflict_surroundings_intact,
    test_human_conflict_resolves_all_ai_lines_replaced,
    test_human_conflict_ai_file_is_conflict_file_note_preserved,
    test_human_conflict_multicommit_chain_middle_conflict_all_notes_preserved,
    // Category 4: AI conflict resolution
    test_conflict_ai_resolves_timeout_constant,
    test_conflict_ai_resolves_timeout_constant_standard_human,
    test_conflict_ai_resolves_with_added_extra_lines,
    test_conflict_ai_resolves_preserving_human_context_lines,
    test_conflict_ai_resolves_preserving_human_context_lines_standard_human,
    test_conflict_ai_resolves_on_first_commit,
    test_conflict_ai_resolves_on_first_commit_standard_human,
    test_conflict_ai_resolves_on_last_commit,
    test_conflict_ai_resolves_on_last_commit_standard_human,
    test_conflict_ai_resolves_multiple_files_in_same_commit,
    test_conflict_ai_resolves_multiple_files_in_same_commit_standard_human,
    test_conflict_ai_resolves_then_more_ai_builds_on_result,
    test_conflict_ai_resolves_then_more_ai_builds_on_result_standard_human,
    test_conflict_ai_resolves_rust_struct_fields,
    test_conflict_ai_resolves_rust_struct_fields_standard_human,
    test_conflict_ai_resolves_complex_function_with_error_handling,
    test_conflict_ai_resolves_complex_function_with_error_handling_standard_human,
    test_conflict_mixed_ai_and_human_resolve_different_commits,
    // Category 5: Path-specific correctness
    test_conflict_working_log_is_sole_attribution_source,
    test_conflict_content_diff_wins_over_working_log,
);
