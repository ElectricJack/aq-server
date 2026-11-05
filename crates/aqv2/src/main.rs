use clap::Parser;
use serde_json::{Value, json};
#[tokio::main]
async fn main() -> std::process::ExitCode {
    let json_output = std::env::args()
        .take_while(|a| a != "--")
        .any(|a| a == "--json");
    let cli = match aqv2::cli::Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return std::process::ExitCode::SUCCESS;
        }
        Err(error) => {
            if json_output {
                print_json(
                    json!({"schema_version":1,"error":aqv2::error("invalid_arguments","Invalid command arguments; use aqv2 --help")}),
                );
            } else {
                let _ = error.print();
            }
            return std::process::ExitCode::from(2);
        }
    };
    let json_output = cli.json;
    match aqv2::run(cli).await {
        Ok(value) => {
            if json_output {
                print_json(json!({"schema_version":1,"data":value}));
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&value).expect("JSON value")
                );
            }
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            if json_output {
                print_json(json!({"schema_version":1,"error":error}));
            } else {
                // JSON quoting keeps server/input control characters out of the terminal.
                eprintln!("{}: {}", error["code"], error["message"]);
                if let Some(id) = error["operation_id"].as_str() {
                    eprintln!("operation_id: {id}");
                }
                if let Some(recovery) = error["recovery"].as_str() {
                    eprintln!("Recover with: {recovery}");
                }
            }
            std::process::ExitCode::FAILURE
        }
    }
}
fn print_json(value: Value) {
    println!("{}", value);
}
