use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use lettre::{
    message::{header::ContentType, Attachment, MultiPart, SinglePart},
    transport::smtp::authentication::Credentials,
    Message, SmtpTransport, Transport,
};
use serde::Deserialize;

const XKCD_API_URL: &str = "https://xkcd.com/info.0.json";
const HISTORY_FILE: &str = "xkcd_history.txt";
const COMIC_DIR: &str = "comics";

#[derive(Debug, Deserialize)]
struct Comic {
    num: u32,
    safe_title: String,
    img: String,
    alt: String,
    year: String,
    month: String,
    day: String,
}

#[derive(Debug)]
struct Config {
    mail_to: String,
    mail_from: String,
    download: bool,
    mail_attachment: bool,
    smtp_server: String,
    smtp_port: u16,
    smtp_starttls: bool,
    smtp_username: Option<String>,
    smtp_password: Option<String>,
}

fn load_config() -> anyhow::Result<Config> {
    let _ = dotenvy::dotenv();

    fn env(key: &str) -> anyhow::Result<String> {
        std::env::var(key).with_context(|| format!("missing env var {key}"))
    }

    fn env_bool(key: &str, default: bool) -> bool {
        match std::env::var(key) {
            Ok(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"),
            Err(_) => default,
        }
    }

    let config = Config {
        mail_to: env("XKCD_MAIL_TO")?,
        mail_from: env("XKCD_MAIL_FROM")?,
        download: env_bool("XKCD_DOWNLOAD", true),
        mail_attachment: env_bool("XKCD_MAIL_ATTACHMENT", false),
        smtp_server: env("XKCD_SMTP_SERVER")?,
        smtp_port: std::env::var("XKCD_SMTP_PORT")
            .unwrap_or_else(|_| "587".into())
            .parse()
            .context("XKCD_SMTP_PORT must be a number")?,
        smtp_starttls: env_bool("XKCD_SMTP_STARTTLS", true),
        smtp_username: std::env::var("XKCD_SMTP_USERNAME").ok(),
        smtp_password: std::env::var("XKCD_SMTP_PASSWORD").ok(),
    };

    if config.mail_attachment && !config.download {
        bail!("XKCD_DOWNLOAD must be true when XKCD_MAIL_ATTACHMENT is true");
    }
    if config.smtp_username.is_some() != config.smtp_password.is_some() {
        bail!("XKCD_SMTP_USERNAME and XKCD_SMTP_PASSWORD must both be set or both be unset");
    }

    Ok(config)
}

fn fetch_comic(client: &reqwest::blocking::Client) -> anyhow::Result<Comic> {
    let comic = client
        .get(XKCD_API_URL)
        .send()
        .context("failed to reach xkcd API")?
        .error_for_status()
        .context("xkcd API returned error status")?
        .json::<Comic>()
        .context("failed to parse xkcd JSON")?;
    log::debug!("fetched comic #{}: {}", comic.num, comic.safe_title);
    Ok(comic)
}

fn is_seen(comic: &Comic) -> anyhow::Result<bool> {
    let num_str = comic.num.to_string();
    match std::fs::read_to_string(HISTORY_FILE) {
        Ok(contents) => Ok(contents.lines().any(|line| line.trim() == num_str)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).context("failed to read history file"),
    }
}

fn local_filename(comic: &Comic) -> String {
    let basename = comic.img.rsplit('/').next().unwrap_or(comic.img.as_str());
    format!("{}-{}", comic.num, basename)
}

fn download_image(client: &reqwest::blocking::Client, comic: &Comic) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(COMIC_DIR).context("failed to create comics directory")?;

    let filename = local_filename(comic);
    let dest = Path::new(COMIC_DIR).join(&filename);

    let bytes = client
        .get(&comic.img)
        .send()
        .context("failed to fetch comic image")?
        .error_for_status()
        .context("image URL returned error status")?
        .bytes()
        .context("failed to read image bytes")?;

    std::fs::write(&dest, &bytes).with_context(|| format!("failed to write {}", dest.display()))?;

    log::info!("downloaded {filename}");
    Ok(dest)
}

