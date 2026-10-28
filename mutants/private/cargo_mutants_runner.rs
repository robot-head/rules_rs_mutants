//! Rebuilds and re-runs a Rust test binary once per mutant.
//!
//! The aspect records the exact `rustc --test` command line rules_rust would
//! run, in exec-root-relative form, and mirrors every input into the runfiles
//! root under its exec path. So the loop here is: chdir to the runfiles root,
//! swap the crate root and output path for copies under `$TEST_TMPDIR`, and
//! replay. A mutant that makes the tests fail (or hang) is killed; one that
//! leaves them passing is MISSED and fails the test.
//!
//! The aspect's `cargo_mutants_report` output group runs the same loop as a
//! build action instead. There is no runfiles tree to replay from, but the
//! action's working directory is the exec root, which is what the recorded
//! command line expects; the test's runfiles are staged from a manifest. Each
//! mutant's outcome goes to a JSON report, and a missed mutant does not fail
//! the action, since Bazel discards a failed action's outputs.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(50);
const MIN_TIMEOUT: Duration = Duration::from_secs(20);
const BASELINE_TIMEOUT: Duration = Duration::from_secs(900);

/// Replaces the region between `start` and `end` (1-based line/char column,
/// end exclusive) with `replacement`.
///
/// Ported from cargo-mutants' `Span::replace` so the offsets in `mutants.json`
/// mean here exactly what they mean there.
fn span_replace(
    source: &str,
    start: (usize, usize),
    end: (usize, usize),
    replacement: &str,
) -> String {
    let mut out = String::with_capacity(source.len() + replacement.len());
    let (mut line, mut column) = (1usize, 1usize);
    for c in source.chars() {
        if (line, column) == start {
            out.push_str(replacement);
        }
        if line < start.0
            || line > end.0
            || (line == start.0 && column < start.1)
            || (line == end.0 && column >= end.1)
        {
            out.push(c);
        }
        if c == '\n' {
            line += 1;
            column = 1;
        } else if c != '\r' {
            // A carriage return counts as part of the last column.
            column += 1;
        }
    }
    if (line, column) == start {
        out.push_str(replacement);
    }
    out
}

/// One recorded rustc command, replayed against the mutated tree. The library
/// under mutation is rebuilt as an rlib from this, and each integration binary
/// is relinked against that rlib.
struct ReplayManifest {
    process_wrapper: PathBuf,
    rustc_args: Vec<PathBuf>,
    env: PathBuf,
    crate_root: String,
    output: String,
    srcs: Vec<PathBuf>,
    compile_data: Vec<PathBuf>,
    test_args: Vec<String>,
    test_env: Vec<(String, String)>,
}

fn parse_replay_manifest(path: &Path) -> Result<ReplayManifest, String> {
    let text = fs::read_to_string(path)
        .map_err(|err| format!("failed to read replay manifest {}: {err}", path.display()))?;
    let mut process_wrapper = None;
    let mut rustc_args = Vec::new();
    let mut env_file = None;
    let mut crate_root = None;
    let mut output = None;
    let mut srcs = Vec::new();
    let mut compile_data = Vec::new();
    let mut test_args = Vec::new();
    let mut test_env = Vec::new();

    let mut lines = text.lines();
    while let Some(flag) = lines.next() {
        let value = lines
            .next()
            .ok_or_else(|| format!("missing value for {flag} in {}", path.display()))?;
        match flag {
            "--process-wrapper" => process_wrapper = Some(PathBuf::from(value)),
            "--rustc-args" => rustc_args.push(PathBuf::from(value)),
            "--env" => env_file = Some(PathBuf::from(value)),
            "--crate-root" => crate_root = Some(value.to_owned()),
            "--output" => output = Some(value.to_owned()),
            "--src" => srcs.push(PathBuf::from(value)),
            "--compile-data" => compile_data.push(PathBuf::from(value)),
            "--test-arg" => test_args.push(value.to_owned()),
            "--test-env" => {
                let (key, value) = value
                    .split_once('=')
                    .ok_or_else(|| format!("malformed --test-env {value:?}"))?;
                test_env.push((key.to_owned(), value.to_owned()));
            }
            other => return Err(format!("unknown flag {other} in {}", path.display())),
        }
    }

    Ok(ReplayManifest {
        process_wrapper: process_wrapper.ok_or("missing --process-wrapper")?,
        rustc_args,
        env: env_file.ok_or("missing --env")?,
        crate_root: crate_root.ok_or("missing --crate-root")?,
        output: output.ok_or("missing --output")?,
        srcs,
        compile_data,
        test_args,
        test_env,
    })
}

struct Manifest {
    process_wrapper: PathBuf,
    mutants: PathBuf,
    rustc_args: Vec<PathBuf>,
    env: PathBuf,
    crate_root: String,
    output: String,
    srcs: Vec<PathBuf>,
    compile_data: Vec<PathBuf>,
    test_args: Vec<String>,
    test_env: Vec<(String, String)>,
    timeout_multiplier: u32,
    jobs: usize,
    library: Option<ReplayManifest>,
    integration: Vec<ReplayManifest>,
    /// Where to write every tested mutant's outcome as JSON.
    report: Option<PathBuf>,
    /// The lines a mutant must touch to be tested; see `parse_changed_lines`.
    changed_lines: Option<ChangedLines>,
    /// Exit non-zero when a mutant is missed. A build action passes false.
    fail_on_missed: bool,
    /// A build action's runfiles, as `<runfiles path> <exec path>` lines, and
    /// the workspace name the test runs in below them.
    runfiles: Option<PathBuf>,
    workspace: Option<String>,
}

