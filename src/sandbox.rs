//! Kernel-enforced confinement for `bash`.
//!
//! One layer, in the kernel, because a string is not a guard: the first-token
//! check this replaces let `ls; rm -rf .`, `ls $(curl x)` and
//! `git -c alias.p='!sh' p` through. In explore mode it confines `bash` to a
//! scratch directory and a fixed exec allowlist; it degrades with one startup
//! line on a Linux kernel without Landlock, because there is nothing else to
//! use there. Normal mode keeps only `no_new_privs` and the environment
//! whitelist.
//!
//! The network is not confined in either mode: a child reaches what the user
//! reaches. `io_uring_setup` stays denied in explore mode because Landlock does
//! not mediate io_uring, and allowing it would punch a hole in the write
//! confinement.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// What explore mode may execute, by name; the platform code adds the shell.
/// This is a convenience guard, not a boundary: Landlock confines the writes,
/// so a name here can only fail against the kernel. `git` is here whole: with
/// writes denied a subcommand list decides nothing, and `git log` has to keep
/// working. `curl` writes nothing on its own, so a networked read is an
/// explore-mode command like any other. Interpreters and package managers are
/// included for the same reason: their mutation is a write, and writes are
/// what the kernel denies. Excluded on purpose: daemons that write through
/// another process (`docker`, `systemctl`, `dbus-send`), setuid
/// (`sudo`, `mount`), programs whose side effect is a mode or a timestamp
/// rather than a write (`chmod`, `touch`), and tools that exec their own
/// `/nix/store` or `/opt/homebrew` paths (`nix`, `brew`). A name that is not
/// installed is skipped.
pub const COMMANDS: &[&str] = &[
    "apk",
    "apt",
    "apt-cache",
    "apt-get",
    "awk",
    "b2sum",
    "base64",
    "basename",
    "bc",
    "bun",
    "bundle",
    "cargo",
    "cat",
    "cksum",
    "cmp",
    "column",
    "comm",
    "composer",
    "conda",
    "cp",
    "curl",
    "cut",
    "date",
    "df",
    "diff",
    "dirname",
    "dnf",
    "dpkg",
    "du",
    "env",
    "expand",
    "expr",
    "fd",
    "file",
    "find",
    "fmt",
    "fold",
    "free",
    "gem",
    "git",
    "go",
    "grep",
    "groups",
    "head",
    "hexdump",
    "iconv",
    "id",
    "join",
    "jq",
    "ln",
    "locate",
    "ls",
    "md5sum",
    "mkdir",
    "mvn",
    "mv",
    "nl",
    "node",
    "npm",
    "nproc",
    "numfmt",
    "npx",
    "od",
    "pacman",
    "paste",
    "perl",
    "pgrep",
    "pip",
    "pip3",
    "pnpm",
    "poetry",
    "pr",
    "printenv",
    "ps",
    "pwd",
    "python3",
    "readlink",
    "realpath",
    "rg",
    "rm",
    "rmdir",
    "rpm",
    "ruby",
    "rustup",
    "sed",
    "seq",
    "sha1sum",
    "sha256sum",
    "sha512sum",
    "shuf",
    "sort",
    "split",
    "stat",
    "strings",
    "tac",
    "tail",
    "tee",
    "tput",
    "tr",
    "tree",
    "tty",
    "uname",
    "unexpand",
    "uniq",
    "uptime",
    "uv",
    "wc",
    "whereis",
    "which",
    "whoami",
    "xargs",
    "xxd",
    "yarn",
    "zypper",
];

/// The one writable directory in explore mode: `0700`, canonicalised because
/// both sandboxes match real paths, removed at exit.
static SCRATCH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// The second writable directory, and the one that outlives the session: the
/// project's plan. Registered by `main` before the first command runs.
static PLAN: OnceLock<Option<PathBuf>> = OnceLock::new();

/// One line for the startup banner when confinement is weaker than promised.
pub fn init() -> Option<String> {
    #[cfg(target_os = "linux")]
    no_dump();
    imp::notice()
}

