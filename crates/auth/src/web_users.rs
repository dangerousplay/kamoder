//! The web UI's users file: sshd-style PBKDF2 hashes plus a role and a
//! generation counter.
//!
//! File format — one entry per line, `#` starts a comment:
//!
//! ```text
//! <username>:<admin|viewer>:pbkdf2-sha256:<iterations>:<salt-hex>:<digest-hex>:<gen>
//! ```
//!
//! Why the extra columns:
//!
//! - **role** — the server enforces admin/viewer permissions itself; the
//!   browser only hides buttons.
//! - **gen** — a per-user counter bumped on every password or role change and
//!   copied into each session token. PBKDF2 costs about a second on the
//!   400 MHz ARM926EJ-S, so passwords are checked only at login; every later
//!   request is an HMAC check plus a lookup in this in-memory table. A token
//!   whose `gen` no longer matches (or whose user is gone) is refused, so a
//!   deletion, demotion or password reset takes effect on the next request
//!   rather than when the token expires.
//!
//! The table is cached and re-read only when the file's mtime, size or inode
//! changes, so per-request checks cost one `stat`. Every change is written
//! through [`crate::atomic::write`] (`.tmp` + fsync + rename, mode 0600), and
//! the last admin can neither be deleted nor demoted — otherwise the only way
//! back in would be pulling the SD card. Comments are not preserved across a
//! rewrite; the file is regenerated with a fixed header.

use crate::password::{PasswordHash, DEFAULT_ITERATIONS};
use serde::{Deserialize, Serialize};
use snafu::{ensure, ResultExt, Snafu};
use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Longest accepted username, in bytes.
const MAX_USERNAME: usize = 32;

/// Header written at the top of every regenerated file.
const HEADER: &str =
    "# web users, managed by web; manual edits are picked up but comments are not kept.\n\
                      # user:role:pbkdf2-sha256:iterations:salt-hex:digest-hex:generation\n";

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("cannot read {}: {source}", path.display()))]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("cannot write {}: {source}", path.display()))]
    Write {
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

    #[snafu(display("invalid username: {reason}"))]
    InvalidUsername { reason: &'static str },

    #[snafu(display("password must not be empty"))]
    EmptyPassword,

    #[snafu(display("user {user:?} already exists"))]
    UserExists { user: String },

    #[snafu(display("no such user {user:?}"))]
    UnknownUser { user: String },

    #[snafu(display("refusing to remove the last admin"))]
    LastAdmin,

    #[snafu(display("{reason}"))]
    Hash { reason: &'static str },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What a web user may do. Viewers read; admins also change things.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Viewer,
}

impl Role {
    /// The spelling used in the file, in tokens and in the API.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Viewer => "viewer",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "admin" => Ok(Role::Admin),
            "viewer" => Ok(Role::Viewer),
            other => Err(format!("unknown role {other:?} (expected admin or viewer)")),
        }
    }
}

/// A user as listed to admins: no hash material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UserSummary {
    pub name: String,
    pub role: Role,
    pub gen: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    role: Role,
    hash: PasswordHash,
    gen: u32,
}

/// What identifies one version of the file on disk.
///
/// FAT keeps mtimes to two seconds, so size and inode are compared too; our
/// own rewrites always produce a new inode through the rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    ino: u64,
}

impl Stamp {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
            ino: metadata.ino(),
        }
    }
}

/// The users file, cached in memory and reloaded when it changes on disk.
#[derive(Debug, Clone)]
pub struct WebUsers {
    path: PathBuf,
    users: BTreeMap<String, Entry>,
    /// `None` while the file does not exist.
    stamp: Option<Stamp>,
    iterations: NonZeroU32,
}

