// Profiles: prod (default), dev, test — selected by PHANTOM_PROFILE.
//
//   profile  db path                                   port    key file
//   prod     <data_dir>/phantom/phantom.db       18770  <config_dir>/phantom/api_key
//   dev      <data_dir>/phantom/phantom-dev.db   18780  <config_dir>/phantom/api_key
//   test     PHANTOM_DB_PATH (required)             PHANTOM_PORT (required)
//                                                              PHANTOM_KEY_FILE (required)
//
// Test mode wins: it refuses to run against anything under the prod data
// directory, so a mis-set env var cannot touch real data. PHANTOM_PORT
// and PHANTOM_DB_PATH also override dev/prod when set explicitly.

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Prod,
    Dev,
    Test,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub profile: Profile,
    pub db_path: PathBuf,
    pub port: u16,
    pub key_file: PathBuf,
    /// Where this server publishes its bound URL for the CLI and MCP
    /// (`phantom_core::discovery`): prod `<data dir>/api_url`, dev
    /// `<data dir>/api_url-dev`, test beside the database.
    pub url_file: PathBuf,
    /// Scan retention (phantom-9tt): `PHANTOM_KEEP_SCANS_PER_ROOT` and
    /// `PHANTOM_KEEP_SCANS`, else the product defaults.
    pub retention: Retention,
}

/// How many scans survive a completion's prune: the newest `per_root` of
/// each root (so one root's history is never evicted by another root's
/// scans), then the newest `total` overall (so the database stays bounded).
/// The defaults are a PRODUCT DECISION (25 per root, Ted 2026-09-01; 100
/// overall, 2026-09-08 with the history feature); the env overrides exist
/// for operators, not as a settings surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub per_root: usize,
    pub total: usize,
}

impl Retention {
    pub const DEFAULT_PER_ROOT: usize = 25;
    pub const DEFAULT_TOTAL: usize = 100;

    /// Read `PHANTOM_KEEP_SCANS_PER_ROOT` / `PHANTOM_KEEP_SCANS`; a value
    /// that is not a positive integer is a configuration error, never a
    /// silent default (a typo must not become "keep 0 scans").
    pub fn from_env() -> Result<Self, ConfigError> {
        let read = |name: &'static str, default: usize| -> Result<usize, ConfigError> {
            match std::env::var(name) {
                Ok(s) => s
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or(ConfigError::BadRetention(name, s)),
                Err(_) => Ok(default),
            }
        };
        let per_root = read("PHANTOM_KEEP_SCANS_PER_ROOT", Self::DEFAULT_PER_ROOT)?;
        let total = read("PHANTOM_KEEP_SCANS", Self::DEFAULT_TOTAL)?;
        if total < per_root {
            return Err(ConfigError::BadRetention(
                "PHANTOM_KEEP_SCANS",
                format!("{total} (must be >= PHANTOM_KEEP_SCANS_PER_ROOT = {per_root})"),
            ));
        }
        Ok(Self { per_root, total })
    }
}

