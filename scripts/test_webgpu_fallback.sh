#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/test_webgpu_fallback.sh [--strict] [--force-kill-existing] [--binary PATH] [--base-config PATH]

Runs a macOS smoke test for the "WebGpu init failure -> OpenGL fallback" path.
It launches a dedicated kaku-gui process with:
  - front_end = 'WebGpu'
  - webgpu_force_fallback_adapter = true

Then it samples vmmap output and classifies the renderer:
  - OpenGL marker: IOGPUSurfaceMTL
  - WebGpu marker: CAMetalLayer Display Drawable

Exit code:
  0: fallback observed (OpenGL marker found), or WebGpu marker found in non-strict mode
  1: strict mode violation or unable to classify the renderer
EOF
}

STRICT=0
FORCE_KILL_EXISTING=0
BINARY="/Applications/Sumi.app/Contents/MacOS/kaku-gui"
BASE_CONFIG="/Applications/Sumi.app/Contents/Resources/sumi.lua"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --strict)
      STRICT=1
      shift
      ;;
    --force-kill-existing)
      FORCE_KILL_EXISTING=1
      shift
      ;;
    --binary)
      BINARY="$2"
      shift 2
      ;;
    --base-config)
      BASE_CONFIG="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage
      exit 1
      ;;
  esac
done

if [[ ! -x "$BINARY" ]]; then
  echo "Binary is not executable: $BINARY" >&2
  exit 1
fi
if [[ ! -f "$BASE_CONFIG" ]]; then
  echo "Base config not found: $BASE_CONFIG" >&2
  exit 1
fi

for cmd in vmmap pgrep mktemp awk rg; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "Missing required command: $cmd" >&2
    exit 1
  fi
done

tmp_dir="$(mktemp -d -t kaku-webgpu-fallback.XXXXXX)"
cfg_file="$tmp_dir/kaku-fallback.lua"
log_file="$tmp_dir/kaku-fallback.log"
vmmap_file="$tmp_dir/vmmap.txt"
summary_file="$tmp_dir/vmmap-summary.txt"
class_name="com.github.chiww.sumi.fallback.$RANDOM.$RANDOM"

cleanup() {
  local exit_code=$?
  if [[ -n "${pid:-}" ]]; then
    kill "$pid" >/dev/null 2>&1 || true
    wait "$pid" 2>/dev/null || true
  fi
  if [[ "$exit_code" -eq 0 ]]; then
    rm -rf "$tmp_dir"
  else
    echo "Artifacts kept at: $tmp_dir" >&2
  fi
}
trap cleanup EXIT

cat >"$cfg_file" <<EOF
local config = dofile('$BASE_CONFIG')
config.front_end = 'WebGpu'
config.webgpu_force_fallback_adapter = true
return config
EOF

before_pids="$(pgrep -x kaku-gui || true)"
if [[ -n "$before_pids" ]]; then
  if [[ "$FORCE_KILL_EXISTING" -eq 1 ]]; then
    pkill -x kaku-gui >/dev/null 2>&1 || true
    sleep 1
    before_pids=""
  else
    echo "Found existing kaku-gui process(es): $before_pids" >&2
    echo "Close Sumi first, or rerun with --force-kill-existing." >&2
    exit 1
  fi
fi

"$BINARY" --config-file "$cfg_file" start --always-new-process --class "$class_name" >"$log_file" 2>&1 &

pid=""
for _ in $(seq 1 40); do
  sleep 0.25
  now_pids="$(pgrep -x kaku-gui || true)"
  for cand in $now_pids; do
    if ! grep -q -w "$cand" <<<"$before_pids"; then
      pid="$cand"
      break
    fi
  done
  if [[ -n "$pid" ]]; then
    break
  fi
done

if [[ -z "$pid" ]]; then
  echo "FAIL: Could not find a newly launched kaku-gui process." >&2
  echo "Captured log: $log_file" >&2
  tail -n 80 "$log_file" >&2 || true
  exit 1
fi

vmmap "$pid" >"$vmmap_file"
vmmap -summary "$pid" >"$summary_file"

echo "PID: $pid"
awk '/Physical footprint:|IOSurface|MALLOC_SMALL/ {print}' "$summary_file" || true

has_opengl=0
has_webgpu=0
if rg -q "IOGPUSurfaceMTL" "$vmmap_file" || rg -q "^OpenGL GLSL" "$summary_file"; then
  has_opengl=1
fi
if rg -q "CAMetalLayer Display Drawable" "$vmmap_file"; then
  has_webgpu=1
fi

if [[ "$has_opengl" -eq 1 ]]; then
  echo "PASS: OpenGL marker found; fallback path exercised."
  exit 0
fi

if [[ "$has_webgpu" -eq 1 ]]; then
  if [[ "$STRICT" -eq 1 ]]; then
    echo "FAIL: WebGpu marker found; strict mode expected OpenGL fallback." >&2
    exit 1
  fi
  echo "WARN: WebGpu marker found; fallback not exercised on this machine."
  echo "PASS (non-strict): launch succeeded, but fallback was not forced."
  exit 0
fi

echo "FAIL: Could not classify renderer from vmmap output." >&2
echo "Inspect files for details:" >&2
echo "  $vmmap_file" >&2
echo "  $summary_file" >&2
exit 1
