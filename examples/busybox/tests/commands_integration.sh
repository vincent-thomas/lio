#!/usr/bin/env bash

set -euo pipefail

BUSYBOX_BIN="${BUSYBOX:-${1:-busybox}}"

if [[ "$BUSYBOX_BIN" != */* ]]; then
  BUSYBOX_BIN="$(command -v "$BUSYBOX_BIN")"
fi

if [[ -z "${BUSYBOX_BIN:-}" || ! -x "$BUSYBOX_BIN" ]]; then
  printf 'busybox binary not found or not executable: %s\n' "${1:-busybox}" >&2
  exit 1
fi

TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/busybox-integration.XXXXXX")"
trap 'rm -rf "$TEST_ROOT"' EXIT

LAST_STDOUT=""
LAST_STDERR=""
LAST_STATUS=0
CASES=0

fail() {
  local message="$1"
  printf 'FAIL: %s\n' "$message" >&2
  if [[ -n "$LAST_STDOUT" && -f "$LAST_STDOUT" ]]; then
    printf -- '--- stdout ---\n' >&2
    cat "$LAST_STDOUT" >&2
    printf -- '\n' >&2
  fi
  if [[ -n "$LAST_STDERR" && -f "$LAST_STDERR" ]]; then
    printf -- '--- stderr ---\n' >&2
    cat "$LAST_STDERR" >&2
    printf -- '\n' >&2
  fi
  exit 1
}

run_cmd() {
  local use_stdin=0
  local stdin_data=""

  if [[ "${1:-}" == "--stdin" ]]; then
    use_stdin=1
    stdin_data="$2"
    shift 2
  fi

  LAST_STDOUT="$(mktemp "${TEST_ROOT}/stdout.XXXXXX")"
  LAST_STDERR="$(mktemp "${TEST_ROOT}/stderr.XXXXXX")"

  if (( use_stdin )); then
    if printf '%s' "$stdin_data" | "$BUSYBOX_BIN" "$@" >"$LAST_STDOUT" 2>"$LAST_STDERR"; then
      LAST_STATUS=0
    else
      LAST_STATUS=$?
    fi
  else
    if "$BUSYBOX_BIN" "$@" >"$LAST_STDOUT" 2>"$LAST_STDERR"; then
      LAST_STATUS=0
    else
      LAST_STATUS=$?
    fi
  fi
}

assert_status() {
  local expected="$1"
  [[ "$LAST_STATUS" -eq "$expected" ]] || fail "expected exit status $expected, got $LAST_STATUS"
}

assert_stdout() {
  local expected="$1"
  local actual
  actual="$(cat "$LAST_STDOUT")"
  [[ "$actual" == "$expected" ]] || fail "stdout mismatch"
}

assert_stderr() {
  local expected="$1"
  local actual
  actual="$(cat "$LAST_STDERR")"
  [[ "$actual" == "$expected" ]] || fail "stderr mismatch"
}

assert_stdout_contains() {
  local needle="$1"
  grep -Fq "$needle" "$LAST_STDOUT" || fail "stdout missing expected text: $needle"
}

assert_stderr_contains() {
  local needle="$1"
  grep -Fq "$needle" "$LAST_STDERR" || fail "stderr missing expected text: $needle"
}

assert_file_contents() {
  local path="$1"
  local expected="$2"
  local actual
  actual="$(cat "$path")"
  [[ "$actual" == "$expected" ]] || fail "file contents mismatch for $path"
}

assert_exists() {
  local path="$1"
  [[ -e "$path" ]] || fail "expected path to exist: $path"
}

assert_not_exists() {
  local path="$1"
  [[ ! -e "$path" ]] || fail "expected path to be absent: $path"
}

assert_symlink_target() {
  local path="$1"
  local expected="$2"
  local actual
  actual="$(readlink "$path")"
  [[ "$actual" == "$expected" ]] || fail "symlink target mismatch for $path"
}

pass() {
  CASES=$((CASES + 1))
  printf 'ok %02d - %s\n' "$CASES" "$1"
}

cd "$TEST_ROOT"
PHYS_PWD="$(pwd -P)"

printf 'alpha\nbeta\n' > cat.txt
run_cmd echo hello world
assert_status 0
assert_stdout "hello world"
assert_stderr ""
pass "echo"

run_cmd printf 'value:%s\n' done
assert_status 0
assert_stdout "value:done"
assert_stderr ""
pass "printf"

run_cmd seq 3
assert_status 0
assert_stdout $'1\n2\n3'
assert_stderr ""
pass "seq"

run_cmd cat cat.txt
assert_status 0
assert_stdout $'alpha\nbeta'
assert_stderr ""
pass "cat"

printf 'copy me\n' > src.txt
run_cmd cp src.txt dst.txt
assert_status 0
assert_stdout ""
assert_stderr ""
assert_file_contents dst.txt "copy me"
pass "cp"

run_cmd mv dst.txt moved.txt
assert_status 0
assert_stdout ""
assert_stderr ""
assert_not_exists dst.txt
assert_file_contents moved.txt "copy me"
pass "mv"

run_cmd ln -s moved.txt moved.link
assert_status 0
assert_stdout ""
assert_stderr ""
assert_symlink_target moved.link "moved.txt"
pass "ln -s"

run_cmd readlink moved.link
assert_status 0
assert_stdout "moved.txt"
assert_stderr ""
pass "readlink"

run_cmd realpath moved.link
assert_status 0
assert_stdout "${PHYS_PWD}/moved.txt"
assert_stderr ""
pass "realpath"

run_cmd dirname /tmp/example/file.txt
assert_status 0
assert_stdout "/tmp/example"
assert_stderr ""
pass "dirname"

run_cmd basename /tmp/example/file.txt
assert_status 0
assert_stdout "file.txt"
assert_stderr ""
pass "basename"

run_cmd mkdir -p nested/dir
assert_status 0
assert_stdout ""
assert_stderr ""
assert_exists nested/dir
pass "mkdir -p"

run_cmd touch touched.txt
assert_status 0
assert_stdout ""
assert_stderr ""
assert_exists touched.txt
pass "touch"

printf 'one\ntwo\nthree\n' > lines.txt
run_cmd head -n 2 lines.txt
assert_status 0
assert_stdout $'one\ntwo'
assert_stderr ""
pass "head"

run_cmd tail -n 2 lines.txt
assert_status 0
assert_stdout $'two\nthree'
assert_stderr ""
pass "tail"

printf 'aa\nbbb\n' > wc.txt
run_cmd wc wc.txt
assert_status 0
assert_stdout_contains "2       2       7 wc.txt"
assert_stderr ""
pass "wc"

printf 'b\na\na\n' > sort.txt
run_cmd sort sort.txt
assert_status 0
assert_stdout $'a\na\nb'
assert_stderr ""
pass "sort"

printf 'x\nx\ny\n' > uniq.txt
run_cmd uniq uniq.txt
assert_status 0
assert_stdout $'x\ny'
assert_stderr ""
pass "uniq"

printf 'left:right\n' > cut.txt
run_cmd cut -d : -f 2 cut.txt
assert_status 0
assert_stdout "right"
assert_stderr ""
pass "cut"

printf 'a\nb\n' > nl.txt
run_cmd nl nl.txt
assert_status 0
assert_stdout_contains $'1\ta'
assert_stdout_contains $'2\tb'
assert_stderr ""
pass "nl"

run_cmd tac nl.txt
assert_status 0
assert_stdout $'b\na'
assert_stderr ""
pass "tac"

run_cmd cmp cat.txt cat.txt
assert_status 0
assert_stdout ""
assert_stderr ""
pass "cmp"

run_cmd --stdin $'abc\n' tr a-z A-Z
assert_status 0
assert_stdout "ABC"
assert_stderr ""
pass "tr"

run_cmd unlink touched.txt
assert_status 0
assert_stdout ""
assert_stderr ""
assert_not_exists touched.txt
pass "unlink"

printf 'desserts\n' > rev.txt
run_cmd rev rev.txt
assert_status 0
assert_stdout "stressed"
assert_stderr ""
pass "rev"

printf 'remove me\n' > remove.txt
run_cmd rm remove.txt
assert_status 0
assert_stdout ""
assert_stderr ""
assert_not_exists remove.txt
pass "rm"

run_cmd rmdir nested/dir
assert_status 0
assert_stdout ""
assert_stderr ""
assert_not_exists nested/dir
pass "rmdir"

printf 'abc' > digest.txt
run_cmd cksum digest.txt
assert_status 0
assert_stdout "1219131554 3 digest.txt"
assert_stderr ""
pass "cksum"

printf 'abcdef\n' > fold.txt
run_cmd fold -w 3 fold.txt
assert_status 0
assert_stdout $'abc\ndef'
assert_stderr ""
pass "fold"

printf 'AB' > bytes.bin
run_cmd hexdump bytes.bin
assert_status 0
assert_stdout_contains "41 42"
assert_stdout_contains "0000002"
assert_stderr ""
pass "hexdump"

run_cmd od bytes.bin
assert_status 0
assert_stdout_contains "101 102"
assert_stdout_contains "0000002"
assert_stderr ""
pass "od"

printf 'hi\0there\0' > strings.bin
run_cmd strings strings.bin
assert_status 0
assert_stdout "there"
assert_stderr ""
pass "strings"

run_cmd --stdin 'abc' base64
assert_status 0
assert_stdout "YWJj"
assert_stderr ""
pass "base64"

run_cmd --stdin 'abc' base32
assert_status 0
assert_stdout "MFRGG==="
assert_stderr ""
pass "base32"

run_cmd md5sum digest.txt
assert_status 0
assert_stdout "900150983cd24fb0d6963f7d28e17f72  digest.txt"
assert_stderr ""
pass "md5sum"

run_cmd sha1sum digest.txt
assert_status 0
assert_stdout "a9993e364706816aba3e25717850c26c9cd0d89d  digest.txt"
assert_stderr ""
pass "sha1sum"

run_cmd sha256sum digest.txt
assert_status 0
assert_stdout "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  digest.txt"
assert_stderr ""
pass "sha256sum"

run_cmd test 7 -gt 3
assert_status 0
assert_stdout ""
assert_stderr ""
pass "test true"

run_cmd test 1 -eq 2
assert_status 1
assert_stdout ""
assert_stderr ""
pass "test false"

run_cmd timeout 0.1 /bin/sh -c 'sleep 1'
assert_status 124
assert_stdout ""
assert_stderr ""
pass "timeout"

run_cmd --stdin $'a b c\n' xargs echo
assert_status 0
assert_stdout "a b c"
assert_stderr ""
pass "xargs"

printf 'abcd' > dd.in
run_cmd dd if=dd.in of=dd.out bs=2 count=1
assert_status 0
assert_stdout ""
assert_stderr $'1+0 records in\n1+0 records out\n2 bytes copied'
assert_file_contents dd.out "ab"
pass "dd"

run_cmd mktemp
assert_status 0
assert_stderr ""
MKTEMP_PATH="$(cat "$LAST_STDOUT")"
assert_exists "$MKTEMP_PATH"
rm -f "$MKTEMP_PATH"
pass "mktemp"

printf 'aa\nbb\n' > paste_a.txt
printf '11\n22\n' > paste_b.txt
run_cmd paste paste_a.txt paste_b.txt
assert_status 0
assert_stdout $'aa\t11\nbb\t22'
assert_stderr ""
pass "paste"

printf 'a\nb\nc\n' > comm_a.txt
printf 'b\nc\nd\n' > comm_b.txt
run_cmd comm comm_a.txt comm_b.txt
assert_status 0
assert_stdout $'a\n\t\tb\n\t\tc\n\td'
assert_stderr ""
pass "comm"

printf '\n%d command checks passed.\n' "$CASES"
