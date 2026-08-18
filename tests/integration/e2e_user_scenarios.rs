use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use crate::test_utils::extract_json_object;
use git_ai::authorship::stats::CommitStats;
use std::fs;

fn commit_stats(repo: &TestRepo, args: &[&str]) -> CommitStats {
    let raw = repo.git_ai(args).expect("git-ai stats should succeed");
    let json = extract_json_object(&raw);
    serde_json::from_str(&json).expect("valid stats json")
}

fn head_stats(repo: &TestRepo) -> CommitStats {
    commit_stats(repo, &["stats", "HEAD", "--json"])
}

fn assert_stats(
    stats: &CommitStats,
    human: u32,
    ai: u32,
    ai_accepted: u32,
    deleted: u32,
    added: u32,
) {
    assert_eq!(
        stats.human_additions, human,
        "human_additions: expected {human}, got {}",
        stats.human_additions
    );
    assert_eq!(
        stats.ai_additions, ai,
        "ai_additions: expected {ai}, got {}",
        stats.ai_additions
    );
    assert_eq!(
        stats.ai_accepted, ai_accepted,
        "ai_accepted: expected {ai_accepted}, got {}",
        stats.ai_accepted
    );
    assert_eq!(
        stats.git_diff_deleted_lines, deleted,
        "git_diff_deleted_lines: expected {deleted}, got {}",
        stats.git_diff_deleted_lines
    );
    assert_eq!(
        stats.git_diff_added_lines, added,
        "git_diff_added_lines: expected {added}, got {}",
        stats.git_diff_added_lines
    );
}

fn assert_tool_model(stats: &CommitStats, key: &str, ai_additions: u32, ai_accepted: u32) {
    let entry = stats
        .tool_model_breakdown
        .get(key)
        .unwrap_or_else(|| panic!("tool_model_breakdown missing key '{key}'"));
    assert_eq!(
        entry.ai_additions, ai_additions,
        "tool_model_breakdown[{key}].ai_additions: expected {ai_additions}, got {}",
        entry.ai_additions
    );
    assert_eq!(
        entry.ai_accepted, ai_accepted,
        "tool_model_breakdown[{key}].ai_accepted: expected {ai_accepted}, got {}",
        entry.ai_accepted
    );
}

// ---------------------------------------------------------------------------
// Test 1: basic workflow — user creates file, AI adds code, user adds more
// ---------------------------------------------------------------------------
#[test]
fn test_basic_workflow_mixed_authorship() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("example.py");

    // User creates a file with 1 line
    fs::write(&file_path, "def hello():\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human"]).unwrap();

    // AI adds 2 lines
    fs::write(
        &file_path,
        "def hello():\n    print(\"Hello from AI\")\n    return \"AI generated\"\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "example.py"])
        .unwrap();

    // User adds 2 more lines
    fs::write(
        &file_path,
        "def hello():\n    print(\"Hello from AI\")\n    return \"AI generated\"\n\ndef goodbye():\n    print(\"Goodbye from user\")\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human"]).unwrap();

    repo.stage_all_and_commit("Add example.py with mixed authorship")
        .unwrap();

    let blame_output = repo.git_ai(&["blame", "example.py"]).unwrap();
    assert!(
        blame_output.contains("mock_ai"),
        "blame should contain 'mock_ai'"
    );
    assert!(
        blame_output.contains("Test User"),
        "blame should contain 'Test User'"
    );
}

// ---------------------------------------------------------------------------
// Test 2: checkpoint exits successfully
// ---------------------------------------------------------------------------
#[test]
fn test_checkpoint_exits_successfully() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("test.txt");
    fs::write(&file_path, "# Test file\n").unwrap();

    repo.git_ai(&["checkpoint"]).unwrap();
}

// ---------------------------------------------------------------------------
// Test 3: checkpoint mock_ai with file path
// ---------------------------------------------------------------------------
#[test]
fn test_checkpoint_mock_ai_with_file_path() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("ai_file.txt");
    fs::write(&file_path, "AI generated content\n").unwrap();

    repo.git_ai(&["checkpoint", "mock_ai", "ai_file.txt"])
        .unwrap();
}

// ---------------------------------------------------------------------------
// Test 4: blame shows correct attribution after commit
// ---------------------------------------------------------------------------
#[test]
fn test_blame_shows_correct_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("test.txt");

    fs::write(&file_path, "line1\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human"]).unwrap();

    fs::write(&file_path, "line1\nline2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "test.txt"]).unwrap();

    repo.git(&["add", "test.txt"]).unwrap();
    repo.commit("Test commit").unwrap();

    let blame_output = repo.git_ai(&["blame", "test.txt"]).unwrap();
    assert!(blame_output.contains("line1"), "blame should contain line1");
    assert!(blame_output.contains("line2"), "blame should contain line2");
}

