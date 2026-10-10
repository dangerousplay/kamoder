//! Pseudo-terminal allocation and shell spawning.
//!
//! BusyBox on the camera expects a real controlling terminal, so an
//! interactive session needs a PTY rather than a plain pipe pair.
//!
//! `forkpty()` would be the obvious call, but it lives in `libutil`, which the
//! camera's uClibc build does not ship. `nix` is used for the pieces that do
//! exist (`posix_openpt`, `grantpt`, `unlockpt`, `fork`, `waitpid`, `kill`)
//! plus two ioctls declared here.
//!
//! The master is driven asynchronously through [`PtyIo`]: a blocking `write`
//! on it stalls for as long as the child is not reading — forever, if the
//! child has died with input still queued — and on a current-thread runtime
//! that stalls every connection, not just this one.

use nix::pty::{grantpt, posix_openpt, PtyMaster};
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{ForkResult, Pid};
use snafu::{OptionExt, ResultExt, Snafu};
use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

// `TIOCGPTN` — read the pty number behind a `/dev/ptmx` master. Declared here
// because `libc` does not export it for this target.
nix::ioctl_read_bad!(
    tiocgptn,
    nix::request_code_read!(b'T', 0x30, 4),
    libc::c_int
);
// `TIOCSWINSZ` — tell the slave its new dimensions.
nix::ioctl_write_ptr_bad!(tiocswinsz, libc::TIOCSWINSZ, nix::pty::Winsize);

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("argument list is empty"))]
    EmptyArgv,

    #[snafu(display("{what} contains an interior NUL byte"))]
    InteriorNul { what: &'static str },

    #[snafu(display("cannot open /dev/ptmx: {source}"))]
    OpenPty { source: nix::Error },

    #[snafu(display("cannot unlock the pty slave: {source}"))]
    UnlockPty { source: nix::Error },

    #[snafu(display("cannot read the pty number: {source}"))]
    PtyNumber { source: nix::Error },

    #[snafu(display("cannot set the terminal size: {source}"))]
    WindowSize { source: nix::Error },

    #[snafu(display("cannot fork: {source}"))]
    Fork { source: nix::Error },

    #[snafu(display("cannot duplicate the pty master: {source}"))]
    CloneMaster { source: std::io::Error },

    #[snafu(display("cannot make the pty master non-blocking: {source}"))]
    NonBlocking { source: nix::Error },

    #[snafu(display("cannot register the pty master with the runtime: {source}"))]
    Register { source: std::io::Error },

    #[snafu(display("cannot reap the child process: {source}"))]
    Wait { source: nix::Error },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Exit status the shell convention assigns to a signalled process.
const SIGNAL_EXIT_BASE: i32 = 128;

/// A running child process attached to a pseudo-terminal.
#[derive(Debug)]
pub struct Pty {
    master: PtyMaster,
    child: Pid,
    /// Set once the child has been reaped; its pid may belong to someone else
    /// after that, so it must not be signalled again.
    reaped: AtomicBool,
}

impl Pty {
    /// Opens a PTY and runs `program` on the slave side.
    ///
    /// `argv` is the child's argument vector *including* `argv[0]`, which is
    /// deliberately separate from `program`: a login shell is started by
    /// exec'ing `/bin/sh` with `argv[0]` set to `-sh`, and the two differ.
    /// `env` entries are passed as `KEY=VALUE`.
    ///
    /// Nothing is interpreted by a shell on the way in — the child is
    /// `execve`'d directly with this exact vector.
    pub fn spawn(program: &str, argv: &[&str], env: &[String], size: WindowSize) -> Result<Self> {
        if argv.is_empty() {
            return Err(Error::EmptyArgv);
        }

        // Everything the child needs is built *before* the fork. After
        // `fork()` in a multi-threaded process only async-signal-safe calls
        // are legal, and allocating can deadlock on a malloc lock another
        // thread held at the moment of the fork. That is also why the child
        // path below uses raw `libc` rather than `nix`: `nix::unistd::execve`
        // builds its own pointer array, which allocates.
        let program = CString::new(program)
            .ok()
            .context(InteriorNulSnafu { what: "program" })?;
        let argv_owned: Vec<CString> = argv
            .iter()
            .map(|arg| CString::new(*arg))
            .collect::<std::result::Result<_, _>>()
            .ok()
            .context(InteriorNulSnafu { what: "argv" })?;
        let mut argv_ptrs: Vec<*const libc::c_char> =
            argv_owned.iter().map(|arg| arg.as_ptr()).collect();
        argv_ptrs.push(std::ptr::null());

        let env_owned: Vec<CString> = env
            .iter()
            .map(|entry| CString::new(entry.as_str()))
            .collect::<std::result::Result<_, _>>()
            .ok()
            .context(InteriorNulSnafu { what: "env" })?;
        let mut env_ptrs: Vec<*const libc::c_char> =
            env_owned.iter().map(|entry| entry.as_ptr()).collect();
        env_ptrs.push(std::ptr::null());

        let master = open_master()?;
        let slave_path = slave_path(&master)?;
        set_window_size(master.as_fd(), size)?;

        // SAFETY: the child only runs async-signal-safe syscalls on pointers
        // prepared above, and exits without returning into Rust.
        match unsafe { nix::unistd::fork() }.context(ForkSnafu)? {
            ForkResult::Parent { child } => Ok(Self {
                master,
                child,
                reaped: AtomicBool::new(false),
            }),
            ForkResult::Child => {
                // There is no safe way to report an error from here, so any
                // failure exits immediately with a distinct status.
                // SAFETY: every call below is async-signal-safe.
                unsafe {
                    if libc::setsid() == -1 {
                        libc::_exit(127);
                    }
                    let slave = libc::open(slave_path.as_ptr(), libc::O_RDWR);
                    if slave < 0 {
                        libc::_exit(127);
                    }
                    // Make the slave this session's controlling terminal, so
                    // job control and Ctrl-C work in the shell.
                    if libc::ioctl(slave, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                        libc::_exit(127);
                    }
                    for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
                        if libc::dup2(slave, target) == -1 {
                            libc::_exit(127);
                        }
                    }
                    if slave > libc::STDERR_FILENO {
                        libc::close(slave);
                    }
                    // Rust ignores SIGPIPE in this process, and an ignored
                    // signal stays ignored across execve. The shell and its
                    // pipelines expect the default (`yes | head` must end).
                    libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                    libc::execve(program.as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr());
                    libc::_exit(127);
                }
            }
        }
    }

    /// The child's pid, which is also its session and process group id.
    pub fn pid(&self) -> i32 {
        self.child.as_raw()
    }

    /// Non-blocking, runtime-driven access to the master, for both directions.
    ///
    /// Must be called from within a Tokio runtime. This switches the master's
    /// open file description to `O_NONBLOCK`, which is shared with every
    /// duplicate, so nothing may do blocking I/O on this PTY afterwards.
    pub fn io(&self) -> Result<PtyIo> {
        use nix::fcntl::{fcntl, FcntlArg, OFlag};

        let fd = self
            .master
            .as_fd()
            .try_clone_to_owned()
            .context(CloneMasterSnafu)?;
        let flags = fcntl(&fd, FcntlArg::F_GETFL).context(NonBlockingSnafu)?;
        let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
        fcntl(&fd, FcntlArg::F_SETFL(flags)).context(NonBlockingSnafu)?;
        let fd = AsyncFd::new(fd).context(RegisterSnafu)?;
        Ok(PtyIo(Arc::new(fd)))
    }

    /// Tells the child that the terminal was resized.
    pub fn resize(&self, size: WindowSize) -> Result<()> {
        set_window_size(self.master.as_fd(), size)
    }

    /// Sends `SIGHUP` to the child, as a real terminal would on disconnect.
    pub fn hangup(&self) {
        if self.reaped.load(Ordering::Acquire) {
            return;
        }
        if let Err(errno) = kill(self.child, Signal::SIGHUP) {
            tracing::debug!(%errno, child = %self.child, "cannot signal the child");
        }
    }

    /// Reaps the child if it has exited, without blocking.
    pub fn try_wait(&self) -> Result<Option<i32>> {
        if self.reaped.load(Ordering::Acquire) {
            return Ok(None);
        }
        match waitpid(self.child, Some(WaitPidFlag::WNOHANG)).context(WaitSnafu)? {
            WaitStatus::StillAlive => Ok(None),
            status @ (WaitStatus::Exited(..) | WaitStatus::Signaled(..)) => {
                self.reaped.store(true, Ordering::Release);
                Ok(Some(exit_code(status)))
            }
            // Stopped/continued: still alive.
            _ => Ok(None),
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        self.hangup();
        // Reap without blocking, so a wedged child cannot hang the server.
        let _ = self.try_wait();
    }
}

/// The PTY master as an async byte stream: reads are the child's terminal
/// output, writes are its terminal input. Clones share one registration, so
/// one task can read while another writes.
#[derive(Clone)]
pub struct PtyIo(Arc<AsyncFd<OwnedFd>>);

impl AsyncRead for PtyIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.0.poll_read_ready(cx))?;
            let unfilled = buf.initialize_unfilled();
            match guard
                .try_io(|fd| nix::unistd::read(fd.get_ref(), unfilled).map_err(io::Error::from))
            {
                Ok(Ok(read)) => {
                    buf.advance(read);
                    return Poll::Ready(Ok(()));
                }
                // EIO is how Linux reports that the last slave closed, which
                // is the normal way a shell exits: treat it as end of file.
                Ok(Err(error)) if error.raw_os_error() == Some(libc::EIO) => {
                    return Poll::Ready(Ok(()))
                }
                Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
                Ok(Err(error)) => return Poll::Ready(Err(error)),
                Err(_would_block) => continue,
            }
        }
    }
}

