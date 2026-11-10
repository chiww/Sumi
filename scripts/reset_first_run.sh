#!/bin/bash
# Reset Kaku First Run Experience
# This script is for testing purposes. It removes persisted Kaku state
# so that Kaku will trigger the first run setup again.

set -e

CONFIG_DIR="$HOME/.config/sumi"
STATE_FILE="$CONFIG_DIR/state.json"
LEGACY_FILES=(
	"$CONFIG_DIR/.first_run_completed"
	"$CONFIG_DIR/.kaku_config_version"
	"$CONFIG_DIR/.kaku_window_geometry"
	"$CONFIG_DIR/.kaku_window_position"
)

echo "Resetting Sumi First Run..."

if [[ -f "$STATE_FILE" ]]; then
	rm "$STATE_FILE"
	echo "✅ Removed state file: $STATE_FILE"
else
	echo "ℹ️  State file not found: $STATE_FILE"
fi

for file in "${LEGACY_FILES[@]}"; do
	if [[ -f "$file" ]]; then
		rm "$file"
		echo "✅ Removed legacy file: $file"
	fi
done

echo "Now relaunch Sumi to see the First Run experience."