impl WebUsers {
    /// Loads the users file.
    ///
    /// A missing file is an empty table rather than an error: init creates
    /// it from `camera.conf`, and until then the server should still come up
    /// (nobody can log in). A file other users can read is refused, as for
    /// sshd — except on FAT, where modes mean nothing (see [`crate::perms`]).
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let mut users = Self {
            path: path.into(),
            users: BTreeMap::new(),
            stamp: None,
            iterations: NonZeroU32::new(DEFAULT_ITERATIONS).expect("non-zero"),
        };
        users.refresh()?;
        Ok(users)
    }

    /// The file this table mirrors.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Re-reads the file if it changed since the last load; returns whether
    /// it did.
    ///
    /// On error the previous table is kept, so a half-edited file does not
    /// lock everyone out; the caller should log the error.
    pub fn refresh(&mut self) -> Result<bool> {
        let path = self.path.clone();
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let changed = self.stamp.is_some();
                self.users.clear();
                self.stamp = None;
                return Ok(changed);
            }
            Err(source) => return Err(Error::Read { path, source }),
        };
        let stamp = Stamp::of(&metadata);
        if self.stamp == Some(stamp) {
            return Ok(false);
        }
        if let Some(mode) = crate::perms::too_open(&path, &metadata) {
            return PermissionsSnafu { path, mode }.fail();
        }
        let text = std::fs::read_to_string(&path).context(ReadSnafu { path: &path })?;
        self.users = parse(&path, &text)?;
        self.stamp = Some(stamp);
        Ok(true)
    }

    /// Checks a password: `Some((role, gen))` if it matches.
    ///
    /// Costs a full PBKDF2 derivation (about a second on the camera) whether
    /// or not the user exists, so timing does not reveal valid usernames.
    pub fn verify(&self, user: &str, password: &str) -> Option<(Role, u32)> {
        match self.users.get(user) {
            Some(entry) => entry
                .hash
                .verify(password)
                .then_some((entry.role, entry.gen)),
            None => {
                PasswordHash::burn(password);
                None
            }
        }
    }

    /// The current role and generation of `user`, without a password check —
    /// the per-request test that a token is still valid.
    pub fn lookup(&self, user: &str) -> Option<(Role, u32)> {
        self.users.get(user).map(|entry| (entry.role, entry.gen))
    }

    /// All users, sorted by name.
    pub fn list(&self) -> Vec<UserSummary> {
        self.users
            .iter()
            .map(|(name, entry)| UserSummary {
                name: name.clone(),
                role: entry.role,
                gen: entry.gen,
            })
            .collect()
    }

    /// Whether there are no users at all.
    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }

    /// Adds a user and rewrites the file.
    pub fn add(&mut self, user: &str, role: Role, password: &str) -> Result<()> {
        validate_username(user)?;
        ensure!(!password.is_empty(), EmptyPasswordSnafu);
        self.refresh()?;
        ensure!(!self.users.contains_key(user), UserExistsSnafu { user });
        let hash = self.derive(password)?;
        let mut next = self.users.clone();
        next.insert(user.to_owned(), Entry { role, hash, gen: 1 });
        self.persist(next)
    }

    /// Removes a user and rewrites the file. The last admin cannot be removed.
    pub fn delete(&mut self, user: &str) -> Result<()> {
        self.refresh()?;
        let entry = self.entry(user)?;
        ensure!(
            entry.role != Role::Admin || self.admin_count() > 1,
            LastAdminSnafu
        );
        let mut next = self.users.clone();
        next.remove(user);
        self.persist(next)
    }

    /// Changes a user's role, bumping their generation so existing sessions
    /// end. The last admin cannot be demoted. Setting the current role is a
    /// no-op and does not touch the file.
    pub fn set_role(&mut self, user: &str, role: Role) -> Result<()> {
        self.refresh()?;
        let entry = self.entry(user)?;
        if entry.role == role {
            return Ok(());
        }
        ensure!(
            entry.role != Role::Admin || self.admin_count() > 1,
            LastAdminSnafu
        );
        let mut next = self.users.clone();
        let entry = next.get_mut(user).expect("checked above");
        entry.role = role;
        entry.gen = entry.gen.wrapping_add(1);
        self.persist(next)
    }

    /// Replaces a user's password, bumping their generation so existing
    /// sessions end.
    pub fn set_password(&mut self, user: &str, password: &str) -> Result<()> {
        ensure!(!password.is_empty(), EmptyPasswordSnafu);
        self.refresh()?;
        self.entry(user)?;
        let hash = self.derive(password)?;
        let mut next = self.users.clone();
        let entry = next.get_mut(user).expect("checked above");
        entry.hash = hash;
        entry.gen = entry.gen.wrapping_add(1);
        self.persist(next)
    }

    fn entry(&self, user: &str) -> Result<&Entry> {
        self.users.get(user).ok_or_else(|| Error::UnknownUser {
            user: user.to_owned(),
        })
    }

    fn admin_count(&self) -> usize {
        self.users
            .values()
            .filter(|entry| entry.role == Role::Admin)
            .count()
    }

    fn derive(&self, password: &str) -> Result<PasswordHash> {
        PasswordHash::derive(password, self.iterations).map_err(|reason| Error::Hash { reason })
    }

    /// Writes `next` atomically, then adopts it as the cached table.
    fn persist(&mut self, next: BTreeMap<String, Entry>) -> Result<()> {
        let path = &self.path;
        crate::atomic::write(path, render(&next).as_bytes(), 0o600).context(WriteSnafu { path })?;
        let metadata = std::fs::metadata(path).context(ReadSnafu { path })?;
        self.stamp = Some(Stamp::of(&metadata));
        self.users = next;
        Ok(())
    }
}

