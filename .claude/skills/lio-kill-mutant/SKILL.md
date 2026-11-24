---
name: lio-kill-mutant
description: Find and kill surviving cargo-mutants mutants in lio's core bookkeeping, matching what the PR mutants job does. Use when CI's mutants job fails, when a mutants.out report lists MISSED mutants, or before opening a PR that edits a file listed in `.cargo/mutants.toml`.
---

# Killing surviving mutants

CI mutates every changed line in the files `.cargo/mutants.toml` lists
(`examine_globs`) and fails the PR if any mutant survives. The config already
limits each run to the core suites (`--lib`, `cancellation`, `capacity`).

## Reproduce

```bash
git diff origin/main...HEAD > "$TMPDIR/pr.diff"
PROPTEST_CASES=64 LIO_LIFECYCLE_DEPTH=4 CARGO_PROFILE_DEV_DEBUG=0 \
  cargo mutants --in-diff "$TMPDIR/pr.diff" --no-shuffle --jobs 2
```

For a single file: `cargo mutants -f lio/src/slab.rs`. For a CI failure,
read `mutants.out/missed.txt` from the job's `mutants-out` artifact
instead of rerunning everything.

## Kill

For each missed mutant (`file:line:col: replace X with Y in fn`):

1. Read the code and decide what observable behaviour the mutation changes.
2. Write the smallest test that fails with the mutant and passes without
   it, through `ScriptedBackend` + `Checked` when the code is driver
   bookkeeping. Assert the behaviour, not the implementation.
3. Confirm the kill: `cargo mutants -f <file> --re '<mutant description>'`.

## Equivalent mutants

If the mutant cannot change behaviour (as in the existing `exclude_re`
entries), add it to `exclude_re` in `.cargo/mutants.toml` with a comment
that proves equivalence. Anchor the pattern to the function, not only the
operator. Prefer a test whenever one is possible; a skipped mutant is
a gap the nightly job will not see either.
