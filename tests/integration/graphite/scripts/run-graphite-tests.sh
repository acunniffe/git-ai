#!/usr/bin/env bash

# This script runs the remote-backed Graphite integration tests.
# These tests push branches and open pull requests on a shared GitHub
# repository, so they are not part of the default test suite.
#
# Run with:
# ./run-graphite-tests.sh
#
# Or with --no-cleanup to leave the pushed branches and opened PRs in place
# for manual inspection:
# ./run-graphite-tests.sh --no-cleanup
#
# Required environment:
#   GRAPHITE_TEST_TOKEN     Graphite API token (app.graphite.dev/settings/cli)
#   GRAPHITE_TEST_GH_TOKEN  GitHub PAT with `repo` scope on the test repository
#
# Optional:
#   GRAPHITE_TEST_REPO      owner/name of the test repository (default: jumboblip/aug-6)

set -euo pipefail

# Parse arguments
NO_CLEANUP=0
TEST_ARGS=()

for arg in "$@"; do
    if [ "$arg" = "--no-cleanup" ]; then
        NO_CLEANUP=1
    else
        TEST_ARGS+=("$arg")
    fi
done

echo "🔍 Checking Graphite CLI availability..."
if ! command -v gt &> /dev/null; then
    echo "❌ Graphite CLI (gt) is not installed"
    echo "   Install with: npm install -g @withgraphite/graphite-cli@stable"
    exit 1
fi

echo "🔍 Checking GitHub CLI availability..."
if ! command -v gh &> /dev/null; then
    echo "❌ GitHub CLI (gh) is not installed"
    echo "   Install from: https://cli.github.com/"
    exit 1
fi

# Note: `gh auth status` is deliberately NOT checked. The tests supply their own
# GH_TOKEN from GRAPHITE_TEST_GH_TOKEN, and the ambient gh login is typically a
# different account than the one that owns the test repository.

if [ -z "${GRAPHITE_TEST_TOKEN:-}" ]; then
    echo "❌ GRAPHITE_TEST_TOKEN is not set"
    echo "   Generate one at: https://app.graphite.dev/settings/cli"
    exit 1
fi

if [ -z "${GRAPHITE_TEST_GH_TOKEN:-}" ]; then
    echo "❌ GRAPHITE_TEST_GH_TOKEN is not set"
    echo "   Needs a GitHub PAT with 'repo' scope on ${GRAPHITE_TEST_REPO:-jumboblip/aug-6}"
    exit 1
fi

echo "✅ gt and gh are available, both tokens are set"

if [ $NO_CLEANUP -eq 1 ]; then
    echo "⚠️  Cleanup disabled - test branches and PRs will NOT be removed"
    export GIT_AI_TEST_NO_CLEANUP=1
fi

echo ""
echo "🚀 Running Graphite integration tests against ${GRAPHITE_TEST_REPO:-jumboblip/aug-6}..."
echo ""

# `graphite::remote` matches both remote_ops and remote_sync. The tests share a
# serial group internally, so they push notes one at a time regardless of
# --test-threads.
cargo test --test integration graphite::remote -- --ignored --nocapture ${TEST_ARGS[@]+"${TEST_ARGS[@]}"}
