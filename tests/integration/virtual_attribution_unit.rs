use crate::repos::test_repo::{DaemonTestScope, TestRepo};
use git_ai::error::GitAiError;
use git_ai::model::attribution_tracker::{Attribution, LineAttribution};
use git_ai::model::authorship_log_serialization::{
    generate_human_short_hash, generate_session_id, generate_short_hash,
};
use git_ai::model::working_log::{
    AgentId, Checkpoint, CheckpointKind, CheckpointLineStats, InitialAttributions, WorkingLogEntry,
};
use git_ai::operations::authorship::virtual_attribution::VirtualAttributions;
use git_ai::operations::git::repo_storage::PersistedWorkingLog;
use git_ai::operations::git::repository::{Repository, find_repository_in_path};
use std::collections::{BTreeMap, HashMap};
use std::fs;

#[test]
fn test_virtual_attributions() {
    // Create a temporary repo with an initial commit
    let repo = TestRepo::new();

    // Write a test file with some content
    std::fs::write(
        repo.path().join("test_file.rs"),
        "fn main() {\n    println!(\"Hello\");\n}\n",
    )
    .unwrap();
    repo.git_og(&["add", "test_file.rs"]).unwrap();

    // Trigger checkpoint and commit to create proper authorship data
    repo.git_ai(&["checkpoint", "mock_known_human", "test_file.rs"])
        .unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Get the commit SHA
    let commit_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Get gitai repo handle
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();

    // Create VirtualAttributions using the temp repo
    let virtual_attributions = git_ai::tokio_runtime::block_on(async {
        VirtualAttributions::new_for_base_commit(
            gitai_repo.clone(),
            commit_sha.clone(),
            &["test_file.rs".to_string()],
            None,
        )
        .await
    })
    .unwrap();

    // Verify files were tracked
    println!(
        "virtual_attributions files: {:?}",
        virtual_attributions.files()
    );
    println!("base_commit: {}", virtual_attributions.base_commit());
    println!("timestamp: {}", virtual_attributions.timestamp());

    // Print attribution details if available (for debugging)
    if let Some((char_attrs, line_attrs)) = virtual_attributions.get_attributions("test_file.rs") {
        println!("\n=== test_file.rs Attribution Info ===");
        println!("Character-level attributions: {} ranges", char_attrs.len());
        println!("Line-level attributions: {} ranges", line_attrs.len());
    }

    assert!(!virtual_attributions.files().is_empty());
}

fn working_log_fixture() -> (TestRepo, Repository, String, PersistedWorkingLog) {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    repo.commit_untracked_file("base.txt", "base", "base");
    let base_commit = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();
    let repository = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let working_log = repository
        .storage
        .working_log_for_base_commit(&base_commit)
        .unwrap();

    (repo, repository, base_commit, working_log)
}

fn line_attributions(author_id: &str) -> Vec<LineAttribution> {
    vec![LineAttribution::new(1, 1, author_id.to_string(), None)]
}