fn parse_manifest() -> Result<Manifest, String> {
    let mut argv: Vec<String> = Vec::new();
    for arg in env::args().skip(1) {
        match arg.strip_prefix('@') {
            Some(path) => {
                let text = fs::read_to_string(path)
                    .map_err(|err| format!("failed to read manifest {path}: {err}"))?;
                argv.extend(text.lines().map(str::to_owned));
            }
            None => argv.push(arg),
        }
    }

    let mut process_wrapper = None;
    let mut mutants = None;
    let mut rustc_args = Vec::new();
    let mut env_file = None;
    let mut crate_root = None;
    let mut output = None;
    let mut srcs = Vec::new();
    let mut compile_data = Vec::new();
    let mut test_args = Vec::new();
    let mut test_env = Vec::new();
    let mut timeout_multiplier = 5;
    let mut jobs = 1;
    let mut library = None;
    let mut integration = Vec::new();
    let mut report = None;
    let mut changed_lines = None;
    let mut fail_on_missed = true;
    let mut runfiles = None;
    let mut workspace = None;

    let mut argv = argv.into_iter();
    while let Some(flag) = argv.next() {
        let value = argv
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--process-wrapper" => process_wrapper = Some(PathBuf::from(value)),
            "--mutants" => mutants = Some(PathBuf::from(value)),
            "--rustc-args" => rustc_args.push(PathBuf::from(value)),
            "--env" => env_file = Some(PathBuf::from(value)),
            "--crate-root" => crate_root = Some(value),
            "--output" => output = Some(value),
            "--src" => srcs.push(PathBuf::from(value)),
            "--compile-data" => compile_data.push(PathBuf::from(value)),
            "--test-arg" => test_args.push(value),
            "--test-env" => {
                let (key, value) = value
                    .split_once('=')
                    .ok_or_else(|| format!("malformed --test-env {value:?}"))?;
                test_env.push((key.to_owned(), value.to_owned()));
            }
            "--report" => report = Some(PathBuf::from(value)),
            "--changed-lines" => {
                let text = fs::read_to_string(&value)
                    .map_err(|err| format!("failed to read --changed-lines {value}: {err}"))?;
                changed_lines = parse_changed_lines(&text)?;
            }
            "--fail-on-missed" => {
                fail_on_missed = value
                    .parse()
                    .map_err(|err| format!("bad --fail-on-missed {value}: {err}"))?
            }
            "--runfiles" => runfiles = Some(PathBuf::from(value)),
            "--workspace" => workspace = Some(value),
            "--timeout-multiplier" => {
                timeout_multiplier = value
                    .parse()
                    .map_err(|err| format!("bad --timeout-multiplier {value}: {err}"))?
            }
            "--jobs" => {
                jobs = value
                    .parse()
                    .map_err(|err| format!("bad --jobs {value}: {err}"))?
            }
            // One file listing the replay manifests, rather than a flag pair
            // per suite: the launcher stub embeds at most ten arguments.
            "--extras" => {
                let mut lines = read_lines(Path::new(&value))?.into_iter();
                while let Some(flag) = lines.next() {
                    let path = lines
                        .next()
                        .ok_or_else(|| format!("missing value for {flag} in {value}"))?;
                    match flag.as_str() {
                        "--library-replay" => {
                            library = Some(parse_replay_manifest(Path::new(&path))?)
                        }
                        "--integration-replay" => {
                            integration.push(parse_replay_manifest(Path::new(&path))?)
                        }
                        other => return Err(format!("unknown flag {other} in {value}")),
                    }
                }
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }

    Ok(Manifest {
        process_wrapper: process_wrapper.ok_or("missing --process-wrapper")?,
        mutants: mutants.ok_or("missing --mutants")?,
        rustc_args,
        env: env_file.ok_or("missing --env")?,
        crate_root: crate_root.ok_or("missing --crate-root")?,
        output: output.ok_or("missing --output")?,
        srcs,
        compile_data,
        test_args,
        test_env,
        timeout_multiplier: timeout_multiplier.max(1),
        jobs: jobs.max(1),
        library,
        integration,
        report,
        changed_lines,
        fail_on_missed,
        runfiles,
        workspace,
    })
}

/// Inclusive 1-based line ranges, per file.
type ChangedLines = BTreeMap<String, Vec<(usize, usize)>>;

/// Parses `{"files": {"<path>": [[start, end], ...]}}`, the lines a change
/// touches. Paths are the ones mutants are reported under: exec paths, which
/// for a source file of the main repository are its workspace paths. Blank text
/// means no filter, so a build setting left at its default tests every mutant.
fn parse_changed_lines(text: &str) -> Result<Option<ChangedLines>, String> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|err| format!("bad changed lines: {err}"))?;
    let files = value["files"]
        .as_object()
        .ok_or("changed lines need a \"files\" object")?;
    let mut out = ChangedLines::new();
    for (path, ranges) in files {
        let ranges = ranges
            .as_array()
            .ok_or_else(|| format!("changed lines for {path} are not a list"))?;
        let entry = out.entry(path.clone()).or_default();
        for range in ranges {
            let line = |i: usize| {
                range[i]
                    .as_u64()
                    .map(|n| n as usize)
                    .ok_or_else(|| format!("bad changed line range {range} for {path}"))
            };
            entry.push((line(0)?, line(1)?));
        }
    }
    Ok(Some(out))
}

