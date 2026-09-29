// API discovery for the CLI and the MCP server (2026-09-09): where is the
// API RIGHT NOW? The configured port is a request and the stdout
// announcement is the truth (CLAUDE.md) — but only the supervising app hears
// stdout. The CLI and MCP used to assume 8768, and on the day another vendor's
// agent squatted on 8768–8769 they would have talked to it instead of
// Phantom. So the API also PUBLISHES its bound URL to a file, and clients
// read it when no explicit override is given:
//
//   1. `PHANTOM_API_URL` (or the CLI's --api-url)      — the operator's word
//   2. the published file  ~/Library/Application Support/phantom/api_url
//   3. http://127.0.0.1:18770                            — the registered port
//
// The file holds one line, the announced URL. The API writes it after
// binding (atomically: temp + rename) and removes it on graceful shutdown if
// it still holds its own URL, so a stale file means a crash — the next start
// overwrites it, and a client that reads a dead URL says which file it came
// from.

use std::path::{Path, PathBuf};

/// Phantom's registered Spooky Squad port. The squad lives at 187xx
/// (Specter 18765, Vigil 18766, Grimoire 18767, Squelch 18768, Banshee 18769
/// / dev 18779); 8768 was a stamping accident in the well-known range that
/// another vendor's local agent walked into on 2026-09-09.
pub const DEFAULT_API_URL: &str = "http://127.0.0.1:18770";
/// File name inside the data dir (prod). Dev writes `api_url-dev`; the test
/// profile writes beside its database.
pub const PUBLISHED_URL_FILE: &str = "api_url";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlSource {
    /// `PHANTOM_API_URL` or a flag.
    Override,
    /// The API's published file.
    PublishedFile,
    /// Nothing said otherwise.
    Default,
}

impl UrlSource {
    pub fn describe(&self, file: Option<&Path>) -> String {
        match self {
            UrlSource::Override => "from PHANTOM_API_URL".to_string(),
            UrlSource::PublishedFile => format!(
                "from {} (the API publishes its bound port there; a stale file means it stopped without cleaning up)",
                file.map(|p| p.display().to_string()).unwrap_or_else(|| PUBLISHED_URL_FILE.into())
            ),
            UrlSource::Default => "the default (no PHANTOM_API_URL, no published api_url file)".to_string(),
        }
    }
}

/// The prod profile's published-URL file: `$HOME/Library/Application
/// Support/phantom/api_url` — the same convention the Swift client uses for
/// the key file. None when HOME is unset.
pub fn published_url_file() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("phantom")
            .join(PUBLISHED_URL_FILE),
    )
}

/// One trimmed `http://` URL from the file, or None (absent, empty, or not
/// a loopback http URL — a clobbered file must not send a client anywhere
/// surprising).
pub fn read_published_url(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let line = raw.lines().next()?.trim();
    if line.starts_with("http://127.0.0.1:") || line.starts_with("http://localhost:") {
        Some(line.to_string())
    } else {
        None
    }
}

/// The resolution rule, pure: `override_` wins; else the file's URL; else
/// the default. Returns the URL and where it came from.
pub fn resolve_api_url(override_: Option<&str>, file: Option<&Path>) -> (String, UrlSource) {
    if let Some(u) = override_.map(str::trim).filter(|u| !u.is_empty()) {
        return (u.to_string(), UrlSource::Override);
    }
    if let Some(u) = file.and_then(read_published_url) {
        return (u, UrlSource::PublishedFile);
    }
    (DEFAULT_API_URL.to_string(), UrlSource::Default)
}

/// Write `url` to `path` atomically (temp file in the same dir + rename), so
/// a client never reads a half-written line. Creates the parent directory.
pub fn publish_url(path: &Path, url: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("{url}\n"))?;
    std::fs::rename(&tmp, path)
}

/// Remove the file if it still names `url` — never another process's
/// announcement (a newer API may have started while this one drained).
pub fn unpublish_url(path: &Path, url: &str) -> std::io::Result<bool> {
    match read_published_url(path) {
        Some(current) if current == url => {
            std::fs::remove_file(path)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_beats_file_beats_default() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("api_url");
        assert_eq!(
            resolve_api_url(None, Some(&file)),
            (DEFAULT_API_URL.to_string(), UrlSource::Default),
            "no file yet"
        );
        publish_url(&file, "http://127.0.0.1:8771").unwrap();
        assert_eq!(
            resolve_api_url(None, Some(&file)),
            ("http://127.0.0.1:8771".to_string(), UrlSource::PublishedFile)
        );
        assert_eq!(
            resolve_api_url(Some("http://127.0.0.1:9999"), Some(&file)),
            ("http://127.0.0.1:9999".to_string(), UrlSource::Override)
        );
        assert_eq!(
            resolve_api_url(Some("  "), Some(&file)),
            ("http://127.0.0.1:8771".to_string(), UrlSource::PublishedFile),
            "a blank override is no override"
        );
        assert_eq!(resolve_api_url(None, None), (DEFAULT_API_URL.to_string(), UrlSource::Default));
    }

    #[test]
    fn only_a_loopback_http_url_is_believed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("api_url");
        for junk in ["", "\n", "garbage", "https://example.com/", "http://10.0.0.5:18770", "  http://127.0.0.1:18770 \nsecond line"] {
            std::fs::write(&file, junk).unwrap();
            let want = if junk.trim_start().starts_with("http://127.0.0.1:18770") { Some("http://127.0.0.1:18770".to_string()) } else { None };
            assert_eq!(read_published_url(&file), want, "{junk:?}");
        }
        std::fs::write(&file, "http://localhost:8770\n").unwrap();
        assert_eq!(read_published_url(&file).as_deref(), Some("http://localhost:8770"));
    }

    #[test]
    fn publish_is_atomic_and_unpublish_only_removes_its_own_url() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested").join("api_url");
        publish_url(&file, "http://127.0.0.1:18770").unwrap();
        assert!(!file.with_extension("tmp").exists(), "the temp file is gone after the rename");
        assert_eq!(read_published_url(&file).as_deref(), Some("http://127.0.0.1:18770"));
        // A newer server took over: the old one must not remove its file.
        publish_url(&file, "http://127.0.0.1:8769").unwrap();
        assert!(!unpublish_url(&file, "http://127.0.0.1:18770").unwrap());
        assert!(file.exists());
        assert!(unpublish_url(&file, "http://127.0.0.1:8769").unwrap());
        assert!(!file.exists());
        assert!(!unpublish_url(&file, "http://127.0.0.1:8769").unwrap(), "already gone is not an error");
    }

    #[test]
    fn source_descriptions_name_the_file() {
        let p = Path::new("/tmp/x/api_url");
        assert!(UrlSource::PublishedFile.describe(Some(p)).contains("/tmp/x/api_url"));
        assert!(UrlSource::Override.describe(None).contains("PHANTOM_API_URL"));
        assert!(UrlSource::Default.describe(None).contains("default"));
    }

    #[test]
    fn the_prod_file_lives_beside_the_key_file() {
        let p = published_url_file().expect("HOME is set in tests");
        assert!(p.ends_with("Library/Application Support/phantom/api_url"), "{}", p.display());
    }
}
