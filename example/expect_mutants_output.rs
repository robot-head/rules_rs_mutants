use std::env;
use std::process::Command;

fn run() -> Result<(), String> {
    let args = env::args().collect::<Vec<_>>();
    if args.len() < 3 {
        return Err("usage: expect_mutants_output <cargo-mutants-test> <expected>".to_owned());
    }

    let output = Command::new(&args[1])
        .output()
        .map_err(|err| format!("failed to run {}: {err}", args[1]))?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let expected = args[2..].join(" ");
    if !output.status.success() || !combined.contains(&expected) {
        return Err(format!(
            "mutation output did not contain `{}`\n{combined}",
            expected,
        ));
    }

    Ok(())
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
