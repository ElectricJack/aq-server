use aq_human_http::{Authentication, Client, Limits};
use clap::{Parser, Subcommand};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::os::unix::fs::OpenOptionsExt;
use std::{io::Read, path::PathBuf, time::Duration};
use uuid::Uuid;
#[derive(Parser)]
#[command(
    about = "Inspect human notification delivery and submit explicit operator intents",
    version
)]
struct Args {
    #[arg(long)]
    origin: String,
    #[arg(long)]
    cf_access_token_file: Option<PathBuf>,
    #[cfg(feature = "loopback-development")]
    #[arg(long, conflicts_with = "cf_access_token_file")]
    dev_email: Option<String>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Inbox {
        #[arg(long)]
        recipient: Option<Uuid>,
        #[arg(long, default_value_t = 0)]
        after: u64,
        #[arg(long, default_value_t = 32)]
        limit: u32,
    },
    Inspect {
        message_id: Uuid,
    },
    /// Read a previously saved request with stable operation/message IDs.
    Send {
        #[arg(long)]
        request: PathBuf,
    },
    /// Configure a logical destination key; never supply a webhook URL.
    Route {
        #[arg(long)]
        request: PathBuf,
    },
    /// Explicitly accept possible duplicate delivery using a saved intent.
    RetryUnknown {
        #[arg(long)]
        request: PathBuf,
    },
}
fn request<T: DeserializeOwned>(path: PathBuf) -> Result<T, &'static str> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "cannot read request file")?;
    if !file
        .metadata()
        .map_err(|_| "cannot inspect request file")?
        .is_file()
    {
        return Err("request must be a regular file");
    }
    let mut bytes = Vec::new();
    file.take(16385)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read request file")?;
    if bytes.len() > 16384 {
        return Err("request exceeds 16 KiB");
    }
    serde_json::from_slice(&bytes).map_err(|_| "invalid typed request JSON")
}
impl Args {
    fn client(&self) -> Result<Client, String> {
        let auth = if let Some(path) = &self.cf_access_token_file {
            Authentication::CfAccessTokenFile(path.clone())
        } else {
            #[cfg(feature = "loopback-development")]
            {
                Authentication::LoopbackDevelopmentEmail(
                    self.dev_email.clone().ok_or("authentication required")?,
                )
            }
            #[cfg(not(feature = "loopback-development"))]
            {
                return Err("--cf-access-token-file is required".into());
            }
        };
        Client::new(
            &self.origin,
            auth,
            Limits {
                max_request_bytes: 16384,
                max_response_bytes: 524288,
                timeout: Duration::from_secs(12),
            },
        )
        .map_err(|e| e.to_string())
    }
}

async fn run(args: Args) -> Result<Value, String> {
    let client = args.client()?;
    let response = match args.command {
        Command::Inbox {
            recipient,
            after,
            limit,
        } => {
            if after > i64::MAX as u64 || !(1..=64).contains(&limit) {
                return Err("invalid pagination bounds".into());
            }
            let mut query = vec![("after", after.to_string()), ("limit", limit.to_string())];
            if let Some(id) = recipient {
                query.push(("recipient", id.to_string()));
            }
            let query: Vec<_> = query.iter().map(|(k, v)| (*k, v.as_str())).collect();
            client.get_json("/api/notifications", &query).await
        }
        Command::Inspect { message_id } => {
            client
                .get_json(&format!("/api/notifications/{message_id}"), &[])
                .await
        }
        Command::Send { request: path } => {
            client
                .post_json(
                    "/api/notifications",
                    &request::<aq_notifications::SendRequest>(path)?,
                )
                .await
        }
        Command::Route { request: path } => {
            client
                .post_json(
                    "/api/notifications/routes",
                    &request::<aq_notifications::RouteRequest>(path)?,
                )
                .await
        }
        Command::RetryUnknown { request: path } => {
            client
                .post_json(
                    "/api/notifications/retry",
                    &request::<aq_notifications::RetryRequest>(path)?,
                )
                .await
        }
    };
    response.map_err(|e|format!("{e}; mutations are not retried automatically; inspect the original IDs before creating another intent"))
}
#[tokio::main]
async fn main() {
    match run(Args::parse()).await {
        Ok(value) => println!("{}", serde_json::to_string_pretty(&value).unwrap()),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
