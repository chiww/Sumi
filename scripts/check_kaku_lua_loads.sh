#!/bin/bash
# PostToolUse hook: verify the bundled sumi.lua still loads after an edit.
# LuaJIT caps a chunk at 200 local variables and the top-level chunk of the
# bundled config is already at capacity, so one new top-level `local` breaks
# startup while the edit itself looks fine. See "Bundled sumi.lua Pitfalls"
# in config/AGENTS.md.
#
# Reads the hook JSON payload on stdin. Exits 0 (silent) unless the edited
# file is the bundled sumi.lua and it fails to load. Also accepts a file path
# as $1 for manual runs.
set -u

command -v luajit >/dev/null 2>&1 || exit 0

if [[ $# -ge 1 ]]; then
	file_path="$1"
else
	command -v python3 >/dev/null 2>&1 || exit 0
	file_path=$(python3 -c 'import json,sys; print(json.load(sys.stdin).get("tool_input",{}).get("file_path",""))' 2>/dev/null) || exit 0
fi

case "$file_path" in
*/Resources/sumi.lua) ;;
*) exit 0 ;;
esac

[[ -f "$file_path" ]] || exit 0

if ! err=$(luajit -e "assert(loadfile('$file_path'))" 2>&1); then
	echo "sumi.lua no longer loads: $err" >&2
	echo "Likely the LuaJIT 200-locals top-level limit or a syntax error; see 'Bundled sumi.lua Pitfalls' in config/AGENTS.md." >&2
	exit 2
fi
exit 0