#[test]
fn working_log_loaders_use_their_declared_content_sources() {
    let (repo, repository, base_commit, working_log) = working_log_fixture();
    let initial_path = "initial.rs";
    let checkpoint_path = "checkpoint.rs";
    let live_initial = "live initial\n";
    let live_checkpoint = "live checkpoint\n";
    let stored_initial = "stored initial\n";
    let stored_checkpoint = "stored checkpoint\n";
    let captured_initial = "captured initial\n";
    let captured_checkpoint = "captured checkpoint\n";

    fs::write(repo.path().join(initial_path), live_initial).unwrap();
    fs::write(repo.path().join(checkpoint_path), live_checkpoint).unwrap();

    let legacy_agent = AgentId {
        tool: "fixture-tool".to_string(),
        id: "legacy-agent".to_string(),
        model: "fixture-model".to_string(),
    };
    let prompt_id = generate_short_hash(&legacy_agent.id, &legacy_agent.tool);

    working_log
        .write_initial_attributions_with_contents(
            HashMap::from([(initial_path.to_string(), line_attributions(&prompt_id))]),
            HashMap::new(),
            BTreeMap::new(),
            HashMap::from([(initial_path.to_string(), stored_initial.to_string())]),
            BTreeMap::new(),
        )
        .unwrap();

    let checkpoint_blob = working_log.persist_file_version(stored_checkpoint).unwrap();
    let mut checkpoint = Checkpoint::new(
        CheckpointKind::AiAgent,
        String::new(),
        "fixture-agent".to_string(),
        vec![WorkingLogEntry::new(
            checkpoint_path.to_string(),
            checkpoint_blob,
            Vec::new(),
            line_attributions(&prompt_id),
        )],
    );
    checkpoint.agent_id = Some(legacy_agent);
    checkpoint.line_stats = CheckpointLineStats {
        additions: 3,
        deletions: 1,
        ..CheckpointLineStats::default()
    };
    working_log.append_checkpoint(&checkpoint).unwrap();

    let session_agent = AgentId {
        tool: "fixture-tool".to_string(),
        id: "session-agent".to_string(),
        model: "fixture-model".to_string(),
    };
    let expected_session_id = generate_session_id(&session_agent.id, &session_agent.tool);
    let mut session_checkpoint = Checkpoint::new(
        CheckpointKind::AiAgent,
        String::new(),
        "fixture-agent".to_string(),
        Vec::new(),
    );
    session_checkpoint.agent_id = Some(session_agent);
    session_checkpoint.trace_id = Some("trace-id".to_string());
    working_log.append_checkpoint(&session_checkpoint).unwrap();

    let known_human = "Fixture Human <fixture@example.com>";
    working_log
        .append_checkpoint(&Checkpoint::new(
            CheckpointKind::KnownHuman,
            String::new(),
            known_human.to_string(),
            Vec::new(),
        ))
        .unwrap();

    let snapshot = HashMap::from([
        (initial_path.to_string(), captured_initial.to_string()),
        (checkpoint_path.to_string(), captured_checkpoint.to_string()),
    ]);
    let human_author = Some("Committer <committer@example.com>".to_string());

    let live = VirtualAttributions::from_just_working_log(
        repository.clone(),
        base_commit.clone(),
        human_author.clone(),
    )
    .unwrap();
    let captured = VirtualAttributions::from_working_log_snapshot(
        repository.clone(),
        base_commit.clone(),
        human_author.clone(),
        &snapshot,
    )
    .unwrap();
    let persisted =
        VirtualAttributions::from_persisted_working_log(repository, base_commit, human_author)
            .unwrap();

    assert_eq!(live.get_file_content(initial_path).unwrap(), live_initial);
    assert_eq!(
        live.get_file_content(checkpoint_path).unwrap(),
        live_checkpoint
    );
    assert_eq!(
        captured.get_file_content(initial_path).unwrap(),
        stored_initial
    );
    assert_eq!(
        captured.get_file_content(checkpoint_path).unwrap(),
        captured_checkpoint
    );
    assert_eq!(
        persisted.get_file_content(initial_path).unwrap(),
        stored_initial
    );
    assert_eq!(
        persisted.get_file_content(checkpoint_path).unwrap(),
        stored_checkpoint
    );
    for (attributions, file_path, expected_content) in [
        (&live, initial_path, live_initial),
        (&live, checkpoint_path, live_checkpoint),
        (&captured, initial_path, stored_initial),
        (&captured, checkpoint_path, captured_checkpoint),
        (&persisted, initial_path, stored_initial),
        (&persisted, checkpoint_path, stored_checkpoint),
    ] {
        let char_attributions = attributions.get_char_attributions(file_path).unwrap();
        assert_eq!(char_attributions.len(), 1);
        assert_eq!(char_attributions[0].start, 0);
        assert_eq!(char_attributions[0].end, expected_content.len());
    }

    for file_path in [initial_path, checkpoint_path] {
        assert_eq!(
            live.get_line_attributions(file_path),
            captured.get_line_attributions(file_path)
        );
        assert_eq!(
            live.get_line_attributions(file_path),
            persisted.get_line_attributions(file_path)
        );
    }
    assert_eq!(live.prompts, captured.prompts);
    assert_eq!(live.prompts, persisted.prompts);
    assert_eq!(live.humans, captured.humans);
    assert_eq!(live.humans, persisted.humans);
    assert_eq!(live.sessions, captured.sessions);
    assert_eq!(live.sessions, persisted.sessions);
    assert!(live.prompts.contains_key(&prompt_id));
    assert!(live.sessions.contains_key(&expected_session_id));
    assert!(
        live.humans
            .contains_key(&generate_human_short_hash(known_human))
    );
    let prompt = live.prompts.get(&prompt_id).unwrap().get("").unwrap();
    assert_eq!(prompt.total_additions, 3);
    assert_eq!(prompt.total_deletions, 1);
    assert_eq!(prompt.accepted_lines, 2);
    assert_eq!(live.timestamp(), 0);
    assert_eq!(live.blame_start_commit, None);
}