/// Builds one users-file line for `user` with a fresh salt and generation 1.
///
/// Backs `web hash`, which init uses to seed the first admin from
/// `camera.conf` without the plaintext ever reaching the file.
pub fn hash_line(user: &str, role: Role, password: &str) -> Result<String> {
    validate_username(user)?;
    ensure!(!password.is_empty(), EmptyPasswordSnafu);
    let iterations = NonZeroU32::new(DEFAULT_ITERATIONS).expect("non-zero");
    let hash =
        PasswordHash::derive(password, iterations).map_err(|reason| Error::Hash { reason })?;
    Ok(format_line(user, &Entry { role, hash, gen: 1 }))
}

/// Usernames end up in the file, in tokens and in log lines, so they are kept
/// to a conservative alphabet: ASCII letters, digits, `.`, `_`, `-`.
fn validate_username(user: &str) -> Result<()> {
    ensure!(!user.is_empty(), InvalidUsernameSnafu { reason: "empty" });
    ensure!(
        user.len() <= MAX_USERNAME,
        InvalidUsernameSnafu {
            reason: "longer than 32 characters"
        }
    );
    ensure!(
        user.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')),
        InvalidUsernameSnafu {
            reason: "only letters, digits, '.', '_' and '-' are allowed"
        }
    );
    Ok(())
}

fn format_line(user: &str, entry: &Entry) -> String {
    format!("{user}:{}:{}:{}", entry.role, entry.hash, entry.gen)
}

fn render(users: &BTreeMap<String, Entry>) -> String {
    let mut text = String::from(HEADER);
    for (user, entry) in users {
        text.push_str(&format_line(user, entry));
        text.push('\n');
    }
    text
}

fn parse(path: &Path, text: &str) -> Result<BTreeMap<String, Entry>> {
    let mut users = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let malformed = |reason: String| Error::Malformed {
            path: path.to_owned(),
            line: index + 1,
            reason,
        };
        let (user, entry) = parse_line(line).map_err(malformed)?;
        if users.contains_key(&user) {
            return Err(malformed(format!("duplicate user {user:?}")));
        }
        users.insert(user, entry);
    }
    Ok(users)
}

