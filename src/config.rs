use std::path::PathBuf;

use anyhow::Context;
use clap::{Args, Parser};
use serde::Deserialize;

/// The binary only runs the servers; everything else happens over HTTP (the UI and `/dev/api`).
#[derive(Parser, Debug)]
#[command(
    name = "localmail",
    version,
    about = "A local SMTP server and email API emulator (SES, SendGrid, Postmark, Resend, Gmail, Microsoft Graph, Mailgun, Mailjet, Brevo, SparkPost, Mandrill, MailerSend) for offline development"
)]
pub struct Cli {
    #[command(flatten)]
    pub server: ServerArgs,
}

#[derive(Args, Debug, Clone, Default)]
pub struct ServerArgs {
    /// Path to a TOML config file (defaults to ./localmail.toml if present).
    #[arg(long, env = "LOCALMAIL_CONFIG")]
    pub config: Option<PathBuf>,
    /// Interface to bind (both the HTTP and SMTP servers).
    #[arg(long, env = "LOCALMAIL_HOST")]
    pub host: Option<String>,
    /// HTTP port: the web UI, the dev API and the email provider APIs.
    #[arg(long, short, env = "LOCALMAIL_PORT")]
    pub port: Option<u16>,
    /// SMTP port.
    #[arg(long, env = "LOCALMAIL_SMTP_PORT")]
    pub smtp_port: Option<u16>,
    /// Public base URL of the HTTP server (default http://localhost:<port>), for links and setup snippets.
    #[arg(long, env = "LOCALMAIL_BASE_URL")]
    pub base_url: Option<String>,
    /// Directory where captured messages are kept (default ~/.localmail).
    #[arg(long, env = "LOCALMAIL_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
    /// Keep messages in memory only: every start begins with an empty inbox.
    #[arg(long, env = "LOCALMAIL_IN_MEMORY")]
    pub in_memory: bool,
    /// Keep at most this many messages; the oldest are deleted first.
    #[arg(long, env = "LOCALMAIL_MAX_MESSAGES")]
    pub max_messages: Option<usize>,
    /// Largest message accepted over SMTP and the APIs, in bytes.
    #[arg(long, env = "LOCALMAIL_MAX_MESSAGE_SIZE")]
    pub max_message_size: Option<usize>,
    /// Domain of the scenario addresses (e.g. reject@<domain>) that make a send fail on purpose.
    #[arg(long, env = "LOCALMAIL_SCENARIO_DOMAIN")]
    pub scenario_domain: Option<String>,
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    host: Option<String>,
    port: Option<u16>,
    smtp_port: Option<u16>,
    base_url: Option<String>,
    data_dir: Option<PathBuf>,
    in_memory: Option<bool>,
    max_messages: Option<usize>,
    max_message_size: Option<usize>,
    scenario_domain: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub smtp_port: u16,
    pub base_url: String,
    /// `None` keeps messages in memory.
    pub data_dir: Option<PathBuf>,
    pub max_messages: usize,
    pub max_message_size: usize,
    /// Lower-cased.
    pub scenario_domain: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 9402,
            smtp_port: 2525,
            base_url: "http://localhost:9402".into(),
            data_dir: None,
            max_messages: 5000,
            max_message_size: 40 * 1024 * 1024,
            scenario_domain: "localmail.test".into(),
        }
    }
}

impl Config {
    pub fn load(args: &ServerArgs) -> anyhow::Result<Self> {
        let path = args.config.clone().or_else(|| {
            let p = PathBuf::from("localmail.toml");
            p.exists().then_some(p)
        });
        let file: FileConfig = match &path {
            Some(p) => {
                let raw = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
                toml::from_str(&raw).with_context(|| format!("parsing {}", p.display()))?
            }
            None => FileConfig::default(),
        };

        let d = Config::default();
        let port = args.port.or(file.port).unwrap_or(d.port);
        let base_url = args
            .base_url
            .clone()
            .or(file.base_url)
            .unwrap_or_else(|| format!("http://localhost:{port}"))
            .trim_end_matches('/')
            .to_string();
        let in_memory = args.in_memory || file.in_memory.unwrap_or(false);
        let data_dir = if in_memory {
            None
        } else {
            args.data_dir
                .clone()
                .or(file.data_dir)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".localmail")))
        };
        let scenario_domain = args
            .scenario_domain
            .clone()
            .or(file.scenario_domain)
            .unwrap_or(d.scenario_domain)
            .trim()
            .trim_start_matches('@')
            .to_ascii_lowercase();
        if scenario_domain.is_empty() || scenario_domain.contains(['@', ' ']) {
            anyhow::bail!("invalid scenario domain '{scenario_domain}'");
        }
        let max_messages = args.max_messages.or(file.max_messages).unwrap_or(d.max_messages);
        if max_messages == 0 {
            anyhow::bail!("max_messages must be at least 1");
        }

        Ok(Self {
            host: args.host.clone().or(file.host).unwrap_or(d.host),
            port,
            smtp_port: args.smtp_port.or(file.smtp_port).unwrap_or(d.smtp_port),
            base_url,
            data_dir,
            max_messages,
            max_message_size: args.max_message_size.or(file.max_message_size).unwrap_or(d.max_message_size),
            scenario_domain,
        })
    }
}
