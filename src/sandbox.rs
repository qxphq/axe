//! Kernel-enforced confinement for `bash`.
//!
//! Two layers, both in the kernel, because a string is not a guard: the
//! first-token check this replaces let `ls; rm -rf .`, `ls $(curl x)` and
//! `git -c alias.p='!sh' p` through.
//!
//! Layer 1 denies the network to every child, in every mode. It is mandatory:
//! where it cannot be installed the command does not run. Layer 2 confines
//! explore mode to a scratch directory and a fixed exec allowlist; it degrades
//! with one startup line on a Linux kernel without Landlock, because there is
//! nothing else to use there.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// What explore mode may execute, by name; the platform code adds the shell.
/// `git` is here whole: with writes denied a subcommand list decides nothing,
/// and `git log` has to keep working.
pub const COMMANDS: &[&str] = &[
    "cat", "cut", "date", "df", "diff", "du", "fd", "file", "find", "git", "grep", "head", "id",
    "jq", "ls", "ps", "pwd", "rg", "sort", "stat", "tail", "tr", "tree", "uname", "uniq", "wc",
    "which",
];

/// The one writable directory in explore mode: `0700`, canonicalised because
/// both sandboxes match real paths, removed at exit.
static SCRATCH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// One line for the startup banner when confinement is weaker than promised.
pub fn init() -> Option<String> {
    imp::notice()
}

/// The command to spawn: the platform decides program and argv, so `bash()`
/// never names a shell and never builds a profile.
pub fn shell(command: &str, read_only: bool) -> Result<Command, String> {
    imp::shell(command, read_only)
}

/// Best effort, at exit: a static's `Drop` never runs.
pub fn cleanup() {
    if let Some(Some(dir)) = SCRATCH.get() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// First `name` on PATH, canonicalised: Landlock and SBPL both match the real
/// path, and `/bin` is a symlink to `/usr/bin` on most Linux systems.
fn resolve(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .and_then(|candidate| std::fs::canonicalize(candidate).ok())
}

/// The shell as an absolute path: `Command::new("sh")` would leave the lookup
/// to `execvp` in the child, after the sandbox is installed.
fn shell_path() -> Result<PathBuf, String> {
    resolve("sh")
        .or_else(|| resolve("bash"))
        .ok_or_else(|| "sandbox: no sh on PATH".to_string())
}

fn scratch() -> Option<&'static Path> {
    SCRATCH
        .get_or_init(|| {
            use std::os::unix::fs::DirBuilderExt;
            for n in 0..16 {
                let path = std::env::temp_dir().join(format!("nano-{}-{n}", std::process::id()));
                match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                    Ok(()) => return std::fs::canonicalize(&path).ok(),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(_) => return None,
                }
            }
            None
        })
        .as_deref()
}

fn scratch_dir() -> Result<&'static Path, String> {
    scratch().ok_or_else(|| "sandbox: cannot create a scratch directory".to_string())
}

#[cfg(target_os = "linux")]
mod imp {
    use std::ffi::CString;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::OnceLock;

    use super::{resolve, scratch_dir, shell_path, COMMANDS};

    /// `socket` number and `AUDIT_ARCH_*` value the filter pins. `None` on an
    /// architecture this file has no numbers for: Layer 1 is mandatory, so no
    /// command runs there.
    #[cfg(target_arch = "x86_64")]
    const TARGET: Option<(u32, u32)> = Some((41, 0xc000_003e));
    #[cfg(target_arch = "aarch64")]
    const TARGET: Option<(u32, u32)> = Some((198, 0xc000_00b7));
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    const TARGET: Option<(u32, u32)> = None;

    // Classic BPF, from <linux/bpf_common.h>. The class is part of the code,
    // so `JEQ|K` is 0x15 and not 0x10.
    const LD_ABS: u16 = 0x20;
    const JEQ_K: u16 = 0x15;
    const JGE_K: u16 = 0x35;
    const RET_K: u16 = 0x06;
    const RET_ALLOW: u32 = 0x7fff_0000;
    const RET_ERRNO: u32 = 0x0005_0000;
    const EPERM: u32 = 1;

