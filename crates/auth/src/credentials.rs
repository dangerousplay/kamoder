//! Password authentication against a hashed credentials file (`sshd`).
//!
//! Passwords are never stored in the clear. Each entry holds a per-user random
//! salt and a PBKDF2-HMAC-SHA256 digest, verified with `ring`'s constant-time
//! comparison.
//!
//! File format — one entry per line, `#` starts a comment:
//!
//! ```text
//! <username>:pbkdf2-sha256:<iterations>:<salt-hex>:<digest-hex>
//! ```
//!
//! Generate entries with `sshd hash <username>`.

use crate::password::PasswordHash;
use snafu::{ensure, OptionExt, ResultExt, Snafu};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

pub use crate::password::DEFAULT_ITERATIONS;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("cannot read {}: {source}", path.display()))]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display(
        "{} is accessible to other users (mode {mode:o}); run: chmod 600 {}",
        path.display(),
        path.display()
    ))]
    Permissions { path: PathBuf, mode: u32 },

    #[snafu(display("{}:{line}: {reason}", path.display()))]
    Malformed {
        path: PathBuf,
        line: usize,
        reason: String,
    },

    #[snafu(display("{} contains no users", path.display()))]
    NoUsers { path: PathBuf },

    #[snafu(display("{reason}"))]
    Hash { reason: String },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// All credentials loaded from the file.
#[derive(Debug, Clone, Default)]
pub struct Credentials {
    users: HashMap<String, PasswordHash>,
}

impl Credentials {
    /// Loads and validates a credentials file.
    ///
    /// Refuses a file that other users can read: it holds password digests,
    /// and on this device every service runs as root anyway, so there is no
    /// legitimate reason for it to be group- or world-accessible.
    pub fn load(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path).context(ReadSnafu { path })?;
        if let Some(mode) = crate::perms::too_open(path, &metadata) {
            return PermissionsSnafu { path, mode }.fail();
        }

        let text = std::fs::read_to_string(path).context(ReadSnafu { path })?;
        let mut users = HashMap::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (user, entry) = parse_line(line).map_err(|reason| Error::Malformed {
                path: path.to_owned(),
                line: index + 1,
                reason,
            })?;
            users.insert(user, entry);
        }
        ensure!(!users.is_empty(), NoUsersSnafu { path });
        Ok(Self { users })
    }

    /// Whether `user` has an entry.
    pub fn contains(&self, user: &str) -> bool {
        self.users.contains_key(user)
    }

    /// Number of users.
    pub fn len(&self) -> usize {
        self.users.len()
    }

    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }

    /// Checks a username and password.
    ///
    /// An unknown user is still run through a full PBKDF2 derivation against a
    /// dummy salt, so the response time does not reveal which usernames exist.
    pub fn verify(&self, user: &str, password: &str) -> bool {
        match self.users.get(user) {
            Some(entry) => entry.verify(password),
            None => {
                PasswordHash::burn(password);
                false
            }
        }
    }
}

fn parse_line(line: &str) -> Result<(String, PasswordHash), String> {
    let mut fields = line.split(':');
    let user = fields.next().unwrap_or_default();
    let algorithm = fields.next().unwrap_or_default();
    let iterations = fields.next().unwrap_or_default();
    let salt = fields.next().unwrap_or_default();
    let digest = fields.next().unwrap_or_default();
    if fields.next().is_some() {
        return Err("too many fields".into());
    }
    if user.is_empty() {
        return Err("empty username".into());
    }
    let entry = PasswordHash::parse(algorithm, iterations, salt, digest)?;
    Ok((user.to_owned(), entry))
}

/// Builds a credentials line for `user` with a fresh random salt.
pub fn hash_password(user: &str, password: &str, iterations: u32) -> Result<String> {
    ensure!(
        !user.contains(':'),
        HashSnafu {
            reason: "username must not contain ':'"
        }
    );
    let iterations = NonZeroU32::new(iterations).context(HashSnafu {
        reason: "iteration count must be > 0",
    })?;
    let hash = PasswordHash::derive(password, iterations).map_err(|reason| Error::Hash {
        reason: reason.into(),
    })?;
    Ok(format!("{user}:{hash}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::OpenOptionsExt;

    /// Writes `text` to a fresh 0600 file unique to this test.
    fn passwd_file(name: &str, text: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "anyka-auth-credentials-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut file, text.as_bytes()).unwrap();
        path
    }

    #[test]
    fn hash_line_has_the_documented_shape() {
        let line = hash_password("root", "pw", 10).unwrap();
        let fields: Vec<&str> = line.split(':').collect();
        assert_eq!(fields.len(), 5);
        assert_eq!(fields[0], "root");
        assert_eq!(fields[1], "pbkdf2-sha256");
        assert_eq!(fields[2], "10");
        assert_eq!(fields[3].len(), 32);
        assert_eq!(fields[4].len(), 64);
    }

    #[test]
    fn hash_password_rejects_bad_input() {
        assert!(hash_password("a:b", "pw", 10).is_err());
        assert!(hash_password("root", "pw", 0).is_err());
    }

    #[test]
    fn load_and_verify() {
        let line = hash_password("root", "secret", 10).unwrap();
        let path = passwd_file("ok", &format!("# comment\n\n  {line}  \n"));
        let creds = Credentials::load(&path).unwrap();
        assert!(creds.verify("root", "secret"));
        assert!(!creds.verify("root", "wrong"));
        assert!(!creds.verify("nobody", "secret"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn default_iterations_are_unchanged() {
        assert_eq!(DEFAULT_ITERATIONS, 20_000);
    }

    #[test]
    fn empty_file_is_refused() {
        let path = passwd_file("empty", "# nothing\n");
        assert!(matches!(
            Credentials::load(&path),
            Err(Error::NoUsers { .. })
        ));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn malformed_lines_report_their_line_number() {
        let good = hash_password("root", "pw", 10).unwrap();
        for (name, bad) in [
            ("extra", format!("{good}:extra")),
            ("nouser", format!(":{}", good.split_once(':').unwrap().1)),
            ("algo", good.replace("pbkdf2-sha256", "md5")),
        ] {
            let path = passwd_file(name, &format!("# c\n{bad}\n"));
            match Credentials::load(&path) {
                Err(Error::Malformed { line, .. }) => assert_eq!(line, 2, "{name}"),
                other => panic!("{name}: {other:?}"),
            }
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn world_readable_file_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let line = hash_password("root", "pw", 10).unwrap();
        let path = passwd_file("open", &line);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            Credentials::load(&path),
            Err(Error::Permissions { mode: 0o644, .. })
        ));
        std::fs::remove_file(path).unwrap();
    }
}
