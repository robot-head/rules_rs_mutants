# rules_rs_mutants

Mutation testing for Bazel Rust targets, as a normal `bazel test` target.

```bzl
load("@rules_rs_mutants//mutants:cargo_mutants_test.bzl", "cargo_mutants_test")

cargo_mutants_test(
    name = "mylib_mutants",
    test = ":mylib_test",
)
```

```
$ bazel test //:mylib_mutants
//:mylib_mutants    FAILED
  5 mutants: 4 caught, 1 missed, 0 unviable
  MISSED mylib.rs:12:5: replace triple with 0
```

[`cargo-mutants`](https://github.com/sourcefrog/cargo-mutants) is cargo-only — it
enumerates mutants, then copies the tree and drives `cargo build`/`cargo test`
itself ([sourcefrog/cargo-mutants#77](https://github.com/sourcefrog/cargo-mutants/issues/77)
is still open). This splits it at its own seam: cargo-mutants does what only it
can do (syn-parse and list mutants), Bazel does the build and test.

An extraction of [hermeticbuild/rules_rs#212](https://github.com/hermeticbuild/rules_rs/pull/212),
shipped standalone so it can move at its own pace: cargo-mutants' `--list --json`
schema is the integration contract here, and it carries no stability guarantee.

## Setup

```bzl
# MODULE.bazel
bazel_dep(name = "rules_rs", version = "0.0.106")
bazel_dep(name = "rules_rs_mutants", version = "0.1.0")
```

Bring your own `cargo-mutants` binary. It is used only to enumerate mutants, so
it never has to match your build's toolchain:

```bzl
# MODULE.bazel
crate.annotation(crate = "cargo-mutants", gen_binaries = ["cargo-mutants"])
```

```
# .bazelrc
build --@rules_rs_mutants//mutants:cargo_mutants_binary=@crates//:cargo-mutants__cargo-mutants
```

Vendoring is deliberately not offered: upstream ships **x86_64-only** release
assets, so arm64 Linux and arm64 macOS could not download it. An unconfigured
target fails with a pointer back to this section, not a missing-binary error.

## Fan-out

Every mutant is a fresh link plus a test run, so sweeps get slow fast. Both axes
are wired, and they compose:

```bzl
cargo_mutants_test(
    name = "mylib_mutants",
    jobs = 4,          # mutants built and tested at once, on one machine
    shard_count = 8,   # Bazel's native sharding — independent actions, spreads over RBE
    test = ":mylib_test",
)
```

`jobs` gives each concurrent mutant its own scratch source tree, output binary,
and `--out-dir` (rustc drops intermediate `.rcgu.o` codegen-unit objects there
and deletes them after linking, so jobs sharing one clobber each other
mid-link). `shard_count` partitions the mutant list with
`.skip(index).step_by(total)` and touches `TEST_SHARD_STATUS_FILE`.

## Sweeping a whole tree

The same aspect runs over `//...` with no `BUILD` edits. This only enumerates
mutants; it does not run them:

```bash
bazel build //... \
  --aspects=@rules_rs_mutants//mutants:cargo_mutants_test.bzl%cargo_mutants_aspect \
  --output_groups=cargo_mutants
```

## How it works

The aspect records the crate's `rustc --test` command line in `short_path` form
by calling rules_rust's own `construct_arguments`; the runner replays it once
per mutant from the runfiles tree, patching the mutated span into a scratch copy
of the sources.

## Integration tests

A crate whose real coverage lives in `tests/` reports almost every mutant as
missed unless those suites are rebuilt too. Name the library they link and the
suites themselves:

```python
cargo_mutants_test(
    name = "logql_mutants",
    test = ":logql_test",
    library = ":logql",
    integration_tests = [":parser_test", ":planner_test"],
)
```

Each mutant then rebuilds the library as an rlib, relinks every listed suite
against it, and runs them in order, stopping at the first failure -- most
mutants die in the unit tests, and running the rest only to confirm costs the
whole matrix per mutant.

The difference is not marginal. `crabka-logql`'s parser is covered entirely from
`tests/`, and its `syntax.rs` reports **165 survivors** on unit tests alone
against **1** when the suites are included.

Suites are listed rather than discovered, so a suite that needs a container or a
network fixture is not pulled into the sweep by accident, and the cost of adding
one is visible where it is added.

## Limits

- Suites under `tests/` are run only when named. By default a mutant faces the
  target's own `#[cfg(test)]` tests and nothing else; see
  [Integration tests](#integration-tests).
- Not supported on Windows.
- **macOS needs a hermetic C++ toolchain**, such as
  [`@llvm//toolchain:all`](https://github.com/hermeticbuild/toolchains_llvm_bootstrapped).
  Under apple_support's Xcode `cc_wrapper.sh` the replay fails to link with
  `Error: DEVELOPER_DIR not set`: Bazel's `XcodeLocalEnvProvider` derives that
  variable from `APPLE_SDK_PLATFORM` when it executes an action, and a replay is
  not an action. What gets recorded is the input to that expansion, not its
  result.

## Attributes

| attribute | default | meaning |
|---|---|---|
| `test` | — | the `rust_test` target to mutate |
| `jobs` | `1` | mutants built and tested concurrently within one shard |
| `timeout_multiplier` | `5` | mutant test timeout, as a multiple of the unmutated run |
| `env`, `data` | — | as on `rust_test` |

## Example

[`example/`](example/) is a working setup: `well_tested` catches every mutant,
`under_tested` deliberately leaves one alive.

```bash
bazel test //...
```

## Releasing

```bash
git tag v0.1.0 && git push origin v0.1.0
```

That runs the tests, builds the source archive, cuts the GitHub release, and
pushes the registry entry to the BCR fork. The job's last line is a URL to open
the registry pull request from. The one-time token and registry-fork setup it
needs is in [`.bcr/README.md`](.bcr/README.md).

## License

Apache 2.0, same as rules_rs.