    /// Offsets into `struct seccomp_data`: nr, arch, args[0].
    const NR: u32 = 0;
    const ARCH: u32 = 4;
    const ARG0: u32 = 16;

    const IO_URING_SETUP: u32 = 425;
    const X32_BIT: u32 = 0x4000_0000;
    /// Not in libc; CAP_NET_RAW-gated like `AF_PACKET`, and it sends frames.
    const AF_XDP: u32 = 44;

    // libc exposes the syscall numbers for linux-gnu, but neither `prctl` nor
    // its constants, so both are declared here.
    const PR_SET_SECCOMP: libc::c_long = 22;
    const PR_SET_NO_NEW_PRIVS: libc::c_long = 38;
    const SECCOMP_MODE_FILTER: libc::c_long = 2;

    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_long = 1;
    const LANDLOCK_RULE_PATH_BENEATH: libc::c_long = 1;

    // Filesystem rights, in kernel order.
    const FS_EXECUTE: u64 = 1 << 0;
    const FS_WRITE_FILE: u64 = 1 << 1;
    const FS_REMOVE_DIR: u64 = 1 << 4;
    const FS_REMOVE_FILE: u64 = 1 << 5;
    const FS_MAKE_CHAR: u64 = 1 << 6;
    const FS_MAKE_DIR: u64 = 1 << 7;
    const FS_MAKE_REG: u64 = 1 << 8;
    const FS_MAKE_SOCK: u64 = 1 << 9;
    const FS_MAKE_FIFO: u64 = 1 << 10;
    const FS_MAKE_BLOCK: u64 = 1 << 11;
    const FS_MAKE_SYM: u64 = 1 << 12;
    const FS_REFER: u64 = 1 << 13;
    const FS_TRUNCATE: u64 = 1 << 14;
    const NET_CONNECT_TCP: u64 = 1 << 0;
    const NET_BIND_TCP: u64 = 1 << 1;

