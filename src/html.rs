use axum::response::Html;
use serde_json::Value;

pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// `active` is the nav entry to highlight: `inbox`, `attempts` or `setup`.
pub fn page(title: &str, active: &str, body: &str) -> Html<String> {
    let nav = [("inbox", "/", "Inbox"), ("attempts", "/attempts", "Attempts"), ("setup", "/setup", "Setup")]
        .iter()
        .map(|(id, href, label)| {
            let current = if *id == active { r#" aria-current="page""# } else { "" };
            format!(r#"<a href="{href}"{current}>{label}</a>"#)
        })
        .collect::<String>();
    Html(format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} · localmail</title>
<link rel="icon" href="data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 viewBox=%220 0 100 100%22%3E%3Ctext y=%22.9em%22 font-size=%2290%22%3E%E2%9C%89%3C/text%3E%3C/svg%3E">
<link rel="stylesheet" href="/assets/app.css">
</head>
<body class="page-{active}">
<header class="topbar">
  <a class="brand" href="/"><span class="logo">✉</span> local<b>mail</b></a>
  <nav>{nav}</nav>
</header>
<main>
{body}
</main>
<script src="/assets/app.js"></script>
</body>
</html>"#,
        title = esc(title),
    ))
}

/// Embed JSON for the page script.
pub fn data_script(v: &Value) -> String {
    format!(
        r#"<script type="application/json" id="lm-data">{}</script>"#,
        v.to_string().replace('<', "\\u003c").replace('>', "\\u003e").replace('&', "\\u0026")
    )
}
