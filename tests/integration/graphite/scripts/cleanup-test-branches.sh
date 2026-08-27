#!/usr/bin/env bash

# This script cleans up branches and pull requests left on the shared Graphite
# test repository by crashed or --no-cleanup test runs. Every branch the tests
# push is namespaced under 'gtai/', so that prefix is what gets swept.
#
# Usage:
#   export GRAPHITE_TEST_GH_TOKEN=...          # GitHub PAT with 'repo' scope
#   ./cleanup-test-branches.sh                 # sweep the default repo
#   GRAPHITE_TEST_REPO=owner/name ./cleanup-test-branches.sh

set -euo pipefail

# Keep in sync with DEFAULT_GRAPHITE_TEST_REPO and BRANCH_NAMESPACE in
# graphite_test_harness.rs.
REPO="${GRAPHITE_TEST_REPO:-jumboblip/aug-6}"
BRANCH_NAMESPACE="gtai"

echo "🔍 Checking GitHub CLI availability..."
if ! command -v gh &> /dev/null; then
    echo "❌ GitHub CLI (gh) is not installed"
    echo "   Install from: https://cli.github.com/"
    exit 1
fi

if [ -z "${GRAPHITE_TEST_GH_TOKEN:-}" ]; then
    echo "❌ GRAPHITE_TEST_GH_TOKEN is not set"
    echo "   Needs a GitHub PAT with 'repo' scope on $REPO"
    exit 1
fi

# Use the same token the tests use, rather than the ambient gh login, which is
# typically a different account than the one that owns the test repository.
export GH_TOKEN="$GRAPHITE_TEST_GH_TOKEN"

echo "✅ GitHub CLI is available and a token is set"
echo ""
echo "🔍 Searching $REPO for '$BRANCH_NAMESPACE/*' branches..."
echo ""

BRANCHES=$(gh api --paginate "repos/$REPO/branches" \
    --jq ".[] | select(.name | startswith(\"$BRANCH_NAMESPACE/\")) | .name")

if [ -z "$BRANCHES" ]; then
    echo "✅ No leftover test branches found"
    exit 0
fi

BRANCH_COUNT=$(echo "$BRANCHES" | wc -l | tr -d ' ')

echo "Found $BRANCH_COUNT leftover test branches:"
echo ""
while read -r branch; do
    echo "  - $branch"
done <<< "$BRANCHES"
echo ""

read -p "⚠️  Close their PRs and delete all $BRANCH_COUNT branches? [y/N] " -n 1 -r
echo ""

if [[ ! $REPLY =~ ^[Yy]$ ]]; then
    echo "❌ Cleanup cancelled"
    exit 0
fi

echo ""
echo "🗑️  Cleaning up..."
echo ""

DELETED=0
FAILED=0

while read -r branch; do
    echo -n "  $branch... "

    # Close the PR first; deleting the branch alone leaves it open.
    PR_NUMBER=$(gh pr list --repo "$REPO" --head "$branch" --state open \
        --json number --jq '.[0].number' 2>/dev/null || true)
    if [ -n "$PR_NUMBER" ] && [ "$PR_NUMBER" != "null" ]; then
        gh pr close "$PR_NUMBER" --repo "$REPO" &> /dev/null || true
    fi

    if gh api -X DELETE "repos/$REPO/git/refs/heads/$branch" &> /dev/null; then
        echo "✅"
        DELETED=$((DELETED + 1))
    else
        echo "❌"
        FAILED=$((FAILED + 1))
    fi
done <<< "$BRANCHES"

echo ""
echo "✅ Cleanup complete"
echo "   Deleted: $DELETED branches"

if [ $FAILED -gt 0 ]; then
    echo "⚠️  Failed: $FAILED branches"
    exit 1
fi
