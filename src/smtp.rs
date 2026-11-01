//! A small SMTP server (RFC 5321) that keeps every message instead of delivering it. It accepts
//! any AUTH PLAIN or LOGIN credentials and doesn't offer STARTTLS. Scenario addresses apply here
//! too: rejections and refused credentials at RCPT TO, everything else at the end of DATA.

use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::address::Addr;
use crate::attempts::Attempt;
use crate::message::{Draft, Envelope, NewMessage, Provider};
use crate::scenarios::{self, Fail, Scenario};
use crate::state::AppState;
use crate::util::now_ms;

/// Longest command line accepted.
const MAX_LINE: usize = 4096;
const MAX_RECIPIENTS: usize = 1000;
/// Idle clients are disconnected after this long.
const IDLE: Duration = Duration::from_secs(300);

pub async fn serve(listener: TcpListener, state: AppState) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = Session::new(stream, state).run().await {
                        tracing::debug!("smtp session with {peer} ended: {e}");
                    }
                });
            }
            Err(e) => {
                tracing::warn!("smtp accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

struct Session {
    io: BufReader<TcpStream>,
    state: AppState,
    helo: Option<String>,
    auth_user: Option<String>,
    mail_from: Option<String>,
    rcpts: Vec<String>,
    /// The commands of the current transaction, for the attempts log.
    transcript: Vec<String>,
    started: Instant,
}

enum Line {
    Text(String),
    TooLong,
    Closed,
}

impl Session {
    fn new(stream: TcpStream, state: AppState) -> Self {
        Self {
            io: BufReader::new(stream),
            state,
            helo: None,
            auth_user: None,
            mail_from: None,
            rcpts: vec![],
            transcript: vec![],
            started: Instant::now(),
        }
    }

    async fn reply(&mut self, text: &str) -> std::io::Result<()> {
        let stream = self.io.get_mut();
        stream.write_all(text.as_bytes()).await?;
        stream.write_all(b"\r\n").await?;
        stream.flush().await
    }

    async fn read_line(&mut self) -> std::io::Result<Line> {
        let mut buf = vec![];
        let read = tokio::time::timeout(IDLE, (&mut self.io).take(MAX_LINE as u64 + 2).read_until(b'\n', &mut buf))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "idle"))??;
        if read == 0 {
            return Ok(Line::Closed);
        }
        if !buf.ends_with(b"\n") {
            // Skip the rest of an overlong line.
            let mut rest = vec![];
            self.io.read_until(b'\n', &mut rest).await?;
            return Ok(Line::TooLong);
        }
        Ok(Line::Text(String::from_utf8_lossy(&buf).trim_end_matches(['\r', '\n']).to_string()))
    }

    fn reset(&mut self) {
        self.mail_from = None;
        self.rcpts.clear();
        self.transcript.clear();
        self.started = Instant::now();
    }

    async fn run(mut self) -> std::io::Result<()> {
        self.reply("220 localmail ESMTP ready (messages are kept, never delivered)").await?;
        loop {
            let line = match self.read_line().await? {
                Line::Closed => return Ok(()),
                Line::TooLong => {
                    self.reply("500 5.5.2 Line too long").await?;
                    continue;
                }
                Line::Text(l) => l,
            };
            let (verb, arg) = match line.split_once(' ') {
                Some((v, a)) => (v.to_ascii_uppercase(), a.trim().to_string()),
                None => (line.trim().to_ascii_uppercase(), String::new()),
            };
            match verb.as_str() {
                "EHLO" => {
                    self.helo = Some(arg.clone());
                    self.reset();
                    let size = self.state.config.max_message_size;
                    self.reply(&format!(
                        "250-localmail greets {arg}\r\n250-SIZE {size}\r\n250-8BITMIME\r\n250-SMTPUTF8\r\n250-PIPELINING\r\n250-ENHANCEDSTATUSCODES\r\n250 AUTH PLAIN LOGIN"
                    ))
                    .await?;
                }
                "HELO" => {
                    self.helo = Some(arg.clone());
                    self.reset();
                    self.reply(&format!("250 localmail greets {arg}")).await?;
                }
                "AUTH" => self.auth(&arg).await?,
                "MAIL" => self.mail(&arg).await?,
                "RCPT" => self.rcpt(&arg).await?,
                "DATA" => self.data().await?,
                "RSET" => {
                    self.reset();
                    self.reply("250 2.0.0 OK").await?;
                }
                "NOOP" => self.reply("250 2.0.0 OK").await?,
                "VRFY" => self.reply("252 2.1.5 Cannot verify the user, but will accept the message").await?,
                "HELP" => self.reply("214 2.0.0 localmail: EHLO, AUTH, MAIL, RCPT, DATA, RSET, NOOP, QUIT").await?,
                "QUIT" => {
                    self.reply("221 2.0.0 Bye").await?;
                    return Ok(());
                }
                "STARTTLS" => {
                    self.reply("502 5.5.1 STARTTLS isn't supported: localmail only speaks plain SMTP").await?
                }
                "BDAT" => self.reply("502 5.5.1 CHUNKING isn't supported").await?,
                "" => self.reply("500 5.5.2 Empty command").await?,
                _ => self.reply("500 5.5.2 Command not recognized").await?,
            }
        }
    }

    async fn auth(&mut self, arg: &str) -> std::io::Result<()> {
        let mut parts = arg.split_whitespace();
        let mechanism = parts.next().unwrap_or_default().to_ascii_uppercase();
        let initial = parts.next().map(String::from);
        let decode = |s: &str| STANDARD.decode(s.trim()).ok().map(|b| String::from_utf8_lossy(&b).into_owned());
        let user = match mechanism.as_str() {
            "PLAIN" => {
                let response = match initial {
                    Some(r) => r,
                    None => {
                        self.reply("334 ").await?;
                        self.read_text().await?
                    }
                };
                // authzid \0 authcid \0 password
                decode(&response).and_then(|s| s.split('\0').nth(1).map(String::from))
            }
            "LOGIN" => {
                let user = match initial {
                    Some(u) => u,
                    None => {
                        self.reply("334 VXNlcm5hbWU6").await?;
                        self.read_text().await?
                    }
                };
                self.reply("334 UGFzc3dvcmQ6").await?;
                self.read_text().await?;
                decode(&user)
            }
            _ => {
                self.reply("504 5.5.4 Unrecognized authentication type (use PLAIN or LOGIN)").await?;
                return Ok(());
            }
        };
        match user {
            Some(u) => {
                self.auth_user = Some(u);
                self.reply("235 2.7.0 Authentication successful (localmail accepts any credentials)").await
            }
            None => self.reply("501 5.5.2 Cannot decode the response").await,
        }
    }

    async fn read_text(&mut self) -> std::io::Result<String> {
        match self.read_line().await? {
            Line::Text(t) => Ok(t),
            Line::TooLong => Ok(String::new()),
            Line::Closed => Err(std::io::ErrorKind::UnexpectedEof.into()),
        }
    }

    async fn mail(&mut self, arg: &str) -> std::io::Result<()> {
        let Some(path) = path_arg(arg, "FROM:") else {
            return self.reply("501 5.5.4 Syntax: MAIL FROM:<address>").await;
        };
        if self.mail_from.is_some() {
            return self.reply("503 5.5.1 Sender already given").await;
        }
        if let Some(size) = param(arg, "SIZE").and_then(|s| s.parse::<usize>().ok())
            && size > self.state.config.max_message_size
        {
            return self.reply("552 5.3.4 Message size exceeds the limit").await;
        }
        self.started = Instant::now();
        self.transcript.push(format!("MAIL FROM:<{path}>"));
        self.mail_from = Some(path);
        self.reply("250 2.1.0 OK").await
    }

    async fn rcpt(&mut self, arg: &str) -> std::io::Result<()> {
        if self.mail_from.is_none() {
            return self.reply("503 5.5.1 MAIL FROM first").await;
        }
        let Some(path) = path_arg(arg, "TO:").filter(|p| !p.is_empty()) else {
            return self.reply("501 5.5.4 Syntax: RCPT TO:<address>").await;
        };
        if self.rcpts.len() >= MAX_RECIPIENTS {
            return self.reply("452 4.5.3 Too many recipients").await;
        }
        self.transcript.push(format!("RCPT TO:<{path}>"));
        let refused = match scenarios::scenario_of(&path, &self.state.config.scenario_domain) {
            Some(Ok(Scenario::Reject)) => Some(("550 5.1.1 Recipient rejected (reject scenario)", "rejected")),
            Some(Ok(Scenario::Unauthorized)) => {
                Some(("530 5.7.0 Authentication required (unauthorized scenario)", "unauthorized"))
            }
            _ => None,
        };
        if let Some((reply, error)) = refused {
            self.transcript.push(format!("-> {reply}"));
            self.record(
                reply,
                Some(error),
                Some(format!("{path} refused at RCPT TO")),
                vec![path.clone()],
                vec![],
                Some(path),
            );
            return self.reply(reply).await;
        }
        self.rcpts.push(path);
        self.reply("250 2.1.5 OK").await
    }

    async fn data(&mut self) -> std::io::Result<()> {
        let Some(mail_from) = self.mail_from.clone() else {
            return self.reply("503 5.5.1 MAIL FROM first").await;
        };
        if self.rcpts.is_empty() {
            return self.reply("554 5.5.1 No valid recipients").await;
        }
        self.reply("354 End data with <CR><LF>.<CR><LF>").await?;
        let max = self.state.config.max_message_size;
        let mut raw: Vec<u8> = vec![];
        let mut too_big = false;
        loop {
            let mut line = vec![];
            // Bounded, so one huge line can't use up memory before the size check.
            let n = tokio::time::timeout(IDLE, (&mut self.io).take(max as u64 + 2).read_until(b'\n', &mut line))
                .await
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "idle"))??;
            if n == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            if line == b".\r\n" || line == b".\n" {
                break;
            }
            // Dot-stuffing.
            let line = if line.starts_with(b"..") { &line[1..] } else { &line[..] };
            if raw.len() + line.len() > max {
                too_big = true;
            } else {
                raw.extend_from_slice(line);
            }
        }
        if too_big {
            self.reset();
            return self.reply("552 5.3.4 Message size exceeds the limit").await;
        }
        let reply = self.finish(mail_from, raw).await;
        self.reset();
        self.reply(&reply).await
    }

    /// Apply scenarios to a complete message and keep it; returns the reply.
    async fn finish(&mut self, mail_from: String, raw: Vec<u8>) -> String {
        let rcpts: Vec<Addr> = self.rcpts.iter().map(|r| Addr::new(None, r)).collect();
        let mut draft = Draft::from_raw(raw, Some(rcpts.clone()));
        if draft.from.is_none() && !mail_from.is_empty() {
            draft.from = Some(Addr::new(None, &mail_from));
        }
        draft.envelope = Some(Envelope {
            mail_from: mail_from.clone(),
            rcpt_to: self.rcpts.clone(),
            helo: self.helo.clone(),
            auth_user: self.auth_user.clone(),
        });
        let subject = draft.subject.clone();
        let fingerprint = scenarios::fingerprint(draft.from.as_ref(), rcpts.iter(), &subject);
        let plan =
            scenarios::plan(&self.state.flaky, &self.state.config.scenario_domain, rcpts.iter(), &fingerprint, false);
        if let Some(delay) = plan.delay {
            tokio::time::sleep(delay).await;
        }
        let rcpt_list = self.rcpts.clone();
        let failure = plan.failure.as_ref().map(|(f, _)| match f {
            Fail::RateLimit => ("451 4.7.1 Too many messages, slow down (ratelimit scenario)", "rate_limited"),
            Fail::Unauthorized => ("530 5.7.0 Authentication required (unauthorized scenario)", "unauthorized"),
            Fail::Error | Fail::Reject => {
                ("451 4.3.0 Temporary server error, try again later (error or flaky scenario)", "server_error")
            }
        });
        self.transcript.push("DATA".into());
        if let Some((reply, error)) = failure {
            self.transcript.push(format!("-> {reply}"));
            let warnings = plan.warnings.clone();
            self.record_with(
                reply,
                Some(error),
                None,
                rcpt_list,
                vec![],
                plan.applied.clone(),
                Some(subject),
                warnings,
            );
            return reply.to_string();
        }
        let id = Provider::Smtp.new_message_id();
        let msg = NewMessage::from_draft(draft, Provider::Smtp, "SMTP", id.clone(), plan.applied.clone());
        match self.state.store.insert(&msg) {
            Some(seq) => {
                let reply = format!("250 2.0.0 OK: queued as {id}");
                self.transcript.push(format!("-> {reply}"));
                let warnings = plan.warnings.clone();
                self.record_with(
                    &reply,
                    None,
                    None,
                    rcpt_list,
                    vec![seq],
                    plan.applied.clone(),
                    Some(subject),
                    warnings,
                );
                reply
            }
            None => "451 4.3.0 Could not store the message".into(),
        }
    }

    fn record(
        &self,
        reply: &str,
        error: Option<&str>,
        description: Option<String>,
        recipients: Vec<String>,
        messages: Vec<i64>,
        scenario: Option<String>,
    ) {
        self.record_with(reply, error, description, recipients, messages, scenario, None, vec![]);
    }

    #[allow(clippy::too_many_arguments)]
    fn record_with(
        &self,
        reply: &str,
        error: Option<&str>,
        description: Option<String>,
        recipients: Vec<String>,
        messages: Vec<i64>,
        scenario: Option<String>,
        subject: Option<String>,
        warnings: Vec<String>,
    ) {
        let status: u16 = reply.get(..3).and_then(|c| c.parse().ok()).unwrap_or(0);
        let mut transcript = vec![];
        if let Some(h) = &self.helo {
            transcript.push(format!("EHLO {h}"));
        }
        if let Some(u) = &self.auth_user {
            transcript.push(format!("AUTH (as {u})"));
        }
        transcript.extend(self.transcript.iter().cloned());
        self.state.attempts.record(Attempt {
            id: 0,
            at: now_ms(),
            duration_ms: self.started.elapsed().as_millis() as u64,
            provider: Provider::Smtp,
            endpoint: "SMTP".into(),
            status,
            outcome: if status < 400 { "accepted" } else { "failed" },
            from: self.mail_from.clone(),
            recipients,
            subject: subject.filter(|s| !s.is_empty()),
            messages,
            scenario,
            error: error.map(String::from),
            error_description: description.or_else(|| (status >= 400).then(|| reply.to_string())),
            warnings: warnings.into_iter().map(|w| format!("not a scenario address: {w}")).collect(),
            request: Some(transcript.join("\n")),
            response: Some(reply.to_string()),
        });
    }
}