fn format_date(comic: &Comic) -> anyhow::Result<String> {
    let year: i32 = comic.year.parse().context("invalid year")?;
    let month: u32 = comic.month.parse().context("invalid month")?;
    let day: u32 = comic.day.parse().context("invalid day")?;

    let date = chrono::NaiveDate::from_ymd_opt(year, month, day)
        .with_context(|| format!("invalid date {year}-{month}-{day}"))?;

    Ok(date.format("%a %d %b %y").to_string())
}

fn build_transport(config: &Config) -> anyhow::Result<SmtpTransport> {
    let builder = if config.smtp_starttls {
        SmtpTransport::starttls_relay(&config.smtp_server)
            .context("failed to create STARTTLS transport")?
    } else {
        SmtpTransport::relay(&config.smtp_server)
            .context("failed to create SMTP relay transport")?
    };

    let builder = builder.port(config.smtp_port);

    let builder = match (&config.smtp_username, &config.smtp_password) {
        (Some(user), Some(pass)) => {
            builder.credentials(Credentials::new(user.clone(), pass.clone()))
        }
        _ => builder,
    };

    Ok(builder.build())
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn send_email(
    config: &Config,
    comic: &Comic,
    attachment_path: Option<&Path>,
) -> anyhow::Result<()> {
    let date_str = format_date(comic)?;
    let subject = format!(
        "New xkcd {}: {} from {}",
        comic.num, comic.safe_title, date_str
    );
    let comic_url = format!("https://xkcd.com/{}/", comic.num);
    let plain_text = format!("{}\n{}\n\n{}", comic.safe_title, comic_url, comic.alt);
    let html_body = format!(
        r#"<html><body>
<h1><a href="{url}"><img title="{alt}" alt="{title}" style="display:block" src="{img}" /></a></h1>
<p>{alt}</p>
<br>
Mailed by <a href="https://github.com/bryanhiestand/ferrous-comics">ferrous-comics</a>
</body></html>"#,
        url = escape_html(&comic_url),
        img = escape_html(&comic.img),
        title = escape_html(&comic.safe_title),
        alt = escape_html(&comic.alt),
    );

    let alternative = MultiPart::alternative()
        .singlepart(SinglePart::plain(plain_text))
        .singlepart(SinglePart::html(html_body));

    let body = if let Some(path) = attachment_path {
        let filename = local_filename(comic);
        let image_bytes = std::fs::read(path)
            .with_context(|| format!("failed to read attachment {}", path.display()))?;
        let content_type = match path.extension().and_then(|e| e.to_str()) {
            Some("png") => ContentType::parse("image/png").unwrap(),
            Some("jpg") | Some("jpeg") => ContentType::parse("image/jpeg").unwrap(),
            Some("gif") => ContentType::parse("image/gif").unwrap(),
            _ => ContentType::parse("application/octet-stream").unwrap(),
        };
        MultiPart::mixed()
            .multipart(alternative)
            .singlepart(Attachment::new(filename).body(image_bytes, content_type))
    } else {
        alternative
    };

    let email = Message::builder()
        .from(
            config
                .mail_from
                .parse()
                .context("invalid XKCD_MAIL_FROM address")?,
        )
        .to(config
            .mail_to
            .parse()
            .context("invalid XKCD_MAIL_TO address")?)
        .subject(&subject)
        .multipart(body)
        .context("failed to build email message")?;

    let transport = build_transport(config)?;
    transport.send(&email).context("SMTP send failed")?;

    log::info!("emailed comic #{}: {}", comic.num, comic.safe_title);
    Ok(())
}

fn record_seen(comic: &Comic) -> anyhow::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(HISTORY_FILE)
        .context("failed to open history file for writing")?;
    writeln!(file, "{}", comic.num).context("failed to write to history file")?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let config = load_config()?;

    let client = reqwest::blocking::Client::builder()
        .user_agent("ferrous-comics/0.1")
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(60))
        .build()
        .context("failed to build HTTP client")?;

    let comic = fetch_comic(&client)?;

    if is_seen(&comic)? {
        log::info!("comic #{} already seen, exiting", comic.num);
        return Ok(());
    }

    let image_path = if config.download {
        Some(download_image(&client, &comic)?)
    } else {
        None
    };

    let attachment_path = if config.mail_attachment {
        image_path.as_deref()
    } else {
        None
    };

    send_email(&config, &comic, attachment_path)?;
    record_seen(&comic)?;

    Ok(())
}
