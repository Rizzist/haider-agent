The delegated-create mixed-writer regression pairs the current Store with the
immutable schema-33 wave base `088e5a24`. Run it with a fresh evidence directory,
the harness build-slot script, and the loopback-only sandbox policy:

```sh
python3 scripts/regression/delegated-create-mixed-writers.py \
  --evidence /path/to/evidence \
  --build-slot /path/to/runtime/build-slot.sh \
  --sandbox /path/to/loopback-only.sb
```

It runs seven actual old-writer cases, including creation-pin clears and the
old-only A→B→A keep-pin control, plus a parent created by the old Store and
cleared by the candidate. An optional `--candidate-target` reuses a private
worktree-local candidate library target. The script records source identities,
commands, exit codes, binary hashes, and immutable replay journals, then deletes
its own source/build/runtime scratch directory. It requires at least 6 GiB free.

A separate candidate-open stage records normal startup metadata projections
before comparing the complete journals and metadata around receipt replay.