// ---------------------------------------------------------------------------
// Test 5: multiple checkpoints in sequence
// ---------------------------------------------------------------------------
#[test]
fn test_multiple_checkpoints_in_sequence() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("multi.txt");

    fs::write(&file_path, "step1\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human"]).unwrap();

    fs::write(&file_path, "step1\nstep2\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "multi.txt"])
        .unwrap();

    fs::write(&file_path, "step1\nstep2\nstep3\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human"]).unwrap();

    fs::write(&file_path, "step1\nstep2\nstep3\nstep4\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "multi.txt"])
        .unwrap();

    repo.git(&["add", "multi.txt"]).unwrap();
    repo.commit("Test multiple checkpoints in sequence")
        .unwrap();

    let stats = head_stats(&repo);
    assert_stats(&stats, 2, 2, 2, 0, 4);
    assert_tool_model(&stats, "mock_ai::unknown", 2, 2);
}

// ---------------------------------------------------------------------------
// Test 6: stats shows AI contribution after commit
// ---------------------------------------------------------------------------
#[test]
fn test_stats_shows_ai_contribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("stats_test.txt");

    fs::write(&file_path, "user line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human"]).unwrap();

    fs::write(&file_path, "user line\nAI line 1\nAI line 2\nAI line 3\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "stats_test.txt"])
        .unwrap();

    repo.git(&["add", "stats_test.txt"]).unwrap();
    repo.commit("Test stats").unwrap();

    let stats = head_stats(&repo);
    assert_stats(&stats, 1, 3, 3, 0, 4);
    assert_tool_model(&stats, "mock_ai::unknown", 3, 3);
}

// ---------------------------------------------------------------------------
// Test 7: AI deletes lines from file
// ---------------------------------------------------------------------------
#[test]
fn test_ai_deletes_lines() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("code.py");

    let initial = "\
def function1():
    print(\"Keep this\")
    return 1

def function2():
    print(\"AI will delete this\")
    return 2

def function3():
    print(\"Keep this too\")
    return 3
";
    fs::write(&file_path, initial).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human"]).unwrap();

    let after_delete = "\
def function1():
    print(\"Keep this\")
    return 1

def function3():
    print(\"Keep this too\")
    return 3
";
    fs::write(&file_path, after_delete).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "code.py"]).unwrap();

    repo.git(&["add", "code.py"]).unwrap();
    repo.commit("AI deleted function2").unwrap();

    let stats = head_stats(&repo);
    assert_stats(&stats, 7, 0, 0, 0, 7);
    // AI only deleted lines — no additions, so tool_model_breakdown may be empty or have 0s
    if let Some(entry) = stats.tool_model_breakdown.get("mock_ai::unknown") {
        assert_eq!(entry.ai_additions, 0);
        assert_eq!(entry.ai_accepted, 0);
    }
}

// ---------------------------------------------------------------------------
// Test 8: human deletes lines from AI-generated code
// ---------------------------------------------------------------------------
#[test]
fn test_human_deletes_ai_lines() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("calculator.py");

    let ai_code = "\
def add(a, b):
    return a + b
def subtract(a, b):
    return a - b
def multiply(a, b):
    return a * b
";
    fs::write(&file_path, ai_code).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "calculator.py"])
        .unwrap();

    let after_human_delete = "\
def add(a, b):
    return a + b
def multiply(a, b):
    return a * b
";
    fs::write(&file_path, after_human_delete).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human"]).unwrap();

    repo.git(&["add", "calculator.py"]).unwrap();
    repo.commit("AI added functions, human removed one")
        .unwrap();

    let stats = head_stats(&repo);
    assert_stats(&stats, 0, 4, 4, 0, 4);
    assert_tool_model(&stats, "mock_ai::unknown", 4, 4);
}

// ---------------------------------------------------------------------------
// Test 9: AI generates code with empty lines in between
// ---------------------------------------------------------------------------
#[test]
fn test_ai_code_with_empty_lines() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("app.py");

    fs::write(&file_path, "# My Application\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human"]).unwrap();

    let ai_code = "\
# My Application

import os
import sys

def setup():
    print(\"Setting up\")

def main():
    setup()
    print(\"Running main\")

def cleanup():
    print(\"Cleaning up\")

if __name__ == \"__main__\":
    main()
";
    fs::write(&file_path, ai_code).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "app.py"]).unwrap();

    repo.git(&["add", "app.py"]).unwrap();
    repo.commit("AI added code with empty lines").unwrap();

    let stats = head_stats(&repo);
    assert_stats(&stats, 1, 16, 16, 0, 17);
    assert_tool_model(&stats, "mock_ai::unknown", 16, 16);
}

