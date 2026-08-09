use super::{AuthorshipMetadata, BTreeMap};

impl AuthorshipMetadata {
    pub(crate) fn merge_missing_from(&mut self, source: &Self) {
        fn merge<K: Ord + Clone, V: Clone>(target: &mut BTreeMap<K, V>, source: &BTreeMap<K, V>) {
            for (key, value) in source {
                target.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        merge(&mut self.prompts, &source.prompts);
        merge(&mut self.sessions, &source.sessions);
        merge(&mut self.humans, &source.humans);
    }
}

#[test]
fn merge_missing_metadata_preserves_target_records_and_header() {
    let metadata = |owner: &str, exclusive_key: &str| -> AuthorshipMetadata {
        let agent = || serde_json::json!({ "agent_id": { "tool": "", "id": owner, "model": "" }, "human_author": null });
        serde_json::from_value(serde_json::json!({
            "schema_version": format!("{owner}-schema"),
            "git_ai_version": format!("{owner}-git"),
            "base_commit_sha": format!("{owner}-base"),
            "prompts": { (exclusive_key): agent(), "m": agent() },
            "sessions": { (exclusive_key): agent(), "m": agent() },
            "humans": { (exclusive_key): { "author": owner }, "m": { "author": owner } }
        }))
        .unwrap()
    };
    let mut target = metadata("t", "a");
    target.merge_missing_from(&metadata("s", "z"));
    let actual = serde_json::to_value(&target).unwrap();
    for map in ["prompts", "sessions", "humans"] {
        let keys: Vec<_> = actual[map].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["a", "m", "z"]);
    }
    assert_eq!(actual["prompts"]["m"]["agent_id"]["id"], "t");
    assert_eq!(actual["sessions"]["m"]["agent_id"]["id"], "t");
    assert_eq!(actual["humans"]["m"]["author"], "t");
    assert_eq!(target.schema_version, "t-schema");
    assert_eq!(target.git_ai_version.as_deref(), Some("t-git"));
    assert_eq!(target.base_commit_sha, "t-base");
}