/// Keep other processes — including a `bash` child that knows this pid — from
/// reading axe's environment through `/proc`. Best effort: the kernel may
/// refuse, and the `/proc/self/environ` path of the parent itself is closed in
/// the `read` tool, not here.
#[cfg(target_os = "linux")]
fn no_dump() {
    const PR_SET_DUMPABLE: libc::c_long = 4;
    // SAFETY: `prctl` with fixed constants and no pointer arguments.
    unsafe {
        libc::syscall(libc::SYS_prctl, PR_SET_DUMPABLE, 0 as libc::c_long);
    }
}

/// Variables a command needs to behave like a command. Everything else —
/// provider and search tokens included — stays in the parent.
const CHILD_ENV: &[&str] = &[
    "PATH", "HOME", "LANG", "LC_ALL", "TERM", "TMPDIR", "USER", "SHELL",
];

fn confine_env(cmd: &mut Command, read_only: bool) -> Result<(), String> {
    cmd.env_clear();
    for name in CHILD_ENV {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    // A read-only `git` must not try to refresh an index it cannot write.
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    if read_only {
        // On the child only: the parent resolves `temp_dir()` for its own
        // output files, and a process-wide change would move them into the
        // directory `cleanup` deletes.
        cmd.env("TMPDIR", scratch_dir()?);
    }
    Ok(())
}

/// The command to spawn: the platform decides program and argv, so `bash()`
/// never names a shell and never builds a profile.
pub fn shell(command: &str, read_only: bool) -> Result<Command, String> {
    imp::shell(command, read_only)
}

/// A program axe starts itself, with the confinement the mode allows: an MCP
/// server. Read-only keeps the write rule (scratch, the plan directory,
/// `/dev/null`) and the filter that closes `io_uring`, and leaves exec alone:
/// the allowlist is a convenience for `bash`, and a server starts its own
/// runtime (`npx`, `uvx`) the leading-word list would refuse.
pub fn server(
    program: &Path,
    args: &[String],
    env: &[(String, String)],
    read_only: bool,
) -> Result<Command, String> {
    imp::server(program, args, env, read_only)
}

/// First `name` on PATH, canonicalised, for a caller that must exec it.
pub(crate) fn resolve_program(name: &str) -> Option<PathBuf> {
    resolve(name)
}

/// Best effort, at exit: a static's `Drop` never runs.
pub fn cleanup() {
    if let Some(Some(dir)) = SCRATCH.get() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Register the project's plan directory, which explore mode may write and
/// never execute. Canonicalised for the same reason as the scratch directory;
/// a path that cannot be resolved leaves the directory out of the ruleset
/// rather than widening it.
pub fn set_plan(path: &Path) {
    let _ = PLAN.set(std::fs::canonicalize(path).ok());
}

/// The plan directory, when the run has a working directory to keep one in.
pub fn plan() -> Option<&'static Path> {
    PLAN.get().and_then(|dir| dir.as_deref())
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

/// Git's network transports, which `PATH` does not list: `git` execs
/// `git-remote-https` and its siblings from its own exec directory, so without
/// an executable rule here an https remote is refused by the same ruleset that
/// allows `git` itself. `git --exec-path` is the only honest way to the
/// directory, and a host without git simply has nothing to add.
fn git_remotes() -> Vec<PathBuf> {
    let Some(git) = resolve("git") else {
        return Vec::new();
    };
    let Ok(out) = Command::new(git).arg("--exec-path").output() else {
        return Vec::new();
    };
    let Ok(text) = String::from_utf8(out.stdout) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(text.trim()) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("git-remote-"))
        })
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .filter(|path| path.is_file())
        .collect()
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
                let path = std::env::temp_dir().join(format!("axe-{}-{n}", std::process::id()));
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
    use std::ffi::{CString, OsStr};
    use std::io::{self, Read, Seek, SeekFrom};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::OnceLock;

    use super::{confine_env, git_remotes, resolve, scratch_dir, shell_path, COMMANDS};

    /// The `AUDIT_ARCH_*` value the filter pins. `None` on an architecture this
    /// file has no numbers for, where explore mode's `bash` cannot be confined.
    #[cfg(target_arch = "x86_64")]
    const TARGET: Option<u32> = Some(0xc000_003e);
    #[cfg(target_arch = "aarch64")]
    const TARGET: Option<u32> = Some(0xc000_00b7);
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    const TARGET: Option<u32> = None;

    // Classic BPF, from <linux/bpf_common.h>. The class is part of the code,
    // so `JEQ|K` is 0x15 and not 0x10.
    const LD_ABS: u16 = 0x20;
    const JEQ_K: u16 = 0x15;
    const JGE_K: u16 = 0x35;
    const RET_K: u16 = 0x06;
    const RET_ALLOW: u32 = 0x7fff_0000;
    const RET_ERRNO: u32 = 0x0005_0000;
    const EPERM: u32 = 1;

    /// Offsets into `struct seccomp_data`: nr and arch.
    const NR: u32 = 0;
    const ARCH: u32 = 4;

    const IO_URING_SETUP: u32 = 425;
    const X32_BIT: u32 = 0x4000_0000;

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

    /// One field: `handled_access_net` stays zero, so the ruleset says nothing
    /// about sockets and the network is not confined.
    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
    }

    /// Packed, as the kernel declares it: 12 bytes, not 16.
    #[repr(C, packed)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    /// The ELF interpreter is opened with `__FMODE_EXEC`, so Landlock demands
    /// `EXECUTE` on it too: without it every dynamically linked binary is
    /// refused even when its own rule is there. It is read from each binary's
    /// `PT_INTERP` rather than assumed: NixOS has no `/lib64/ld-linux-*.so.2`,
    /// its interpreter lives under `/nix/store`.
    pub(super) fn interpreter(binary: &Path) -> Option<PathBuf> {
        let mut file = std::fs::File::open(binary).ok()?;
        let mut header = [0u8; 64];
        file.read_exact(&mut header).ok()?;
        // ELF, 64-bit, little-endian: the only shape this platform can exec.
        if &header[0..4] != b"\x7fELF" || header[4] != 2 || header[5] != 1 {
            return None;
        }
        let phoff = le64(&header[32..40]);
        let phentsize = le16(&header[54..56]) as usize;
        let phnum = le16(&header[56..58]) as usize;
        // A corrupt table would be a large allocation; real binaries are tiny.
        if phnum == 0 || phentsize < 56 || phnum > 4096 {
            return None;
        }
        let mut table = vec![0u8; phnum * phentsize];
        file.seek(SeekFrom::Start(phoff)).ok()?;
        file.read_exact(&mut table).ok()?;
        for entry in table.chunks_exact(phentsize) {
            if le32(&entry[0..4]) != 3 {
                continue; // PT_INTERP
            }
            let offset = le64(&entry[8..16]);
            let size = le64(&entry[32..40]).min(4096) as usize;
            let mut name = vec![0u8; size];
            file.seek(SeekFrom::Start(offset)).ok()?;
            file.read_exact(&mut name).ok()?;
            name.truncate(name.iter().position(|&b| b == 0).unwrap_or(name.len()));
            return Some(PathBuf::from(OsStr::from_bytes(&name)));
        }
        None
    }

    fn le16(bytes: &[u8]) -> u16 {
        u16::from_le_bytes([bytes[0], bytes[1]])
    }

    fn le32(bytes: &[u8]) -> u32 {
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    }

    fn le64(bytes: &[u8]) -> u64 {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&bytes[..8]);
        u64::from_le_bytes(raw)
    }

    struct Ruleset(OwnedFd);

    pub fn notice() -> Option<String> {
        if TARGET.is_none() {
            return Some(
                "explore mode: bash is unavailable: no seccomp filter for this architecture".into(),
            );
        }
        let Some(abi) = abi() else {
            return Some(
                "sandbox: Landlock is unavailable, so explore mode's bash is unconfined: any \
                 command runs and anything is writable"
                    .to_string(),
            );
        };
        // The truncation right arrived with ABI 3: below it `truncate(2)` and
        // `ftruncate(2)` are not mediated at all, so a file outside the two
        // writable directories can still be emptied.
        (abi < 3).then(|| {
            format!(
                "sandbox: this kernel's Landlock (ABI {abi}) cannot deny truncation, so explore \
                 mode's bash can still empty a file outside its writable directories"
            )
        })
    }

    pub fn shell(command: &str, read_only: bool) -> Result<Command, String> {
        let shell = shell_path()?;
        // Built here, in the parent: no allocation after fork. Both the filter
        // and the ruleset belong to explore mode: outside it there is nothing
        // for them to protect, and the arch pin would break a 32-bit binary.
        let audit_arch = if read_only { TARGET } else { None };
        if read_only && audit_arch.is_none() {
            return Err("sandbox: bash is unavailable on this architecture".into());
        }
        let rules = if read_only {
            ruleset(&shell)?.map(|set| set.0.as_raw_fd())
        } else {
            None
        };
        let mut cmd = Command::new(&shell);
        cmd.arg("-c").arg(command);
        confine_env(&mut cmd, read_only)?;
        // SAFETY: `pre_exec` is unsafe because the closure runs in the child
        // between fork and exec; this one calls only fixed syscalls on state
        // inherited from the parent, before any user program runs.
        unsafe {
            cmd.pre_exec(move || {
                no_new_privs()?;
                if let Some(audit_arch) = audit_arch {
                    seccomp(audit_arch)?;
                }
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

    /// Every right axe's explore mode handles. `EXECUTE` is part of it: a
    /// ruleset that handles it and grants it to nothing refuses every exec.
    fn rights(abi: i32) -> u64 {
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
        fs
    }

    /// The ruleset with `handled_access_fs` only: one shape for both the
    /// `bash` ruleset and the write-only one an MCP server gets.
    fn create(fs: u64) -> Result<Ruleset, String> {
        let attr = RulesetAttr {
            handled_access_fs: fs,
        };
        // The 8-byte shape every ABI understands: no `handled_access_net`.
        let size = std::mem::size_of::<RulesetAttr>();
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
        Ok(Ruleset(unsafe { OwnedFd::from_raw_fd(fd as RawFd) }))
    }

    fn build(abi: i32, shell: &Path) -> Result<Ruleset, String> {
        let fs = rights(abi);
        let set = create(fs)?;

        let mut binaries: Vec<PathBuf> = COMMANDS.iter().filter_map(|name| resolve(name)).collect();
        binaries.push(shell.to_path_buf());
        binaries.extend(git_remotes());
        let loaders: Vec<PathBuf> = binaries
            .iter()
            .filter_map(|binary| interpreter(binary))
            .filter_map(|loader| std::fs::canonicalize(loader).ok())
            .collect();
        binaries.extend(loaders);
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
        // not become a program. The plan directory takes the same rights for
        // the same reason, and is the only writable path that survives exit.
        allow(&set, fs & !FS_EXECUTE, scratch_dir()?)?;
        if let Some(plan) = super::plan() {
            allow(&set, fs & !FS_EXECUTE, plan)?;
        }
        Ok(set)
    }

    /// The ruleset an MCP server gets in explore mode: the same two writable
    /// directories, and nothing said about exec, so a server may start its own
    /// runtime.
    fn writeset(abi: i32) -> Result<Ruleset, String> {
        let fs = rights(abi) & !FS_EXECUTE;
        let set = create(fs)?;
        allow(
            &set,
            FS_WRITE_FILE | (fs & FS_TRUNCATE),
            Path::new("/dev/null"),
        )?;
        allow(&set, fs, scratch_dir()?)?;
        if let Some(plan) = super::plan() {
            allow(&set, fs, plan)?;
        }
        Ok(set)
    }

    fn ruleset_writes() -> Result<Option<&'static Ruleset>, String> {
        let Some(abi) = abi() else {
            return Ok(None);
        };
        static SET: OnceLock<Result<Ruleset, String>> = OnceLock::new();
        match SET.get_or_init(|| writeset(abi)) {
            Ok(set) => Ok(Some(set)),
            Err(e) => Err(e.clone()),
        }
    }

    pub fn server(
        program: &Path,
        args: &[String],
        env: &[(String, String)],
        read_only: bool,
    ) -> Result<Command, String> {
        let audit_arch = if read_only { TARGET } else { None };
        if read_only && audit_arch.is_none() {
            return Err(
                "sandbox: MCP servers are unavailable on this architecture in explore mode".into(),
            );
        }
        let rules = if read_only {
            ruleset_writes()?.map(|set| set.0.as_raw_fd())
        } else {
            None
        };
        let mut cmd = Command::new(program);
        cmd.args(args);
        confine_env(&mut cmd, read_only)?;
        for (name, value) in env {
            cmd.env(name, value);
        }
        // SAFETY: as `shell`'s closure: only fixed syscalls, before any user
        // program runs, on state inherited from the parent.
        unsafe {
            cmd.pre_exec(move || {
                no_new_privs()?;
                if let Some(audit_arch) = audit_arch {
                    seccomp(audit_arch)?;
                }
                if let Some(fd) = rules {
                    restrict(fd)?;
                }
                Ok(())
            });
        }
        Ok(cmd)
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

    fn seccomp(audit_arch: u32) -> io::Result<()> {
        let filter = filter(audit_arch);
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

    /// The arch word is pinned first: a 32-bit binary would reach the syscalls
    /// below through a different number, and `x32` would slip past a filter
    /// that reads the number alone. What is left to deny is `io_uring_setup`,
    /// which can issue the file operations Landlock is supposed to mediate.
    fn filter(audit_arch: u32) -> [libc::sock_filter; 8] {
        let insn = |code: u16, jt: u8, jf: u8, k: u32| libc::sock_filter { code, jt, jf, k };
        let deny = RET_ERRNO | EPERM;
        [
            insn(LD_ABS, 0, 0, ARCH),          // 0  A = arch
            insn(JEQ_K, 1, 0, audit_arch),     // 1  -> 3, else 2
            insn(RET_K, 0, 0, deny),           // 2  another ABI: deny all
            insn(LD_ABS, 0, 0, NR),            // 3  A = nr
            insn(JGE_K, 2, 0, X32_BIT),        // 4  x32 -> 7
            insn(JEQ_K, 1, 0, IO_URING_SETUP), // 5  -> 7, else 6
            insn(RET_K, 0, 0, RET_ALLOW),      // 6
            insn(RET_K, 0, 0, deny),           // 7
        ]
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::OnceLock;

    use super::{confine_env, git_remotes, resolve, scratch_dir, shell_path, COMMANDS};

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
        confine_env(&mut cmd, read_only)?;
        Ok(cmd)
    }

    /// A program axe starts itself: same write rule, and no exec rule, so the
    /// server may start its own runtime.
    pub fn server(
        program: &Path,
        args: &[String],
        env: &[(String, String)],
        read_only: bool,
    ) -> Result<Command, String> {
        let mut cmd = Command::new(SANDBOX_EXEC);
        cmd.arg("-p").arg(writes_profile(read_only)?).arg(program);
        cmd.args(args);
        confine_env(&mut cmd, read_only)?;
        for (name, value) in env {
            cmd.env(name, value);
        }
        Ok(cmd)
    }

    /// `(allow default)` outside explore mode; the write deny and the two
    /// grants inside it.
    fn writes_profile(read_only: bool) -> Result<&'static str, String> {
        if !read_only {
            return profile(false);
        }
        static EXPLORE: OnceLock<Result<String, String>> = OnceLock::new();
        match EXPLORE.get_or_init(build_writes) {
            Ok(profile) => Ok(profile.as_str()),
            Err(e) => Err(e.clone()),
        }
    }

    fn build_writes() -> Result<String, String> {
        let mut profile = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
        profile.push_str(&format!(
            "(allow file-write* (subpath {}) (literal \"/dev/null\"))\n",
            quote(scratch_dir()?)
        ));
        if let Some(plan) = super::plan() {
            profile.push_str(&format!("(allow file-write* (subpath {}))\n", quote(plan)));
        }
        Ok(profile)
    }

    fn profile(read_only: bool) -> Result<&'static str, String> {
        static NORMAL: OnceLock<String> = OnceLock::new();
        static EXPLORE: OnceLock<Result<String, String>> = OnceLock::new();
        if !read_only {
            return Ok(NORMAL
                .get_or_init(|| "(version 1)\n(allow default)\n".to_string())
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
        let mut profile = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
        profile.push_str(&format!(
            "(allow file-write* (subpath {}) (literal \"/dev/null\"))\n",
            quote(scratch_dir()?)
        ));
        // The plan directory: writable across sessions, never executable.
        if let Some(plan) = super::plan() {
            profile.push_str(&format!("(allow file-write* (subpath {}))\n", quote(plan)));
        }
        profile.push_str("(deny process-exec)\n(allow process-exec");
        let mut paths: Vec<PathBuf> = COMMANDS.iter().filter_map(|name| resolve(name)).collect();
        paths.extend(["sh", "bash"].iter().copied().filter_map(resolve));
        paths.extend(TRAMPOLINES.iter().copied().filter_map(xcrun));
        paths.extend(git_remotes());
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
    use std::path::Path;
    use std::process::Command;

    pub fn notice() -> Option<String> {
        Some("bash is unavailable: this platform has no sandbox".into())
    }

    pub fn shell(_command: &str, _read_only: bool) -> Result<Command, String> {
        Err("sandbox: this platform has no sandbox; bash is unavailable".into())
    }

    pub fn server(
        _program: &Path,
        _args: &[String],
        _env: &[(String, String)],
        _read_only: bool,
    ) -> Result<Command, String> {
        Err("sandbox: this platform has no sandbox; MCP servers are unavailable".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_is_nonempty_and_resolves_sh() {
        assert!(COMMANDS.contains(&"ls"));
        assert!(COMMANDS.contains(&"git"));
        assert!(COMMANDS.contains(&"sed"));
        assert!(COMMANDS.contains(&"pgrep"));
        assert!(resolve("sh").is_some());
    }

    #[test]
    fn child_environment_is_scrubbed() {
        // A variable the parent has and the whitelist does not.
        let name = std::env::vars_os()
            .filter_map(|(name, _)| name.to_str().map(str::to_string))
            .find(|name| {
                !CHILD_ENV.contains(&name.as_str())
                    && name != "GIT_OPTIONAL_LOCKS"
                    && name.bytes().enumerate().all(|(at, byte)| {
                        byte.is_ascii_alphabetic()
                            || byte == b'_'
                            || (at > 0 && byte.is_ascii_digit())
                    })
            });
        let Some(name) = name else { return };
        let Ok(mut cmd) = shell(&format!("printenv {name}"), false) else {
            return;
        };
        let out = cmd.output().unwrap();
        assert!(out.stdout.is_empty(), "{name} leaked into the child");
    }

    #[test]
    fn init_and_shell_build_without_running() {
        let _ = init();
        assert!(shell("true", false).is_ok());
        assert!(shell("true", true).is_ok());
        cleanup();
    }

    /// The loader is read from the binary, so a distro that keeps it anywhere
    /// (NixOS: `/nix/store`) still execs; the old fixed path list did not.
    #[cfg(target_os = "linux")]
    #[test]
    fn interpreter_is_read_from_pt_interp() {
        let exe = std::env::current_exe().unwrap();
        let loader = imp::interpreter(&exe).expect("a dynamically linked test binary");
        assert!(loader.is_file(), "{}", loader.display());
        let plain = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert_eq!(imp::interpreter(&plain), None);
    }

    /// End to end: an allowlisted command runs under the explore ruleset. A
    /// loader left out of the ruleset fails this with `EACCES`.
    #[cfg(target_os = "linux")]
    #[test]
    fn explore_mode_runs_an_allowed_command() {
        let Ok(mut cmd) = shell("ls /dev/null", true) else {
            return;
        };
        let out = cmd.output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "ls refused: {stderr}");
    }
}
