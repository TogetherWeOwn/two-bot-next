#!/bin/sh
set -eu

# PR jobs measure the test merge, not the event's PR head.
git rev-parse --verify HEAD > pipeline-benchmark-revision.txt
printf '%s\n' "${PR_HEAD_SHA:-}" > pipeline-benchmark-pr-head.txt
