//! golden-capture — write the native RasmEncoder kernel byte snapshot.
//!
//!   cargo run --bin golden-capture --features opt-metrics
//!
//! Builds the kernel through the live native JASM/Rasm path and records every
//! assembler-emitted symbol's bytes into `bench/golden/kernel.json`.

use std::path::Path;
use std::process::ExitCode;

use wf64::Wf64Session;

fn main() -> ExitCode {
    match run() {
        Ok(n) => {
            println!("captured {n} kernel symbols");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("golden-capture failed: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<usize> {
    let mut session = Wf64Session::new()?;
    let golden = wf64::golden::capture(&mut session)?;

    let out = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("bench")
        .join("golden")
        .join("kernel.json");
    std::fs::create_dir_all(out.parent().unwrap())?;
    let json = serde_json::to_string_pretty(&golden)?;
    std::fs::write(&out, json)?;
    println!("wrote {}", out.display());

    let raw = golden.values().filter(|g| g.raw.is_some()).count();
    println!("  {} symbols carry an extern-free raw golden (self-inspection-safe)", raw);
    Ok(golden.len())
}
