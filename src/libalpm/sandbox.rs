//! The download sandbox, as libalpm's sandbox.c, sandbox_fs.c and
//! sandbox_syscalls.c: a forked child drops to `DownloadUser`, may only
//! write to its temporary download directory (Landlock), and is denied a
//! list of dangerous system calls (seccomp). It reports its log messages to
//! the parent over a pipe.
use super::log::{self, Frontend, Level};
use super::Handle;
use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};

impl Handle {
    /// `_alpm_use_sandbox`: only root downloads in a sandbox, and only when
    /// a download user is set and some restriction is enabled.
    pub(crate) fn use_sandbox(&self) -> bool {
        (unsafe { libc::getuid() }) == 0
            && self.sandboxuser.is_some()
            && (!self.disable_sandbox_filesystem || !self.disable_sandbox_syscalls)
    }
}

/// A user's ids, from the password database.
#[derive(Clone, Copy)]
pub(crate) struct User {
    pub uid: libc::uid_t,
    pub gid: libc::gid_t,
}

/// `getpwnam`; Err carries errno when the lookup itself failed.
pub(crate) fn lookup_user(name: &str) -> std::result::Result<Option<User>, i32> {
    let name = CString::new(name).map_err(|_| libc::EINVAL)?;
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let mut buf = vec![0u8; 16 * 1024];
    let rc = unsafe { libc::getpwnam_r(name.as_ptr(), &mut pw, buf.as_mut_ptr().cast(), buf.len(), &mut result) };
    if rc != 0 {
        return Err(rc);
    }
    Ok((!result.is_null()).then_some(User { uid: pw.pw_uid, gid: pw.pw_gid }))
}

/// `alpm_sandbox_setup_child`: restrict this process and become `user`.
pub(crate) fn setup_child(handle: &Handle, user: User, sandbox_path: &str) -> bool {
    if unsafe { libc::getuid() } != 0 {
        return false;
    }
    if !handle.disable_sandbox_filesystem && !restrict_writes_to(sandbox_path) {
        return false;
    }
    // Make sure no more privileges can be gained later; failure is fine.
    unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if !handle.disable_sandbox_syscalls && !filter_syscalls() {
        return false;
    }
    unsafe { libc::setgid(user.gid) == 0 && libc::setgroups(0, std::ptr::null()) == 0 && libc::setuid(user.uid) == 0 }
}

// Landlock's ABI (linux/landlock.h).
const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;
const FS_EXECUTE: u64 = 1 << 0;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
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
const FS_READ: u64 = FS_READ_FILE | FS_READ_DIR;
const FS_WRITE: u64 = FS_WRITE_FILE
    | FS_REMOVE_DIR
    | FS_REMOVE_FILE
    | FS_MAKE_CHAR
    | FS_MAKE_DIR
    | FS_MAKE_REG
    | FS_MAKE_SOCK
    | FS_MAKE_FIFO
    | FS_MAKE_BLOCK
    | FS_MAKE_SYM;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

fn last_error() -> String {
    super::strerror(&std::io::Error::last_os_error())
}

fn open_dir_path(path: &str) -> Option<OwnedFd> {
    let c = CString::new(path).ok()?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC | libc::O_DIRECTORY) };
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
}

fn add_rule(ruleset: &OwnedFd, dir: &OwnedFd, allowed_access: u64) -> bool {
    use std::os::fd::AsRawFd;
    let rule = PathBeneathAttr { allowed_access, parent_fd: dir.as_raw_fd() };
    unsafe {
        libc::syscall(libc::SYS_landlock_add_rule, ruleset.as_raw_fd(), LANDLOCK_RULE_PATH_BENEATH, &rule as *const PathBeneathAttr, 0) == 0
    }
}

/// `_alpm_sandbox_fs_restrict_writes_to`: everything becomes read-only
/// except `path`.
fn restrict_writes_to(path: &str) -> bool {
    use std::os::fd::AsRawFd;
    let abi = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<RulesetAttr>(), 0usize, LANDLOCK_CREATE_RULESET_VERSION) };
    if abi < 0 {
        log::error("restricting filesystem access failed because Landlock is not supported by the kernel!\n");
        return false;
    }
    let mut handled = FS_READ | FS_WRITE | FS_REFER | FS_TRUNCATE | FS_EXECUTE;
    if abi < 2 {
        log::debug("Landlock ABI < 2, LANDLOCK_ACCESS_FS_REFER is not supported\n");
        handled &= !FS_REFER;
    }
    if abi < 3 {
        log::debug("Landlock ABI < 3, LANDLOCK_ACCESS_FS_TRUNCATE is not supported\n");
        handled &= !FS_TRUNCATE;
    }
    let attr = RulesetAttr { handled_access_fs: handled };
    let fd = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &attr as *const RulesetAttr, std::mem::size_of::<RulesetAttr>(), 0) };
    if fd < 0 {
        log::error(&format!("restricting filesystem access failed because the Landlock ruleset could not be created: {}\n", last_error()));
        return false;
    }
    let ruleset = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    let Some(root) = open_dir_path("/") else {
        log::error(&format!("opening the root filesystem to make it read-only via Landlock failed: {}\n", last_error()));
        return false;
    };
    if !add_rule(&ruleset, &root, FS_READ) {
        log::error(&format!("restricting filesystem access failed because the Landlock rule for / could not be added: {}\n", last_error()));
        return false;
    }
    let Some(dir) = open_dir_path(path) else {
        log::error(&format!("opening the download directory to make it writable via Landlock failed: {}\n", last_error()));
        return false;
    };
    if !add_rule(&ruleset, &dir, (FS_READ | FS_WRITE | FS_TRUNCATE) & handled) {
        log::error(&format!(
            "restricting filesystem access failed because the Landlock rule for the temporary download directory could not be added: {}\n",
            last_error()
        ));
        return false;
    }
    if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0) } != 0 {
        log::error(&format!("restricting filesystem access failed because the Landlock ruleset could not be applied: {}\n", last_error()));
        return false;
    }
    log::debug(&format!("filesystem access has been restricted to {path}, Landlock ABI is {abi}\n"));
    true
}