/// Whether a mutant's span touches a changed line. A mutant that replaces a
/// whole function body starts on the signature, so a change anywhere in the
/// body has to count, not only one on the mutant's first line.
fn touches_changed_lines(changed: &ChangedLines, mutant: &serde_json::Value) -> bool {
    let (Some(file), Some(start), Some(end)) = (
        mutant["file"].as_str(),
        mutant["span"]["start"]["line"].as_u64(),
        mutant["span"]["end"]["line"].as_u64(),
    ) else {
        return false;
    };
    let (start, end) = (start as usize, end as usize);
    changed
        .get(file)
        .is_some_and(|ranges| ranges.iter().any(|&(from, to)| from <= end && start <= to))
}

/// Bazel's native test sharding, as `(index, total)`.
///
/// Creating the status file is how a test runner tells Bazel it honoured the
/// split; without it Bazel fails the target rather than silently running the
/// whole set in every shard.
fn shard() -> Result<(usize, usize), String> {
    let total = match env::var("TEST_TOTAL_SHARDS") {
        Ok(total) => total
            .parse::<usize>()
            .map_err(|err| format!("bad TEST_TOTAL_SHARDS {total:?}: {err}"))?,
        Err(_) => return Ok((0, 1)),
    };
    if total == 0 {
        return Ok((0, 1));
    }
    let index = env::var("TEST_SHARD_INDEX")
        .map_err(|_| "TEST_TOTAL_SHARDS is set but TEST_SHARD_INDEX is not".to_owned())?;
    let index = index
        .parse::<usize>()
        .map_err(|err| format!("bad TEST_SHARD_INDEX {index:?}: {err}"))?;
    if index >= total {
        return Err(format!(
            "TEST_SHARD_INDEX {index} is out of range for {total} shards"
        ));
    }
    if let Some(status) = env::var_os("TEST_SHARD_STATUS_FILE") {
        write(Path::new(&status), "")?;
    }
    Ok((index, total))
}

fn read_lines(path: &Path) -> Result<Vec<String>, String> {
    Ok(fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?
        .lines()
        .map(str::to_owned)
        .collect())
}

/// cargo-mutants normalizes line endings before computing spans, so we must too.
fn read_source(path: &Path) -> Result<String, String> {
    Ok(fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?
        .replace("\r\n", "\n"))
}

fn write(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    fs::write(path, contents).map_err(|err| format!("failed to write {}: {err}", path.display()))
}

fn copy(path: &Path, dest: &Path) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    // Not fs::copy, which carries the source's mode over: Bazel inputs are
    // read-only under remote execution, and the scratch copies are written to.
    let bytes =
        fs::read(path).map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    fs::write(dest, bytes).map_err(|err| {
        format!(
            "failed to copy {} to {}: {err}",
            path.display(),
            dest.display()
        )
    })
}

fn wait_with_timeout(mut child: Child, timeout: Duration) -> Result<Option<i32>, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(status.code().unwrap_or(1))),
            Ok(None) => {}
            Err(err) => return Err(format!("failed to wait for child: {err}")),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(POLL);
    }
}

struct Replay {
    /// False for the library stage: it produces an rlib for the integration
    /// binaries to link, and there is nothing to execute.
    runnable: bool,
    process_wrapper: PathBuf,
    argv: Vec<String>,
    env: Vec<(String, String)>,
    binary: PathBuf,
    test_args: Vec<String>,
    test_env: Vec<(String, String)>,
    test_working_dir: PathBuf,
}

impl Replay {
    /// Compiles the current contents of the scratch tree. A failure is not an
    /// error: a mutant that does not compile is what cargo-mutants calls
    /// "unviable". `Err` is reserved for not being able to run rustc at all.
    fn build(&self) -> Result<Result<(), String>, String> {
        let out = Command::new(&self.process_wrapper)
            .args(&self.argv)
            .env_clear()
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .output()
            .map_err(|err| format!("failed to run {}: {err}", self.process_wrapper.display()))?;
        Ok(if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        })
    }

    fn run_tests(&self, timeout: Duration, verbose: bool) -> Result<Option<i32>, String> {
        let mut command = Command::new(&self.binary);
        command
            .args(&self.test_args)
            .envs(self.test_env.iter().map(|(key, value)| (key, value)))
            .current_dir(&self.test_working_dir);
        if verbose {
            command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        } else {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
        let child = command
            .spawn()
            .map_err(|err| format!("failed to run {}: {err}", self.binary.display()))?;
        wait_with_timeout(child, timeout)
    }

    fn remove_binary(&self) -> Result<(), String> {
        match fs::remove_file(&self.binary) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(format!("failed to remove {}: {err}", self.binary.display())),
        }
    }
}

fn substitute(value: &str, from: &str, to: &Path) -> String {
    value.replace(from, &to.to_string_lossy())
}

