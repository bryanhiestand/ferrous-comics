use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

pub(crate) const XKCD_BASE_URL: &str = "https://xkcd.com";
pub(crate) const USER_AGENT: &str = concat!("ferrous-comics/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Deserialize)]
pub struct Comic {
    pub num: u32,
    pub safe_title: String,
    pub img: String,
    pub alt: String,
    pub year: String,
    pub month: String,
    pub day: String,
}

pub fn fetch_comic(agent: &ureq::Agent, base_url: &str) -> anyhow::Result<Comic> {
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

/// Fetches a specific comic by number. Returns `Ok(None)` when xkcd returns 404
/// (the comic intentionally doesn't exist, e.g. #404), and `Err` for transient failures.
pub fn fetch_comic_by_num(
    agent: &ureq::Agent,
    base_url: &str,
    num: u32,
) -> anyhow::Result<Option<Comic>> {
    let url = format!("{}/{}/info.0.json", base_url.trim_end_matches('/'), num);
    let resp = match agent.get(&url).set("User-Agent", USER_AGENT).call() {
        Ok(r) => r,
        Err(ureq::Error::Status(404, _)) => return Ok(None),
        Err(ureq::Error::Status(code, _)) => {
            return Err(anyhow::anyhow!("xkcd API returned HTTP {code}"));
        }
        Err(ureq::Error::Transport(t)) => {
            return Err(anyhow::Error::new(t).context("failed to reach xkcd API"));
        }
    };
    let comic = resp
        .into_json::<Comic>()
        .context("failed to parse xkcd JSON")?;
    anyhow::ensure!(
        comic.num == num,
        "xkcd API returned comic #{} for request #{}",
        comic.num,
        num
    );
    log::debug!("fetched comic #{}: {}", comic.num, comic.safe_title);
    Ok(Some(comic))
}

pub(crate) fn local_filename(comic: &Comic) -> String {
    let basename = comic.img.rsplit('/').next().unwrap_or(comic.img.as_str());
    format!("{}-{}", comic.num, basename)
}

pub fn download_image(
    agent: &ureq::Agent,
    comic: &Comic,
    dest_dir: &Path,
) -> anyhow::Result<PathBuf> {
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

#[cfg(test)]
pub(crate) fn make_comic(num: u32) -> Comic {
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

#[cfg(test)]
mod tests {
    use super::*;

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

    // ── fetch_comic_by_num ────────────────────────────────────────────────────

    #[test]
    fn fetch_comic_by_num_success() {
        let mut server = mockito::Server::new();
        let body = r#"{"num":42,"safe_title":"Answer","img":"https://imgs.xkcd.com/comics/answer.png","alt":"Alt","year":"2025","month":"1","day":"1","title":"Answer"}"#;
        let _m = server
            .mock("GET", "/42/info.0.json")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body)
            .create();

        let agent = ureq::AgentBuilder::new().build();
        let comic = fetch_comic_by_num(&agent, &server.url(), 42)
            .unwrap()
            .unwrap();
        assert_eq!(comic.num, 42);
    }

    #[test]
    fn fetch_comic_by_num_404() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/404/info.0.json")
            .with_status(404)
            .create();

        let agent = ureq::AgentBuilder::new().build();
        let result = fetch_comic_by_num(&agent, &server.url(), 404).unwrap();
        assert!(result.is_none(), "404 should return Ok(None)");
    }

    #[test]
    fn fetch_comic_by_num_500() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/99/info.0.json")
            .with_status(500)
            .create();

        let agent = ureq::AgentBuilder::new().build();
        let result = fetch_comic_by_num(&agent, &server.url(), 99);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("500"));
    }

    #[test]
    fn fetch_comic_by_num_wrong_num() {
        let mut server = mockito::Server::new();
        // Server returns comic #1 but we requested #99
        let body = r#"{"num":1,"safe_title":"Barrel","img":"https://imgs.xkcd.com/comics/barrel.jpg","alt":"Alt","year":"2006","month":"1","day":"1","title":"Barrel"}"#;
        let _m = server
            .mock("GET", "/99/info.0.json")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body)
            .create();

        let agent = ureq::AgentBuilder::new().build();
        let result = fetch_comic_by_num(&agent, &server.url(), 99);
        assert!(result.is_err(), "mismatched num should be an error");
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

        let dest_dir = tempfile::TempDir::new().unwrap();
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

        let dest_dir = tempfile::TempDir::new().unwrap();
        let agent = ureq::AgentBuilder::new().build();
        let result = download_image(&agent, &comic, dest_dir.path());
        assert!(result.is_err());
    }
}
