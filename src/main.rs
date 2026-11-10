use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use localmail::config::{Cli, Config};
use localmail::{AppState, router, smtp};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    serve(&Cli::parse().server).await
}

async fn serve(args: &localmail::config::ServerArgs) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "localmail=info,tower_http=info".into()))
        .init();

    let config = Config::load(args)?;
    let http_addr = format!("{}:{}", config.host, config.port);
    let smtp_addr = format!("{}:{}", config.host, config.smtp_port);
    let db = config.data_dir.as_ref().map(|d| d.join("messages.db"));
    let base = config.base_url.clone();
    let state = AppState::new(config)?;

    let http = tokio::net::TcpListener::bind(&http_addr).await.with_context(|| format!("binding {http_addr}"))?;
    let smtp_listener =
        tokio::net::TcpListener::bind(&smtp_addr).await.with_context(|| format!("binding {smtp_addr}"))?;
    tokio::spawn(smtp::serve(smtp_listener, state.clone()));

    tracing::info!("localmail listening on http://{http_addr} and smtp://{smtp_addr}");
    tracing::info!("inbox:            {base}/");
    tracing::info!("SMTP:             {smtp_addr}");
    tracing::info!("Amazon SES:       {base}/v2/email/outbound-emails (v2), {base}/ (v1)");
    tracing::info!("SendGrid:         {base}/v3/mail/send");
    tracing::info!("Postmark:         {base}/email");
    tracing::info!("Resend:           {base}/emails");
    tracing::info!("Gmail:            {base}/gmail/v1/users/me/messages/send");
    tracing::info!("Microsoft Graph:  {base}/v1.0/me/sendMail");
    tracing::info!("Mailgun:          {base}/v3/<domain>/messages");
    tracing::info!("Mailjet:          {base}/v3.1/send");
    tracing::info!("Brevo:            {base}/v3/smtp/email");
    tracing::info!("SparkPost:        {base}/api/v1/transmissions");
    tracing::info!("Mandrill:         {base}/api/1.0/messages/send");
    tracing::info!("MailerSend:       {base}/v1/email");
    tracing::info!("OAuth tokens:     {base}/token (Google), {base}/<tenant>/oauth2/v2.0/token (Microsoft)");
    tracing::info!("dev API:          {base}/dev/api/messages");
    match db {
        Some(db) => tracing::info!("messages:         {}", db.display()),
        None => tracing::info!("messages:         in memory"),
    }
    axum::serve(http, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