fn link(target: &Path, dest: &Path) -> Result<(), String> {
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(target, dest);
    #[cfg(windows)]
    let result = fs::copy(target, dest).map(|_| ());
    result.map_err(|err| format!("failed to link {}: {err}", dest.display()))
}

/// Returns the directory to replay from — one whose layout mirrors exec paths.
///
/// Normally that is the runfiles tree, but `--nobuild_runfile_links` leaves it
/// unmaterialized, so rebuild it from the manifest. Links suffice: every write
/// the replay makes is redirected into `scratch`.
fn replay_root(scratch: &Path) -> Result<PathBuf, String> {
    let srcdir = env::var_os("TEST_SRCDIR")
        .map(PathBuf::from)
        .ok_or("TEST_SRCDIR is not set; cargo_mutants_test must be run as a test")?;
    if srcdir.is_dir() {
        return Ok(srcdir);
    }

    let manifest = env::var_os("RUNFILES_MANIFEST_FILE")
        .map(PathBuf::from)
        .ok_or("neither a runfiles tree nor RUNFILES_MANIFEST_FILE is available")?;
    let root = scratch.join("runfiles");
    for entry in read_lines(&manifest)? {
        let (rel, target) = entry.split_once(' ').unwrap_or((entry.as_str(), ""));
        let dest = root.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
        }
        if target.is_empty() {
            fs::write(&dest, "")
                .map_err(|err| format!("failed to write {}: {err}", dest.display()))?;
        } else {
            link(Path::new(target), &dest)?;
        }
    }
    Ok(root)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Caught,
    /// Caught by the time limit rather than by a failing test.
    Timeout,
    Unviable,
    Missed,
}

impl Outcome {
    fn name(self) -> &'static str {
        match self {
            Outcome::Caught => "caught",
            Outcome::Timeout => "timeout",
            Outcome::Unviable => "unviable",
            Outcome::Missed => "missed",
        }
    }
}

/// cargo-mutants' description of a mutant without its location, e.g.
/// `replace == with != in triple`.
fn description(mutant: &serde_json::Value) -> String {
    let name = mutant["name"].as_str().unwrap_or("");
    let location = format!(
        "{}:{}:{}: ",
        mutant["file"].as_str().unwrap_or(""),
        mutant["span"]["start"]["line"],
        mutant["span"]["start"]["column"],
    );
    name.strip_prefix(&location).unwrap_or(name).to_owned()
}

/// How a missed mutant is printed.
fn missed_label(mutant: &serde_json::Value) -> String {
    format!(
        "{}:{}:{}: replace {} with {}",
        mutant["file"].as_str().unwrap_or("?"),
        mutant["span"]["start"]["line"],
        mutant["span"]["start"]["column"],
        mutant["function"]["function_name"].as_str().unwrap_or("?"),
        mutant["replacement"].as_str().unwrap_or("?"),
    )
}

/// The `--report` JSON: every tested mutant and its outcome, sorted by
/// location so that the file is the same however the jobs interleaved.
fn report_json(results: &[(&serde_json::Value, Outcome)]) -> serde_json::Value {
    let mut mutants: Vec<serde_json::Value> = results
        .iter()
        .map(|(mutant, outcome)| {
            serde_json::json!({
                "file": mutant["file"],
                "line": mutant["span"]["start"]["line"],
                "column": mutant["span"]["start"]["column"],
                "genre": mutant["genre"],
                "description": description(mutant),
                "outcome": outcome.name(),
            })
        })
        .collect();
    mutants.sort_by_key(|m| {
        (
            m["file"].as_str().unwrap_or("").to_owned(),
            m["line"].as_u64(),
            m["column"].as_u64(),
            m["description"].as_str().unwrap_or("").to_owned(),
        )
    });
    serde_json::json!({ "mutants": mutants })
}

/// Prepares one replay sandbox: a private copy of the sources, plus a command
/// line whose reads and writes are redirected into it.
///
/// Concurrent jobs cannot share one — they patch the same files and link to the
/// same output path.
fn prepare(
    scratch: &Path,
    manifest: &Manifest,
    args: &[String],
    environment: &[(String, String)],
    originals: &BTreeMap<PathBuf, String>,
    test_working_dir: &Path,
    test_env: &[(String, String)],
) -> Result<Replay, String> {
    for (src, text) in originals {
        write(&scratch.join(src), text)?;
    }
    for data in &manifest.compile_data {
        if !originals.contains_key(data) {
            copy(data, &scratch.join(data))?;
        }
    }
    let crate_root = scratch.join(&manifest.crate_root);
    let binary = scratch.join("mutant_test_binary");
    let mut argv = Vec::with_capacity(args.len());
    for arg in args {
        // rustc drops its intermediate `.rcgu.o` files in `--out-dir` and
        // deletes them after linking, so jobs sharing one clobber each other.
        if let Some(dir) = arg.strip_prefix("--out-dir=") {
            let dir = scratch.join(dir);
            fs::create_dir_all(&dir)
                .map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
            argv.push(format!("--out-dir={}", dir.display()));
            continue;
        }
        let arg = substitute(arg, &manifest.output, &binary);
        argv.push(substitute(&arg, &manifest.crate_root, &crate_root));
    }
    Ok(Replay {
        runnable: true,
        test_env: test_env.to_vec(),
        process_wrapper: manifest.process_wrapper.clone(),
        argv,
        env: environment.to_vec(),
        binary,
        test_args: manifest.test_args.clone(),
        test_working_dir: test_working_dir.to_path_buf(),
    })
}

