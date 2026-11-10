#!/usr/bin/env bash

set -euo pipefail

PATH_LINE='[[ ":$PATH:" != *":$HOME/.config/sumi/zsh/bin:"* ]] && export PATH="$HOME/.config/sumi/zsh/bin:$PATH" # Sumi PATH Integration'
SOURCE_LINE='[[ -f "$HOME/.config/sumi/zsh/sumi.zsh" ]] && source "$HOME/.config/sumi/zsh/sumi.zsh" # Sumi Shell Integration'

normalize_kaku_path_line_file() {
  local input_file="$1"
  local output_file="$2"

  awk -v path_line="$PATH_LINE" '
BEGIN { replaced = 0; extra = 0 }
{
  if ($0 ~ /^[[:space:]]*#/ ) {
    print
    next
  }

  if ($0 ~ /^[[:space:]]*\[\[/ &&
      $0 ~ /sumi\/zsh\/bin/ &&
      $0 ~ /&&[[:space:]]*export[[:space:]]+PATH=/) {
    if (!replaced) {
      print path_line
      replaced = 1
    } else {
      extra++
    }
    next
  }

  print
}
END {
  if (replaced && extra > 0) {
    exit 2
  } else if (replaced) {
    exit 0
  }
  exit 3
}
' "$input_file" >"$output_file"
}

normalize_kaku_source_line_file() {
  local input_file="$1"
  local output_file="$2"

  awk -v source_line="$SOURCE_LINE" '
BEGIN { replaced = 0; extra = 0 }
{
  if ($0 ~ /^[[:space:]]*#/ ) {
    print
    next
  }

  if ($0 ~ /^[[:space:]]*\[\[/ &&
      $0 ~ /sumi\/zsh\/sumi\.zsh/ &&
      $0 ~ /&&[[:space:]]*source[[:space:]]/) {
    if (!replaced) {
      print source_line
      replaced = 1
    } else {
      extra++
    }
    next
  }

  print
}
END {
  if (replaced && extra > 0) {
    exit 2
  } else if (replaced) {
    exit 0
  }
  exit 3
}
' "$input_file" >"$output_file"
}

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

run_normalize() {
  local normalize_fn="$1"
  local input_text="$2"
  local expected_status="$3"
  local expected_output="$4"
  local label="$5"

  local tmp_dir input_file output_file expected_file status
  tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/kaku-normalize-test.XXXXXX")"
  input_file="$tmp_dir/input.zshrc"
  output_file="$tmp_dir/output.zshrc"
  expected_file="$tmp_dir/expected.zshrc"
  printf '%s' "$input_text" >"$input_file"
  printf '%s' "$expected_output" >"$expected_file"

  if "$normalize_fn" "$input_file" "$output_file"; then
    status=0
  else
    status=$?
  fi
  [[ "$status" == "$expected_status" ]] || fail "$label status expected $expected_status got $status"
  assert_file_eq "$expected_file" "$output_file" "$label output"
  rm -rf "$tmp_dir"
}

run_normalize \
  normalize_kaku_path_line_file \
  $'export PATH="$HOME/bin:$PATH"\n[[ ":$PATH:" != *":$HOME/.config/sumi/zsh/bin:"* ]] && export PATH="$HOME/.config/sumi/zsh/bin:$PATH"\n' \
  0 \
  $'export PATH="$HOME/bin:$PATH"\n'"$PATH_LINE"$'\n' \
  "legacy path line is replaced"

run_normalize \
  normalize_kaku_path_line_file \
  $'# [[ ":$PATH:" != *":$HOME/.config/sumi/zsh/bin:"* ]] && export PATH="$HOME/.config/sumi/zsh/bin:$PATH"\n'"$PATH_LINE"$'\n'"$PATH_LINE"$'\n' \
  2 \
  $'# [[ ":$PATH:" != *":$HOME/.config/sumi/zsh/bin:"* ]] && export PATH="$HOME/.config/sumi/zsh/bin:$PATH"\n'"$PATH_LINE"$'\n' \
  "duplicate path lines collapse"

run_normalize \
  normalize_kaku_path_line_file \
  $'if [[ -d "$HOME/.config/sumi/zsh/bin" ]]; then\n  export PATH="$PATH:$HOME/.config/sumi/zsh/bin"\nfi\n' \
  3 \
  $'if [[ -d "$HOME/.config/sumi/zsh/bin" ]]; then\n  export PATH="$PATH:$HOME/.config/sumi/zsh/bin"\nfi\n' \
  "custom path logic is preserved"

run_normalize \
  normalize_kaku_source_line_file \
  $'export PATH="$HOME/bin:$PATH"\n[[ -f "$HOME/.config/sumi/zsh/sumi.zsh" ]] && source "$HOME/.config/sumi/zsh/sumi.zsh" # Sumi Shell Integration\n' \
  0 \
  $'export PATH="$HOME/bin:$PATH"\n'"$SOURCE_LINE"$'\n' \
  "legacy line is replaced"

run_normalize \
  normalize_kaku_source_line_file \
  $'[[ -f "\\/Users/lex/.config/sumi/zsh/sumi.zsh" ]] && source "\\/Users/lex/.config/sumi/zsh/sumi.zsh" # Sumi Shell Integration\n' \
  0 \
  "$SOURCE_LINE"$'\n' \
  "escaped absolute path line is normalized"

run_normalize \
  normalize_kaku_source_line_file \
  $'[[ "$TERM_PROGRAM" == "Sumi" ]] && [[ -f "$HOME/.config/sumi/zsh/sumi.zsh" ]] && source "$HOME/.config/sumi/zsh/sumi.zsh" # Sumi Shell Integration\n' \
  0 \
  "$SOURCE_LINE"$'\n' \
  "term_program guarded line is normalized"

run_normalize \
  normalize_kaku_source_line_file \
  $'# [[ -f "$HOME/.config/sumi/zsh/sumi.zsh" ]] && source "$HOME/.config/sumi/zsh/sumi.zsh"\n'"$SOURCE_LINE"$'\n'"$SOURCE_LINE"$'\n' \
  2 \
  $'# [[ -f "$HOME/.config/sumi/zsh/sumi.zsh" ]] && source "$HOME/.config/sumi/zsh/sumi.zsh"\n'"$SOURCE_LINE"$'\n' \
  "comments preserved and duplicate active lines collapsed"

run_normalize \
  normalize_kaku_source_line_file \
  $'export PATH="$HOME/bin:$PATH"\n# no kaku integration here\n' \
  3 \
  $'export PATH="$HOME/bin:$PATH"\n# no kaku integration here\n' \
  "no matching line returns no-op status"

echo "normalize_kaku_source_line smoke tests passed"
