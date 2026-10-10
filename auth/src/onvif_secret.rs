//! The ONVIF credentials file, `etc/onvif.secret`.
//!
//! One `user:password` line; blank lines and `#` comments are ignored. The
//! password is stored in plaintext on purpose: WS-UsernameToken
//! `PasswordDigest` is `Base64(SHA1(nonce + created + password))`, so the
//! server has to know the password itself — a PBKDF2 hash like the web and
//! SSH users have cannot be checked against it. That is why this is a separate
//! account, not a web user, and why the file must not be readable by others
//! (checked here, except on FAT where modes do not exist).
//!
//! Read by `onvif` and by `rtsp`, whose RTSP Digest (`HA1 = MD5(user:realm:
//! password)`) needs the plaintext for the same reason, so NVRs use one
//! account for both.

use crate::perms;
use snafu::{ensure, OptionExt, ResultExt, Snafu};
use std::path::{Path, PathBuf};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display(
        "ONVIF credentials file {} is missing; create it with one `user:password` line \
         (mode 600), or pass --no-auth to deliberately run an open ONVIF service",
        path.display()
    ))]
    Missing { path: PathBuf },

    #[snafu(display("cannot read {}: {source}", path.display()))]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display(
        "{} is accessible to other users (mode {mode:o}); it holds a plaintext password — run: chmod 600 {}",
        path.display(),
        path.display()
    ))]
    TooOpen { path: PathBuf, mode: u32 },

    #[snafu(display(
        "{} has no credentials line; expected `user:password`, or pass --no-auth",
        path.display()
    ))]
    Empty { path: PathBuf },

    #[snafu(display("{} line {line}: expected `user:password` with both parts non-empty", path.display()))]
    Malformed { path: PathBuf, line: usize },

    #[snafu(display("{} line {line}: only one credentials line is allowed", path.display()))]
    Duplicate { path: PathBuf, line: usize },
}

/// The single ONVIF account.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret {
    pub user: String,
    pub password: String,
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secret")
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl Secret {
    /// Reads and validates the file, refusing one other users can read.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return MissingSnafu { path }.fail()
            }
            Err(source) => return Err(source).context(ReadSnafu { path }),
        };
        if let Some(mode) = perms::too_open(path, &metadata) {
            return TooOpenSnafu { path, mode }.fail();
        }
        let text = std::fs::read_to_string(path).context(ReadSnafu { path })?;
        Self::parse(&text, path)
    }

    /// Parses the file contents; `path` is only used in error messages.
    pub fn parse(text: &str, path: &Path) -> Result<Self, Error> {
        let mut found: Option<Secret> = None;
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            // Only line endings are stripped from the credential itself:
            // leading/trailing spaces could be part of a password.
            let raw = raw.trim_end_matches('\r');
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            // Split at the first ':' — a user name cannot contain one, a
            // password may.
            let (user, password) = raw.split_once(':').context(MalformedSnafu { path, line })?;
            let user = user.trim();
            ensure!(
                !user.is_empty() && !password.is_empty(),
                MalformedSnafu { path, line }
            );
            ensure!(found.is_none(), DuplicateSnafu { path, line });
            found = Some(Secret {
                user: user.to_owned(),
                password: password.to_owned(),
            });
        }
        found.context(EmptySnafu { path })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn parse(text: &str) -> Result<Secret, Error> {
        Secret::parse(text, Path::new("etc/onvif.secret"))
    }

    #[test]
    fn parses_a_single_line() {
        let secret = parse("admin:hunter2\n").unwrap();
        assert_eq!(secret.user, "admin");
        assert_eq!(secret.password, "hunter2");
    }

    #[test]
    fn skips_comments_blank_lines_and_crlf() {
        let secret = parse("# seeded by init.sh\n\n   \r\nnvr:pa:ss:word\r\n# end\n").unwrap();
        assert_eq!(secret.user, "nvr");
        // Only the first ':' separates; the password keeps the rest.
        assert_eq!(secret.password, "pa:ss:word");
    }

    #[test]
    fn empty_or_comment_only_is_refused() {
        assert!(matches!(parse(""), Err(Error::Empty { .. })));
        assert!(matches!(parse("# nothing\n\n"), Err(Error::Empty { .. })));
    }

    #[test]
    fn malformed_lines_are_refused() {
        assert!(matches!(
            parse("admin\n"),
            Err(Error::Malformed { line: 1, .. })
        ));
        assert!(matches!(
            parse("# c\nadmin:\n"),
            Err(Error::Malformed { line: 2, .. })
        ));
        assert!(matches!(parse(":pw\n"), Err(Error::Malformed { .. })));
    }

    #[test]
    fn a_second_account_is_refused() {
        assert!(matches!(
            parse("a:1\nb:2\n"),
            Err(Error::Duplicate { line: 2, .. })
        ));
    }

    #[test]
    fn debug_does_not_print_the_password() {
        let secret = parse("admin:hunter2").unwrap();
        assert!(!format!("{secret:?}").contains("hunter2"));
    }

    fn temp_file(name: &str, contents: &str, mode: u32) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("anyka-onvif-secret-{}-{name}", std::process::id()));
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn load_checks_permissions_and_existence() {
        let private = temp_file("private", "admin:pw\n", 0o600);
        assert_eq!(Secret::load(&private).unwrap().user, "admin");

        // The host temp dir is never FAT, so the mode check applies.
        let open = temp_file("open", "admin:pw\n", 0o644);
        assert!(matches!(
            Secret::load(&open),
            Err(Error::TooOpen { mode: 0o644, .. })
        ));

        let missing = std::env::temp_dir().join("anyka-onvif-secret-does-not-exist");
        let error = Secret::load(&missing).unwrap_err();
        assert!(matches!(error, Error::Missing { .. }));
        assert!(error.to_string().contains("--no-auth"));

        std::fs::remove_file(private).unwrap();
        std::fs::remove_file(open).unwrap();
    }
}
