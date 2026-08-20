#!/usr/bin/env bash

set -o errexit -o nounset -o pipefail

# Argument provided by the reusable workflow caller, see
# https://github.com/bazel-contrib/.github/blob/v7.7.0/.github/workflows/release_ruleset.yaml
TAG=$1
# The prefix matches what GitHub generates for source archives, so switching
# between a released artifact and a source archive leaves `strip_prefix` alone.
PREFIX="rules_rs_mutants-${TAG:1}"
ARCHIVE="rules_rs_mutants-$TAG.tar.gz"

# NB: `export-ignore` configuration for 'git archive' is in /.gitattributes
git archive --format=tar --prefix="${PREFIX}"/ "${TAG}" | gzip > "$ARCHIVE"

cat << EOF
## Add to your \`MODULE.bazel\` file:

\`\`\`starlark
bazel_dep(name = "rules_rs_mutants", version = "${TAG:1}")
\`\`\`

Then point the flag at a \`cargo-mutants\` binary built from your own lockfile:

\`\`\`starlark
crate.annotation(crate = "cargo-mutants", gen_binaries = ["cargo-mutants"])
\`\`\`

\`\`\`
build --@rules_rs_mutants//mutants:cargo_mutants_binary=@crates//:cargo-mutants__cargo-mutants
\`\`\`
EOF