    /// A pre-6.7 kernel knows only the first field; the size passed with it
    /// says how much of the struct exists.
    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
    }

    /// Packed, as the kernel declares it: 12 bytes, not 16.
    #[repr(C, packed)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    /// The ELF interpreter is opened with `__FMODE_EXEC`, so Landlock demands
    /// `EXECUTE` on it too: without it every dynamically linked binary is
    /// refused even when its own rule is there.
    const LOADERS: &[&str] = &[
        "/lib64/ld-linux-x86-64.so.2",
        "/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
        "/lib/ld-linux-aarch64.so.1",
        "/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1",
        "/lib/ld-musl-x86_64.so.1",
        "/lib/ld-musl-aarch64.so.1",
    ];

    struct Ruleset(OwnedFd);

    pub fn notice() -> Option<String> {
        if TARGET.is_none() {
            return Some("bash is unavailable: no seccomp filter for this architecture".into());
        }
        abi().is_none().then(|| {
            "sandbox: Landlock is unavailable, so explore mode's bash is unconfined: any \
             command runs and anything is writable (its network is still denied)"
                .to_string()
        })
    }

    pub fn shell(command: &str, read_only: bool) -> Result<Command, String> {
        let Some((socket_nr, audit_arch)) = TARGET else {
            return Err("sandbox: bash is unavailable on this architecture".into());
        };
        let shell = shell_path()?;
        // Built here, in the parent: no allocation after fork.
        let rules = if read_only {
            ruleset(&shell)?.map(|set| set.0.as_raw_fd())
        } else {
            None
        };
        let mut cmd = Command::new(&shell);
        cmd.arg("-c").arg(command);
        // A read-only `git` must not try to refresh an index it cannot write.
        cmd.env("GIT_OPTIONAL_LOCKS", "0");
        if read_only {
            // On the child only: the parent resolves `temp_dir()` for its own
            // output files, and a process-wide change would move them into the
            // directory `cleanup` deletes.
            cmd.env("TMPDIR", scratch_dir()?);
        }
        // SAFETY: `pre_exec` is unsafe because the closure runs in the child
        // between fork and exec; this one calls only fixed syscalls on state
        // inherited from the parent, before any user program runs.
        unsafe {
            cmd.pre_exec(move || {
                no_new_privs()?;
                seccomp(socket_nr, audit_arch)?;
                if let Some(fd) = rules {
                    restrict(fd)?;
                }
                Ok(())
            });
        }
        Ok(cmd)
    }

    /// The kernel ABI, or `None` when the LSM is absent or the kernel predates
    /// Landlock. One syscall, so the startup probe can afford it.
    fn abi() -> Option<i32> {
        static ABI: OnceLock<Option<i32>> = OnceLock::new();
        *ABI.get_or_init(|| {
            // SAFETY: the version probe takes a null ruleset with size 0 and
            // the VERSION flag; the kernel reads nothing from this process.
            let ret = unsafe {
                libc::syscall(
                    libc::SYS_landlock_create_ruleset,
                    std::ptr::null::<RulesetAttr>(),
                    0 as libc::c_long,
                    LANDLOCK_CREATE_RULESET_VERSION,
                )
            };
            (ret > 0).then_some(ret as i32)
        })
    }

    /// `Ok(None)` when the kernel has no Landlock at all, which is reported at
    /// startup. An error means a ruleset that should have worked did not: the
    /// command is refused rather than run unconfined.
    fn ruleset(shell: &Path) -> Result<Option<&'static Ruleset>, String> {
        let Some(abi) = abi() else {
            return Ok(None);
        };
        static SET: OnceLock<Result<Ruleset, String>> = OnceLock::new();
        match SET.get_or_init(|| build(abi, shell)) {
            Ok(set) => Ok(Some(set)),
            Err(e) => Err(e.clone()),
        }
    }

    fn build(abi: i32, shell: &Path) -> Result<Ruleset, String> {
        let mut fs = FS_EXECUTE
            | FS_WRITE_FILE
            | FS_REMOVE_DIR
            | FS_REMOVE_FILE
            | FS_MAKE_CHAR
            | FS_MAKE_DIR
            | FS_MAKE_REG
            | FS_MAKE_SOCK
            | FS_MAKE_FIFO
            | FS_MAKE_BLOCK
            | FS_MAKE_SYM;
        if abi >= 2 {
            fs |= FS_REFER;
        }
        if abi >= 3 {
            fs |= FS_TRUNCATE;
        }
        let attr = RulesetAttr {
            handled_access_fs: fs,
            handled_access_net: if abi >= 4 {
                NET_CONNECT_TCP | NET_BIND_TCP
            } else {
                0
            },
        };
        let size = if abi >= 4 {
            std::mem::size_of::<RulesetAttr>()
        } else {
            8
        };
        // SAFETY: `attr` is a valid `RulesetAttr` and `size` is the prefix
        // the running ABI understands; the kernel copies, never retains it.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                size as libc::c_long,
                0 as libc::c_long,
            )
        };
        if fd < 0 {
            return Err(format!(
                "sandbox: landlock_create_ruleset: {}",
                io::Error::last_os_error()
            ));
        }
        // SAFETY: `fd` is a fresh non-negative descriptor returned by the
        // call above; `OwnedFd` takes ownership and closes it exactly once.
        let set = Ruleset(unsafe { OwnedFd::from_raw_fd(fd as RawFd) });

        let mut binaries: Vec<PathBuf> = COMMANDS.iter().filter_map(|name| resolve(name)).collect();
        binaries.push(shell.to_path_buf());
        binaries.extend(
            LOADERS
                .iter()
                .filter_map(|path| std::fs::canonicalize(path).ok()),
        );
        binaries.sort();
        binaries.dedup();
        for binary in &binaries {
            allow(&set, FS_EXECUTE, binary)?;
        }
        // `> /dev/null` is an open with O_TRUNC, which is checked against
        // TRUNCATE too: one right alone breaks the redirect.
        allow(
            &set,
            FS_WRITE_FILE | (fs & FS_TRUNCATE),
            Path::new("/dev/null"),
        )?;
        // Everything writable, nothing executable: a script written here must
        // not become a program.
        allow(&set, fs & !FS_EXECUTE, scratch_dir()?)?;
        Ok(set)
    }

    fn allow(set: &Ruleset, access: u64, path: &Path) -> Result<(), String> {
        let c = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| format!("sandbox: path has a NUL: {}", path.display()))?;
        // SAFETY: `c` is a NUL-terminated `CString`, and the flags are a
        // read-only path open; the returned descriptor is checked below.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(format!(
                "sandbox: open {}: {}",
                path.display(),
                io::Error::last_os_error()
            ));
        }
        // SAFETY: `fd` is the non-negative descriptor just opened; ownership
        // moves into `OwnedFd`, which closes it once.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let rule = PathBeneathAttr {
            allowed_access: access,
            parent_fd: fd.as_raw_fd(),
        };
        // SAFETY: the ruleset and parent descriptors are valid, and `rule`
        // outlives the call; the kernel copies the struct.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                set.0.as_raw_fd() as libc::c_long,
                LANDLOCK_RULE_PATH_BENEATH,
                &rule as *const PathBeneathAttr,
                0 as libc::c_long,
            )
        };
        if ret != 0 {
            return Err(format!(
                "sandbox: landlock_add_rule {}: {}",
                path.display(),
                io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    fn no_new_privs() -> io::Result<()> {
        // SAFETY: `prctl` with fixed constants and no pointer arguments.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_prctl,
                PR_SET_NO_NEW_PRIVS,
                1 as libc::c_long,
                0 as libc::c_long,
                0 as libc::c_long,
                0 as libc::c_long,
            )
        };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn seccomp(socket_nr: u32, audit_arch: u32) -> io::Result<()> {
        let filter = filter(socket_nr, audit_arch);
        let prog = libc::sock_fprog {
            len: filter.len() as u16,
            filter: filter.as_ptr() as *mut libc::sock_filter,
        };
        // SAFETY: `prog` points at `filter`, a stack array that outlives the
        // call, and its `len` matches; the kernel copies the filter.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_prctl,
                PR_SET_SECCOMP,
                SECCOMP_MODE_FILTER,
                &prog as *const libc::sock_fprog,
            )
        };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn restrict(fd: RawFd) -> io::Result<()> {
        // SAFETY: `fd` is the ruleset descriptor this process owns, and
        // `restrict_self` takes no pointer arguments.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_landlock_restrict_self,
                fd as libc::c_long,
                0 as libc::c_long,
            )
        };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// The arch word is pinned first: a 32-bit binary would reach `socket`
    /// through `socketcall`, with a number this filter lets through.
    fn filter(socket_nr: u32, audit_arch: u32) -> [libc::sock_filter; 16] {
        let insn = |code: u16, jt: u8, jf: u8, k: u32| libc::sock_filter { code, jt, jf, k };
        let deny = RET_ERRNO | EPERM;
        [
            insn(LD_ABS, 0, 0, ARCH),                  // 0  A = arch
            insn(JEQ_K, 1, 0, audit_arch),             // 1  -> 3, else 2
            insn(RET_K, 0, 0, deny),                   // 2  another ABI: deny all
            insn(LD_ABS, 0, 0, NR),                    // 3  A = nr
            insn(JGE_K, 9, 0, X32_BIT),                // 4  x32 -> 14
            insn(JEQ_K, 8, 0, IO_URING_SETUP),         // 5  -> 14
            insn(JEQ_K, 0, 8, socket_nr),              // 6  -> 7, else 15
            insn(LD_ABS, 0, 0, ARG0),                  // 7  A = args[0]
            insn(JEQ_K, 5, 0, libc::AF_INET as u32),   // 8  -> 14
            insn(JEQ_K, 4, 0, libc::AF_INET6 as u32),  // 9  -> 14
            insn(JEQ_K, 3, 0, libc::AF_PACKET as u32), // 10 -> 14
            insn(JEQ_K, 2, 0, libc::AF_VSOCK as u32),  // 11 -> 14
            insn(JEQ_K, 1, 0, AF_XDP),                 // 12 -> 14
            insn(RET_K, 0, 0, RET_ALLOW),              // 13 another family
            insn(RET_K, 0, 0, deny),                   // 14
            insn(RET_K, 0, 0, RET_ALLOW),              // 15
        ]
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::OnceLock;

    use super::{resolve, scratch_dir, shell_path, COMMANDS};

    const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

    /// Apple shims that re-exec a toolchain binary the profile cannot see:
    /// `/usr/bin/git` is one, and `realpath` does not reveal it. Probe-driven;
    /// a missing entry is `EPERM` at exec, never an escape.
    const TRAMPOLINES: &[&str] = &["git"];

    pub fn notice() -> Option<String> {
        (!Path::new(SANDBOX_EXEC).exists())
            .then(|| "bash is unavailable: /usr/bin/sandbox-exec is missing".to_string())
    }

    pub fn shell(command: &str, read_only: bool) -> Result<Command, String> {
        let profile = profile(read_only)?;
        let shell = shell_path()?;
        let mut cmd = Command::new(SANDBOX_EXEC);
        cmd.arg("-p")
            .arg(profile)
            .arg(&shell)
            .arg("-c")
            .arg(command);
        cmd.env("GIT_OPTIONAL_LOCKS", "0");
        if read_only {
            cmd.env("TMPDIR", scratch_dir()?);
        }
        Ok(cmd)
    }

    fn profile(read_only: bool) -> Result<&'static str, String> {
        static NORMAL: OnceLock<String> = OnceLock::new();
        static EXPLORE: OnceLock<Result<String, String>> = OnceLock::new();
        if !read_only {
            return Ok(NORMAL
                .get_or_init(|| "(version 1)\n(allow default)\n(deny network*)\n".to_string())
                .as_str());
        }
        match EXPLORE.get_or_init(build) {
            Ok(profile) => Ok(profile.as_str()),
            Err(e) => Err(e.clone()),
        }
    }

    /// Last match wins, so the allows follow the denies. Every path is
    /// canonical: `/var` is `/private/var` here, and a grant on the short form
    /// matches nothing.
    fn build() -> Result<String, String> {
        let mut profile =
            String::from("(version 1)\n(allow default)\n(deny network*)\n(deny file-write*)\n");
        profile.push_str(&format!(
            "(allow file-write* (subpath {}) (literal \"/dev/null\"))\n",
            quote(scratch_dir()?)
        ));
        profile.push_str("(deny process-exec)\n(allow process-exec");
        let mut paths: Vec<PathBuf> = COMMANDS.iter().filter_map(|name| resolve(name)).collect();
        paths.extend(["sh", "bash"].iter().copied().filter_map(resolve));
        paths.extend(TRAMPOLINES.iter().copied().filter_map(xcrun));
        paths.sort();
        paths.dedup();
        for path in &paths {
            profile.push_str(&format!(" (literal {})", quote(path)));
        }
        profile.push_str(")\n");
        Ok(profile)
    }

    fn quote(path: &Path) -> String {
        let text = path.to_string_lossy();
        format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
    }

    fn xcrun(name: &str) -> Option<PathBuf> {
        let out = Command::new("/usr/bin/xcrun")
            .arg("-f")
            .arg(name)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let path = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
        path.is_file()
            .then(|| std::fs::canonicalize(&path).unwrap_or(path))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    use std::process::Command;

    pub fn notice() -> Option<String> {
        Some("bash is unavailable: this platform has no sandbox".into())
    }

    pub fn shell(_command: &str, _read_only: bool) -> Result<Command, String> {
        Err("sandbox: this platform has no sandbox; bash is unavailable".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_is_nonempty_and_resolves_sh() {
        assert!(COMMANDS.contains(&"ls"));
        assert!(COMMANDS.contains(&"git"));
        assert!(resolve("sh").is_some());
    }

    #[test]
    fn init_and_shell_build_without_running() {
        let _ = init();
        assert!(shell("true", false).is_ok());
        assert!(shell("true", true).is_ok());
        cleanup();
    }
}