// ---------------------------------------------------------------------------
// Test 10: AI creates a new file from scratch
// ---------------------------------------------------------------------------
#[test]
fn test_ai_creates_new_file() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("new_module.py");

    let ai_code = "\
class DataProcessor:
    def __init__(self):
        self.data = []
    def process(self, item):
        self.data.append(item)
        return item
    def get_results(self):
        return self.data
";
    fs::write(&file_path, ai_code).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "new_module.py"])
        .unwrap();

    repo.git(&["add", "new_module.py"]).unwrap();
    repo.commit("AI created new module").unwrap();

    let stats = head_stats(&repo);
    assert_stats(&stats, 0, 8, 8, 0, 8);
    assert_tool_model(&stats, "mock_ai::unknown", 8, 8);
}

// ---------------------------------------------------------------------------
// Test 12: AI refactors its own code (SKIPPED — issue #162)
// ---------------------------------------------------------------------------
#[test]
#[ignore = "https://github.com/git-ai-project/git-ai/issues/162"]
fn test_squash_authorship_ai_refactor() {
    let _repo = TestRepo::new();
}

// ---------------------------------------------------------------------------
// Test 13: Two AI commits, reset last commit, then recommit (SKIPPED — issue #169)
// ---------------------------------------------------------------------------
#[test]
#[ignore = "https://github.com/git-ai-project/git-ai/issues/169"]
fn test_reset_and_recommit_preserves_authorship() {
    let _repo = TestRepo::new();
}

// ---------------------------------------------------------------------------
// Test 14: AI authorship is preserved after rebase
// ---------------------------------------------------------------------------
#[test]
fn test_rebase_preserves_ai_authorship() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("base.py");
    let default_branch = repo.current_branch();

    // Create initial state on main
    let base_content = "\
# Base module
def base_function():
    return \"base\"
";
    fs::write(&file_path, base_content).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human"]).unwrap();
    repo.git(&["add", "base.py"]).unwrap();
    repo.commit("Initial base file").unwrap();

    // Create feature branch
    repo.git(&["checkout", "-b", "feature-branch"]).unwrap();

    // AI creates a file on feature branch
    let feature_path = repo.path().join("feature.py");
    let feature_code = "\
def ai_feature():
    print(\"AI generated feature\")
    return \"feature\"
class AIHelper:
    def __init__(self):
        self.name = \"AI Helper\"
    def help(self):
        return \"AI assistance\"
";
    fs::write(&feature_path, feature_code).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "feature.py"])
        .unwrap();
    repo.git(&["add", "feature.py"]).unwrap();
    repo.commit("AI creates feature module").unwrap();

    let stats_before = head_stats(&repo);
    assert_stats(&stats_before, 0, 8, 8, 0, 8);
    assert_tool_model(&stats_before, "mock_ai::unknown", 8, 8);

    let blame_before = repo.git_ai(&["blame", "feature.py"]).unwrap();
    assert!(blame_before.contains("mock_ai"));

    // Switch back to main and create a new commit
    repo.git(&["checkout", &default_branch]).unwrap();
    fs::write(
        &file_path,
        "\
# Base module
def base_function():
    return \"base\"

def new_base_function():
    return \"new base\"
",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human"]).unwrap();
    repo.git(&["add", "base.py"]).unwrap();
    repo.commit("Add new function to base").unwrap();

    // Rebase feature branch onto updated main
    repo.git(&["checkout", "feature-branch"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    // Verify AI authorship is preserved after rebase
    let stats_after = head_stats(&repo);
    assert_stats(&stats_after, 0, 8, 8, 0, 8);
    assert_tool_model(&stats_after, "mock_ai::unknown", 8, 8);

    assert!(repo.path().join("feature.py").exists());
    let content = fs::read_to_string(repo.path().join("feature.py")).unwrap();
    assert!(content.contains("ai_feature"));
}

// ---------------------------------------------------------------------------
// Test 15: AI attribution preserved after fixing conflict during rebase
// ---------------------------------------------------------------------------
#[test]
fn test_rebase_conflict_resolution_preserves_authorship() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("shared.py");
    let default_branch = repo.current_branch();

    // Create initial file on main
    let initial = "\
def function_one():
    return 1
def function_two():
    return 2
";
    fs::write(&file_path, initial).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human"]).unwrap();
    repo.git(&["add", "shared.py"]).unwrap();
    repo.commit("Initial shared file").unwrap();

    // Feature branch: AI modifies function_two and adds ai_function
    repo.git(&["checkout", "-b", "feature-ai"]).unwrap();

    let ai_edit = "\
def function_one():
    return 1
def function_two():
    # AI enhanced this function
    result = 2 * 2
    return result
def ai_function():
    print(\"AI added this\")
    return \"ai_data\"
";
    fs::write(&file_path, ai_edit).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "shared.py"])
        .unwrap();
    repo.git(&["add", "shared.py"]).unwrap();
    repo.commit("AI enhances function_two and adds ai_function")
        .unwrap();

    let ai_stats = head_stats(&repo);
    assert_stats(&ai_stats, 0, 6, 6, 1, 6);
    assert_tool_model(&ai_stats, "mock_ai::unknown", 6, 6);

    // Go back to main and make conflicting changes
    repo.git(&["checkout", &default_branch]).unwrap();

    let human_edit = "\
def function_one():
    return 1
