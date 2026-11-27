# Tests for safe baseline pruning

## A missing import can hide an existing baselined error

```scrut {output_stream: stdout}
$ mkdir -p $TMPDIR/baseline_prune_safety && \
> printf 'from helper import value\nx: str = value\n' > $TMPDIR/baseline_prune_safety/main.py && \
> echo 'value = 1' > $TMPDIR/baseline_prune_safety/helper.py && \
> printf 'baseline = "baseline.json"\n' > $TMPDIR/baseline_prune_safety/pyrefly.toml && \
> cd $TMPDIR/baseline_prune_safety && \
> $PYREFLY check main.py --update-baseline --summary=none --output-format=omit-errors >/dev/null 2>/dev/null; \
> cp baseline.json original.json && \
> $JQ -r '.errors[].name' baseline.json && \
> rm helper.py
bad-assignment
[0]
```

Pruning leaves the baseline unchanged while the import is broken.
See https://github.com/pyrefly-org/pyrefly/issues/5760.

```scrut {output_stream: stderr}
$ cd $TMPDIR/baseline_prune_safety && \
> $PYREFLY check main.py --prune-baseline --summary=none --output-format=omit-errors
 INFO Skipped baseline pruning because new errors remain
[1]
```

```scrut {output_stream: stdout}
$ cd $TMPDIR/baseline_prune_safety && cmp -s original.json baseline.json
[0]
```

## Display filters do not make a broken import safe

```scrut {output_stream: stderr}
$ cd $TMPDIR/baseline_prune_safety && \
> cp original.json baseline.json && \
> $PYREFLY check main.py --prune-baseline --only=bad-assignment --summary=none
 INFO Skipped baseline pruning because new errors remain
[0]
```

```scrut {output_stream: stdout}
$ cd $TMPDIR/baseline_prune_safety && cmp -s original.json baseline.json
[0]
```

## Warnings below the failure threshold do not block pruning

```scrut {output_stream: stderr}
$ cd $TMPDIR/baseline_prune_safety && \
> cp original.json baseline.json && \
> $PYREFLY check main.py --prune-baseline --warn=missing-import --summary=none --output-format=omit-errors
 INFO Removed 1 unused suppression from the baseline file
[0]
```

## Warnings at the failure threshold block pruning

```scrut {output_stream: stderr}
$ cd $TMPDIR/baseline_prune_safety && \
> cp original.json baseline.json && \
> $PYREFLY check main.py --prune-baseline --warn=missing-import --min-severity=warn --summary=none --output-format=omit-errors
 INFO Skipped baseline pruning because new errors remain
[1]
```

```scrut {output_stream: stdout}
$ cd $TMPDIR/baseline_prune_safety && cmp -s original.json baseline.json
[0]
```

## Baseline matches do not block pruning even when displayed as errors

```scrut {output_stream: stderr}
$ cd $TMPDIR/baseline_prune_safety && \
> echo 'value = 1' > helper.py && \
> $JQ '.errors += [{"path": "gone.py", "column": 1, "name": "bad-assignment"}]' original.json > baseline.json && \
> $PYREFLY check main.py --prune-baseline --baseline-error-level=error --summary=none --output-format=omit-errors
 INFO Removed 1 unused suppression from the baseline file
[1]
```

```scrut {output_stream: stdout}
$ $JQ '.errors | length' $TMPDIR/baseline_prune_safety/baseline.json
1
[0]
```