fn parse_line(line: &str) -> Result<(String, Entry), String> {
    let fields: Vec<&str> = line.split(':').collect();
    let [user, role, algorithm, iterations, salt, digest, gen] = fields[..] else {
        return Err(format!("expected 7 fields, found {}", fields.len()));
    };
    validate_username(user).map_err(|e| e.to_string())?;
    let role: Role = role.parse()?;
    let hash = PasswordHash::parse(algorithm, iterations, salt, digest)?;
    let gen: u32 = gen
        .parse()
        .map_err(|_| "generation is not a number".to_string())?;
    Ok((user.to_owned(), Entry { role, hash, gen }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fresh directory per test; tests run in parallel.
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("anyka-auth-web-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An empty table whose hashes are cheap enough for tests.
    fn table(dir: &Path) -> WebUsers {
        let mut users = WebUsers::load(dir.join("web-users")).unwrap();
        users.iterations = NonZeroU32::new(10).unwrap();
        users
    }

    #[test]
    fn missing_file_is_an_empty_table() {
        let dir = scratch("missing");
        let users = WebUsers::load(dir.join("web-users")).unwrap();
        assert!(users.is_empty());
        assert_eq!(users.verify("admin", "pw"), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn add_verify_and_round_trip_through_the_file() {
        let dir = scratch("roundtrip");
        let mut users = table(&dir);
        users.add("admin", Role::Admin, "adminpw").unwrap();
        users.add("view.er-1", Role::Viewer, "viewpw").unwrap();

        assert_eq!(users.verify("admin", "adminpw"), Some((Role::Admin, 1)));
        assert_eq!(users.verify("admin", "viewpw"), None);
        assert_eq!(users.verify("view.er-1", "viewpw"), Some((Role::Viewer, 1)));
        assert_eq!(users.verify("ghost", "adminpw"), None);

        let reloaded = WebUsers::load(users.path()).unwrap();
        assert_eq!(reloaded.users, users.users);
        assert_eq!(reloaded.list(), users.list());

        let text = std::fs::read_to_string(users.path()).unwrap();
        let line = text.lines().find(|l| l.starts_with("admin:")).unwrap();
        let fields: Vec<&str> = line.split(':').collect();
        assert_eq!(fields.len(), 7);
        assert_eq!(&fields[1..4], ["admin", "pbkdf2-sha256", "10"]);
        assert_eq!(fields[6], "1");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn written_file_is_private_and_leaves_no_tmp() {
        let dir = scratch("mode");
        let mut users = table(&dir);
        users.add("admin", Role::Admin, "pw").unwrap();
        let mode = std::fs::metadata(users.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!crate::atomic::tmp_path(users.path()).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn duplicate_and_invalid_users_are_refused() {
        let dir = scratch("invalid");
        let mut users = table(&dir);
        users.add("admin", Role::Admin, "pw").unwrap();
        assert!(matches!(
            users.add("admin", Role::Viewer, "pw"),
            Err(Error::UserExists { .. })
        ));
        for bad in ["", "a:b", "a b", "#x", "é", &"x".repeat(33)] {
            assert!(
                matches!(
                    users.add(bad, Role::Viewer, "pw"),
                    Err(Error::InvalidUsername { .. })
                ),
                "{bad:?}"
            );
        }
        assert!(matches!(
            users.add("other", Role::Viewer, ""),
            Err(Error::EmptyPassword)
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn last_admin_cannot_be_deleted_or_demoted() {
        let dir = scratch("guard");
        let mut users = table(&dir);
        users.add("root", Role::Admin, "pw").unwrap();
        users.add("guest", Role::Viewer, "pw").unwrap();

        assert!(matches!(users.delete("root"), Err(Error::LastAdmin)));
        assert!(matches!(
            users.set_role("root", Role::Viewer),
            Err(Error::LastAdmin)
        ));
        assert_eq!(users.lookup("root"), Some((Role::Admin, 1)));

        // With a second admin, either may go.
        users.set_role("guest", Role::Admin).unwrap();
        users.set_role("root", Role::Viewer).unwrap();
        assert!(matches!(users.delete("guest"), Err(Error::LastAdmin)));
        users.delete("root").unwrap();
        assert_eq!(users.lookup("root"), None);
        // Viewers can always be deleted.
        users.add("v", Role::Viewer, "pw").unwrap();
        users.delete("v").unwrap();
        assert!(matches!(
            users.delete("nobody"),
            Err(Error::UnknownUser { .. })
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn changes_bump_the_generation() {
        let dir = scratch("gen");
        let mut users = table(&dir);
        users.add("root", Role::Admin, "old").unwrap();
        users.add("bob", Role::Viewer, "pw").unwrap();

        users.set_password("root", "new").unwrap();
        assert_eq!(users.verify("root", "old"), None);
        assert_eq!(users.verify("root", "new"), Some((Role::Admin, 2)));

        users.set_role("bob", Role::Admin).unwrap();
        assert_eq!(users.lookup("bob"), Some((Role::Admin, 2)));
        // Same role: nothing changes.
        users.set_role("bob", Role::Admin).unwrap();
        assert_eq!(users.lookup("bob"), Some((Role::Admin, 2)));

        assert!(matches!(
            users.set_password("nobody", "x"),
            Err(Error::UnknownUser { .. })
        ));
        assert_eq!(WebUsers::load(users.path()).unwrap().users, users.users);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn external_edits_are_picked_up() {
        let dir = scratch("reload");
        let mut users = table(&dir);
        users.add("root", Role::Admin, "pw").unwrap();
        assert!(!users.refresh().unwrap());

        let mut other = WebUsers::load(users.path()).unwrap();
        other.iterations = NonZeroU32::new(10).unwrap();
        other.add("second", Role::Viewer, "pw").unwrap();

        assert!(users.refresh().unwrap());
        assert_eq!(users.lookup("second"), Some((Role::Viewer, 1)));

        std::fs::remove_file(users.path()).unwrap();
        assert!(users.refresh().unwrap());
        assert!(users.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mutations_see_external_edits_first() {
        let dir = scratch("stale");
        let mut a = table(&dir);
        a.add("root", Role::Admin, "pw").unwrap();
        let mut b = WebUsers::load(a.path()).unwrap();
        b.iterations = NonZeroU32::new(10).unwrap();
        b.add("bob", Role::Viewer, "pw").unwrap();
        // `a` has not refreshed, but its write must not drop bob.
        a.add("carol", Role::Viewer, "pw").unwrap();
        let names: Vec<String> = WebUsers::load(a.path())
            .unwrap()
            .list()
            .into_iter()
            .map(|u| u.name)
            .collect();
        assert_eq!(names, ["bob", "carol", "root"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bad_lines_are_reported_and_the_old_table_kept() {
        let dir = scratch("bad");
        let mut users = table(&dir);
        users.add("root", Role::Admin, "pw").unwrap();
        let good = std::fs::read_to_string(users.path()).unwrap();
        let line = good.lines().find(|l| l.starts_with("root:")).unwrap();

        let cases = [
            ("fields", line.rsplit_once(':').unwrap().0.to_owned()),
            ("role", line.replacen(":admin:", ":owner:", 1)),
            ("algo", line.replace("pbkdf2-sha256", "md5")),
            ("gen", format!("{}:x", line.rsplit_once(':').unwrap().0)),
            ("user", line.replacen("root:", "ro ot:", 1)),
            ("dup", format!("{line}\n{line}")),
        ];
        for (name, bad) in cases {
            crate::atomic::write(users.path(), format!("# c\n{bad}\n").as_bytes(), 0o600).unwrap();
            match users.refresh() {
                Err(Error::Malformed { line, .. }) => assert!(line >= 2, "{name}"),
                other => panic!("{name}: {other:?}"),
            }
            assert_eq!(users.lookup("root"), Some((Role::Admin, 1)), "{name}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn world_readable_file_is_refused() {
        let dir = scratch("perms");
        let path = dir.join("web-users");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            WebUsers::load(&path),
            Err(Error::Permissions { mode: 0o644, .. })
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hash_line_parses_back() {
        let line = hash_line("admin", Role::Admin, "secret").unwrap();
        let (user, entry) = parse_line(&line).unwrap();
        assert_eq!(user, "admin");
        assert_eq!(entry.role, Role::Admin);
        assert_eq!(entry.gen, 1);
        assert!(entry.hash.verify("secret"));
        assert!(line.contains(":pbkdf2-sha256:20000:"));
        assert!(hash_line("a:b", Role::Admin, "x").is_err());
        assert!(hash_line("a", Role::Admin, "").is_err());
    }

    #[test]
    fn role_spellings() {
        assert_eq!("admin".parse::<Role>(), Ok(Role::Admin));
        assert_eq!("viewer".parse::<Role>(), Ok(Role::Viewer));
        assert!("Admin".parse::<Role>().is_err());
        assert_eq!(Role::Viewer.to_string(), "viewer");
    }
}