/// Loads one recorded rustc command, redirected into `scratch`.
///
/// `relink` is the library's original rlib path paired with the one this
/// scratch tree builds. Every recorded `--extern name=<original>` is rewritten
/// to the scratch copy; without that the integration binaries would link the
/// *unmutated* library and report every mutant as caught, which is worse than
/// not running them at all.
fn prepare_replay(
    scratch: &Path,
    replay: &ReplayManifest,
    runfiles_cwd: &str,
    originals: &BTreeMap<PathBuf, String>,
    test_working_dir: &Path,
    test_env: &[(String, String)],
    relink: Option<(&str, &Path)>,
) -> Result<Replay, String> {
    for src in &replay.srcs {
        // Sources of the crate under mutation are already in scratch and must
        // not be overwritten with their originals; anything else is this
        // target's own and has to be copied in.
        if !originals.contains_key(src) {
            copy(src, &scratch.join(src))?;
        }
    }
    for data in &replay.compile_data {
        if !originals.contains_key(data) {
            copy(data, &scratch.join(data))?;
        }
    }

    let mut args = Vec::new();
    for args_file in &replay.rustc_args {
        args.extend(read_lines(args_file)?);
    }
    let mut environment: Vec<(String, String)> = Vec::new();
    for line in read_lines(&replay.env)? {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("malformed env entry {line:?}"))?;
        let value = value
            .replace("${pwd}", runfiles_cwd)
            .replace("${exec_root}", runfiles_cwd)
            .replace("${output_base}", runfiles_cwd);
        environment.push((key.to_owned(), value));
    }
    for key in ["PATH", "HOME", "SYSTEMROOT", "TMPDIR", "TEST_TMPDIR"] {
        if let Some(value) = env::var_os(key) {
            environment.push((key.to_owned(), value.to_string_lossy().into_owned()));
        }
    }

    let crate_root = scratch.join(&replay.crate_root);
    let output = scratch.join(&replay.output);
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }

    // A library is written by `--out-dir` plus the crate name, so rustc names
    // it `libfoo.rlib` while Bazel declared `libfoo-1234.rlib` and the extern
    // that links it names the hash. Pin the path with `-o` instead of letting
    // rustc derive it, so the two agree.
    let needs_explicit_output = !args.iter().any(|arg| arg == "-o" || arg.starts_with("-o="))
        && args.iter().any(|arg| arg.starts_with("--crate-type=rlib"));

    let mut argv = Vec::with_capacity(args.len() + 2);
    for arg in &args {
        if let Some(dir) = arg.strip_prefix("--out-dir=") {
            let dir = scratch.join(dir);
            fs::create_dir_all(&dir)
                .map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
            argv.push(format!("--out-dir={}", dir.display()));
            continue;
        }
        let arg = substitute(arg, &replay.output, &output);
        let arg = substitute(&arg, &replay.crate_root, &crate_root);
        argv.push(match relink {
            Some((from, to)) => substitute(&arg, from, to),
            None => arg,
        });
    }
    if needs_explicit_output {
        argv.push("-o".to_owned());
        argv.push(output.display().to_string());
    }

    Ok(Replay {
        runnable: relink.is_some(),
        // The suite's own `env` last, so that it wins.
        test_env: test_env.iter().chain(&replay.test_env).cloned().collect(),
        process_wrapper: replay.process_wrapper.clone(),
        argv,
        env: environment,
        binary: output,
        test_args: replay.test_args.clone(),
        test_working_dir: test_working_dir.to_path_buf(),
    })
}

/// What a test run's exit says about a mutant: `None` when the tests passed.
fn test_outcome(exit: Option<i32>) -> Option<Outcome> {
    match exit {
        Some(0) => None,
        Some(_) => Some(Outcome::Caught),
        None => Some(Outcome::Timeout),
    }
}

fn evaluate_inner(replay: &Replay, extra: &[Replay], timeout: Duration) -> Result<Outcome, String> {
    let built = replay.build()?;
    if built.is_err() {
        return Ok(Outcome::Unviable);
    }
    // The unit tests run first and the suites only if they pass: most mutants
    // die here, before an integration binary needs to be relinked at all.
    if let Some(outcome) = test_outcome(replay.run_tests(timeout, false)?) {
        return Ok(outcome);
    }

    for stage in extra {
        if stage.build()?.is_err() {
            return Ok(Outcome::Unviable);
        }
        if stage.runnable {
            if let Some(outcome) = test_outcome(stage.run_tests(timeout, false)?) {
                return Ok(outcome);
            }
            stage.remove_binary()?;
        }
    }
    // Nothing failed, so nothing noticed the change.
    Ok(Outcome::Missed)
}

