use std::env;
use std::fs;
use std::process::ExitCode;

use vize_atelier_syrinx::{SyrinxCompileOptions, measure_rsx_coverage};

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(input) = args.next() else {
        eprintln!("usage: syrinx-rsx-coverage <component.vue> [report.json]");
        return ExitCode::from(2);
    };
    let output = args.next();
    if args.next().is_some() {
        eprintln!("usage: syrinx-rsx-coverage <component.vue> [report.json]");
        return ExitCode::from(2);
    }
    let source = match fs::read_to_string(&input) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("failed to read {input}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let options = SyrinxCompileOptions {
        filename: input.clone(),
        component_name: input
            .rsplit('/')
            .next()
            .and_then(|name| name.strip_suffix(".vue"))
            .map(str::to_owned),
        component_id: 100,
        protocol_schema_sha256: "0".repeat(64),
        ..Default::default()
    };
    let mut report = match measure_rsx_coverage(&source, options) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    report.classifier_revision =
        env::var("VIZE_RSX_COVERAGE_REVISION").unwrap_or_else(|_| "working-tree".to_owned());
    let json =
        serde_json::to_string_pretty(&report).expect("coverage report is serializable") + "\n";
    if let Some(output) = output {
        if let Err(error) = fs::write(&output, json) {
            eprintln!("failed to write {output}: {error}");
            return ExitCode::FAILURE;
        }
    } else {
        print!("{json}");
    }
    ExitCode::SUCCESS
}
