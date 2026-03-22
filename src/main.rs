use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use lettre::{
    message::{header::ContentType, Attachment, MultiPart, SinglePart},
    transport::smtp::authentication::Credentials,
    Message, SmtpTransport, Transport,
};
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

const XKCD_API_URL: &str = "https://xkcd.com/info.0.json";
const USER_AGENT: &str = "ferrous-comics/0.1";
const LEGACY_HISTORY_FILE: &str = "xkcd_history.txt";
const COMIC_DIR: &str = "comics";
const COMICS_TABLE: TableDefinition<u32, &str> = TableDefinition::new("comics");

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

#[derive(Debug, Serialize, Deserialize)]
struct ComicRecord {
    num: u32,
    /// Unix timestamp (seconds). 0 means migrated from legacy file — timestamp unknown.
    first_seen_utc: i64,
    image_downloaded: bool,
    email_sent: bool,
    /// Unix timestamp of successful email send, if any.
    email_sent_utc: Option<i64>,
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
    db_path: PathBuf,
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
        db_path: std::env::var("XKCD_DB_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("xkcd_comics.db")),
    };

    if config.mail_attachment && !config.download {
        bail!("XKCD_DOWNLOAD must be true when XKCD_MAIL_ATTACHMENT is true");
    }
    if config.smtp_username.is_some() != config.smtp_password.is_some() {
        bail!("XKCD_SMTP_USERNAME and XKCD_SMTP_PASSWORD must both be set or both be unset");
    }

    Ok(config)
}

fn open_db(path: &Path) -> anyhow::Result<Database> {
    Database::create(path).with_context(|| format!("failed to open database at {}", path.display()))
}

/// One-time migration: imports comic numbers from the legacy xkcd_history.txt into the database,
/// then renames the file to xkcd_history.txt.migrated. No-op if the file does not exist.
fn migrate_history_file(db: &Database) -> anyhow::Result<()> {
    let path = PathBuf::from(LEGACY_HISTORY_FILE);
    if !path.exists() {
        return Ok(());
    }

    let contents = std::fs::read_to_string(&path)
        .context("failed to read legacy history file for migration")?;
    let nums: Vec<u32> = contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().parse::<u32>())
        .collect::<Result<_, _>>()
        .context("invalid comic number in legacy history file")?;

    let count = nums.len();
    let wtx = db
        .begin_write()
        .context("failed to begin migration write transaction")?;
    {
        let mut table = wtx
            .open_table(COMICS_TABLE)
            .context("failed to open comics table for migration")?;
        for num in nums {
            if table
                .get(num)
                .context("failed to query comics table")?
                .is_none()
            {
                let record = ComicRecord {
                    num,
                    first_seen_utc: 0, // unknown — sentinel for migrated entries
                    image_downloaded: false,
                    email_sent: true, // assume sent — was in history
                    email_sent_utc: None,
                };
                let json = serde_json::to_string(&record)
                    .context("failed to serialize migrated record")?;
                table
                    .insert(num, json.as_str())
                    .context("failed to insert migrated record")?;
            }
        }
    }
    wtx.commit()
        .context("failed to commit migration transaction")?;

    let migrated_path = format!("{}.migrated", LEGACY_HISTORY_FILE);
    std::fs::rename(&path, &migrated_path)
        .context("failed to rename legacy history file after migration")?;
    log::info!(
        "migrated {count} comics from {LEGACY_HISTORY_FILE} to database (backup: {migrated_path})"
    );

    Ok(())
}

fn is_seen(db: &Database, comic: &Comic) -> anyhow::Result<bool> {
    let rtx = db
        .begin_read()
        .context("failed to begin read transaction")?;
    match rtx.open_table(COMICS_TABLE) {
        Ok(table) => Ok(table
            .get(comic.num)
            .context("failed to query comics table")?
            .is_some()),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(false),
        Err(e) => Err(e).context("failed to open comics table"),
    }
}

/// Records a comic as seen immediately, before attempting download or email.
/// This prevents duplicate emails if the process is re-run after a partial failure.
fn record_first_seen(db: &Database, comic: &Comic) -> anyhow::Result<()> {
    let record = ComicRecord {
        num: comic.num,
        first_seen_utc: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
        image_downloaded: false,
        email_sent: false,
        email_sent_utc: None,
    };
    let json = serde_json::to_string(&record).context("failed to serialize comic record")?;
    let wtx = db
        .begin_write()
        .context("failed to begin write transaction")?;
    {
        let mut table = wtx
            .open_table(COMICS_TABLE)
            .context("failed to open comics table")?;
        table
            .insert(comic.num, json.as_str())
            .context("failed to insert comic record")?;
    }
    wtx.commit().context("failed to commit comic record")?;
    Ok(())
}

