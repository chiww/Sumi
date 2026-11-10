#!/usr/bin/env bash
# Smoke tests for the cleanup_legacy_inline_block logic in setup_zsh.sh.
# The parser only removes legacy blocks when every non-empty line matches a
# known Kaku-managed line. Unknown user lines keep the block intact.

set -euo pipefail

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

assert_file_eq() {
  local expected_file="$1"
  local actual_file="$2"
  local label="$3"
  if ! cmp -s "$expected_file" "$actual_file"; then
    echo "Expected:" >&2
    cat "$expected_file" >&2
    echo "Actual:" >&2
    cat "$actual_file" >&2
    fail "$label"
  fi
}

is_known_legacy_line() {
  local line="$1"
  [[ -z "${line//[[:space:]]/}" ]] && return 0

  grep -Fqx -- "$line" <<'EOF'
export KAKU_ZSH_DIR="$HOME/.config/sumi/zsh"
source "$KAKU_ZSH_DIR/plugins/zsh-syntax-highlighting/zsh-syntax-highlighting.zsh"
fi
EOF
}

run_cleanup() {
  local input_file="$1"
  local output_file="$2"
  local line
  local -a block_lines=()
  local in_block=0
  local saw_kaku_var=0
  local saw_syntax=0

  : >"$output_file"

  while IFS= read -r line || [[ -n "$line" ]]; do
    if [[ "$in_block" == "0" ]]; then
      if [[ "$line" == "# Sumi Shell Integration" ]]; then
        in_block=1
        saw_kaku_var=0
        saw_syntax=0
        block_lines=()
        continue
      fi

      printf '%s\n' "$line" >>"$output_file"
      continue
    fi

    block_lines+=("$line")
    [[ "$line" == *KAKU_ZSH_DIR* ]] && saw_kaku_var=1
    [[ "$line" == *zsh-syntax-highlighting/zsh-syntax-highlighting.zsh* ]] && saw_syntax=1

    if [[ "$saw_kaku_var" == "1" && "$saw_syntax" == "1" && "$line" =~ ^[[:space:]]*fi[[:space:]]*$ ]]; then
      local managed=1
      local block_line
      for block_line in "${block_lines[@]}"; do
        if ! is_known_legacy_line "$block_line"; then
          managed=0
          break
        fi
      done

      if [[ "$managed" == "0" ]]; then
        printf '%s\n' "# Sumi Shell Integration" >>"$output_file"
        for block_line in "${block_lines[@]}"; do
          printf '%s\n' "$block_line" >>"$output_file"
        done
      fi

      in_block=0
      saw_kaku_var=0
      saw_syntax=0
      block_lines=()
    fi
  done <"$input_file"

  if [[ "$in_block" == "1" ]]; then
    : >"$output_file"
    return 42
  fi
}

run_test() {
  local input_text="$1"
  local expected_status="$2"
  local expected_output="$3"
  local label="$4"

  local tmp_dir input_file output_file expected_file status
  tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/kaku-cleanup-test.XXXXXX")"
  input_file="$tmp_dir/input.zshrc"
  output_file="$tmp_dir/output.zshrc"
  expected_file="$tmp_dir/expected.zshrc"
  printf '%s' "$input_text" >"$input_file"
  printf '%s' "$expected_output" >"$expected_file"

  if run_cleanup "$input_file" "$output_file"; then
    status=0
  else
    status=$?
  fi
  [[ "$status" == "$expected_status" ]] || fail "$label: status expected $expected_status got $status"
  assert_file_eq "$expected_file" "$output_file" "$label"
  rm -rf "$tmp_dir"
}

LEGACY_BLOCK='# Sumi Shell Integration
export KAKU_ZSH_DIR="$HOME/.config/sumi/zsh"
source "$KAKU_ZSH_DIR/plugins/zsh-syntax-highlighting/zsh-syntax-highlighting.zsh"
fi'

# Test 1: managed legacy block is stripped, surrounding lines preserved.
run_test \
  $'export PATH="$HOME/bin:$PATH"\n'"$LEGACY_BLOCK"$'\nexport FOO=bar\n' \
  0 \
  $'export PATH="$HOME/bin:$PATH"\nexport FOO=bar\n' \
  "managed legacy block is removed and surrounding lines preserved"

# Test 2: no legacy block present - file is passed through unchanged.
run_test \
  $'export PATH="$HOME/bin:$PATH"\n# no kaku block here\n' \
  0 \
  $'export PATH="$HOME/bin:$PATH"\n# no kaku block here\n' \
  "no legacy block passes through unchanged"

# Test 3: unterminated block (missing closing fi) returns exit code 42.
run_test \
  $'# Sumi Shell Integration\nexport KAKU_ZSH_DIR="$HOME/.config/sumi/zsh"\n' \
  42 \
  "" \
  "unterminated block exits 42 and produces no output"

# Test 4: unknown user lines inside the block preserve the entire block.
run_test \
  $'export PATH="$HOME/bin:$PATH"\n# Sumi Shell Integration\nexport KAKU_ZSH_DIR="$HOME/.config/sumi/zsh"\nexport PATH="$HOME/.claude/bin:$PATH"\nsource "$HOME/.claude/shell/zshrc"\nsource "$KAKU_ZSH_DIR/plugins/zsh-syntax-highlighting/zsh-syntax-highlighting.zsh"\nfi\nexport FOO=bar\n' \
  0 \
  $'export PATH="$HOME/bin:$PATH"\n# Sumi Shell Integration\nexport KAKU_ZSH_DIR="$HOME/.config/sumi/zsh"\nexport PATH="$HOME/.claude/bin:$PATH"\nsource "$HOME/.claude/shell/zshrc"\nsource "$KAKU_ZSH_DIR/plugins/zsh-syntax-highlighting/zsh-syntax-highlighting.zsh"\nfi\nexport FOO=bar\n' \
  "custom lines inside legacy block keep the block unchanged"

echo "cleanup_legacy_inline_block smoke tests passed"
