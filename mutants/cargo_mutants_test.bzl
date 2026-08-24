"""Mutation testing for Rust crates."""

load("@hermetic_launcher//launcher:lib.bzl", "launcher")
load("//mutants/private:aspect.bzl", _cargo_mutants_aspect = "cargo_mutants_aspect")
load("//mutants/private:providers.bzl", "CargoMutantsInfo", "CargoMutantsReplayInfo")

cargo_mutants_aspect = _cargo_mutants_aspect

def _declare_test_executable(ctx):
    name = ctx.label.name
    if ctx.target_platform_has_constraint(ctx.attr._windows_constraint[platform_common.ConstraintValueInfo]):
        name += ".exe"
    return ctx.actions.declare_file(name)

def _cargo_mutants_test_impl(ctx):
    info = ctx.attr.test[CargoMutantsInfo]

    # The library the integration tests link, recorded so a mutated rlib can be
    # produced, and one replay per integration binary that links it.
    library = ctx.attr.library[CargoMutantsReplayInfo] if ctx.attr.library else None
    integration = [test[CargoMutantsReplayInfo] for test in ctx.attr.integration_tests]
    if integration and not library:
        fail("integration_tests needs `library` set to the rust_library they link")

    embedded_args, transformed_args = launcher.args_from_entrypoint(ctx.executable._runner)
    embedded_args.extend([
        "@" + info.manifest.path,
        "--timeout-multiplier",
        str(ctx.attr.timeout_multiplier),
        "--jobs",
        str(ctx.attr.jobs),
    ])
    if library:
        embedded_args.extend(["--library-replay", library.manifest.path])
    for replay in integration:
        embedded_args.extend(["--integration-replay", replay.manifest.path])

    executable = _declare_test_executable(ctx)
    launcher.compile_stub(
        ctx = ctx,
        embedded_args = embedded_args,
        transformed_args = transformed_args,
        output_file = executable,
    )

    # The recorded command line is exec-root relative, so mirror exec paths into
    # the runfiles root; the runner chdirs there and replays it with no rewriting.
    runfiles = ctx.runfiles(
        files = [ctx.executable._runner] + ctx.files.data,
        root_symlinks = {
            file.path: file
            for replay in [info] + ([library] if library else []) + integration
            for file in replay.inputs.to_list()
        },
    ).merge_all(
        [ctx.attr.test[DefaultInfo].default_runfiles, ctx.attr._runner[DefaultInfo].default_runfiles] +
        [test[DefaultInfo].default_runfiles for test in ctx.attr.integration_tests] +
        [data[DefaultInfo].default_runfiles for data in ctx.attr.data],
    )

    environment = dict(ctx.attr.test[RunEnvironmentInfo].environment)
    environment.update(ctx.attr.env)

    return [
        DefaultInfo(executable = executable, runfiles = runfiles),
        RunEnvironmentInfo(
            environment = environment,
            inherited_environment = ctx.attr.test[RunEnvironmentInfo].inherited_environment,
        ),
    ]

# Configuration comes from `//mutants:config`, a label_flag pointing at a
# cargo-mutants config file (`exclude_globs`, `exclude_re`, and the rest):
#
#     build --@rules_rs_mutants//mutants:config=//:mutants_config
#
# Paths inside it are workspace paths -- the ones the results print -- so the
# same file serves a plain `cargo mutants` run.

cargo_mutants_test = rule(
    implementation = _cargo_mutants_test_impl,
    doc = """Rebuilds and re-runs a `rust_test` target once per mutant.

Mutants are enumerated by `cargo-mutants`, which you supply yourself:

```python
# MODULE.bazel
crate.annotation(crate = "cargo-mutants", gen_binaries = ["cargo-mutants"])
```

```
# .bazelrc
build --@rules_rs_mutants//mutants:cargo_mutants_binary=@crates//:cargo-mutants__cargo-mutants
```

By default only the test target's own `#[cfg(test)]` tests run against each
mutant. To let suites under `tests/` catch mutants too, name the library they
link and the suites themselves:

```python
cargo_mutants_test(
    name = "logql_mutants",
    test = ":logql_test",
    library = ":logql",
    integration_tests = [":parser_test", ":planner_test"],
)
```

Each mutant then rebuilds the library as an rlib, relinks every listed suite
against it, and runs them in order, stopping at the first failure. This matters
more than it sounds: a crate whose parser is covered entirely from `tests/`
reports almost every mutant as missed without it.

Sweeps are slow — every mutant is a fresh link plus a test run. Use `jobs` to
fan out within one machine and the standard `shard_count` attribute to fan out
across several.
""",
    attrs = {
        "data": attr.label_list(allow_files = True),
        "env": attr.string_dict(doc = "Environment variables set while running the mutation sweep."),
        "jobs": attr.int(
            default = 1,
            doc = """Mutants to build and test concurrently within one shard.

Each job gets its own scratch build tree, so raising this costs disk and
memory as well as CPU. To spread the work over several machines instead, set
`shard_count`.""",
        ),
        "library": attr.label(
            aspects = [_cargo_mutants_aspect],
            providers = [CargoMutantsReplayInfo],
            doc = """The `rust_library` the `integration_tests` link.

Recorded so each mutant can be rebuilt as an rlib for them to link against.
Required when `integration_tests` is set, ignored otherwise.""",
        ),
        "integration_tests": attr.label_list(
            aspects = [_cargo_mutants_aspect],
            providers = [CargoMutantsReplayInfo],
            doc = """`rust_test` targets under `tests/` to run against each mutant.

Listed explicitly rather than discovered, so a suite that needs a container or a
network fixture is not pulled into the sweep by accident, and so the cost of
adding one is visible at the call site.""",
        ),
        "test": attr.label(
            mandatory = True,
            aspects = [_cargo_mutants_aspect],
            providers = [CargoMutantsInfo],
            doc = "The `rust_test` target to mutate.",
        ),
        "timeout_multiplier": attr.int(
            default = 5,
            doc = "Mutant test timeout, as a multiple of the unmutated run's duration.",
        ),
        "_runner": attr.label(
            default = Label("//mutants/private:cargo_mutants_runner"),
            executable = True,
            cfg = "target",
        ),
        "_windows_constraint": attr.label(default = "@platforms//os:windows"),
    },
    test = True,
    toolchains = [
        launcher.finalizer_toolchain_type,
        launcher.template_toolchain_type,
    ],
)
