use anyhow::{Context, bail};

#[derive(Debug)]
pub struct Config {
    pub mail_to: Vec<String>,
    pub mail_from: String,
    pub download: bool,
    pub mail_attachment: bool,
    pub smtp_server: String,
    pub smtp_port: u16,
    pub smtp_starttls: bool,
    pub smtp_username: Option<String>,
    pub smtp_password: Option<String>,
    pub backfill_limit: u32,
}

pub fn load_config() -> anyhow::Result<Config> {
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
        backfill_limit: std::env::var("XKCD_BACKFILL_LIMIT")
            .unwrap_or_else(|_| "5".into())
            .parse()
            .context("XKCD_BACKFILL_LIMIT must be a non-negative integer")?,
    };

    validate_config(&config)?;
    Ok(config)
}

pub fn validate_config(config: &Config) -> anyhow::Result<()> {
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

#[cfg(test)]
pub(crate) fn make_config() -> Config {
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
        backfill_limit: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
