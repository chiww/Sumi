#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
source "$SCRIPT_DIR/common.sh"

echo "zshz_jump_provider: starting (zsh=$(command -v zsh 2>/dev/null || echo MISSING), bash=$BASH_VERSION)" >&2

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/kaku-zshz-jump-provider.XXXXXX")"
cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT

HOME="$tmp_dir/home"
ZDOTDIR="$HOME"
mkdir -p "$HOME"

vendor_dir="$tmp_dir/vendor"
create_stub_vendor_dir "$vendor_dir"

# Minimal fast-syntax-highlighting stub
cat >"$vendor_dir/fast-syntax-highlighting/fast-syntax-highlighting.plugin.zsh" <<'EOF'
typeset -g KAKU_TEST_FAST_SH_SOURCED=1
_zsh_highlight() { :; }
EOF

# Minimal zsh-z stub that defines the zshz function and tracks source count
cat >"$vendor_dir/zsh-z/zsh-z.plugin.zsh" <<'EOF'
typeset -g KAKU_TEST_ZSHZ_SOURCE_COUNT=$(( ${KAKU_TEST_ZSHZ_SOURCE_COUNT:-0} + 1 ))
zshz() { :; }
z() { zshz "$@"; }
EOF

echo "zshz_jump_provider: running setup_zsh.sh" >&2
setup_out=""
setup_status=0
setup_out="$(
  HOME="$HOME" \
  ZDOTDIR="$ZDOTDIR" \
  KAKU_INIT_INTERNAL=1 \
  KAKU_SKIP_TOOL_BOOTSTRAP=1 \
  KAKU_SKIP_TERMINFO_BOOTSTRAP=1 \
  KAKU_VENDOR_DIR="$vendor_dir" \
  bash "$REPO_ROOT/assets/shell-integration/setup_zsh.sh" --update-only 2>&1
)" || setup_status=$?
if [[ "$setup_status" -ne 0 ]]; then
  echo "zshz_jump_provider: setup_zsh.sh failed (exit $setup_status):" >&2
  echo "$setup_out" >&2
  exit 1
fi

kaku_zsh="$HOME/.config/sumi/zsh/sumi.zsh"
if [[ ! -f "$kaku_zsh" ]]; then
  echo "zshz_jump_provider: sumi.zsh not created at $kaku_zsh" >&2
  exit 1
fi

# Test 1: zsh-z plugin is sourced and zshz function is available
with_zshz=""
if ! with_zshz="$(
  TERM=xterm-256color \
  HOME="$HOME" \
  ZDOTDIR="$ZDOTDIR" \
  zsh -f -c '
source "$HOME/.config/sumi/zsh/sumi.zsh"
if (( ${+functions[zshz]} )); then
  print -r -- "__KAKU_ZSHZ_LOADED__:1"
else
  print -r -- "__KAKU_ZSHZ_LOADED__:0"
fi
' 2>&1
)"; then
  echo "zshz_jump_provider: zsh with zsh-z exited non-zero:" >&2
  echo "$with_zshz" >&2
  exit 1
fi

case "$with_zshz" in
  *__KAKU_ZSHZ_LOADED__:1* ) ;;
  * )
    echo "zshz_jump_provider: zshz function not defined after sourcing sumi.zsh:" >&2
    echo "$with_zshz" >&2
    exit 1
    ;;
esac

# Test 2: when zshz is already defined, sumi.zsh must not source zsh-z again
with_existing_provider=""
if ! with_existing_provider="$(
  TERM=xterm-256color \
  HOME="$HOME" \
  ZDOTDIR="$ZDOTDIR" \
  zsh -f -c '
# Simulate user having already loaded zsh-z themselves
typeset -g KAKU_TEST_ZSHZ_SOURCE_COUNT=0
zshz() { :; }
source "$HOME/.config/sumi/zsh/sumi.zsh"
print -r -- "__KAKU_NO_DOUBLE_SOURCE__:${KAKU_TEST_ZSHZ_SOURCE_COUNT}"
' 2>&1
)"; then
  echo "zshz_jump_provider: zsh with existing provider exited non-zero:" >&2
  echo "$with_existing_provider" >&2
  exit 1
fi

case "$with_existing_provider" in
  *__KAKU_NO_DOUBLE_SOURCE__:0* ) ;;
  * )
    echo "zshz_jump_provider: zsh-z sourced again despite existing zshz function:" >&2
    echo "$with_existing_provider" >&2
    exit 1
    ;;
esac

# Test 3: when zoxide already owns z, sumi.zsh must not source zsh-z or override z.
with_zoxide_provider=""
if ! with_zoxide_provider="$(
  TERM=xterm-256color \
  HOME="$HOME" \
  ZDOTDIR="$ZDOTDIR" \
  zsh -f -c '
# Simulate zoxide init zsh having run before Kaku integration.
typeset -g KAKU_TEST_ZSHZ_SOURCE_COUNT=0
__zoxide_z() { :; }
_zoxide_z() { :; }
z() { __zoxide_z "$@"; }
source "$HOME/.config/sumi/zsh/sumi.zsh"
print -r -- "__KAKU_NO_ZSHZ_SOURCE_FOR_ZOXIDE__:${KAKU_TEST_ZSHZ_SOURCE_COUNT}"
if (( ${+functions[zshz]} )); then
  print -r -- "__KAKU_ZSHZ_DEFINED_WITH_ZOXIDE__:1"
else
  print -r -- "__KAKU_ZSHZ_DEFINED_WITH_ZOXIDE__:0"
fi
' 2>&1
)"; then
  echo "zshz_jump_provider: zsh with existing zoxide provider exited non-zero:" >&2
  echo "$with_zoxide_provider" >&2
  exit 1
fi

case "$with_zoxide_provider" in
  *__KAKU_NO_ZSHZ_SOURCE_FOR_ZOXIDE__:0*__KAKU_ZSHZ_DEFINED_WITH_ZOXIDE__:0* ) ;;
  * )
    echo "zshz_jump_provider: zsh-z was loaded despite existing zoxide provider:" >&2
    echo "$with_zoxide_provider" >&2
    exit 1
    ;;
esac

# Test 4: when zsh-z plugin file is missing, no errors should occur (graceful degradation)
without_zshz=""
if ! without_zshz="$(
  TERM=xterm-256color \
  HOME="$HOME" \
  ZDOTDIR="$ZDOTDIR" \
  zsh -f -c '
# Remove plugin file to simulate missing install
rm -f "$HOME/.config/sumi/zsh/plugins/zsh-z/zsh-z.plugin.zsh" 2>/dev/null || true
source "$HOME/.config/sumi/zsh/sumi.zsh"
print -r -- "__KAKU_NO_ZSHZ_OK__:0"
' 2>&1
)"; then
  echo "zshz_jump_provider: zsh without zsh-z exited non-zero:" >&2
  echo "$without_zshz" >&2
  exit 1
fi

case "$without_zshz" in
  *__KAKU_NO_ZSHZ_OK__:0* ) ;;
  * )
    echo "zshz_jump_provider: sumi.zsh errored when zsh-z plugin is absent:" >&2
    echo "$without_zshz" >&2
    exit 1
    ;;
esac

echo "zshz_jump_provider smoke test passed"