#[test]
fn working_log_loaders_preserve_missing_initial_snapshot_policy() {
    let (repo, repository, base_commit, working_log) = working_log_fixture();
    let file_path = "missing-initial-snapshot.rs";
    let live_content = "live content\n";
    let captured_content = "captured content\n";
    fs::write(repo.path().join(file_path), live_content).unwrap();

    working_log
        .write_initial(InitialAttributions {
            files: HashMap::from([(file_path.to_string(), line_attributions("fixture-agent"))]),
            ..InitialAttributions::default()
        })
        .unwrap();

    let live =
        VirtualAttributions::from_just_working_log(repository.clone(), base_commit.clone(), None)
            .unwrap();
    let captured = VirtualAttributions::from_working_log_snapshot(
        repository.clone(),
        base_commit.clone(),
        None,
        &HashMap::from([(file_path.to_string(), captured_content.to_string())]),
    )
    .unwrap();
    let persisted =
        match VirtualAttributions::from_persisted_working_log(repository, base_commit, None) {
            Ok(_) => panic!("persisted loader should reject missing INITIAL snapshot"),
            Err(error) => error,
        };

    assert_eq!(live.get_file_content(file_path).unwrap(), live_content);
    assert_eq!(
        captured.get_file_content(file_path).unwrap(),
        captured_content
    );
    match persisted {
        GitAiError::Generic(message) => assert_eq!(
            message,
            format!("INITIAL missing persisted file snapshot for {file_path}")
        ),
        error => panic!("expected missing INITIAL snapshot error, got {error:?}"),
    }
}

#[test]
fn snapshot_loader_swallows_missing_checkpoint_blob_but_persisted_loader_fails() {
    let (_repo, repository, base_commit, working_log) = working_log_fixture();
    let file_path = "missing-checkpoint-blob.rs";
    working_log
        .append_checkpoint(&Checkpoint::new(
            CheckpointKind::AiAgent,
            String::new(),
            "fixture-agent".to_string(),
            vec![WorkingLogEntry::new(
                file_path.to_string(),
                "missing-blob".to_string(),
                Vec::new(),
                line_attributions("fixture-agent"),
            )],
        ))
        .unwrap();

    let captured = VirtualAttributions::from_working_log_snapshot(
        repository.clone(),
        base_commit.clone(),
        None,
        &HashMap::new(),
    )
    .unwrap();
    let persisted = VirtualAttributions::from_persisted_working_log(repository, base_commit, None);

    assert_eq!(captured.get_file_content(file_path).unwrap(), "");
    assert!(captured.get_line_attributions(file_path).is_some());
    match persisted {
        Err(GitAiError::IoError(error)) => {
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        }
        Err(error) => panic!("expected missing checkpoint blob I/O error, got {error:?}"),
        Ok(_) => panic!("persisted loader should reject a missing checkpoint blob"),
    }
}

#[test]
fn persisted_loader_skips_empty_entries_before_reading_their_blob() {
    let (_repo, repository, base_commit, working_log) = working_log_fixture();
    let file_path = "empty-entry.rs";
    working_log
        .append_checkpoint(&Checkpoint::new(
            CheckpointKind::Human,
            String::new(),
            "fixture-human".to_string(),
            vec![WorkingLogEntry::new(
                file_path.to_string(),
                "missing-blob".to_string(),
                Vec::new(),
                Vec::new(),
            )],
        ))
        .unwrap();

    let persisted =
        VirtualAttributions::from_persisted_working_log(repository, base_commit, None).unwrap();

    assert!(persisted.get_file_content(file_path).is_none());
    assert!(persisted.get_line_attributions(file_path).is_none());
}