/// The system calls the download child may not make, from
/// sandbox_syscalls.c (those that exist on x86_64).
#[cfg(target_arch = "x86_64")]
const DENIED_SYSCALLS: &[libc::c_long] = &[
    // kernel modules
    libc::SYS_delete_module,
    libc::SYS_finit_module,
    libc::SYS_init_module,
    // mount
    libc::SYS_chroot,
    libc::SYS_fsconfig,
    libc::SYS_fsmount,
    libc::SYS_fsopen,
    libc::SYS_fspick,
    libc::SYS_mount,
    libc::SYS_mount_setattr,
    libc::SYS_move_mount,
    libc::SYS_open_tree,
    libc::SYS_pivot_root,
    libc::SYS_umount2,
    // keyring
    libc::SYS_add_key,
    libc::SYS_keyctl,
    libc::SYS_request_key,
    // CPU emulation
    libc::SYS_modify_ldt,
    // debug
    libc::SYS_kcmp,
    libc::SYS_lookup_dcookie,
    libc::SYS_perf_event_open,
    libc::SYS_pidfd_getfd,
    libc::SYS_ptrace,
    // set clock
    libc::SYS_adjtimex,
    libc::SYS_clock_adjtime,
    libc::SYS_clock_settime,
    libc::SYS_settimeofday,
    // raw IO
    libc::SYS_ioperm,
    libc::SYS_iopl,
    // kexec
    libc::SYS_kexec_file_load,
    libc::SYS_kexec_load,
    // reboot
    libc::SYS_reboot,
    // privileged
    libc::SYS_acct,
    libc::SYS_bpf,
    libc::SYS_capset,
    libc::SYS_fanotify_init,
    libc::SYS_fanotify_mark,
    libc::SYS_nfsservctl,
    libc::SYS_open_by_handle_at,
    libc::SYS_personality,
    // obsolete
    libc::SYS__sysctl,
    libc::SYS_afs_syscall,
    174, // create_module, gone from libc
    177, // get_kernel_syms
    libc::SYS_getpmsg,
    libc::SYS_putpmsg,
    178, // query_module
    libc::SYS_security,
    libc::SYS_sysfs,
    libc::SYS_tuxcall,
    libc::SYS_uselib,
    libc::SYS_ustat,
    libc::SYS_vserver,
    // swap
    libc::SYS_swapon,
    libc::SYS_swapoff,
];

/// A seccomp program that allows everything except `DENIED_SYSCALLS`,
/// which fail with EPERM. Like libseccomp's, it also refuses system calls
/// from other ABIs (i386, x32), whose numbers mean something else.
#[cfg(target_arch = "x86_64")]
fn syscall_filter() -> Vec<libc::sock_filter> {
    const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
    const X32_SYSCALL_BIT: u32 = 0x4000_0000;
    let stmt = |code: u32, k: u32| libc::sock_filter { code: code as u16, jt: 0, jf: 0, k };
    let jump = |code: u32, k: u32, jt: u8, jf: u8| libc::sock_filter { code: code as u16, jt, jf, k };
    let ld = libc::BPF_LD | libc::BPF_W | libc::BPF_ABS;
    let jeq = libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K;
    let jge = libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K;
    let ret = libc::BPF_RET | libc::BPF_K;
    let deny = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
    // offsetof(struct seccomp_data, arch) and (…, nr)
    let mut program = vec![
        stmt(ld, 4),
        jump(jeq, AUDIT_ARCH_X86_64, 1, 0),
        stmt(ret, deny),
        stmt(ld, 0),
        jump(jge, X32_SYSCALL_BIT, 0, 1),
        stmt(ret, deny),
    ];
    let n = DENIED_SYSCALLS.len();
    for (i, nr) in DENIED_SYSCALLS.iter().enumerate() {
        // On a match, jump to the deny after the list; otherwise fall through.
        program.push(jump(jeq, *nr as u32, (n - i) as u8, 0));
    }
    program.push(stmt(ret, libc::SECCOMP_RET_ALLOW));
    program.push(stmt(ret, deny));
    program
}