impl AsyncWrite for PtyIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = ready!(self.0.poll_write_ready(cx))?;
            match guard.try_io(|fd| nix::unistd::write(fd.get_ref(), data).map_err(io::Error::from))
            {
                Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // A terminal has no half-close; the master stays open for output.
        Poll::Ready(Ok(()))
    }
}

/// Terminal dimensions.
#[derive(Debug, Clone, Copy)]
pub struct WindowSize {
    pub columns: u16,
    pub rows: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl Default for WindowSize {
    fn default() -> Self {
        Self {
            columns: 80,
            rows: 24,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

impl WindowSize {
    /// Builds a size from the client's request, clamping to what `winsize` can
    /// hold — the SSH protocol carries these as `u32`.
    pub fn from_request(columns: u32, rows: u32, pixel_width: u32, pixel_height: u32) -> Self {
        Self {
            columns: columns.min(u16::MAX as u32) as u16,
            rows: rows.min(u16::MAX as u32) as u16,
            pixel_width: pixel_width.min(u16::MAX as u32) as u16,
            pixel_height: pixel_height.min(u16::MAX as u32) as u16,
        }
    }
}

fn open_master() -> Result<PtyMaster> {
    use nix::fcntl::OFlag;

    let master =
        posix_openpt(OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_CLOEXEC).context(OpenPtySnafu)?;
    // On a devpts mount `grantpt` is a no-op, but it is cheap and keeps this
    // correct anywhere that still needs the ownership fix-up.
    grantpt(&master).context(UnlockPtySnafu)?;
    nix::pty::unlockpt(&master).context(UnlockPtySnafu)?;
    Ok(master)
}

/// Resolves `/dev/pts/<n>` for a master.
///
/// `nix::pty::ptsname` is not thread-safe and `ptsname_r` is a glibc extension
/// the crosstool's uClibc does not export, so the number is read directly.
fn slave_path(master: &PtyMaster) -> Result<CString> {
    let mut number: libc::c_int = 0;
    // SAFETY: live fd and a live `c_int` out-parameter, which is what TIOCGPTN
    // expects.
    unsafe { tiocgptn(master.as_raw_fd(), &mut number) }.context(PtyNumberSnafu)?;
    CString::new(format!("/dev/pts/{number}"))
        .ok()
        .context(InteriorNulSnafu { what: "pty path" })
}

fn set_window_size(fd: BorrowedFd<'_>, size: WindowSize) -> Result<()> {
    let winsize = nix::pty::Winsize {
        ws_row: size.rows,
        ws_col: size.columns,
        ws_xpixel: size.pixel_width,
        ws_ypixel: size.pixel_height,
    };
    // SAFETY: live fd and a fully initialised `winsize`, which is what
    // TIOCSWINSZ expects.
    unsafe { tiocswinsz(fd.as_raw_fd(), &winsize) }.context(WindowSizeSnafu)?;
    Ok(())
}

/// Turns a wait status into an SSH exit status.
fn exit_code(status: WaitStatus) -> i32 {
    match status {
        WaitStatus::Exited(_, code) => code,
        // Convention borrowed from shells: 128 + signal number.
        WaitStatus::Signaled(_, signal, _) => SIGNAL_EXIT_BASE + signal as i32,
        _ => 1,
    }
}

/// Reads the login shell for `uid` from `/etc/passwd`, falling back to
/// `/bin/sh`.
pub fn login_shell(uid: u32) -> String {
    const FALLBACK: &str = "/bin/sh";
    let Ok(passwd) = std::fs::read_to_string("/etc/passwd") else {
        return FALLBACK.into();
    };
    for line in passwd.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() < 7 {
            continue;
        }
        if fields[2].parse::<u32>() == Ok(uid) && !fields[6].is_empty() {
            return fields[6].to_owned();
        }
    }
    FALLBACK.into()
}
