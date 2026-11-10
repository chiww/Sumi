#!/usr/bin/env bash
# Prepare a release: bump versions, generate RELEASE_NOTES.md draft, optionally
# bump config version, run check scripts, commit and push.
#
# Usage: ./scripts/prep_release.sh <bump> [options]
#
#   <bump>          patch | minor | major | <explicit version like 0.11.0>
#
# Options:
#   --dry-run       Preview generated content; do not modify files or commit.
#   --no-edit       Skip opening $EDITOR after generating drafts.
#   --no-push       Commit but do not push to origin.
#   --bump-config   Increment assets/shell-integration/config_version.txt by 1
#                   and append two stub highlight rows for the new config
#                   version. Use this when the release ships a config schema
#                   change.
#   --include-chore Include refactor/docs/style/chore/test commits in the
#                   release notes draft (default: only feat/fix/perf/other).
#
# Examples:
#   ./scripts/prep_release.sh patch
#   ./scripts/prep_release.sh minor --dry-run
#   ./scripts/prep_release.sh 0.11.0 --bump-config

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

# shellcheck source=lib/preflight.sh
source "$REPO_ROOT/scripts/lib/preflight.sh"

DRY_RUN=0
NO_EDIT=0
NO_PUSH=0
BUMP_CONFIG=0
INCLUDE_CHORE=0
TARGET_BUMP=""

usage() {
    sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h|--help)        usage; exit 0 ;;
        --dry-run)        DRY_RUN=1; shift ;;
        --no-edit)        NO_EDIT=1; shift ;;
        --no-push)        NO_PUSH=1; shift ;;
        --bump-config)    BUMP_CONFIG=1; shift ;;
        --include-chore)  INCLUDE_CHORE=1; shift ;;
        --) shift; break ;;
        -*) die "Unknown flag: $1" ;;
        *)
            if [[ -z "$TARGET_BUMP" ]]; then
                TARGET_BUMP="$1"
            else
                die "Unexpected argument: $1"
            fi
            shift
            ;;
    esac
done

if [[ -z "$TARGET_BUMP" ]]; then
    usage
    exit 1
fi

# Pre-flight: skip in dry-run so user can preview without a clean tree.
if [[ "$DRY_RUN" == "0" ]]; then
    check_clean_git
fi

current_version=$(get_cargo_version "$REPO_ROOT")
gui_version=$(get_kaku_gui_version "$REPO_ROOT")

if [[ "$current_version" != "$gui_version" ]]; then
    die "Cargo.toml versions are inconsistent: kaku=$current_version, kaku-gui=$gui_version. Fix before bumping."
fi

case "$TARGET_BUMP" in
    patch|minor|major)
        new_version=$(bump_semver "$current_version" "$TARGET_BUMP")
        ;;
    *)
        if is_valid_semver "$TARGET_BUMP"; then
            new_version="$TARGET_BUMP"
        else
            die "Invalid bump or version: $TARGET_BUMP"
        fi
        ;;
esac

if ! semver_lt "$current_version" "$new_version"; then
    die "New version $new_version is not greater than current $current_version"
fi

log_info "Bumping version: $current_version → $new_version"

TAG_PATTERN='^[Vv][0-9]+\.[0-9]+\.[0-9]+$'
prev_tag=$(git tag --sort=-version:refname | grep -E "$TAG_PATTERN" | head -n1 || true)
if [[ -z "$prev_tag" ]]; then
    log_warn "No previous release tag found; using full git history."
    log_range="HEAD"
else
    log_info "Previous release tag: $prev_tag"
    log_range="${prev_tag}..HEAD"
fi

# Group commit subjects by conventional prefix.
declare -a feats=() fixes=() perfs=() refactors=() docs_=() chores=() others=()