/// `_alpm_sandbox_syscalls_filter`.
#[cfg(target_arch = "x86_64")]
fn filter_syscalls() -> bool {
    let program = syscall_filter();
    let fprog = libc::sock_fprog { len: program.len() as u16, filter: program.as_ptr() as *mut libc::sock_filter };
    let rc = unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &fprog as *const libc::sock_fprog) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        log::error(&format!("error restricting syscalls via seccomp: {errno}!\n"));
        return false;
    }
    log::debug(&format!("successfully restricted {} syscalls via seccomp\n", DENIED_SYSCALLS.len()));
    true
}

#[cfg(not(target_arch = "x86_64"))]
fn filter_syscalls() -> bool {
    true
}

/// Frames on the pipe from the child: a tag byte, then the payload.
const FRAME_LOG: u8 = 0;
const FRAME_RESULT: u8 = 1;

/// The child's front end: log messages go to the parent. The child raises
/// no events or questions while downloading.
pub(crate) struct PipeFrontend(pub std::sync::Mutex<std::fs::File>);

impl Frontend for PipeFrontend {
    fn log(&self, level: Level, message: &str) {
        let level = match level {
            Level::Error => 0u8,
            Level::Warning => 1,
            Level::Debug => 2,
        };
        let mut frame = vec![FRAME_LOG, level];
        frame.extend_from_slice(&(message.len() as u32).to_ne_bytes());
        frame.extend_from_slice(message.as_bytes());
        let _ = self.0.lock().unwrap_or_else(|e| e.into_inner()).write_all(&frame);
    }
}

/// Send the child's final result: one byte per payload.
pub(crate) fn send_result(pipe: &std::sync::Mutex<std::fs::File>, result: &[u8]) -> bool {
    let mut frame = vec![FRAME_RESULT];
    frame.extend_from_slice(&(result.len() as u32).to_ne_bytes());
    frame.extend_from_slice(result);
    pipe.lock().unwrap_or_else(|e| e.into_inner()).write_all(&frame).is_ok()
}

/// Relay the child's messages to this process's front end until the pipe
/// closes. Returns the child's result, if it sent one, and whether the
/// stream was well formed.
pub(crate) fn relay(mut pipe: std::fs::File) -> (Option<Vec<u8>>, bool) {
    let mut result = None;
    let read_frame = |pipe: &mut std::fs::File| -> std::io::Result<Option<(u8, u8, Vec<u8>)>> {
        let mut tag = [0u8; 1];
        if pipe.read(&mut tag)? == 0 {
            return Ok(None);
        }
        let mut level = [0u8; 1];
        if tag[0] == FRAME_LOG {
            pipe.read_exact(&mut level)?;
        } else if tag[0] != FRAME_RESULT {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let mut len = [0u8; 4];
        pipe.read_exact(&mut len)?;
        let len = u32::from_ne_bytes(len) as usize;
        if len > 1 << 20 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let mut body = vec![0u8; len];
        pipe.read_exact(&mut body)?;
        Ok(Some((tag[0], level[0], body)))
    };
    loop {
        match read_frame(&mut pipe) {
            Ok(None) => return (result, true),
            Ok(Some((FRAME_LOG, level, body))) => {
                let message = String::from_utf8_lossy(&body);
                match level {
                    0 => log::error(&message),
                    1 => log::warning(&message),
                    _ => log::debug(&message),
                }
            }
            Ok(Some((_, _, body))) => result = Some(body),
            Err(_) => return (result, false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_syscall_filter_denies_listed_calls_and_foreign_abis_and_allows_the_rest() {
        // Run the BPF program by hand over a few inputs.
        let program = syscall_filter();
        let run = |arch: u32, nr: u32| -> u32 {
            let mut pc = 0;
            let mut acc = 0u32;
            loop {
                let ins = program[pc];
                let code = ins.code as u32;
                match code & 0x07 {
                    c if c == libc::BPF_LD => acc = if ins.k == 4 { arch } else { nr },
                    c if c == libc::BPF_RET => return ins.k,
                    _ => {
                        let taken = if code & 0xf0 == libc::BPF_JEQ { acc == ins.k } else { acc >= ins.k };
                        pc += if taken { ins.jt } else { ins.jf } as usize;
                    }
                }
                pc += 1;
            }
        };
        let deny = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
        let x86_64 = 0xC000_003E;
        assert_eq!(deny, run(x86_64, libc::SYS_ptrace as u32));
        assert_eq!(deny, run(x86_64, libc::SYS_swapoff as u32));
        assert_eq!(deny, run(x86_64, libc::SYS_delete_module as u32));
        assert_eq!(libc::SECCOMP_RET_ALLOW, run(x86_64, libc::SYS_read as u32));
        assert_eq!(libc::SECCOMP_RET_ALLOW, run(x86_64, libc::SYS_connect as u32));
        assert_eq!(deny, run(x86_64, 0x4000_0000 | libc::SYS_read as u32));
        assert_eq!(deny, run(0x4000_0003, libc::SYS_read as u32));
    }
}
