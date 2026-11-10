#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

VERSION_FILE="assets/shell-integration/config_version.txt"
HIGHLIGHTS_FILE="assets/shell-integration/config_update_highlights.tsv"
CHECK_SCRIPT="assets/shell-integration/check_config_version.sh"
FIRST_RUN_SCRIPT="assets/shell-integration/first_run.sh"
KAKU_LUA="assets/macos/Sumi.app/Contents/Resources/sumi.lua"

file_contains_literal() {
	local needle="$1"
	local file="$2"

	if command -v rg >/dev/null 2>&1; then
		rg -Fq -- "$needle" "$file"
	else
		grep -Fq -- "$needle" "$file"
	fi
}

if [[ ! -f "$VERSION_FILE" ]]; then
	echo "Missing config version file: $VERSION_FILE" >&2
	exit 1
fi

config_version="$(tr -d '[:space:]' < "$VERSION_FILE" || true)"
if [[ ! "$config_version" =~ ^[0-9]+$ ]]; then
	echo "Invalid config version in $VERSION_FILE: $config_version" >&2
	exit 1
fi

if [[ ! -f "$HIGHLIGHTS_FILE" ]]; then
	echo "Missing config highlights file: $HIGHLIGHTS_FILE" >&2
	exit 1
fi

if ! awk -F '\t' '
	BEGIN { ok = 1 }
	NF == 0 { next }
	$0 ~ /^[[:space:]]*#/ { next }
	NF < 2 || $1 !~ /^[0-9]+$/ || $2 == "" { ok = 0 }
	END { exit ok ? 0 : 1 }
' "$HIGHLIGHTS_FILE"; then
	echo "Invalid config highlights format in $HIGHLIGHTS_FILE" >&2
	exit 1
fi

for script in "$CHECK_SCRIPT" "$FIRST_RUN_SCRIPT"; do
	if ! file_contains_literal 'read_bundled_config_version "$SCRIPT_DIR"' "$script"; then
		echo "Expected $script to read config_version.txt via read_bundled_config_version" >&2
		exit 1
	fi
done

if ! file_contains_literal 'config_version.txt' "$KAKU_LUA"; then
	echo "Expected $KAKU_LUA to read config_version.txt" >&2
	exit 1
fi

echo "Config release readiness passed for version $config_version"