/// The address in `FROM:<a@b> SIZE=…` or `TO:<a@b>`; `<>` is the null sender.
fn path_arg(arg: &str, prefix: &str) -> Option<String> {
    let rest =
        arg.get(..prefix.len()).filter(|p| p.eq_ignore_ascii_case(prefix)).map(|_| arg[prefix.len()..].trim())?;
    let path = match rest.strip_prefix('<') {
        Some(r) => r.split_once('>')?.0,
        None => rest.split_whitespace().next().unwrap_or_default(),
    };
    // Drop a source route (`@a,@b:user@c`).
    Some(path.rsplit(':').next().unwrap_or(path).trim().to_string())
}

/// A `KEY=value` parameter after the path.
fn param<'a>(arg: &'a str, key: &str) -> Option<&'a str> {
    arg.split_whitespace().skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.eq_ignore_ascii_case(key).then_some(v)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(path_arg("FROM:<a@b.test> SIZE=10", "FROM:"), Some("a@b.test".into()));
        assert_eq!(path_arg("from: <a@b.test>", "FROM:"), Some("a@b.test".into()));
        assert_eq!(path_arg("FROM:<>", "FROM:"), Some(String::new()));
        assert_eq!(path_arg("TO:<@relay:c@d.test>", "TO:"), Some("c@d.test".into()));
        assert_eq!(path_arg("TO:c@d.test", "TO:"), Some("c@d.test".into()));
        assert_eq!(path_arg("TO c@d.test", "TO:"), None);
        assert_eq!(param("FROM:<a@b> SIZE=1024 BODY=8BITMIME", "size"), Some("1024"));
    }
}
