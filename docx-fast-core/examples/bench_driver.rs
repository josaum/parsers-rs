//! One-shot driver for `scripts/bench-instructions.sh`: parses one file,
//! serializes it to JSON and exits, so the whole process can be measured
//! (instructions retired, user CPU time, peak memory) on a machine too busy
//! for wall-clock benchmarks. See `docs/PERFORMANCE.md`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(path) = std::env::args_os().nth(1) else {
        eprintln!("usage: bench_driver <file>");
        return ExitCode::from(2);
    };
    let json = docx_fast_core::parse_docx(&path, docx_fast_core::ParseOptions::default())
        .map_err(|error| format!("{error:#}"))
        .and_then(|document| serde_json::to_vec(&document).map_err(|error| error.to_string()));
    match json {
        Ok(json) => {
            println!("json_bytes={}", json.len());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
