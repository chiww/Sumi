#!/usr/bin/env bash
set -euo pipefail

# Process-start benchmark: launch -> first window exists. OS caches are retained.
# Results are printed directly in terminal (no file output).

RUNS="${RUNS:-10}"
WARMUP="${WARMUP:-5}"
WAIT_TIMEOUT_SEC="${WAIT_TIMEOUT_SEC:-15}"

if ! command -v hyperfine >/dev/null 2>&1; then
	printf 'Error: hyperfine is required. Install with: brew install hyperfine\n' >&2
	exit 1
fi

# Format: DisplayName:AppNameForOpen:ProcessNameForPgrep
declare -a TERMINALS=(
	"Sumi:Sumi:kaku-gui"
	"Ghostty:Ghostty:ghostty"
	"Alacritty:Alacritty:alacritty"
)

BENCH_DIR=$(mktemp -d "${TMPDIR:-/tmp}/kaku-compare.XXXXXX")
cleanup_run() {
	local pid
	if [[ -f "$BENCH_DIR/pid" ]]; then
		pid=$(cat "$BENCH_DIR/pid")
		kill -TERM "$pid" 2>/dev/null || true
		for _ in {1..200}; do
			kill -0 "$pid" 2>/dev/null || break
			sleep 0.01
		done
		kill -KILL "$pid" 2>/dev/null || true
		rm -f "$BENCH_DIR/pid"
	fi
}
trap 'cleanup_run; rm -rf "$BENCH_DIR"' EXIT


wait_first_window() {
	local pid="$1"
	local timeout_sec="$2"

	# Avoid infinite wait: keep polling until timeout
	osascript <<OSA
set timeoutSeconds to ${timeout_sec}
set startAt to (current date)
tell application "System Events"
  repeat
    if exists (first process whose unix id is ${pid}) then
      tell (first process whose unix id is ${pid})
        if (count of windows) > 0 then
          return
        end if
      end tell
    end if
    if ((current date) - startAt) > timeoutSeconds then
      error "timeout waiting first window" number 124
    end if
    delay 0.01
  end repeat
end tell
OSA
}

start_once() {
	local executable="$1"
	local proc_name="$2"
	if pgrep -x "$proc_name" >/dev/null; then
		printf 'Refusing to benchmark while %s is running.\n' "$proc_name" >&2
		return 1
	fi
	"$executable" >"$BENCH_DIR/app.log" 2>&1 &
	local pid=$!
	printf '%s\n' "$pid" > "$BENCH_DIR/pid"
	wait_first_window "$pid" "$WAIT_TIMEOUT_SEC"
}

export WAIT_TIMEOUT_SEC BENCH_DIR
export -f cleanup_run wait_first_window start_once

declare -a INSTALLED=()
declare -a HYPERFINE_ARGS=()

printf 'Checking installed apps...\n'
for term in "${TERMINALS[@]}"; do
	IFS=':' read -r display_name app_name proc_name <<<"$term"

	if [[ -d "/Applications/${app_name}.app" || -d "$HOME/Applications/${app_name}.app" ]]; then
		if pgrep -x "$proc_name" >/dev/null; then
			printf 'Error: close %s yourself before benchmarking; existing sessions are preserved.\n' "$display_name" >&2
			exit 1
		fi
		app_path="/Applications/${app_name}.app"
		[[ -d "$app_path" ]] || app_path="$HOME/Applications/${app_name}.app"
		binary=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$app_path/Contents/Info.plist")
		printf -v quoted_binary '%q' "$app_path/Contents/MacOS/$binary"
		printf '  [+] %s\n' "$display_name"
		INSTALLED+=("$term")
		HYPERFINE_ARGS+=(--command-name "$display_name" "start_once $quoted_binary $proc_name")
	else
		printf '  [-] %s (not found)\n' "$display_name"
	fi
done

if [[ ${#INSTALLED[@]} -lt 2 ]]; then
	printf 'Error: need at least 2 installed terminals to compare.\n' >&2
	exit 1
fi

printf '\nBenchmark config: runs=%s warmup=%s timeout=%ss\n\n' "$RUNS" "$WARMUP" "$WAIT_TIMEOUT_SEC"

hyperfine \
	--shell bash \
	--prepare cleanup_run \
	--cleanup cleanup_run \
	--warmup "$WARMUP" \
	--runs "$RUNS" \
	--style full \
	--sort mean-time \
	"${HYPERFINE_ARGS[@]}"
