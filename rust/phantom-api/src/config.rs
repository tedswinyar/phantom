// Profiles: prod (default), dev, test — selected by PHANTOM_PROFILE.
//
//   profile  db path                                   port    key file
//   prod     <data_dir>/phantom/phantom.db       8768   <config_dir>/phantom/api_key
//   dev      <data_dir>/phantom/phantom-dev.db   8778   <config_dir>/phantom/api_key
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
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
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
                Self {
                    profile,
                    db_path,
                    port: env_port.ok_or(ConfigError::MissingTestEnv("PHANTOM_PORT"))?,
                    key_file: env_key
                        .ok_or(ConfigError::MissingTestEnv("PHANTOM_KEY_FILE"))?,
                }
            }
            // Resolve platform dirs ONLY when the env override is absent:
            // `unwrap_or(app_data_dir()?…)` evaluated `app_data_dir()?` even
            // when `PHANTOM_DB_PATH`/`PHANTOM_KEY_FILE` were set, so an
            // explicit override still failed on a headless box where
            // `dirs::data_dir()` is None. `match` defers the fallible call.
            Profile::Dev => Self {
                profile,
                db_path: match env_db {
                    Some(p) => p,
                    None => app_data_dir()?.join("phantom-dev.db"),
                },
                port: env_port.unwrap_or(8778),
                key_file: match env_key {
                    Some(p) => p,
                    None => app_config_dir()?.join("api_key"),
                },
            },
            Profile::Prod => Self {
                profile,
                db_path: match env_db {
                    Some(p) => p,
                    None => app_data_dir()?.join("phantom.db"),
                },
                port: env_port.unwrap_or(8768),
                key_file: match env_key {
                    Some(p) => p,
                    None => app_config_dir()?.join("api_key"),
                },
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
    fn default_profile_is_prod_on_port_8768() {
        with_env(&[], || {
            let c = Config::from_env().unwrap();
            assert_eq!(c.profile, Profile::Prod);
            assert_eq!(c.port, 8768);
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
