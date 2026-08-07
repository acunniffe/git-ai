use super::range_diff::{MAX_PENDING_DROPPED_COMMITS, parse_range_diff_output};

fn leading_drops(count: usize) -> String {
    (0..count)
        .map(|index| {
            format!(
                "{}:  {:040x} < -:  ---------------------------------------- trunk commit {}\n",
                index + 1,
                index + 1,
                index + 1
            )
        })
        .collect()
}

fn matched_pair(ordinal: usize) -> String {
    format!(
        "{ordinal}:  {} = 1:  {} feature commit\n",
        "a".repeat(40),
        "b".repeat(40)
    )
}

#[test]
fn divergent_leading_drops_are_discarded_before_first_match() {
    let dropped_count = MAX_PENDING_DROPPED_COMMITS + 1;
    let output = format!(
        "{}{}",
        leading_drops(dropped_count),
        matched_pair(dropped_count + 1)
    );

    assert_eq!(
        parse_range_diff_output(&output),
        vec![("a".repeat(40), "b".repeat(40))]
    );
}

#[test]
fn leading_drops_at_the_limit_remain_valid_squash_mappings() {
    let output = format!(
        "{}{}",
        leading_drops(MAX_PENDING_DROPPED_COMMITS),
        matched_pair(MAX_PENDING_DROPPED_COMMITS + 1)
    );
    let mappings = parse_range_diff_output(&output);

    assert_eq!(mappings.len(), MAX_PENDING_DROPPED_COMMITS + 1);
    assert_eq!(mappings.last(), Some(&("a".repeat(40), "b".repeat(40))));
}
