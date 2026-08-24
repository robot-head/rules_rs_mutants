"""Providers for cargo-mutants rules."""

CargoMutantsInfo = provider(
    doc = "Everything needed to replay a Rust test binary build once per mutant.",
    fields = {
        "inputs": "depset[File]: Every file the recorded command line reads, keyed by exec path.",
        "manifest": "File: Multiline args file describing the replay to cargo_mutants_runner.",
        "mutants_json": "File: `cargo-mutants --list --json` output for the crate under test.",
    },
)

CargoMutantsReplayInfo = provider(
    doc = """One recorded rustc invocation that can be replayed against mutated sources.

Produced for a target that is *not* itself the crate under mutation: the library
whose rlib the integration tests link, and each integration test binary that
links it. Unlike `CargoMutantsInfo` there is no mutant list -- these are rebuilt
in service of someone else's mutants, never enumerated for their own.""",
    fields = {
        "inputs": "depset[File]: Every file the recorded command line reads.",
        "manifest": "File: Multiline args file describing this replay.",
    },
)