while IFS='|' read -r sha subject; do
    [[ -z "$sha" ]] && continue
    if [[ "$subject" == Merge\ * ]]; then continue; fi
    if [[ "$subject" == "style: auto format" ]]; then continue; fi

    case "$subject" in
        feat:*|feat\(*)             feats+=("$subject ($sha)") ;;
        fix:*|fix\(*)               fixes+=("$subject ($sha)") ;;
        perf:*|perf\(*)             perfs+=("$subject ($sha)") ;;
        refactor:*|refactor\(*)     refactors+=("$subject ($sha)") ;;
        docs:*|docs\(*)             docs_+=("$subject ($sha)") ;;
        style:*|style\(*|chore:*|chore\(*|test:*|test\(*)
                                    chores+=("$subject ($sha)") ;;
        *)                          others+=("$subject ($sha)") ;;
    esac
done < <(git log "$log_range" --no-merges --pretty=format:'%h|%s')

# First-time contributors in this release range.
new_contributors=""
if [[ -n "$prev_tag" ]]; then
    prior=$(git log "$prev_tag" --format='%aN' 2>/dev/null | sort -u || true)
    current=$(git log "$log_range" --format='%aN' | sort -u)
    new_contributors=$(comm -13 <(printf '%s\n' "$prior") <(printf '%s\n' "$current") \
        | grep -v '^github-actions' \
        | grep -v '^$' || true)
fi

emit_section() {
    local title="$1"
    shift
    local count=$#
    if (( count > 0 )); then
        printf '\n**%s (%d)**\n' "$title" "$count"
        local item
        for item in "$@"; do
            printf -- '- %s\n' "$item"
        done
    fi
}

draft_path=$(mktemp -t kaku-release-notes-draft.XXXXXX)
# shellcheck disable=SC2064
trap "rm -f '$draft_path'" EXIT

{
    cat <<HEADER
# V${new_version} <emoji TBD>

<div align="center">
  <img src="https://raw.githubusercontent.com/tw93/Kaku/main/assets/logo.png" alt="Kaku Logo" width="120" height="120" />
  <h1 style="margin: 12px 0 6px;">Sumi V${new_version}</h1>
  <p><em>A fast, out-of-the-box terminal built for AI coding.</em></p>
</div>

### Changelog

<!--
Auto-generated commit summary below. Rewrite as numbered prose items
(each one starts with **Topic Name**: short description). Then duplicate the
prose to "### 更新日志" in Chinese and delete this comment + the bulleted
commit list before publishing.
-->
HEADER

    emit_section "Features" "${feats[@]+"${feats[@]}"}"
    emit_section "Fixes" "${fixes[@]+"${fixes[@]}"}"
    emit_section "Performance" "${perfs[@]+"${perfs[@]}"}"
    if [[ "$INCLUDE_CHORE" == "1" ]]; then
        emit_section "Refactoring" "${refactors[@]+"${refactors[@]}"}"
        emit_section "Docs" "${docs_[@]+"${docs_[@]}"}"
        emit_section "Chores" "${chores[@]+"${chores[@]}"}"
    fi
    emit_section "Other" "${others[@]+"${others[@]}"}"

    cat <<TAIL

### 更新日志

<!-- TODO: translate the prose Changelog above into Chinese, same numbered structure. -->
TAIL

    if [[ -n "$new_contributors" ]]; then
        printf '\n<!-- First-time contributors detected in this release; fold into the "Special thanks" line below. -->\n'
        printf 'First-time contributors:\n'
        while IFS= read -r c; do
            [[ -n "$c" ]] && printf -- '- %s\n' "$c"
        done <<<"$new_contributors"
    fi

    cat <<FOOTER

Special thanks to <contributors> for their contributions to this release.

> https://github.com/tw93/Kaku
FOOTER
} > "$draft_path"

if [[ "$DRY_RUN" == "1" ]]; then
    log_info "=== Dry run ==="
    log_info "Would bump version: $current_version → $new_version"
    log_info "Would write: kaku/Cargo.toml, kaku-gui/Cargo.toml, .github/RELEASE_NOTES.md, Cargo.lock"
    if [[ "$BUMP_CONFIG" == "1" ]]; then
        log_info "Would also bump config_version.txt and append 2 highlight stubs to config_update_highlights.tsv"
    fi
    echo
    echo "=== Generated RELEASE_NOTES.md draft ==="
    cat "$draft_path"
    echo "=== End of draft ==="
    exit 0
fi

# Write RELEASE_NOTES.md.
release_notes_path="$REPO_ROOT/.github/RELEASE_NOTES.md"
cp "$draft_path" "$release_notes_path"
log_info "Wrote RELEASE_NOTES.md draft (recoverable from git if needed)"

# Bump Cargo.toml versions. macOS sed requires the empty `''` after -i.
# Anchor on full line to avoid hitting dependency lines like `version = "1.0"`.
sed -i '' "s|^version = \"${current_version}\"$|version = \"${new_version}\"|" "$REPO_ROOT/kaku/Cargo.toml"
sed -i '' "s|^version = \"${current_version}\"$|version = \"${new_version}\"|" "$REPO_ROOT/kaku-gui/Cargo.toml"

# Verify both files were actually updated.
new_in_kaku=$(get_cargo_version "$REPO_ROOT")
new_in_gui=$(get_kaku_gui_version "$REPO_ROOT")
if [[ "$new_in_kaku" != "$new_version" || "$new_in_gui" != "$new_version" ]]; then
    die "Cargo.toml bump failed (kaku=$new_in_kaku, kaku-gui=$new_in_gui). Restore: git checkout kaku/Cargo.toml kaku-gui/Cargo.toml"
fi

# Refresh Cargo.lock so the prep commit ships with a consistent lockfile.
# `cargo metadata` updates Cargo.lock when workspace member versions change.
log_info "Refreshing Cargo.lock via cargo metadata..."
if ! cargo metadata --format-version 1 --offline >/dev/null 2>&1; then
    log_warn "cargo metadata --offline failed; retrying online (may fetch indexes)"
    if ! cargo metadata --format-version 1 >/dev/null; then
        die "cargo metadata failed after version bump. Restore: git checkout kaku/Cargo.toml kaku-gui/Cargo.toml Cargo.lock"
    fi
fi

# Optionally bump config version + add stub highlights.
config_changed=0
if [[ "$BUMP_CONFIG" == "1" ]]; then
    config_version_file="$REPO_ROOT/assets/shell-integration/config_version.txt"
    highlights_file="$REPO_ROOT/assets/shell-integration/config_update_highlights.tsv"
    current_config=$(tr -d '[:space:]' < "$config_version_file")
    new_config=$((current_config + 1))
    printf '%d\n' "$new_config" > "$config_version_file"
    {
        printf '%d\t<English highlight 1 — describe the config change>\n' "$new_config"
        printf '%d\t<中文 highlight 1 — 这次配置变更内容>\n' "$new_config"
    } >> "$highlights_file"
    log_info "Bumped config_version: $current_config → $new_config (added 2 stub highlights)"
    config_changed=1
fi

# Open editor for review.
if [[ "$NO_EDIT" == "0" ]]; then
    editor="${EDITOR:-vi}"
    files_to_edit=("$release_notes_path")
    if [[ "$config_changed" == "1" ]]; then
        files_to_edit+=("$REPO_ROOT/assets/shell-integration/config_update_highlights.tsv")
    fi
    log_info "Opening editor ($editor) — close all files when done"
    "$editor" "${files_to_edit[@]}"
fi

# Validate the result before staging.
log_info "Running scripts/check_release_notes.sh..."
"$REPO_ROOT/scripts/check_release_notes.sh"

log_info "Running scripts/check_release_config.sh..."
"$REPO_ROOT/scripts/check_release_config.sh"

# Stage and commit.
files_to_add=(
    "$REPO_ROOT/kaku/Cargo.toml"
    "$REPO_ROOT/kaku-gui/Cargo.toml"
    "$REPO_ROOT/Cargo.lock"
    "$release_notes_path"
)
if [[ "$config_changed" == "1" ]]; then
    files_to_add+=(
        "$REPO_ROOT/assets/shell-integration/config_version.txt"
        "$REPO_ROOT/assets/shell-integration/config_update_highlights.tsv"
    )
fi

git add -- "${files_to_add[@]}"

if git diff --cached --quiet; then
    die "Nothing staged. Did the editor session revert the draft?"
fi

git commit -m "chore(release): prepare V${new_version}"
log_info "Committed prep changes as 'chore(release): prepare V${new_version}'"

if [[ "$NO_PUSH" == "0" ]]; then
    git push origin main
    log_info "Pushed to origin/main"
else
    log_warn "Skipped push (--no-push). Push manually before running release.sh:"
    log_warn "  git push origin main"
fi

log_info ""
log_info "Prep complete. Suggested next steps:"
log_info "  ./scripts/release.sh --dry-run    # validate the full release path"
log_info "  ./scripts/release.sh              # build, notarize, publish, tap"