def function_two():
    # Human modified this differently
    value = 2 + 2
    return value
def human_function():
    return \"human_data\"
";
    fs::write(&file_path, human_edit).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human"]).unwrap();
    repo.git(&["add", "shared.py"]).unwrap();
    repo.commit("Human modifies function_two and adds human_function")
        .unwrap();

    let human_stats = head_stats(&repo);
    assert_stats(&human_stats, 5, 0, 0, 1, 5);
    assert!(human_stats.tool_model_breakdown.is_empty());

    // Rebase — will conflict
    repo.git(&["checkout", "feature-ai"]).unwrap();
    let rebase_result = repo.git(&["rebase", &default_branch]);
    assert!(rebase_result.is_err(), "Rebase should conflict");

    // Resolve the conflict
    let resolved = "\
def function_one():
    return 1
def function_two():
    # AI enhanced this function
    result = 2 * 2
    return result
def ai_function():
    print(\"AI added this\")
    return \"ai_data\"
def human_function():
    return \"human_data\"
";
    fs::write(&file_path, resolved).unwrap();
    repo.git(&["add", "shared.py"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();

    // Verify AI authorship preserved after conflict resolution
    let stats_after = head_stats(&repo);
    assert_stats(&stats_after, 0, 6, 6, 3, 6);
    assert_tool_model(&stats_after, "mock_ai::unknown", 6, 6);

    let blame_after = repo.git_ai(&["blame", "shared.py"]).unwrap();
    assert!(blame_after.contains("mock_ai"));
    assert!(blame_after.contains("Test User"));

    let content = fs::read_to_string(&file_path).unwrap();
    assert!(content.contains("AI added this"));
    assert!(content.contains("human_data"));
}

// ---------------------------------------------------------------------------
// Test 16: git-ai stats range command
// ---------------------------------------------------------------------------
#[test]
fn test_stats_range_command() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("example.txt");

    // Create an anchor commit so we have a valid HEAD
    fs::write(repo.path().join("README.md"), "# Test\n").unwrap();
    repo.git(&["add", "README.md"]).unwrap();
    repo.commit("Initial commit").unwrap();

    let base_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Commit 1: Human adds 3 lines
    let human_content = "\
H: Human Line 1
H: Human Line 2
H: Human Line 3
";
    fs::write(&file_path, human_content).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "example.txt"])
        .unwrap();
    repo.git(&["add", "example.txt"]).unwrap();
    repo.commit("Commit 1: Human adds 3 lines").unwrap();

    let commit1_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Commit 2: AI adds 5 more lines
    let ai_content = "\
H: Human Line 1
H: Human Line 2
H: Human Line 3
AI: AI Line 1
AI: AI Line 2
AI: AI Line 3
AI: AI Line 4
AI: AI Line 5
";
    fs::write(&file_path, ai_content).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "example.txt"])
        .unwrap();
    repo.git(&["add", "example.txt"]).unwrap();
    repo.commit("Commit 2: AI adds 5 lines").unwrap();

    let commit2_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Test range: base_commit..commit2 (includes both commits)
    let range = format!("{base_sha}..{commit2_sha}");
    let raw = repo
        .git_ai(&["stats", &range, "--json"])
        .expect("stats range should succeed");
    let json = extract_json_object(&raw);
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");

    let range_stats = &parsed["range_stats"];
    // Range stats re-compute attribution against the range boundary, so known_human
    // attestation from individual commits may become unknown in the range view.
    let total_human = range_stats["human_additions"].as_u64().unwrap_or(0)
        + range_stats["unknown_additions"].as_u64().unwrap_or(0);
    assert_eq!(total_human, 3, "total non-AI additions in range");
    assert_eq!(range_stats["ai_additions"], 5);
    assert_eq!(range_stats["ai_accepted"], 5);
    assert_eq!(range_stats["git_diff_deleted_lines"], 0);
    assert_eq!(range_stats["git_diff_added_lines"], 8);
    assert_eq!(
        range_stats["tool_model_breakdown"]["mock_ai::unknown"]["ai_additions"],
        5
    );
    assert_eq!(
        range_stats["tool_model_breakdown"]["mock_ai::unknown"]["ai_accepted"],
        5
    );

    let authorship_stats = &parsed["authorship_stats"];
    assert_eq!(authorship_stats["total_commits"], 2);
    assert_eq!(authorship_stats["commits_with_authorship"], 2);

    // Test narrower range: commit1..commit2 (only commit 2)
    let range_single = format!("{commit1_sha}..{commit2_sha}");
    let raw_single = repo
        .git_ai(&["stats", &range_single, "--json"])
        .expect("stats range should succeed");
    let json_single = extract_json_object(&raw_single);
    let parsed_single: serde_json::Value = serde_json::from_str(&json_single).expect("valid JSON");

    let range_stats_single = &parsed_single["range_stats"];
    assert_eq!(range_stats_single["ai_additions"], 5);
    assert_eq!(range_stats_single["ai_accepted"], 5);

    assert_eq!(parsed_single["authorship_stats"]["total_commits"], 1);
}

