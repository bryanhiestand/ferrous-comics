mod config;
mod db;
mod email;
mod http;

use std::path::{Path, PathBuf};
use std::time::Duration;

use config::load_config;
use db::{
    cmd_dump, is_seen, last_seen_num, migrate_history_file, open_db, record_download_success,
    record_email_success, record_first_seen,
};
use email::send_email;
use http::{Comic, XKCD_BASE_URL, download_image, fetch_comic, fetch_comic_by_num};

const LEGACY_HISTORY_FILE: &str = "xkcd_history.txt";
const COMIC_DIR: &str = "comics";

fn cmd_version() {
    println!(
        "{} {} ({})",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        env!("GIT_HASH"),
    );
}

fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    if std::env::args().nth(1).as_deref() == Some("version") {
        cmd_version();
        return Ok(());
    }

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

    let latest = fetch_comic(&agent, XKCD_BASE_URL)?;

    // Determine which comic numbers to process this run, oldest first.
    // When there is no history and backfill is enabled, start far enough back so that
    // exactly backfill_limit comics are delivered on the first run.
    let last = last_seen_num(&db)?;
    let start = last.map(|n| n.saturating_add(1)).unwrap_or_else(|| {
        if config.backfill_limit > 0 {
            latest.num.saturating_sub(config.backfill_limit - 1)
        } else {
            latest.num
        }
    });
    let candidates: Vec<u32> = if config.backfill_limit > 0 {
        (start..=latest.num)
            .take(config.backfill_limit as usize)
            .collect()
    } else {
        vec![latest.num]
    };

    if candidates.is_empty() {
        log::info!("comic #{} already seen, exiting", latest.num);
        return Ok(());
    }

    for num in candidates {
        let comic = if num == latest.num {
            latest.clone()
        } else {
            match fetch_comic_by_num(&agent, XKCD_BASE_URL, num) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    // Comic intentionally doesn't exist (e.g. #404). Mark seen so we advance past it.
                    log::info!("comic #{num} does not exist, marking as seen");
                    record_first_seen(
                        &db,
                        &Comic {
                            num,
                            safe_title: String::new(),
                            img: String::new(),
                            alt: String::new(),
                            year: String::new(),
                            month: String::new(),
                            day: String::new(),
                        },
                    )?;
                    continue;
                }
                Err(e) => {
                    log::warn!("skipping comic #{num}: {e:#}");
                    continue;
                }
            }
        };

        if is_seen(&db, &comic)? {
            log::info!("comic #{} already seen, skipping", comic.num);
            continue;
        }

        // Mark seen immediately — prevents duplicate emails if a later step fails and cron retries.
        record_first_seen(&db, &comic)?;

        let image_path = if config.download {
            match download_image(&agent, &comic, Path::new(COMIC_DIR)) {
                Ok(path) => {
                    record_download_success(&db, comic.num)?;
                    Some(path)
                }
                Err(e) => {
                    log::warn!("failed to download comic #{}: {e:#}", comic.num);
                    continue;
                }
            }
        } else {
            None
        };

        let attachment_path = if config.mail_attachment {
            image_path.as_deref()
        } else {
            None
        };

        if let Err(e) = send_email(&config, &comic, attachment_path) {
            log::warn!("failed to email comic #{}: {e:#}", comic.num);
            continue;
        }
        record_email_success(&db, comic.num)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use db::{COMICS_TABLE, ComicRecord};
    use http::make_comic;

    /// Helper: compute backfill candidates using the same logic as main()
    fn backfill_candidates(last: Option<u32>, latest: u32, limit: u32) -> Vec<u32> {
        let start = last.map(|n| n.saturating_add(1)).unwrap_or_else(|| {
            if limit > 0 {
                latest.saturating_sub(limit - 1)
            } else {
                latest
            }
        });
        if limit > 0 {
            (start..=latest).take(limit as usize).collect()
        } else {
            vec![latest]
        }
    }

    // ── backfill candidate logic ──────────────────────────────────────────────

    #[test]
    fn backfill_candidates_empty_db() {
        // No prior history + backfill enabled → deliver limit comics ending at latest
        assert_eq!(backfill_candidates(None, 100, 5), vec![96, 97, 98, 99, 100]);
    }

    #[test]
    fn backfill_candidates_empty_db_limit_one() {
        // limit=1 must not underflow (100 - (1-1) = 100)
        assert_eq!(backfill_candidates(None, 100, 1), vec![100]);
    }

    #[test]
    fn backfill_candidates_up_to_date() {
        // Already seen the latest — empty list
        assert_eq!(backfill_candidates(Some(100), 100, 5), Vec::<u32>::new());
    }

    #[test]
    fn backfill_candidates_with_gap() {
        assert_eq!(backfill_candidates(Some(98), 101, 5), vec![99, 100, 101]);
    }

    #[test]
    fn backfill_candidates_capped() {
        // Gap of 11 but limit=3 → only first 3
        assert_eq!(backfill_candidates(Some(90), 101, 3), vec![91, 92, 93]);
    }

    #[test]
    fn backfill_candidates_limit_zero() {
        // limit=0 disables backfill — only latest
        assert_eq!(backfill_candidates(Some(98), 101, 0), vec![101]);
    }

    // ── backfill 404 marked seen ──────────────────────────────────────────────

    #[test]
    fn backfill_404_marked_seen() {
        // When fetch_comic_by_num returns Ok(None), the num should be recorded as seen
        // so the next run advances past it. We test this via the DB directly.
        let (_dir, db) = db::make_db();
        let placeholder = make_comic(404);
        record_first_seen(&db, &placeholder).unwrap();
        assert!(is_seen(&db, &placeholder).unwrap());
        // email_sent should be false — a 404 placeholder was never emailed
        let rtx = db.begin_read().unwrap();
        let table = rtx.open_table(COMICS_TABLE).unwrap();
        let json = table.get(404u32).unwrap().unwrap().value().to_owned();
        let rec: ComicRecord = serde_json::from_str(&json).unwrap();
        assert!(!rec.email_sent);
    }
}
