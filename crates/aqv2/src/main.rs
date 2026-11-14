use clap::{CommandFactory, FromArgMatches};
use serde_json::{Value, json};
#[tokio::main]
async fn main() -> std::process::ExitCode {
    let program = aqv2::program();
    let json_output = std::env::args()
        .take_while(|a| a != "--")
        .any(|a| a == "--json");
    let parsed =
        aqv2::with_registry_help(aqv2::cli::Cli::command().name(program).bin_name(program))
            .try_get_matches()
            .and_then(|m| aqv2::cli::Cli::from_arg_matches(&m));
    let cli = match parsed {
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
                    json!({"schema_version":1,"error":aqv2::error("invalid_arguments",&format!("Invalid command arguments; use {program} --help"))}),
                );
            } else {
                let _ = error.print();
            }
            return std::process::ExitCode::from(2);
        }
    };
    let json_output = json_output || cli.json;
    if let aqv2::cli::Command::Result(command) = &cli.commands.command
        && !matches!(command, aqv2::cli::ResultCommand::Command(_))
    {
        return result(command, json_output);
    }
    let render = cli.commands.command.render();
    match aqv2::run(cli).await {
        Ok(value) => {
            if json_output {
                print_json(json!({"schema_version":1,"data":value}));
            } else {
                println!("{}", aqv2::render::render(render, &value));
            }
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            let code = aqv2::exit_code(&error);
            if json_output {
                print_json(json!({"schema_version":1,"error":error}));
            } else if error["effect"] == "not_applied" && error["command"].is_string() {
                eprintln!("{program}: {}", aqv2::render::refusal(&error));
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
            std::process::ExitCode::from(code)
        }
    }
}
/// `result submit|status` is worker-local: it needs no connection, and it prints
/// the local result document with `aq-agent result`'s exit statuses (0 ready or
/// recorded, 10 pending, 11 conflict, 13 needs reconciliation or unknown, 15
/// unusable channel or request).
fn result(command: &aqv2::cli::ResultCommand, json_output: bool) -> std::process::ExitCode {
    use aqv2::cli::ResultCommand as R;
    let submit = match command {
        R::Submit { request_file } => Some(request_file.as_path()),
        R::Status | R::Command(_) => None,
    };
    let (value, code, key) = match aqv2::result::run(submit) {
        Ok(o) => (o.value, o.exit_code, "data"),
        // Never a raw parser, filesystem or path error.
        Err(_) => (aqv2::result::refused(), 15, "error"),
    };
    if json_output {
        print_json(json!({"schema_version":1, key:value}));
    } else if code == 15 {
        eprintln!("{value}");
    } else {
        println!("{value}");
    }
    std::process::ExitCode::from(code)
}
fn print_json(value: Value) {
    println!("{}", value);
}