// ---------------------------------------------------------------------------
// Test 17: interactive rebase with squash preserves authorship
// ---------------------------------------------------------------------------
#[test]
#[cfg(not(target_os = "windows"))]
fn test_interactive_rebase_squash_preserves_authorship() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("api_handler.py");

    // Base commit
    let base_content = "\
from flask import Flask, request, jsonify

app = Flask(__name__)

# API endpoint placeholder
";
    fs::write(&file_path, base_content).unwrap();
    repo.git(&["add", "api_handler.py"]).unwrap();
    repo.commit("Base commit with initial API structure")
        .unwrap();

    let base_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // COMMIT 1: Human adds 2 lines, AI adds 3 lines
    let human_edit1 = "\
from flask import Flask, request, jsonify

app = Flask(__name__)

@app.route('/api/users', methods=['POST'])
def create_user():
# API endpoint placeholder
";
    fs::write(&file_path, human_edit1).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human"]).unwrap();

    let ai_edit1 = "\
from flask import Flask, request, jsonify

app = Flask(__name__)

@app.route('/api/users', methods=['POST'])
def create_user():
    data = request.get_json()
    username = data.get('username', '') if data else ''
    return jsonify({'user': username}), 201
# API endpoint placeholder
";
    fs::write(&file_path, ai_edit1).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "api_handler.py"])
        .unwrap();

    repo.git(&["add", "api_handler.py"]).unwrap();
    repo.commit("Commit 1: Add user creation endpoint with basic implementation")
        .unwrap();

    let stats1 = head_stats(&repo);
    assert_stats(&stats1, 2, 3, 3, 0, 5);

    // COMMIT 2: Human adds 2 lines, AI deletes 1 AI line and adds 2 lines
    let human_edit2 = "\
from flask import Flask, request, jsonify

app = Flask(__name__)

@app.route('/api/users', methods=['POST'])
def create_user():
    data = request.get_json()
    username = data.get('username', '') if data else ''
    return jsonify({'user': username}), 201
    # TODO: Add proper database integration
    # TODO: Add authentication check
# API endpoint placeholder
";
    fs::write(&file_path, human_edit2).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human"]).unwrap();

    let ai_edit2 = "\
from flask import Flask, request, jsonify

app = Flask(__name__)

@app.route('/api/users', methods=['POST'])
def create_user():
    data = request.get_json()
    username = data.get('username', '') if data else ''
    # TODO: Add proper database integration
    # TODO: Add authentication check
    if not username or len(username) < 3:
        return jsonify({'error': 'Invalid username'}), 400
# API endpoint placeholder
";
    fs::write(&file_path, ai_edit2).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "api_handler.py"])
        .unwrap();

    repo.git(&["add", "api_handler.py"]).unwrap();
    repo.commit("Commit 2: Add documentation and improve validation")
        .unwrap();

    let stats2 = head_stats(&repo);
    assert_stats(&stats2, 2, 2, 2, 1, 4);

    // Interactive rebase: squash last 2 commits into 1
    let script_content = "#!/bin/sh\n\
        sed -i.bak '2s/pick/squash/' \"$1\"\n";
    let script_path = repo.path().join("squash_script.sh");
    fs::write(&script_path, script_content).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).unwrap();
    }

    repo.git_with_env(
        &["rebase", "-i", &base_sha],
        &[
            ("GIT_SEQUENCE_EDITOR", script_path.to_str().unwrap()),
            ("GIT_EDITOR", "true"),
        ],
        None,
    )
    .expect("Interactive rebase with squash should succeed");

    let squashed_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Verify one commit after base
    let count = repo
        .git(&["rev-list", "--count", &format!("{base_sha}..HEAD")])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(count, "1", "Should have exactly 1 commit after squash");

    let blame_after = repo.git_ai(&["blame", "api_handler.py"]).unwrap();
    assert!(blame_after.contains("mock_ai"));
    assert!(blame_after.contains("Test User"));

    let squashed_stats = commit_stats(&repo, &["stats", &squashed_sha, "--json"]);
    assert_stats(&squashed_stats, 4, 4, 4, 0, 8);
    assert_tool_model(&squashed_stats, "mock_ai::unknown", 4, 4);
}

