#!/usr/bin/env bash

set -euo pipefail

TMUX_SOURCE_LINE='source-file "$HOME/.config/sumi/tmux/sumi.tmux.conf" # Sumi tmux Integration'

normalize_kaku_tmux_source_line_file() {
  local input_file="$1"
  local output_file="$2"

  awk -v source_line="$TMUX_SOURCE_LINE" '
BEGIN { replaced = 0; extra = 0 }
{
  if ($0 ~ /^[[:space:]]*#/ ) {
    print
    next
  }

  if ($0 ~ /^[[:space:]]*source-file[[:space:]]+/ &&
      $0 ~ /sumi\/tmux\/sumi\.tmux\.conf/) {
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
  local input_text="$1"
  local expected_status="$2"
  local expected_output="$3"
  local label="$4"

  local tmp_dir input_file output_file expected_file status
  tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/kaku-tmux-test.XXXXXX")"
  input_file="$tmp_dir/input.tmux.conf"
  output_file="$tmp_dir/output.tmux.conf"
  expected_file="$tmp_dir/expected.tmux.conf"
  printf '%s' "$input_text" >"$input_file"
  printf '%s' "$expected_output" >"$expected_file"

  if normalize_kaku_tmux_source_line_file "$input_file" "$output_file"; then
    status=0
  else
    status=$?
  fi
  [[ "$status" == "$expected_status" ]] || fail "$label status expected $expected_status got $status"
  assert_file_eq "$expected_file" "$output_file" "$label output"
  rm -rf "$tmp_dir"
}

run_normalize \
  $'set -g default-terminal "screen-256color"\nsource-file ~/.config/sumi/tmux/sumi.tmux.conf\n' \
  0 \
  $'set -g default-terminal "screen-256color"\n'"$TMUX_SOURCE_LINE"$'\n' \
  "legacy tmux source line is normalized"

run_normalize \
  $'# source-file "$HOME/.config/sumi/tmux/sumi.tmux.conf"\n'"$TMUX_SOURCE_LINE"$'\n'"$TMUX_SOURCE_LINE"$'\n' \
  2 \
  $'# source-file "$HOME/.config/sumi/tmux/sumi.tmux.conf"\n'"$TMUX_SOURCE_LINE"$'\n' \
  "duplicate tmux source lines collapse while comments are preserved"

run_normalize \
  $'set -g status on\n# no kaku integration here\n' \
  3 \
  $'set -g status on\n# no kaku integration here\n' \
  "non matching tmux config remains unchanged"

echo "tmux integration smoke tests passed"