impl Default for Retention {
    fn default() -> Self {
        Self { per_root: Self::DEFAULT_PER_ROOT, total: Self::DEFAULT_TOTAL }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0} must be a positive integer (got {1:?})")]
    BadRetention(&'static str, String),
    #[error("PHANTOM_PROFILE must be prod, dev, or test (got {0:?})")]
    BadProfile(String),
    #[error("test profile requires {0} to be set")]
    MissingTestEnv(&'static str),
    #[error("{0} is not a valid port: {1}")]
    BadPort(&'static str, String),
    #[error("test profile refuses a database under the prod data dir: {0}")]
    TestPointsAtProdData(String),
    #[error("cannot determine platform data/config directory")]
    NoPlatformDirs,
}

/// True if `db_path` lands inside `data_dir`, judged by both the literal
/// path (catches `..` traversal, which resolves away) and the canonicalized
/// deepest-existing ancestor (catches symlinks that point into the prod
/// dir). Either hit is a refusal — this is a data-safety backstop, so it
/// errs toward refusing.
fn points_at_prod_data(db_path: &std::path::Path, data_dir: &std::path::Path) -> bool {
    if db_path.starts_with(data_dir) {
        return true;
    }
    // Walk up to the deepest ancestor that exists and canonicalize it; the
    // db file and leaf dirs may not exist yet.
    let canon_data = data_dir.canonicalize();
    let mut probe = db_path;
    loop {
        if let Ok(real) = probe.canonicalize() {
            match &canon_data {
                Ok(cd) if real.starts_with(cd) => return true,
                // If the prod dir itself doesn't exist, compare against its
                // literal form (nothing to resolve to).
                Err(_) if real.starts_with(data_dir) => return true,
                _ => return false,
            }
        }
        match probe.parent() {
            Some(p) if !p.as_os_str().is_empty() => probe = p,
            _ => return false,
        }
    }
}

fn app_data_dir() -> Result<PathBuf, ConfigError> {
    Ok(dirs::data_dir()
        .ok_or(ConfigError::NoPlatformDirs)?
        .join("phantom"))
}

fn app_config_dir() -> Result<PathBuf, ConfigError> {
    Ok(dirs::config_dir()
        .ok_or(ConfigError::NoPlatformDirs)?
        .join("phantom"))
}

impl Config {
    /// Resolve configuration from the environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        let profile = match std::env::var("PHANTOM_PROFILE").as_deref() {
            Err(_) | Ok("prod") => Profile::Prod,
            Ok("dev") => Profile::Dev,
            Ok("test") => Profile::Test,
            Ok(other) => return Err(ConfigError::BadProfile(other.into())),
        };

        let env_db = std::env::var("PHANTOM_DB_PATH").ok().map(PathBuf::from);
        let env_port = match std::env::var("PHANTOM_PORT") {
            Ok(s) => Some(
                s.parse::<u16>()
                    .map_err(|_| ConfigError::BadPort("PHANTOM_PORT", s))?,
            ),
            Err(_) => None,
        };
        let env_key = std::env::var("PHANTOM_KEY_FILE").ok().map(PathBuf::from);
        let retention = Retention::from_env()?;

        let config = match profile {
            Profile::Test => {
                let db_path =
                    env_db.ok_or(ConfigError::MissingTestEnv("PHANTOM_DB_PATH"))?;
                let data_dir = app_data_dir()?;
                // Compare RESOLVED paths, not literal strings: a symlink whose
                // literal path sits outside the prod dir but resolves inside it
                // would otherwise defeat this guard. Canonicalize the deepest
                // existing ancestor (the db file itself needn't exist yet) and
                // also keep the literal check for the `..`-traversal case where
                // nothing resolves.
                if points_at_prod_data(&db_path, &data_dir) {
                    return Err(ConfigError::TestPointsAtProdData(
                        db_path.display().to_string(),
                    ));
                }
                let url_file = db_path.parent().map(|d| d.join("api_url")).unwrap_or_else(|| PathBuf::from("api_url"));
                Self {
                    profile,
                    retention,
                    db_path,
                    port: env_port.ok_or(ConfigError::MissingTestEnv("PHANTOM_PORT"))?,
                    key_file: env_key
                        .ok_or(ConfigError::MissingTestEnv("PHANTOM_KEY_FILE"))?,
                    url_file,
                }
            }
            // Resolve platform dirs ONLY when the env override is absent:
            // `unwrap_or(app_data_dir()?…)` evaluated `app_data_dir()?` even
            // when `PHANTOM_DB_PATH`/`PHANTOM_KEY_FILE` were set, so an
            // explicit override still failed on a headless box where
            // `dirs::data_dir()` is None. `match` defers the fallible call.
            Profile::Dev => Self {
                profile,
                retention,
                db_path: match env_db {
                    Some(p) => p,
                    None => app_data_dir()?.join("phantom-dev.db"),
                },
                port: env_port.unwrap_or(18780),
                key_file: match env_key {
                    Some(p) => p,
                    None => app_config_dir()?.join("api_key"),
                },
                url_file: app_data_dir()?.join("api_url-dev"),
            },
            Profile::Prod => Self {
                profile,
                retention,
                db_path: match env_db {
                    Some(p) => p,
                    None => app_data_dir()?.join("phantom.db"),
                },
                port: env_port.unwrap_or(18770),
                key_file: match env_key {
                    Some(p) => p,
                    None => app_config_dir()?.join("api_key"),
                },
                url_file: app_data_dir()?.join("api_url"),
            },
        };
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Env-var tests mutate process-global state; serialize them.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_env(vars: &[(&str, Option<&str>)], f: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().unwrap();
        let all = [
            "PHANTOM_PROFILE",
            "PHANTOM_DB_PATH",
            "PHANTOM_PORT",
            "PHANTOM_KEY_FILE",
            "PHANTOM_KEEP_SCANS_PER_ROOT",
            "PHANTOM_KEEP_SCANS",
        ];
        let saved: Vec<_> = all.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        for k in all {
            unsafe { std::env::remove_var(k) };
        }
        for (k, v) in vars {
            if let Some(v) = v {
                unsafe { std::env::set_var(k, v) };
            }
        }
        f();
        for (k, v) in saved {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
    }

    #[test]
    fn url_file_is_beside_the_database_for_test_and_in_the_data_dir_otherwise() {
        with_env(&[("PHANTOM_PROFILE", Some("test")), ("PHANTOM_DB_PATH", Some("/tmp/somewhere/phantom.db")), ("PHANTOM_PORT", Some("0")), ("PHANTOM_KEY_FILE", Some("/tmp/somewhere/api_key"))], || {
            let c = Config::from_env().unwrap();
            assert_eq!(c.url_file, PathBuf::from("/tmp/somewhere/api_url"));
        });
        with_env(&[], || {
            let c = Config::from_env().unwrap();
            assert!(c.url_file.ends_with("phantom/api_url"), "{}", c.url_file.display());
            assert_eq!(c.url_file.parent(), c.db_path.parent(), "beside the prod database");
        });
        with_env(&[("PHANTOM_PROFILE", Some("dev"))], || {
            let c = Config::from_env().unwrap();
            assert!(c.url_file.ends_with("phantom/api_url-dev"), "{}", c.url_file.display());
        });
    }

    #[test]
    fn retention_defaults_and_env_overrides() {
        with_env(&[], || {
            let c = Config::from_env().unwrap();
            assert_eq!(c.retention, Retention { per_root: 25, total: 100 });
        });
        with_env(&[("PHANTOM_KEEP_SCANS_PER_ROOT", Some("5")), ("PHANTOM_KEEP_SCANS", Some("12"))], || {
            assert_eq!(Config::from_env().unwrap().retention, Retention { per_root: 5, total: 12 });
        });
        // Junk, zero, and a total below the per-root cap are errors, never
        // a silent default.
        for (k, v) in [
            ("PHANTOM_KEEP_SCANS", "many"),
            ("PHANTOM_KEEP_SCANS_PER_ROOT", "0"),
            ("PHANTOM_KEEP_SCANS", "-3"),
        ] {
            with_env(&[(k, Some(v))], || {
                let err = Config::from_env().unwrap_err();
                assert!(matches!(err, ConfigError::BadRetention(name, _) if name == k), "{k}={v}: {err}");
            });
        }
        with_env(&[("PHANTOM_KEEP_SCANS_PER_ROOT", Some("50")), ("PHANTOM_KEEP_SCANS", Some("10"))], || {
            let err = Config::from_env().unwrap_err().to_string();
            assert!(err.contains("PHANTOM_KEEP_SCANS") && err.contains(">="), "{err}");
        });
    }

    #[test]
    fn default_profile_is_prod_on_port_18770() {
        with_env(&[], || {
            let c = Config::from_env().unwrap();
            assert_eq!(c.profile, Profile::Prod);
            assert_eq!(c.port, 18770);
            assert!(c.db_path.ends_with("phantom/phantom.db"));
        });
    }

    #[test]
    fn unknown_profile_is_rejected() {
        with_env(&[("PHANTOM_PROFILE", Some("staging"))], || {
            assert!(matches!(
                Config::from_env(),
                Err(ConfigError::BadProfile(_))
            ));
        });
    }

    #[test]
    fn test_profile_requires_explicit_db_port_and_key() {
        with_env(&[("PHANTOM_PROFILE", Some("test"))], || {
            assert!(matches!(
                Config::from_env(),
                Err(ConfigError::MissingTestEnv("PHANTOM_DB_PATH"))
            ));
        });
    }

    #[test]
    fn test_profile_refuses_prod_data_dir() {
        let prod_db = app_data_dir().unwrap().join("anything.db");
        let prod_db = prod_db.to_str().unwrap().to_string();
        with_env(
            &[
                ("PHANTOM_PROFILE", Some("test")),
                ("PHANTOM_DB_PATH", Some(&prod_db)),
                ("PHANTOM_PORT", Some("18999")),
                ("PHANTOM_KEY_FILE", Some("/tmp/k")),
            ],
            || {
                assert!(matches!(
                    Config::from_env(),
                    Err(ConfigError::TestPointsAtProdData(_))
                ));
            },
        );
    }

    #[test]
    fn test_profile_refuses_symlink_into_prod_data() {
        // A symlink whose literal path is outside the prod dir but which
        // RESOLVES inside it must still be refused (adversarial finding,
        // 2026-08-20). Skipped only if the prod data dir cannot be created.
        let Ok(prod) = app_data_dir() else { return };
        if std::fs::create_dir_all(&prod).is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("sneaky");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&prod, &link).unwrap();
        let db = link.join("evil.db");
        let db = db.to_str().unwrap().to_string();
        with_env(
            &[
                ("PHANTOM_PROFILE", Some("test")),
                ("PHANTOM_DB_PATH", Some(&db)),
                ("PHANTOM_PORT", Some("0")),
                ("PHANTOM_KEY_FILE", Some("/tmp/spooky-symlink-test-key")),
            ],
            || {
                assert!(
                    matches!(Config::from_env(), Err(ConfigError::TestPointsAtProdData(_))),
                    "a symlink resolving into the prod data dir must be refused"
                );
            },
        );
    }

    #[test]
    fn test_profile_with_full_env_resolves() {
        with_env(
            &[
                ("PHANTOM_PROFILE", Some("test")),
                ("PHANTOM_DB_PATH", Some("/tmp/spooky-test/notes.db")),
                ("PHANTOM_PORT", Some("0")),
                ("PHANTOM_KEY_FILE", Some("/tmp/spooky-test/api_key")),
            ],
            || {
                let c = Config::from_env().unwrap();
                assert_eq!(c.profile, Profile::Test);
                assert_eq!(c.port, 0);
            },
        );
    }

    #[test]
    fn garbage_port_is_rejected_not_defaulted() {
        with_env(&[("PHANTOM_PORT", Some("many"))], || {
            assert!(matches!(Config::from_env(), Err(ConfigError::BadPort(..))));
        });
    }
}