// ---------------------------------------------------------------------------
// Test 18: rebase feature branch with mixed authorship onto diverged main
// ---------------------------------------------------------------------------
#[test]
fn test_rebase_mixed_authorship_diverged_main() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("app.py");
    let default_branch = repo.current_branch();

    // Create initial state
    let initial = "\
# Application Module
# This file contains the main application logic

def main():
    print(\"Application starting\")

# Utility functions section
# Add utility functions below

# Data processing section
# Add data processing functions below

# End of file
";
    fs::write(&file_path, initial).unwrap();
    repo.git(&["add", "app.py"]).unwrap();
    repo.commit("Initial application setup").unwrap();

    let common_ancestor = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Create feature branch
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // Human adds function signature
    let human_edit = "\
# Application Module
# This file contains the main application logic

def main():
    print(\"Application starting\")

# Utility functions section
# Add utility functions below

# Data processing section
# Add data processing functions below

def process_data(input_data):
    # Validate input

# End of file
";
    fs::write(&file_path, human_edit).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human"]).unwrap();

    // AI adds implementation
    let ai_edit = "\
# Application Module
# This file contains the main application logic

def main():
    print(\"Application starting\")

# Utility functions section
# Add utility functions below

# Data processing section
# Add data processing functions below

def process_data(input_data):
    # Validate input
    if not input_data:
        return None
    result = input_data.upper()
    return result

# End of file
";
    fs::write(&file_path, ai_edit).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "app.py"]).unwrap();

    repo.git(&["add", "app.py"]).unwrap();
    repo.commit("Feature: Add data processing function")
        .unwrap();

    let stats_before = head_stats(&repo);
    assert_stats(&stats_before, 3, 4, 4, 0, 7);
    assert_tool_model(&stats_before, "mock_ai::unknown", 4, 4);

    let blame_before = repo.git_ai(&["blame", "app.py"]).unwrap();
    assert!(blame_before.contains("mock_ai"));
    assert!(blame_before.contains("Test User"));

    // Switch to main and create 3 diverging commits (modifying utility section)
    repo.git(&["checkout", &default_branch]).unwrap();

    // Main commit 1
    let main1 = "\
# Application Module
# This file contains the main application logic
import logging

def main():
    print(\"Application starting\")

# Utility functions section
# Add utility functions below

def get_config():
    return {\"debug\": True}

# Data processing section
# Add data processing functions below

# End of file
";
    fs::write(&file_path, main1).unwrap();
    repo.git(&["add", "app.py"]).unwrap();
    repo.commit("Main: Add logging and get_config utility")
        .unwrap();

    // Main commit 2
    let main2 = "\
# Application Module
# This file contains the main application logic
import logging

def main():
    print(\"Application starting\")

# Utility functions section
# Add utility functions below

def get_config():
    return {\"debug\": True}

def log_message(msg):
    logging.info(msg)

# Data processing section
# Add data processing functions below

# End of file
";
    fs::write(&file_path, main2).unwrap();
    repo.git(&["add", "app.py"]).unwrap();
    repo.commit("Main: Add log_message utility").unwrap();

    // Main commit 3
    let main3 = "\
# Application Module
# This file contains the main application logic
import logging

def main():
    print(\"Application starting\")

# Utility functions section
# Add utility functions below

def get_config():
    return {\"debug\": True}

def log_message(msg):
    logging.info(msg)

def handle_error(err):
    logging.error(f\"Error: {err}\")

# Data processing section
# Add data processing functions below

# End of file
";
    fs::write(&file_path, main3).unwrap();
    repo.git(&["add", "app.py"]).unwrap();
    repo.commit("Main: Add handle_error utility").unwrap();

    let main_head = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Verify 3 commits ahead
    let commits_ahead = repo
        .git(&[
            "rev-list",
            "--count",
            &format!("{common_ancestor}..{default_branch}"),
        ])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(commits_ahead, "3");

    // Rebase feature onto main
    repo.git(&["checkout", "feature"]).unwrap();
    repo.git(&["rebase", &default_branch]).unwrap();

    // Verify stats preserved after rebase
    let stats_after = head_stats(&repo);
    assert_stats(&stats_after, 3, 4, 4, 0, 7);
    assert_tool_model(&stats_after, "mock_ai::unknown", 4, 4);

    let blame_after = repo.git_ai(&["blame", "app.py"]).unwrap();
    assert!(blame_after.contains("mock_ai"));
    assert!(blame_after.contains("Test User"));

    // Verify properly rebased onto main
    let merge_base = repo
        .git(&["merge-base", &default_branch, "feature"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(merge_base, main_head);

    let ahead = repo
        .git(&["rev-list", "--count", &format!("{default_branch}..feature")])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(ahead, "1");

    // Verify content from both branches present
    let content = fs::read_to_string(&file_path).unwrap();
    assert!(content.contains("process_data"));
    assert!(content.contains("input_data.upper()"));
    assert!(content.contains("import logging"));
    assert!(content.contains("get_config"));
    assert!(content.contains("log_message"));
    assert!(content.contains("handle_error"));
}

// ---------------------------------------------------------------------------
// Test: Issue #394 — Multi-user collaboration with code reformatting and AI
//
// Scenario from https://github.com/git-ai-project/git-ai/issues/394:
// 1. User test-a creates a single-line function and commits
// 2. User test-b checkpoints, reformats + wraps that function with AI lines,
//    checkpoints as AI, and commits
// 3. Verify blame attribution after each commit
// ---------------------------------------------------------------------------
#[test]
fn test_issue_394_multiuser_reformat_ai_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("hello.js");

    // --- Commit 1: user test-a creates the file ---
    repo.git(&["config", "user.name", "test-a"]).unwrap();
    repo.git(&["config", "user.email", "test-a@example.com"])
        .unwrap();

    let initial = "function hello() {console.log('hello')}\n";
    fs::write(&file_path, initial).unwrap();
    // No checkpoints — this is a plain untracked commit
    repo.stage_all_and_commit("Initial commit by test-a")
        .unwrap();

    let mut file = repo.filename("hello.js");
    file.assert_committed_lines(lines![
        "function hello() {console.log('hello')}".unattributed_human(),
    ]);

    // --- Commit 2: user test-b reformats + adds AI lines ---
    repo.git(&["config", "user.name", "test-b"]).unwrap();
    repo.git(&["config", "user.email", "test-b@example.com"])
        .unwrap();

    // Pre-edit checkpoint (untracked/legacy human) — mimics AI agent preset's
    // before-edit snapshot to exclude prior changes
    repo.git_ai(&["checkpoint", "human", "hello.js"]).unwrap();

    let modified = "\
console.log('a')
function hello() {
    console.log('hello')
}
console.log('b')
";
    fs::write(&file_path, modified).unwrap();

    // Post-edit AI checkpoint
    repo.git_ai(&["checkpoint", "mock_ai", "hello.js"]).unwrap();

    repo.stage_all_and_commit("AI-assisted edit by test-b")
        .unwrap();

    // Verify blame: the 2 new wrapper lines should be AI, and the 3
    // reformatted function lines' attribution is what issue #394 questions.
    let blame_output = repo.git_ai(&["blame", "hello.js"]).unwrap();
    eprintln!("=== git-ai blame output (issue #394) ===\n{blame_output}");

    let stats = head_stats(&repo);
    eprintln!(
        "=== commit stats (issue #394) ===\nhuman_additions={}, ai_additions={}, ai_accepted={}",
        stats.human_additions, stats.ai_additions, stats.ai_accepted
    );

    // Issue #394 reported 60% test-b / 40% AI (3 reformatted function lines
    // attributed to the committer instead of AI).  Current behaviour: all 5
    // lines are attributed to AI (the entire diff between the pre-edit and
    // post-edit checkpoints is AI), so the original split no longer reproduces.
    let mut file = repo.filename("hello.js");
    file.assert_committed_lines(lines![
        "console.log('a')".ai(),
        "function hello() {".ai(),
        "    console.log('hello')".ai(),
        "}".ai(),
        "console.log('b')".ai(),
    ]);
}

// ---------------------------------------------------------------------------
// squash merge concatenates AI and human changes
//
// Originally `test_squash_authorship_concatenates`, which drove the removed
// `git-ai squash-authorship` command directly. That command was deleted in the
// rewrite; squash attribution now flows through the unified
// `git-ai ci local merge` path. The scenario (a 5-line file edited across two
// human+AI commits, then squashed) is preserved; assertions reflect the new
// content-based attribution logic.
// ---------------------------------------------------------------------------
#[test]
fn test_squash_authorship_concatenates() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("example.txt");

    // Anchor commit so we have a valid HEAD / base.
    fs::write(repo.path().join("README.md"), "# Test\n").unwrap();
    repo.git(&["add", "README.md"]).unwrap();
    repo.commit("Initial commit").unwrap();
    let base_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    // Initial file with 5 lines.
    let initial = "\
Line 1: Initial
Line 2: Initial
Line 3: Initial
Line 4: Initial
Line 5: Initial
";
    fs::write(&file_path, initial).unwrap();
    repo.git(&["add", "example.txt"]).unwrap();
    repo.commit("Initial file with 5 lines").unwrap();

    // Feature branch carrying the human+AI commit stack.
    repo.git(&["checkout", "-b", "feature"]).unwrap();

    // COMMIT 1: human adds 2 lines, AI adds 3 and deletes 2.
    let human_edit = "\
Line 1: Initial
Line 2: Initial
H: Human Line 1
H: Human Line 2
Line 3: Initial
Line 4: Initial
Line 5: Initial
";
    fs::write(&file_path, human_edit).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "example.txt"])
        .unwrap();

    let ai_edit = "\
Line 1: Initial
H: Human Line 1
H: Human Line 2
AI: AI Line 1
AI: AI Line 2
AI: AI Line 3
Line 4: Initial
Line 5: Initial
";
    fs::write(&file_path, ai_edit).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "example.txt"])
        .unwrap();

    repo.git(&["add", "example.txt"]).unwrap();
    repo.commit("Commit 1: Human adds 2, AI adds 3 and deletes 2")
        .unwrap();
    let stats1 = head_stats(&repo);
    assert_stats(&stats1, 2, 3, 3, 2, 5);
    assert_tool_model(&stats1, "mock_ai::unknown", 3, 3);

    // COMMIT 2: human deletes 1 line, AI adds 2 and deletes 3.
    let human_edit2 = "\
Line 1: Initial
H: Human Line 1
H: Human Line 2
AI: AI Line 1
AI: AI Line 2
AI: AI Line 3
Line 5: Initial
";
    fs::write(&file_path, human_edit2).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "example.txt"])
        .unwrap();

    let ai_edit2 = "\
H: Human Line 2
AI: AI Line 1
AI: AI Line 3
AI: AI Line 4
AI: AI Line 5
Line 5: Initial
";
    fs::write(&file_path, ai_edit2).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "example.txt"])
        .unwrap();

    repo.git(&["add", "example.txt"]).unwrap();
    repo.commit("Commit 2: Human deletes 1, AI adds 2 and deletes 3")
        .unwrap();
    let head_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    let stats2 = head_stats(&repo);
    assert_stats(&stats2, 0, 2, 2, 4, 2);
    assert_tool_model(&stats2, "mock_ai::unknown", 2, 2);

    // Squash the feature branch onto main via raw git, then run the CI rewrite
    // (the unified path that replaced `git-ai squash-authorship`).
    repo.git_og(&["checkout", "main"]).unwrap();
    repo.git_og(&["merge", "--squash", "feature"]).unwrap();
    repo.git_og(&[
        "commit",
        "-m",
        "Squashed: combined changes from both commits",
    ])
    .unwrap();
    let squashed_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    let output = repo
        .git_ai(&[
            "ci",
            "local",
            "merge",
            "--merge-commit-sha",
            squashed_sha.as_str(),
            "--base-ref",
            "main",
            "--head-ref",
            "feature",
            "--head-sha",
            head_sha.as_str(),
            "--base-sha",
            base_sha.as_str(),
            "--skip-fetch",
            "--skip-push",
        ])
        .expect("ci local merge should succeed");
    assert!(
        output.contains("authorship rewritten successfully"),
        "expected ci local merge to rewrite authorship, got: {output}"
    );

    // The squashed commit carries both AI and human attribution.
    let blame_after = repo.git_ai(&["blame", "example.txt"]).unwrap();
    assert!(
        blame_after.contains("mock_ai"),
        "squashed blame should contain 'mock_ai', got:\n{blame_after}"
    );
    assert!(
        blame_after.contains("Test User"),
        "squashed blame should contain 'Test User', got:\n{blame_after}"
    );

    // Squashed final state (6 lines): "H: Human Line 2" is human; all four
    // surviving AI lines ("AI: AI Line 1/3/4/5") retain AI attribution; the
    // trailing "Line 5: Initial" is unchanged base context (committer).
    //
    // `git-ai ci local merge` routes a squash merge through the SAME
    // handle_squash_merge path the local daemon uses (union of every source
    // commit's note), so CI attribution matches the daemon: ai=4. Of the net
    // 5 added lines, 4 are AI and 1 is the human line.
    let squashed_stats = commit_stats(&repo, &["stats", &squashed_sha, "--json"]);
    assert_stats(&squashed_stats, 1, 4, 4, 4, 5);
    assert_tool_model(&squashed_stats, "mock_ai::unknown", 4, 4);
}
