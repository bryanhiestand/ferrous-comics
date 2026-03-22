use std::path::Path;

use anyhow::Context;
use lettre::{
    message::{header::ContentType, Attachment, MultiPart, SinglePart},
    transport::smtp::authentication::Credentials,
    Message, SmtpTransport, Transport,
};

use crate::config::Config;
use crate::http::{local_filename, Comic};

pub(crate) fn escape_html(s: &str) -> String {
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

pub(crate) fn format_date(comic: &Comic) -> anyhow::Result<String> {
    let year: i32 = comic.year.parse().context("invalid year")?;
    let month: u32 = comic.month.parse().context("invalid month")?;
    let day: u32 = comic.day.parse().context("invalid day")?;

    let date = chrono::NaiveDate::from_ymd_opt(year, month, day)
        .with_context(|| format!("invalid date {year}-{month}-{day}"))?;

    Ok(date.format("%a %d %b %y").to_string())
}

pub(crate) fn build_email(
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
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<style>
  body{{font-family:sans-serif;max-width:640px;margin:0 auto;padding:16px;background:#fff;color:#222}}
  h1{{font-size:1.1em;margin:0 0 4px 0}}
  .meta{{color:#666;font-size:0.85em;margin-bottom:14px}}
  .comic img{{max-width:100%;height:auto;display:block}}
  .alt{{background:#f5f5f5;border-left:3px solid #999;padding:8px 12px;margin:12px 0;font-style:italic;font-size:0.95em}}
  .footer{{font-size:0.8em;color:#aaa;margin-top:20px;border-top:1px solid #eee;padding-top:8px}}
  a{{color:#1a0dab}}
</style>
</head>
<body>
<h1><a href="{url}">{title}</a></h1>
<div class="meta">#{num} &middot; {date}</div>
<div class="comic"><a href="{url}"><img src="{img}" alt="{alt}" title="{title}" style="max-width:100%;display:block"></a></div>
<div class="alt">{alt}</div>
<div class="footer">Mailed by <a href="https://github.com/bryanhiestand/ferrous-comics">ferrous-comics</a></div>
</body>
</html>"#,
        url = escape_html(&comic_url),
        img = escape_html(&comic.img),
        title = escape_html(&comic.safe_title),
        alt = escape_html(&comic.alt),
        num = comic.num,
        date = escape_html(&date_str),
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

pub fn send_email(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::make_config;
    use crate::http::make_comic;

    fn email_bytes(config: &Config, comic: &Comic, attachment: Option<&Path>) -> String {
        let msg = build_email(config, comic, attachment).unwrap();
        String::from_utf8(msg.formatted()).unwrap()
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

    // ── build_email ───────────────────────────────────────────────────────────

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
        let dir = tempfile::TempDir::new().unwrap();
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
    fn email_html_has_meta_charset() {
        let config = make_config();
        let comic = make_comic(1);
        let raw = email_bytes(&config, &comic, None);
        // "<meta charset" appears before the = so is not QP-encoded
        assert!(raw.contains("<meta charset"), "meta charset missing");
    }

    #[test]
    fn email_html_title_is_text_link() {
        let config = make_config();
        let comic = make_comic(1);
        let raw = email_bytes(&config, &comic, None);
        // h1 contains the title as link text, not wrapped in an img tag
        assert!(
            raw.contains("Test Comic</a></h1>"),
            "title not rendered as h1 link text"
        );
    }

    #[test]
    fn email_html_alt_title_attributes() {
        let config = make_config();
        let mut comic = make_comic(1);
        comic.safe_title = "SafeTitle".to_string();
        comic.alt = "AltText".to_string();
        let raw = email_bytes(&config, &comic, None);
        // QP encodes `=` as `=3D`; check both forms to be safe
        let has_correct_alt = raw.contains("alt=3D\"AltText\"") || raw.contains("alt=\"AltText\"");
        let has_correct_title =
            raw.contains("title=3D\"SafeTitle\"") || raw.contains("title=\"SafeTitle\"");
        assert!(
            has_correct_alt,
            "img alt attribute does not contain alt text"
        );
        assert!(
            has_correct_title,
            "img title attribute does not contain safe_title"
        );
    }

    #[test]
    fn email_html_meta_line() {
        let config = make_config();
        let comic = make_comic(42);
        let raw = email_bytes(&config, &comic, None);
        assert!(raw.contains("#42"), "comic number missing from meta line");
        assert!(
            raw.contains("&middot;"),
            "middot separator missing from meta line"
        );
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
