#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

VERSION_FILE="assets/shell-integration/config_version.txt"
HIGHLIGHTS_FILE="assets/shell-integration/config_update_highlights.tsv"
TAG_PATTERN='^[Vv][0-9]+\.[0-9]+\.[0-9]+$'
current_release_version=$(grep '^version =' "$REPO_ROOT/kaku/Cargo.toml" | head -n1 | cut -d'"' -f2)

echo "=== Config Version Check ==="
echo ""

current_config_version=$(cat "$VERSION_FILE" | tr -d '[:space:]')
echo "Current config version: $current_config_version"

previous_tag=$(
    git tag --sort=-version:refname \
        | grep -E "$TAG_PATTERN" \
        | grep -Eiv "^v${current_release_version}$" \
        | head -n 1 \
        || true # a fork or a shallow clone may carry no release tags
)

if [[ -z "$previous_tag" ]]; then
    echo "Warning: no previous release tag found, skipping previous release comparison"
    previous_config_version=""
else
    previous_config_version=$(git show "${previous_tag}:${VERSION_FILE}" 2>/dev/null | tr -d '[:space:]' || true)
    if [[ ! "$previous_config_version" =~ ^[0-9]+$ ]]; then
        echo "Warning: could not read config version from $previous_tag, skipping previous release comparison"
        previous_config_version=""
    else
        echo "Previous release tag: $previous_tag"
        echo "Previous release config version: $previous_config_version"
    fi
fi

if [[ -n "$previous_config_version" ]]; then
    min_config_version=$((previous_config_version + 1))
    echo "Minimum config version for this release: $min_config_version"
    echo ""

    if [[ "$current_config_version" -lt "$min_config_version" ]]; then
        echo "Error: config version is too low"
        echo "  Repository value: $current_config_version"
        echo "  Minimum value:    $min_config_version"
        exit 1
    fi
fi

new_highlights=$(grep "^$current_config_version	" "$HIGHLIGHTS_FILE" 2>/dev/null || echo "")

if [[ -z "$new_highlights" ]]; then
    echo "Warning: no highlights found for version $current_config_version"
    echo ""
    echo "If this release updates bundled config behavior, add entries to $HIGHLIGHTS_FILE:"
    echo "$current_config_version	<更新内容（英文）>"
    echo "$current_config_version	<更新内容（中文）>"
    echo ""
    echo "Versions currently present in the highlights file:"
    cut -f1 "$HIGHLIGHTS_FILE" | sort -u -n | tail -5
    exit 1
else
    echo "Found highlights for version $current_config_version:"
    echo "$new_highlights" | head -3
    echo ""

    count=$(echo "$new_highlights" | wc -l)
    echo "Total highlight entries: $count"

    if [[ $count -lt 2 ]]; then
        echo "Error: at least 2 highlight entries are required for version $current_config_version"
        echo "Add matching English and Chinese entries to $HIGHLIGHTS_FILE"
        exit 1
    fi
fi

echo ""
echo "Config version check passed"