fn evaluate(
    replay: &Replay,
    extra: &[Replay],
    scratch: &Path,
    originals: &BTreeMap<PathBuf, String>,
    mutant: &serde_json::Value,
    timeout: Duration,
) -> Result<Outcome, String> {
    // `mutants_list` keeps Bazel exec paths inside its synthetic tree, so a
    // mutant's `file` is already the path this scratch tree uses.
    let file = mutant["file"]
        .as_str()
        .ok_or_else(|| format!("mutant has no file field: {mutant}"))?;
    let replacement = mutant["replacement"]
        .as_str()
        .ok_or_else(|| format!("mutant has no replacement: {mutant}"))?;
    let at = |end: &str, part: &str| -> Result<usize, String> {
        mutant["span"][end][part]
            .as_u64()
            .map(|n| n as usize)
            .ok_or_else(|| format!("mutant has no span.{end}.{part}: {mutant}"))
    };
    let start = (at("start", "line")?, at("start", "column")?);
    let end = (at("end", "line")?, at("end", "column")?);

    let source = originals
        .get(Path::new(file))
        .ok_or_else(|| format!("mutant names {file}, which is not a source of this crate"))?;
    let path = scratch.join(file);
    write(&path, &span_replace(source, start, end, replacement))?;
    let outcome = evaluate_inner(replay, extra, timeout);
    write(&path, source)?;
    replay.remove_binary()?;
    for stage in extra {
        stage.remove_binary()?;
    }
    outcome
}

/// Rebuilds a build action's runfiles tree under `root` from `--runfiles`, a
/// file of `<runfiles path> <exec path>` lines, and returns the environment a
/// test expects to find it through.
///
/// A test that reads a data dependency looks it up in its runfiles, so without
/// this every mutant would die on a missing file and count as caught.
fn stage_runfiles(
    listing: &Path,
    root: &Path,
    exec_root: &Path,
) -> Result<Vec<(String, String)>, String> {
    for entry in read_lines(listing)? {
        let (rel, target) = entry
            .split_once(' ')
            .ok_or_else(|| format!("malformed runfiles entry {entry:?}"))?;
        let dest = root.join(rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
        }
        if dest.symlink_metadata().is_err() {
            link(&exec_root.join(target), &dest)?;
        }
    }
    let root = root.to_string_lossy().into_owned();
    Ok(vec![
        ("RUNFILES_DIR".to_owned(), root.clone()),
        ("TEST_SRCDIR".to_owned(), root),
    ])
}