#[test]
fn character_only_checkpoints_convert_or_clear_stale_attributions() {
    let (_repo, repository, base_commit, working_log) = working_log_fixture();
    let converted_path = "character-only.rs";
    let cleared_path = "cleared.rs";
    working_log
        .write_initial_attributions_with_contents(
            HashMap::from([(cleared_path.to_string(), line_attributions("stale-agent"))]),
            HashMap::new(),
            BTreeMap::new(),
            HashMap::from([(cleared_path.to_string(), "stale content\n".to_string())]),
            BTreeMap::new(),
        )
        .unwrap();

    let converted_blob = working_log.persist_file_version("converted\n").unwrap();
    let cleared_blob = working_log.persist_file_version("").unwrap();
    working_log
        .append_checkpoint(&Checkpoint::new(
            CheckpointKind::AiAgent,
            String::new(),
            "fixture-agent".to_string(),
            vec![
                WorkingLogEntry::new(
                    converted_path.to_string(),
                    converted_blob,
                    vec![Attribution::new(
                        0,
                        "converted\n".len(),
                        "converted-agent".to_string(),
                        0,
                    )],
                    Vec::new(),
                ),
                WorkingLogEntry::new(
                    cleared_path.to_string(),
                    cleared_blob,
                    vec![Attribution::new(0, 1, "clearing-agent".to_string(), 0)],
                    Vec::new(),
                ),
            ],
        ))
        .unwrap();

    let persisted =
        VirtualAttributions::from_persisted_working_log(repository, base_commit, None).unwrap();

    assert_eq!(
        persisted.get_line_attributions(converted_path).unwrap(),
        &line_attributions("converted-agent")
    );
    assert!(persisted.get_line_attributions(cleared_path).is_none());
    assert_eq!(persisted.get_file_content(cleared_path).unwrap(), "");
}

#[test]
fn working_log_loaders_discard_all_checkpoints_when_jsonl_is_corrupt() {
    let (_repo, repository, base_commit, working_log) = working_log_fixture();
    let initial_path = "initial-only.rs";
    let initial_content = "initial only\n";
    working_log
        .write_initial_attributions_with_contents(
            HashMap::from([(initial_path.to_string(), line_attributions("fixture-agent"))]),
            HashMap::new(),
            BTreeMap::new(),
            HashMap::from([(initial_path.to_string(), initial_content.to_string())]),
            BTreeMap::new(),
        )
        .unwrap();

    let ignored_path = "ignored-valid-checkpoint.rs";
    let ignored_blob = working_log
        .persist_file_version("ignored checkpoint\n")
        .unwrap();
    working_log
        .append_checkpoint(&Checkpoint::new(
            CheckpointKind::AiAgent,
            String::new(),
            "fixture-agent".to_string(),
            vec![WorkingLogEntry::new(
                ignored_path.to_string(),
                ignored_blob,
                Vec::new(),
                line_attributions("fixture-agent"),
            )],
        ))
        .unwrap();
    let valid_prefix = fs::read_to_string(working_log.checkpoints_file()).unwrap();
    fs::write(
        working_log.checkpoints_file(),
        format!("{valid_prefix}{{not-json}}\n"),
    )
    .unwrap();

    let live =
        VirtualAttributions::from_just_working_log(repository.clone(), base_commit.clone(), None)
            .unwrap();
    let captured = VirtualAttributions::from_working_log_snapshot(
        repository.clone(),
        base_commit.clone(),
        None,
        &HashMap::from([(initial_path.to_string(), "captured fallback\n".to_string())]),
    )
    .unwrap();
    let persisted =
        VirtualAttributions::from_persisted_working_log(repository, base_commit, None).unwrap();

    assert!(live.get_line_attributions(initial_path).is_some());
    assert!(captured.get_line_attributions(initial_path).is_some());
    assert!(persisted.get_line_attributions(initial_path).is_some());
    assert!(live.get_line_attributions(ignored_path).is_none());
    assert!(captured.get_line_attributions(ignored_path).is_none());
    assert!(persisted.get_line_attributions(ignored_path).is_none());
    assert_eq!(
        captured.get_file_content(initial_path).unwrap(),
        initial_content
    );
    assert_eq!(
        persisted.get_file_content(initial_path).unwrap(),
        initial_content
    );
}

crate::reuse_tests_in_worktree!(
    test_virtual_attributions,
    working_log_loaders_use_their_declared_content_sources,
    working_log_loaders_preserve_missing_initial_snapshot_policy,
    snapshot_loader_swallows_missing_checkpoint_blob_but_persisted_loader_fails,
    persisted_loader_skips_empty_entries_before_reading_their_blob,
    character_only_checkpoints_convert_or_clear_stale_attributions,
    working_log_loaders_discard_all_checkpoints_when_jsonl_is_corrupt,
);
