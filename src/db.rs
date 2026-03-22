use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::http::Comic;

pub(crate) const COMICS_TABLE: TableDefinition<u32, &str> = TableDefinition::new("comics");

#[derive(Debug, Serialize, Deserialize)]
pub struct ComicRecord {
    pub num: u32,
    /// Unix timestamp (seconds). 0 means migrated from legacy file — timestamp unknown.
    pub first_seen_utc: i64,
    pub image_downloaded: bool,
    pub email_sent: bool,
    /// Unix timestamp of successful email send, if any.
    pub email_sent_utc: Option<i64>,
}

pub fn open_db(path: &Path) -> anyhow::Result<Database> {
    Database::create(path).with_context(|| format!("failed to open database at {}", path.display()))
}

/// One-time migration: imports comic numbers from a legacy history file into the database,
/// then renames the file to `<path>.migrated`. No-op if the file does not exist.
pub fn migrate_history_file(db: &Database, path: &Path) -> anyhow::Result<()> {
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

pub fn is_seen(db: &Database, comic: &Comic) -> anyhow::Result<bool> {
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
pub fn record_first_seen(db: &Database, comic: &Comic) -> anyhow::Result<()> {
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

pub fn record_download_success(db: &Database, num: u32) -> anyhow::Result<()> {
    update_record(db, num, |r| r.image_downloaded = true)
}

pub fn record_email_success(db: &Database, num: u32) -> anyhow::Result<()> {
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

/// Returns the highest comic number recorded in the database, or `None` if empty.
pub fn last_seen_num(db: &Database) -> anyhow::Result<Option<u32>> {
    let rtx = db
        .begin_read()
        .context("failed to begin read transaction")?;
    match rtx.open_table(COMICS_TABLE) {
        Ok(table) => Ok(table
            .last()
            .context("failed to read last entry from comics table")?
            .map(|(k, _)| k.value())),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
        Err(e) => Err(e).context("failed to open comics table"),
    }
}

/// Prints all comic records in the database as newline-delimited JSON, sorted by comic number.
pub fn cmd_dump(db: &Database, out: &mut impl Write) -> anyhow::Result<()> {
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

#[cfg(test)]
pub(crate) fn make_db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::TempDir::new().unwrap();
    let db = Database::create(dir.path().join("test.db")).unwrap();
    (dir, db)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::make_comic;

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
        let dir = tempfile::TempDir::new().unwrap();
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
        let dir = tempfile::TempDir::new().unwrap();
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
        let dir = tempfile::TempDir::new().unwrap();
        let (_dbdir, db) = make_db();
        let history = dir.path().join("xkcd_history.txt");
        std::fs::write(&history, "not_a_number\n").unwrap();

        let result = migrate_history_file(&db, &history);
        assert!(result.is_err());
    }

    // ── last_seen_num ─────────────────────────────────────────────────────────

    #[test]
    fn last_seen_num_empty() {
        let (_dir, db) = make_db();
        assert_eq!(last_seen_num(&db).unwrap(), None);
    }

    #[test]
    fn last_seen_num_multiple() {
        let (_dir, db) = make_db();
        // Insert out of order to confirm max, not insertion-last
        record_first_seen(&db, &make_comic(3)).unwrap();
        record_first_seen(&db, &make_comic(1)).unwrap();
        record_first_seen(&db, &make_comic(2)).unwrap();
        assert_eq!(last_seen_num(&db).unwrap(), Some(3));
    }
}