fn run() -> Result<(), String> {
    let (shard_index, shard_total) = shard()?;
    // A build action has no TEST_TMPDIR. Its sandbox gives each action a
    // private temporary directory, and the pid keeps unsandboxed ones apart.
    let scratch = match env::var_os("TEST_TMPDIR") {
        Some(dir) => PathBuf::from(dir).join("cargo_mutants"),
        None => env::temp_dir().join(format!("cargo_mutants.{}", std::process::id())),
    };
    let _ = fs::remove_dir_all(&scratch);

    // As a build action the working directory is already the exec root, which
    // the recorded command line is relative to. As a test it is the runfiles
    // tree, which mirrors the exec root.
    let (runfiles, test_working_dir) = if env::var_os("TEST_SRCDIR").is_none() {
        let exec_root = env::current_dir()
            .map_err(|err| format!("failed to read the working directory: {err}"))?;
        (exec_root, None)
    } else {
        let runfiles = replay_root(&scratch)?;
        let working_dir = env::var_os("TEST_WORKSPACE")
            .map(|workspace| runfiles.join(workspace))
            .unwrap_or_else(|| runfiles.clone());
        (runfiles, Some(working_dir))
    };
    env::set_current_dir(&runfiles)
        .map_err(|err| format!("failed to enter {}: {err}", runfiles.display()))?;

    // Parsed only now: the `@manifest` argument is itself an exec-root path.
    let manifest = parse_manifest()?;

    let mut test_env = Vec::new();
    let test_working_dir = match (test_working_dir, &manifest.runfiles) {
        (Some(dir), _) => dir,
        (None, Some(listing)) => {
            let root = scratch.join("runfiles");
            test_env = stage_runfiles(listing, &root, &runfiles)?;
            let tmp = scratch.join("test_tmp");
            fs::create_dir_all(&tmp)
                .map_err(|err| format!("failed to create {}: {err}", tmp.display()))?;
            test_env.push(("TEST_TMPDIR".to_owned(), tmp.to_string_lossy().into_owned()));
            match &manifest.workspace {
                Some(workspace) => {
                    test_env.push(("TEST_WORKSPACE".to_owned(), workspace.clone()));
                    root.join(workspace)
                }
                None => root,
            }
        }
        (None, None) => {
            return Err(
                "run as a build action without --runfiles; the tests could not find their data"
                    .to_owned(),
            );
        }
    };
    fs::create_dir_all(&test_working_dir)
        .map_err(|err| format!("failed to create {}: {err}", test_working_dir.display()))?;
    test_env.extend(manifest.test_env.iter().cloned());

    let mut originals: BTreeMap<PathBuf, String> = BTreeMap::new();
    for src in &manifest.srcs {
        originals.insert(src.clone(), read_source(src)?);
    }

    let mut args = Vec::new();
    for args_file in &manifest.rustc_args {
        args.extend(read_lines(args_file)?);
    }

    // rules_rust relies on process_wrapper to expand these; it only sees the
    // ones on the command line, so expand the recorded environment here.
    let cwd = runfiles.to_string_lossy().into_owned();
    let mut environment: Vec<(String, String)> = Vec::new();
    for line in read_lines(&manifest.env)? {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("malformed env entry {line:?}"))?;
        let value = value
            .replace("${pwd}", &cwd)
            .replace("${exec_root}", &cwd)
            .replace("${output_base}", &cwd);
        environment.push((key.to_owned(), value));
    }
    for key in ["PATH", "HOME", "SYSTEMROOT", "TMPDIR", "TEST_TMPDIR"] {
        if let Some(value) = env::var_os(key) {
            environment.push((key.to_owned(), value.to_string_lossy().into_owned()));
        }
    }

    let mutants: serde_json::Value = serde_json::from_str(&read_source(&manifest.mutants)?)
        .map_err(|err| format!("failed to parse {}: {err}", manifest.mutants.display()))?;
    let mutants = mutants
        .as_array()
        .ok_or("expected `cargo-mutants --list --json` to emit an array")?;
    // Filtered before sharding, so the shards split only the mutants that run.
    let work: Vec<&serde_json::Value> = mutants
        .iter()
        .filter(|mutant| {
            manifest
                .changed_lines
                .as_ref()
                .is_none_or(|changed| touches_changed_lines(changed, mutant))
        })
        .skip(shard_index)
        .step_by(shard_total)
        .collect();
    if manifest.changed_lines.is_some() {
        println!(
            "{} of {} mutants touch the changed lines",
            work.len(),
            mutants.len()
        );
        // Nothing to test, so nothing to check the build or tests for either.
        if work.is_empty() {
            return finish(&manifest, &[], 0);
        }
    }

    let jobs = manifest.jobs.min(work.len().max(1));
    let scratches: Vec<PathBuf> = (0..jobs).map(|job| scratch.join(job.to_string())).collect();
    let replays = scratches
        .iter()
        .map(|scratch| {
            prepare(
                scratch,
                &manifest,
                &args,
                &environment,
                &originals,
                &test_working_dir,
                &test_env,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Per job: the mutated library as an rlib, then every integration binary
    // relinked against it. Empty unless `integration_tests` was set.
    let extras: Vec<Vec<Replay>> = scratches
        .iter()
        .map(|scratch| -> Result<Vec<Replay>, String> {
            let Some(library) = &manifest.library else {
                return Ok(Vec::new());
            };
            let mut stages = vec![prepare_replay(
                scratch,
                library,
                &cwd,
                &originals,
                &test_working_dir,
                &test_env,
                None,
            )?];
            let rlib = scratch.join(&library.output);
            for test in &manifest.integration {
                stages.push(prepare_replay(
                    scratch,
                    test,
                    &cwd,
                    &originals,
                    &test_working_dir,
                    &test_env,
                    Some((library.output.as_str(), rlib.as_path())),
                )?);
            }
            Ok(stages)
        })
        .collect::<Result<Vec<_>, _>>()?;

    if let Err(stderr) = replays[0].build()? {
        return Err(format!("the unmutated crate failed to build:\n{stderr}"));
    }
    let started = Instant::now();
    match replays[0].run_tests(BASELINE_TIMEOUT, true)? {
        Some(0) => {}
        _ => return Err("the unmutated tests do not pass; fix them first".to_owned()),
    }
    let mut baseline = started.elapsed();

    // Every integration stage is built, and every runnable one is also run
    // unmutated. Both halves matter. A suite that already fails would fail
    // against every mutant too, and each would be recorded as caught -- a
    // perfect score reported by a broken sweep. And the timeout is derived from
    // the slowest baseline rather than the unit binary's alone, because a suite
    // that legitimately runs longer than the unit tests would otherwise be
    // killed on every mutant and counted as catching it.
    for stage in extras.first().into_iter().flatten() {
        if let Err(stderr) = stage.build()? {
            return Err(format!(
                "an unmutated integration stage failed to build:\n{stderr}"
            ));
        }
        if !stage.runnable {
            continue;
        }
        let started = Instant::now();
        match stage.run_tests(BASELINE_TIMEOUT, true)? {
            Some(0) => {}
            _ => {
                return Err(format!(
                    "the unmutated integration suite {} does not pass; fix it first",
                    stage.binary.display()
                ));
            }
        }
        baseline = baseline.max(started.elapsed());
        stage.remove_binary()?;
    }
    for stage in extras.first().into_iter().flatten() {
        stage.remove_binary()?;
    }
    let timeout = (baseline * manifest.timeout_multiplier).max(MIN_TIMEOUT);

    let next = AtomicUsize::new(0);
    let outcomes: Mutex<Vec<(&serde_json::Value, Outcome)>> = Mutex::new(Vec::new());
    let failure: Mutex<Option<String>> = Mutex::new(None);
    std::thread::scope(|scope| {
        for ((replay, extra), scratch) in replays.iter().zip(&extras).zip(&scratches) {
            let (next, outcomes, failure) = (&next, &outcomes, &failure);
            let (work, originals) = (&work, &originals);
            scope.spawn(move || {
                while failure.lock().unwrap().is_none() {
                    let Some(mutant) = work.get(next.fetch_add(1, Ordering::Relaxed)) else {
                        break;
                    };
                    match evaluate(replay, extra, scratch, originals, mutant, timeout) {
                        Ok(outcome) => outcomes.lock().unwrap().push((*mutant, outcome)),
                        Err(err) => *failure.lock().unwrap() = Some(err),
                    }
                }
            });
        }
    });
    if let Some(err) = failure.into_inner().unwrap() {
        return Err(err);
    }
    let _ = fs::remove_dir_all(&scratch);
    finish(&manifest, &outcomes.into_inner().unwrap(), work.len())
}

/// Prints the summary, writes `--report`, and decides the exit.
fn finish(
    manifest: &Manifest,
    results: &[(&serde_json::Value, Outcome)],
    total: usize,
) -> Result<(), String> {
    let count = |want: Outcome| results.iter().filter(|(_, o)| *o == want).count();
    // Jobs finish out of order; sorted so the log is stable.
    let mut missed: Vec<String> = results
        .iter()
        .filter(|(_, outcome)| *outcome == Outcome::Missed)
        .map(|(mutant, _)| missed_label(mutant))
        .collect();
    missed.sort();

    println!(
        "{total} mutants: {} caught, {} missed, {} unviable",
        count(Outcome::Caught) + count(Outcome::Timeout),
        missed.len(),
        count(Outcome::Unviable),
    );
    for label in &missed {
        println!("MISSED {label}");
    }
    if let Some(report) = &manifest.report {
        let text = serde_json::to_string_pretty(&report_json(results))
            .map_err(|err| format!("failed to encode the report: {err}"))?;
        write(report, &(text + "\n"))?;
    }
    if missed.is_empty() || !manifest.fail_on_missed {
        Ok(())
    } else {
        Err(format!("{} mutants survived", missed.len()))
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("cargo_mutants: {err}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::span_replace;

    #[test]
    fn replaces_within_a_line() {
        assert_eq!(
            span_replace("fn a() -> i32 { 1 }", (1, 15), (1, 20), "{ 0 }"),
            "fn a() -> i32 { 0 }"
        );
    }

    #[test]
    fn replaces_across_lines() {
        let source = "fn a() {\n    body();\n}\n";
        assert_eq!(span_replace(source, (1, 8), (3, 2), "{}"), "fn a() {}\n");
    }

    #[test]
    fn replaces_at_end_of_input() {
        assert_eq!(span_replace("ab", (1, 3), (1, 3), "c"), "abc");
    }

    #[test]
    fn copies_compile_data_as_bytes() {
        let root =
            std::env::temp_dir().join(format!("cargo_mutants_runner_test.{}", std::process::id()));
        let source = root.join("source");
        let dest = root.join("dest");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&source, [0xff, 0x00, b'\n']).unwrap();
        super::copy(&source, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), [0xff, 0x00, b'\n']);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn mutant(file: &str, start: u64, end: u64) -> serde_json::Value {
        serde_json::json!({
            "name": format!("{file}:{start}:5: replace triple -> i32 with 0"),
            "file": file,
            "function": {"function_name": "triple"},
            "span": {"start": {"line": start, "column": 5}, "end": {"line": end, "column": 6}},
            "replacement": "0",
            "genre": "FnValue",
        })
    }

    #[test]
    fn blank_changed_lines_mean_no_filter() {
        assert_eq!(super::parse_changed_lines(" \n").unwrap(), None);
    }

    #[test]
    fn parses_changed_lines() {
        let changed = super::parse_changed_lines(r#"{"files": {"a.rs": [[1, 3], [9, 9]]}}"#)
            .unwrap()
            .unwrap();
        assert_eq!(changed["a.rs"], vec![(1, 3), (9, 9)]);
    }

    #[test]
    fn rejects_malformed_changed_lines() {
        for text in [
            "{",
            r#"{"files": []}"#,
            r#"{"files": {"a.rs": [["x", 1]]}}"#,
        ] {
            assert!(super::parse_changed_lines(text).is_err(), "{text}");
        }
    }

    #[test]
    fn a_change_anywhere_in_the_span_counts() {
        let changed = super::parse_changed_lines(r#"{"files": {"a.rs": [[5, 5]]}}"#)
            .unwrap()
            .unwrap();
        // A function body replacement starts on the signature, line 3.
        assert!(super::touches_changed_lines(
            &changed,
            &mutant("a.rs", 3, 7)
        ));
        assert!(super::touches_changed_lines(
            &changed,
            &mutant("a.rs", 5, 5)
        ));
        assert!(!super::touches_changed_lines(
            &changed,
            &mutant("a.rs", 6, 9)
        ));
        assert!(!super::touches_changed_lines(
            &changed,
            &mutant("b.rs", 5, 5)
        ));
    }

    #[test]
    fn a_hang_is_a_timeout_and_a_failure_is_caught() {
        assert_eq!(super::test_outcome(Some(0)), None);
        assert_eq!(super::test_outcome(Some(101)), Some(super::Outcome::Caught));
        assert_eq!(super::test_outcome(None), Some(super::Outcome::Timeout));
    }

    #[test]
    fn report_is_sorted_and_drops_the_location_from_the_description() {
        let (a, b) = (mutant("b.rs", 2, 2), mutant("a.rs", 7, 7));
        let report =
            super::report_json(&[(&a, super::Outcome::Missed), (&b, super::Outcome::Timeout)]);
        assert_eq!(
            report,
            serde_json::json!({"mutants": [
                {"file": "a.rs", "line": 7, "column": 5, "genre": "FnValue",
                 "description": "replace triple -> i32 with 0", "outcome": "timeout"},
                {"file": "b.rs", "line": 2, "column": 5, "genre": "FnValue",
                 "description": "replace triple -> i32 with 0", "outcome": "missed"},
            ]})
        );
    }
}
