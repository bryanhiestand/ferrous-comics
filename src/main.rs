use std::io::{Read, Write};
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

const XKCD_BASE_URL: &str = "https://xkcd.com";
const USER_AGENT: &str = concat!("ferrous-comics/", env!("CARGO_PKG_VERSION"));
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
    mail_to: Vec<String>,
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
        mail_to: env("XKCD_MAIL_TO")?
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
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

    validate_config(&config)?;
    Ok(config)
}

fn validate_config(config: &Config) -> anyhow::Result<()> {
    if config.mail_to.is_empty() {
        bail!("XKCD_MAIL_TO must contain at least one address");
    }
    if config.mail_attachment && !config.download {
        bail!("XKCD_DOWNLOAD must be true when XKCD_MAIL_ATTACHMENT is true");
    }
    if config.smtp_username.is_some() != config.smtp_password.is_some() {
        bail!("XKCD_SMTP_USERNAME and XKCD_SMTP_PASSWORD must both be set or both be unset");
    }
    Ok(())
}

fn open_db(path: &Path) -> anyhow::Result<Database> {
    Database::create(path).with_context(|| format!("failed to open database at {}", path.display()))
}

/// One-time migration: imports comic numbers from a legacy history file into the database,
/// then renames the file to `<path>.migrated`. No-op if the file does not exist.
fn migrate_history_file(db: &Database, path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let contents = std::fs::read_to_string(path)
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

    let migrated_path = PathBuf::from(format!("{}.migrated", path.display()));
    std::fs::rename(path, &migrated_path)
        .context("failed to rename legacy history file after migration")?;
    log::info!(
        "migrated {count} comics from {} to database (backup: {})",
        path.display(),
        migrated_path.display()
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

fn fetch_comic(agent: &ureq::Agent, base_url: &str) -> anyhow::Result<Comic> {
    let url = format!("{}/info.0.json", base_url.trim_end_matches('/'));
    let comic = agent
        .get(&url)
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => anyhow::anyhow!("xkcd API returned HTTP {code}"),
            ureq::Error::Transport(t) => anyhow::Error::new(t).context("failed to reach xkcd API"),
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

fn download_image(agent: &ureq::Agent, comic: &Comic, dest_dir: &Path) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dest_dir).context("failed to create comics directory")?;

    let filename = local_filename(comic);
    let dest = dest_dir.join(&filename);

    let mut bytes = Vec::new();
    agent
        .get(&comic.img)
        .set("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => anyhow::anyhow!("image URL returned HTTP {code}"),
            ureq::Error::Transport(t) => {
                anyhow::Error::new(t).context("failed to fetch comic image")
            }
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

fn build_email(
    config: &Config,
    comic: &Comic,
    attachment_path: Option<&Path>,
) -> anyhow::Result<Message> {
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

    let mut builder = Message::builder().from(
        config
            .mail_from
            .parse()
            .context("invalid XKCD_MAIL_FROM address")?,
    );
    for addr in &config.mail_to {
        builder = builder.to(addr
            .parse()
            .with_context(|| format!("invalid XKCD_MAIL_TO address: {addr}"))?);
    }
    builder
        .subject(&subject)
        .multipart(body)
        .context("failed to build email message")
}

fn send_email(
    config: &Config,
    comic: &Comic,
    attachment_path: Option<&Path>,
) -> anyhow::Result<()> {
    let email = build_email(config, comic, attachment_path)?;
    let transport = build_transport(config)?;
    transport.send(&email).context("SMTP send failed")?;
    log::info!("emailed comic #{}: {}", comic.num, comic.safe_title);
    Ok(())
}

/// Prints all comic records in the database as newline-delimited JSON, sorted by comic number.
fn cmd_dump(db: &Database, out: &mut impl Write) -> anyhow::Result<()> {
    let rtx = db
        .begin_read()
        .context("failed to begin read transaction")?;
    match rtx.open_table(COMICS_TABLE) {
        Ok(table) => {
            for entry in table.iter().context("failed to iterate comics table")? {
                let (_, value) = entry.context("failed to read comics table entry")?;
                writeln!(out, "{}", value.value()).context("failed to write output")?;
            }
            Ok(())
        }
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(()), // empty database
        Err(e) => Err(e).context("failed to open comics table"),
    }
}

fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let db_path = std::env::var("XKCD_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("xkcd_comics.db"));
    let db = open_db(&db_path)?;

    migrate_history_file(&db, Path::new(LEGACY_HISTORY_FILE))?;

    if std::env::args().nth(1).as_deref() == Some("dump") {
        return cmd_dump(&db, &mut std::io::stdout());
    }

    let config = load_config()?;

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(30))
        .timeout(Duration::from_secs(60))
        .build();

    let comic = fetch_comic(&agent, XKCD_BASE_URL)?;

    if is_seen(&db, &comic)? {
        log::info!("comic #{} already seen, exiting", comic.num);
        return Ok(());
    }

    // Mark seen immediately — prevents duplicate emails if a later step fails and cron retries.
    record_first_seen(&db, &comic)?;

    let image_path = if config.download {
        let path = download_image(&agent, &comic, Path::new(COMIC_DIR))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn make_db() -> (TempDir, Database) {
        let dir = TempDir::new().unwrap();
        let db = Database::create(dir.path().join("test.db")).unwrap();
        (dir, db)
    }

    fn make_comic(num: u32) -> Comic {
        Comic {
            num,
            safe_title: "Test Comic".to_string(),
            img: "https://imgs.xkcd.com/comics/test.png".to_string(),
            alt: "Alt text here".to_string(),
            year: "2025".to_string(),
            month: "3".to_string(),
            day: "15".to_string(),
        }
    }

    fn make_config() -> Config {
        Config {
            mail_to: vec!["to@example.com".to_string()],
            mail_from: "from@example.com".to_string(),
            download: true,
            mail_attachment: false,
            smtp_server: "smtp.example.com".to_string(),
            smtp_port: 587,
            smtp_starttls: true,
            smtp_username: None,
            smtp_password: None,
        }
    }

    // ── escape_html ───────────────────────────────────────────────────────────

    #[test]
    fn escape_html_basic() {
        assert_eq!(escape_html("& < > \" '"), "&amp; &lt; &gt; &quot; &#39;");
    }

    #[test]
    fn escape_html_no_change() {
        let s = "plain string with no special chars";
        assert_eq!(escape_html(s), s);
    }

    // ── local_filename ────────────────────────────────────────────────────────

    #[test]
    fn local_filename_strips_path() {
        let mut c = make_comic(123);
        c.img = "https://imgs.xkcd.com/comics/foo.png".to_string();
        assert_eq!(local_filename(&c), "123-foo.png");
    }

    #[test]
    fn local_filename_no_slash() {
        let mut c = make_comic(123);
        c.img = "bare".to_string();
        assert_eq!(local_filename(&c), "123-bare");
    }

    // ── format_date ───────────────────────────────────────────────────────────

    #[test]
    fn format_date_valid() {
        let c = make_comic(1);
        assert_eq!(format_date(&c).unwrap(), "Sat 15 Mar 25");
    }

    #[test]
    fn format_date_invalid_month() {
        let mut c = make_comic(1);
        c.month = "13".to_string();
        assert!(format_date(&c).is_err());
    }

    // ── validate_config ───────────────────────────────────────────────────────

    #[test]
    fn config_attachment_requires_download() {
        let mut cfg = make_config();
        cfg.download = false;
        cfg.mail_attachment = true;
        assert!(validate_config(&cfg).is_err());
    }

    #[test]
    fn config_username_without_password() {
        let mut cfg = make_config();
        cfg.smtp_username = Some("user".to_string());
        cfg.smtp_password = None;
        assert!(validate_config(&cfg).is_err());
    }

    #[test]
    fn config_password_without_username() {
        let mut cfg = make_config();
        cfg.smtp_username = None;
        cfg.smtp_password = Some("pass".to_string());
        assert!(validate_config(&cfg).is_err());
    }

    #[test]
    fn config_username_and_password_both_ok() {
        let mut cfg = make_config();
        cfg.smtp_username = Some("user".to_string());
        cfg.smtp_password = Some("pass".to_string());
        assert!(validate_config(&cfg).is_ok());
    }

    #[test]
    fn config_neither_credentials_ok() {
        let cfg = make_config();
        assert!(validate_config(&cfg).is_ok());
    }

    #[test]
    fn config_mail_to_single() {
        let mut cfg = make_config();
        cfg.mail_to = vec!["a@example.com".to_string()];
        assert!(validate_config(&cfg).is_ok());
        assert_eq!(cfg.mail_to.len(), 1);
    }

    #[test]
    fn config_mail_to_multiple() {
        // Parsing logic lives in load_config; test it via the split/trim/filter chain directly
        let raw = "a@example.com, b@example.com";
        let parsed: Vec<String> = raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(parsed, vec!["a@example.com", "b@example.com"]);
    }

    #[test]
    fn config_mail_to_trims_whitespace() {
        let raw = "  a@example.com  ,  b@example.com  ";
        let parsed: Vec<String> = raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(parsed, vec!["a@example.com", "b@example.com"]);
    }

    #[test]
    fn config_mail_to_empty_errors() {
        let mut cfg = make_config();
        cfg.mail_to = vec![];
        assert!(validate_config(&cfg).is_err());
    }

    // ── Database ──────────────────────────────────────────────────────────────

    #[test]
    fn is_seen_empty_db() {
        let (_dir, db) = make_db();
        let comic = make_comic(42);
        assert!(!is_seen(&db, &comic).unwrap());
    }

    #[test]
    fn is_seen_after_record() {
        let (_dir, db) = make_db();
        let comic = make_comic(42);
        record_first_seen(&db, &comic).unwrap();
        assert!(is_seen(&db, &comic).unwrap());
    }

    #[test]
    fn record_first_seen_fields() {
        let (_dir, db) = make_db();
        let comic = make_comic(100);
        record_first_seen(&db, &comic).unwrap();

        let rtx = db.begin_read().unwrap();
        let table = rtx.open_table(COMICS_TABLE).unwrap();
        let json = table.get(100u32).unwrap().unwrap().value().to_owned();
        let rec: ComicRecord = serde_json::from_str(&json).unwrap();

        assert_eq!(rec.num, 100);
        assert!(!rec.email_sent);
        assert!(!rec.image_downloaded);
    }

    #[test]
    fn record_download_success_sets_flag() {
        let (_dir, db) = make_db();
        let comic = make_comic(7);
        record_first_seen(&db, &comic).unwrap();
        record_download_success(&db, 7).unwrap();

        let rtx = db.begin_read().unwrap();
        let table = rtx.open_table(COMICS_TABLE).unwrap();
        let json = table.get(7u32).unwrap().unwrap().value().to_owned();
        let rec: ComicRecord = serde_json::from_str(&json).unwrap();
        assert!(rec.image_downloaded);
    }

    #[test]
    fn record_email_success_sets_flag() {
        let (_dir, db) = make_db();
        let comic = make_comic(8);
        record_first_seen(&db, &comic).unwrap();
        record_email_success(&db, 8).unwrap();

        let rtx = db.begin_read().unwrap();
        let table = rtx.open_table(COMICS_TABLE).unwrap();
        let json = table.get(8u32).unwrap().unwrap().value().to_owned();
        let rec: ComicRecord = serde_json::from_str(&json).unwrap();
        assert!(rec.email_sent);
        assert!(rec.email_sent_utc.is_some());
    }

    #[test]
    fn cmd_dump_output() {
        let (_dir, db) = make_db();
        // Insert in descending order to prove dump outputs in ascending key order, not insertion order
        let c2 = make_comic(2);
        let c1 = make_comic(1);
        record_first_seen(&db, &c2).unwrap();
        record_first_seen(&db, &c1).unwrap();

        let mut out = Vec::new();
        cmd_dump(&db, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].contains("\"num\":1"),
            "expected num:1 first (ascending order)"
        );
        assert!(
            lines[1].contains("\"num\":2"),
            "expected num:2 second (ascending order)"
        );
    }

    #[test]
    fn cmd_dump_empty_db() {
        let (_dir, db) = make_db();
        let mut out = Vec::new();
        cmd_dump(&db, &mut out).unwrap();
        assert!(out.is_empty());
    }

    // ── migrate_history_file ──────────────────────────────────────────────────

    #[test]
    fn migrate_noop_if_no_file() {
        let (_dir, db) = make_db();
        let result = migrate_history_file(&db, Path::new("/nonexistent/path/history.txt"));
        assert!(result.is_ok());
    }

    #[test]
    fn migrate_imports_records() {
        let dir = TempDir::new().unwrap();
        let (_dbdir, db) = make_db();
        let history = dir.path().join("xkcd_history.txt");
        std::fs::write(&history, "100\n200\n300\n").unwrap();

        migrate_history_file(&db, &history).unwrap();

        // File should be renamed
        assert!(!history.exists());
        assert!(dir.path().join("xkcd_history.txt.migrated").exists());

        // Records should be in db with correct sentinel fields
        let rtx = db.begin_read().unwrap();
        let table = rtx.open_table(COMICS_TABLE).unwrap();
        for num in [100u32, 200, 300] {
            let json = table.get(num).unwrap().unwrap().value().to_owned();
            let rec: ComicRecord = serde_json::from_str(&json).unwrap();
            assert_eq!(rec.num, num);
            assert!(
                rec.email_sent,
                "migrated record should have email_sent=true"
            );
            assert_eq!(
                rec.first_seen_utc, 0,
                "migrated record should have first_seen_utc=0"
            );
        }
    }

    #[test]
    fn migrate_skips_duplicates() {
        let dir = TempDir::new().unwrap();
        let (_dbdir, db) = make_db();

        // Pre-insert comic 100 with email_sent=false
        let comic = make_comic(100);
        record_first_seen(&db, &comic).unwrap();

        let history = dir.path().join("xkcd_history.txt");
        std::fs::write(&history, "100\n").unwrap();

        migrate_history_file(&db, &history).unwrap();

        // The existing record should NOT be overwritten (email_sent stays false)
        let rtx = db.begin_read().unwrap();
        let table = rtx.open_table(COMICS_TABLE).unwrap();
        let json = table.get(100u32).unwrap().unwrap().value().to_owned();
        let rec: ComicRecord = serde_json::from_str(&json).unwrap();
        assert!(!rec.email_sent); // was false before migration, should still be false
    }

    #[test]
    fn migrate_invalid_line() {
        let dir = TempDir::new().unwrap();
        let (_dbdir, db) = make_db();
        let history = dir.path().join("xkcd_history.txt");
        std::fs::write(&history, "not_a_number\n").unwrap();

        let result = migrate_history_file(&db, &history);
        assert!(result.is_err());
    }

    // ── fetch_comic ───────────────────────────────────────────────────────────

    #[test]
    fn fetch_comic_success() {
        let mut server = mockito::Server::new();
        let body = r#"{"num":3222,"safe_title":"Test","img":"https://imgs.xkcd.com/comics/test.png","alt":"Alt","year":"2025","month":"3","day":"15","title":"Test"}"#;
        let _m = server
            .mock("GET", "/info.0.json")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body)
            .create();

        let agent = ureq::AgentBuilder::new().build();
        let comic = fetch_comic(&agent, &server.url()).unwrap();
        assert_eq!(comic.num, 3222);
        assert_eq!(comic.safe_title, "Test");
    }

    #[test]
    fn fetch_comic_http_error() {
        let mut server = mockito::Server::new();
        let _m = server.mock("GET", "/info.0.json").with_status(500).create();

        let agent = ureq::AgentBuilder::new().build();
        let err = fetch_comic(&agent, &server.url()).unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    #[test]
    fn fetch_comic_bad_json() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/info.0.json")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("not json")
            .create();

        let agent = ureq::AgentBuilder::new().build();
        let result = fetch_comic(&agent, &server.url());
        assert!(result.is_err());
    }

    // ── download_image ────────────────────────────────────────────────────────

    #[test]
    fn download_image_success() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/comics/test.png")
            .with_status(200)
            .with_header("content-type", "image/png")
            .with_body(b"fakepngbytes".as_ref())
            .create();

        let mut comic = make_comic(99);
        comic.img = format!("{}/comics/test.png", server.url());

        let dest_dir = TempDir::new().unwrap();
        let agent = ureq::AgentBuilder::new().build();
        let path = download_image(&agent, &comic, dest_dir.path()).unwrap();

        assert!(path.exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"fakepngbytes");
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), "99-test.png");
    }

    #[test]
    fn download_image_http_error() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/comics/notfound.png")
            .with_status(404)
            .create();

        let mut comic = make_comic(99);
        comic.img = format!("{}/comics/notfound.png", server.url());

        let dest_dir = TempDir::new().unwrap();
        let agent = ureq::AgentBuilder::new().build();
        let result = download_image(&agent, &comic, dest_dir.path());
        assert!(result.is_err());
    }

    // ── build_email ───────────────────────────────────────────────────────────

    fn email_bytes(config: &Config, comic: &Comic, attachment: Option<&Path>) -> String {
        let msg = build_email(config, comic, attachment).unwrap();
        String::from_utf8(msg.formatted()).unwrap()
    }

    #[test]
    fn email_subject_format() {
        let config = make_config();
        let comic = make_comic(3222);
        let raw = email_bytes(&config, &comic, None);
        assert!(raw.contains("3222"));
        assert!(raw.contains("Test Comic"));
    }

    #[test]
    fn email_html_contains_title() {
        let config = make_config();
        let comic = make_comic(1);
        let raw = email_bytes(&config, &comic, None);
        assert!(raw.contains("Test Comic"));
        assert!(raw.contains("https://xkcd.com/1/"));
    }

    #[test]
    fn email_html_escapes_special_chars() {
        let config = make_config();
        let mut comic = make_comic(1);
        comic.safe_title = "<script>".to_string();
        let raw = email_bytes(&config, &comic, None);
        // HTML body is quoted-printable encoded; &lt; may be split across lines.
        // Check that the escape entity prefix appears (confirms < was escaped).
        assert!(raw.contains("&lt"), "expected &lt in HTML body");
        assert!(raw.contains("&gt"), "expected &gt in HTML body");
    }

    #[test]
    fn email_plain_text_contains_url() {
        let config = make_config();
        let comic = make_comic(42);
        let raw = email_bytes(&config, &comic, None);
        assert!(raw.contains("https://xkcd.com/42/"));
        assert!(raw.contains("Alt text here"));
    }

    #[test]
    fn email_no_attachment_is_alternative() {
        let config = make_config();
        let comic = make_comic(1);
        let raw = email_bytes(&config, &comic, None);
        assert!(raw.contains("multipart/alternative"));
    }

    #[test]
    fn email_with_attachment_is_mixed() {
        let dir = TempDir::new().unwrap();
        let img_path = dir.path().join("1-test.png");
        std::fs::write(&img_path, b"fakepng").unwrap();

        let config = make_config();
        let comic = make_comic(1);
        let raw = email_bytes(&config, &comic, Some(&img_path));
        assert!(raw.contains("multipart/mixed"));
    }

    #[test]
    fn email_from_to_addresses() {
        let config = make_config();
        let comic = make_comic(1);
        let raw = email_bytes(&config, &comic, None);
        assert!(raw.contains("to@example.com"));
        assert!(raw.contains("from@example.com"));
    }

    #[test]
    fn email_multiple_recipients() {
        let mut config = make_config();
        config.mail_to = vec!["a@example.com".to_string(), "b@example.com".to_string()];
        let comic = make_comic(1);
        let raw = email_bytes(&config, &comic, None);
        assert!(raw.contains("a@example.com"), "first recipient missing");
        assert!(raw.contains("b@example.com"), "second recipient missing");
    }
}