fn record_download_success(db: &Database, num: u32) -> anyhow::Result<()> {
    update_record(db, num, |r| r.image_downloaded = true)
}

fn record_email_success(db: &Database, num: u32) -> anyhow::Result<()> {
    update_record(db, num, |r| {
        r.email_sent = true;
        r.email_sent_utc = Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
        );
    })
}

fn update_record(db: &Database, num: u32, f: impl FnOnce(&mut ComicRecord)) -> anyhow::Result<()> {
    let wtx = db
        .begin_write()
        .context("failed to begin write transaction")?;
    {
        let mut table = wtx
            .open_table(COMICS_TABLE)
            .context("failed to open comics table")?;
        let json_str = table
            .get(num)
            .context("failed to query comics table")?
            .with_context(|| format!("comic #{num} not found in database for update"))?
            .value()
            .to_owned();
        let mut record: ComicRecord =
            serde_json::from_str(&json_str).context("failed to deserialize comic record")?;
        f(&mut record);
        let json = serde_json::to_string(&record).context("failed to serialize comic record")?;
        table
            .insert(num, json.as_str())
            .context("failed to update comic record")?;
    }
    wtx.commit()
        .context("failed to commit comic record update")?;
    Ok(())
}

fn fetch_comic(agent: &ureq::Agent) -> anyhow::Result<Comic> {
    let comic = agent
        .get(XKCD_API_URL)
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => anyhow::anyhow!("xkcd API returned HTTP {code}"),
            ureq::Error::Transport(t) => anyhow::anyhow!("failed to reach xkcd API: {t}"),
        })?
        .into_json::<Comic>()
        .context("failed to parse xkcd JSON")?;
    log::debug!("fetched comic #{}: {}", comic.num, comic.safe_title);
    Ok(comic)
}

fn local_filename(comic: &Comic) -> String {
    let basename = comic.img.rsplit('/').next().unwrap_or(comic.img.as_str());
    format!("{}-{}", comic.num, basename)
}

fn download_image(agent: &ureq::Agent, comic: &Comic) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(COMIC_DIR).context("failed to create comics directory")?;

    let filename = local_filename(comic);
    let dest = Path::new(COMIC_DIR).join(&filename);

    let mut bytes = Vec::new();
    agent
        .get(&comic.img)
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => anyhow::anyhow!("image URL returned HTTP {code}"),
            ureq::Error::Transport(t) => anyhow::anyhow!("failed to fetch comic image: {t}"),
        })?
        .into_reader()
        .read_to_end(&mut bytes)
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

/// Prints all comic records in the database as newline-delimited JSON, sorted by comic number.
fn cmd_dump(db: &Database) -> anyhow::Result<()> {
    let rtx = db
        .begin_read()
        .context("failed to begin read transaction")?;
    match rtx.open_table(COMICS_TABLE) {
        Ok(table) => {
            for entry in table.iter().context("failed to iterate comics table")? {
                let (_, value) = entry.context("failed to read comics table entry")?;
                println!("{}", value.value());
            }
            Ok(())
        }
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(()), // empty database
        Err(e) => Err(e).context("failed to open comics table"),
    }
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let config = load_config()?;

    let db = open_db(&config.db_path)?;

    if std::env::args().nth(1).as_deref() == Some("dump") {
        return cmd_dump(&db);
    }

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(30))
        .timeout(Duration::from_secs(60))
        .build();

    let comic = fetch_comic(&agent)?;

    migrate_history_file(&db)?;

    if is_seen(&db, &comic)? {
        log::info!("comic #{} already seen, exiting", comic.num);
        return Ok(());
    }

    // Mark seen immediately — prevents duplicate emails if a later step fails and cron retries.
    record_first_seen(&db, &comic)?;

    let image_path = if config.download {
        let path = download_image(&agent, &comic)?;
        record_download_success(&db, comic.num)?;
        Some(path)
    } else {
        None
    };

    let attachment_path = if config.mail_attachment {
        image_path.as_deref()
    } else {
        None
    };

    send_email(&config, &comic, attachment_path)?;
    record_email_success(&db, comic.num)?;

    Ok(())
}
