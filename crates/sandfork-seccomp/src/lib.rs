//! The seccomp policy, as data.
//!
//! The policy is a denylist: allow by default, return an error for the calls
//! in [`policy`]. Each entry records which group it belongs to and why, and a
//! unit test forks a child per entry, installs the filter, makes the call and
//! asserts the expected error.
//!
//! Invariants:
//!
//! * **Installed last.** Sandbox setup needs `mount`, `pivot_root` and friends,
//!   which the filter forbids. The filter goes on immediately before `execve`.
//! * **`clone3` returns `ENOSYS`, everything else `EPERM`.** `clone3` passes its
//!   flags in a struct that a BPF filter cannot read. `ENOSYS` makes glibc fall
//!   back to `clone`, whose flags are in a register the filter can inspect.
//!   `EPERM` would simply make `fork` fail.
//! * **Foreign architectures are killed.** seccompiler's generated prologue
//!   kills the process on any audit architecture other than the target, which
//!   closes the 32-bit syscall table as a bypass.
//! * **Filters are inherited and can never be removed.** That is why a zygote
//!   has none and each of its children installs its own.

use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};
use std::collections::BTreeMap;
use std::io;

/// When a denied call is actually denied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum When {
    Always,
    /// Deny if argument `arg` has any of these bits set.
    AnyBit {
        arg: u8,
        bits: &'static [u64],
    },
    /// Deny unless argument `arg` equals one of these values.
    NotIn {
        arg: u8,
        allowed: &'static [u64],
    },
}

#[derive(Debug, Clone)]
pub struct Deny {
    pub name: &'static str,
    pub nr: i64,
    pub errno: i32,
    pub when: When,
    pub group: &'static str,
    pub why: &'static str,
}

/// The namespace bits of `clone(2)`'s first argument. `CLONE_NEWTIME` is not
/// listed: its value overlaps the exit-signal mask in `clone`, and it is only
/// reachable through `clone3` and `unshare`, both denied outright.
pub const CLONE_NS_BITS: &[u64] = &[
    libc::CLONE_NEWNS as u64,
    libc::CLONE_NEWCGROUP as u64,
    libc::CLONE_NEWUTS as u64,
    libc::CLONE_NEWIPC as u64,
    libc::CLONE_NEWUSER as u64,
    libc::CLONE_NEWPID as u64,
    libc::CLONE_NEWNET as u64,
];

/// `personality(2)` values that stay allowed: `PER_LINUX` and the query value.
const PERSONALITY_OK: &[u64] = &[0, 0xffff_ffff];

fn d(name: &'static str, nr: libc::c_long, group: &'static str, why: &'static str) -> Deny {
    Deny {
        name,
        nr: nr as i64,
        errno: libc::EPERM,
        when: When::Always,
        group,
        why,
    }
}

/// The denylist.
pub fn policy() -> Vec<Deny> {
    const MNT: &str = "mounts and namespaces";
    const MNT_WHY: &str = "the sandbox's view of the world is fixed at creation";
    const PROC: &str = "other processes";
    const PROC_WHY: &str = "no reading or controlling other processes";
    const KRN: &str = "kernel attack surface";
    const KRN_WHY: &str = "interfaces with long histories of kernel vulnerabilities";
    const ADM: &str = "host administration";
    const ADM_WHY: &str = "no reason for a sandbox to touch these";
    const LEG: &str = "handles and legacy";
    const LEG_WHY: &str = "known escape routes and legacy interfaces";

    let mut p =
        vec![
        d("mount", libc::SYS_mount, MNT, MNT_WHY),
        d("umount2", libc::SYS_umount2, MNT, MNT_WHY),
        d("pivot_root", libc::SYS_pivot_root, MNT, MNT_WHY),
        d("chroot", libc::SYS_chroot, MNT, MNT_WHY),
        d("unshare", libc::SYS_unshare, MNT, MNT_WHY),
        d("setns", libc::SYS_setns, MNT, MNT_WHY),
        d("open_tree", libc::SYS_open_tree, MNT, MNT_WHY),
        d("move_mount", libc::SYS_move_mount, MNT, MNT_WHY),
        d("fsopen", libc::SYS_fsopen, MNT, MNT_WHY),
        d("fsconfig", libc::SYS_fsconfig, MNT, MNT_WHY),
        d("fsmount", libc::SYS_fsmount, MNT, MNT_WHY),
        d("fspick", libc::SYS_fspick, MNT, MNT_WHY),
        d("mount_setattr", libc::SYS_mount_setattr, MNT, MNT_WHY),
        Deny {
            name: "clone",
            nr: libc::SYS_clone as i64,
            errno: libc::EPERM,
            when: When::AnyBit { arg: 0, bits: CLONE_NS_BITS },
            group: "process creation with namespace flags",
            why: "no new namespaces from inside a sandbox",
        },
        Deny {
            name: "clone3",
            nr: libc::SYS_clone3 as i64,
            errno: libc::ENOSYS,
            when: When::Always,
            group: "process creation with namespace flags",
            why: "flags sit in a struct seccomp cannot read; ENOSYS makes glibc fall back to clone",
        },
        d("ptrace", libc::SYS_ptrace, PROC, PROC_WHY),
        d("process_vm_readv", libc::SYS_process_vm_readv, PROC, PROC_WHY),
        d("process_vm_writev", libc::SYS_process_vm_writev, PROC, PROC_WHY),
        d("pidfd_getfd", libc::SYS_pidfd_getfd, PROC, PROC_WHY),
        d("bpf", libc::SYS_bpf, KRN, KRN_WHY),
        d("perf_event_open", libc::SYS_perf_event_open, KRN, KRN_WHY),
        d("userfaultfd", libc::SYS_userfaultfd, KRN, KRN_WHY),
        d("io_uring_setup", libc::SYS_io_uring_setup, KRN, KRN_WHY),
        d("io_uring_enter", libc::SYS_io_uring_enter, KRN, KRN_WHY),
        d("io_uring_register", libc::SYS_io_uring_register, KRN, KRN_WHY),
        d("keyctl", libc::SYS_keyctl, KRN, KRN_WHY),
        d("add_key", libc::SYS_add_key, KRN, KRN_WHY),
        d("request_key", libc::SYS_request_key, KRN, KRN_WHY),
        d("kexec_load", libc::SYS_kexec_load, ADM, ADM_WHY),
        d("kexec_file_load", libc::SYS_kexec_file_load, ADM, ADM_WHY),
        d("init_module", libc::SYS_init_module, ADM, ADM_WHY),
        d("finit_module", libc::SYS_finit_module, ADM, ADM_WHY),
        d("delete_module", libc::SYS_delete_module, ADM, ADM_WHY),
        d("reboot", libc::SYS_reboot, ADM, ADM_WHY),
        d("swapon", libc::SYS_swapon, ADM, ADM_WHY),
        d("swapoff", libc::SYS_swapoff, ADM, ADM_WHY),
        d("acct", libc::SYS_acct, ADM, ADM_WHY),
        d("syslog", libc::SYS_syslog, ADM, ADM_WHY),
        d("quotactl", libc::SYS_quotactl, ADM, ADM_WHY),
        d("settimeofday", libc::SYS_settimeofday, ADM, ADM_WHY),
        d("clock_settime", libc::SYS_clock_settime, ADM, ADM_WHY),
        d("adjtimex", libc::SYS_adjtimex, ADM, ADM_WHY),
        d("clock_adjtime", libc::SYS_clock_adjtime, ADM, ADM_WHY),
        d("open_by_handle_at", libc::SYS_open_by_handle_at, LEG, LEG_WHY),
        d("name_to_handle_at", libc::SYS_name_to_handle_at, LEG, LEG_WHY),
        Deny {
            name: "personality",
            nr: libc::SYS_personality as i64,
            errno: libc::EPERM,
            when: When::NotIn { arg: 0, allowed: PERSONALITY_OK },
            group: LEG,
            why: "only PER_LINUX; other personalities change the syscall ABI",
        },
    ];
    #[cfg(target_arch = "x86_64")]
    p.extend([
        d("iopl", libc::SYS_iopl, LEG, LEG_WHY),
        d("ioperm", libc::SYS_ioperm, LEG, LEG_WHY),
        d("uselib", libc::SYS_uselib, LEG, LEG_WHY),
    ]);
    p
}

fn err(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, e.to_string())
}

fn rules_for(when: &When) -> io::Result<Vec<SeccompRule>> {
    Ok(match when {
        When::Always => vec![],
        // One rule per bit; rules for a syscall are OR-ed together.
        When::AnyBit { arg, bits } => bits
            .iter()
            .map(|b| {
                SeccompRule::new(vec![SeccompCondition::new(
                    *arg,
                    SeccompCmpArgLen::Qword,
                    SeccompCmpOp::MaskedEq(*b),
                    *b,
                )
                .map_err(err)?])
                .map_err(err)
            })
            .collect::<io::Result<Vec<_>>>()?,
        // One rule whose conditions are AND-ed: arg != a0 && arg != a1 && ...
        When::NotIn { arg, allowed } => {
            let conds = allowed
                .iter()
                .map(|v| {
                    SeccompCondition::new(*arg, SeccompCmpArgLen::Qword, SeccompCmpOp::Ne, *v)
                        .map_err(err)
                })
                .collect::<io::Result<Vec<_>>>()?;
            vec![SeccompRule::new(conds).map_err(err)?]
        }
    })
}

/// Compile a policy into BPF programs, one per distinct errno. A syscall
/// appears in exactly one program, and every program allows what it does not
/// match, so stacking them is equivalent to one filter with per-call actions.
pub fn compile(policy: &[Deny]) -> io::Result<Vec<BpfProgram>> {
    let arch: TargetArch = std::env::consts::ARCH.try_into().map_err(err)?;
    let mut by_errno: BTreeMap<i32, BTreeMap<i64, Vec<SeccompRule>>> = BTreeMap::new();
    for deny in policy {
        let slot = by_errno.entry(deny.errno).or_default();
        if slot.insert(deny.nr, rules_for(&deny.when)?).is_some() {
            return Err(err(format!("syscall {} listed twice", deny.name)));
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    for deny in policy {
        if !seen.insert(deny.nr) {
            return Err(err(format!("syscall {} listed with two errnos", deny.name)));
        }
    }
    by_errno
        .into_iter()
        .map(|(errno, rules)| {
            let filter = SeccompFilter::new(
                rules,
                SeccompAction::Allow,
                SeccompAction::Errno(errno as u32),
                arch,
            )
            .map_err(err)?;
            let prog: BpfProgram = filter.try_into().map_err(err)?;
            Ok(prog)
        })
        .collect()
}

/// Install compiled programs on the calling thread. Requires `no_new_privs`
/// (or CAP_SYS_ADMIN). Makes no allocation, so it is safe after `fork`.
pub fn install(progs: &[BpfProgram]) -> io::Result<()> {
    for p in progs {
        seccompiler::apply_filter(p).map_err(err)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fork, install the filter in the child, make one syscall, and report the
    /// errno through the exit status.
    fn errno_in_child(progs: &[BpfProgram], nr: i64, args: [u64; 3]) -> i32 {
        // SAFETY: the child only calls async-signal-safe functions (prctl,
        // seccomp, syscall, _exit) before exiting.
        unsafe {
            let pid = libc::fork();
            assert!(pid >= 0);
            if pid == 0 {
                libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
                if install(progs).is_err() {
                    libc::_exit(250);
                }
                let r = libc::syscall(
                    nr as libc::c_long,
                    args[0],
                    args[1],
                    args[2],
                    0u64,
                    0u64,
                    0u64,
                );
                let e = if r == -1 {
                    *libc::__errno_location()
                } else {
                    0
                };
                libc::_exit(e.min(249));
            }
            let mut st = 0;
            assert_eq!(libc::waitpid(pid, &mut st, 0), pid);
            assert!(libc::WIFEXITED(st), "child was killed: status {st}");
            libc::WEXITSTATUS(st)
        }
    }

    #[test]
    fn every_denied_call_returns_its_errno() {
        let pol = policy();
        let progs = compile(&pol).unwrap();
        for deny in &pol {
            let args = match &deny.when {
                When::Always => [0, 0, 0],
                When::AnyBit { bits, .. } => [bits[0] | libc::SIGCHLD as u64, 0, 0],
                When::NotIn { .. } => [0x0008, 0, 0], // PER_LINUX32-ish: not allowed
            };
            let got = errno_in_child(&progs, deny.nr, args);
            assert_eq!(
                got, deny.errno,
                "{} should fail with errno {}",
                deny.name, deny.errno
            );
        }
    }

    #[test]
    fn each_namespace_bit_is_refused_for_clone() {
        let progs = compile(&policy()).unwrap();
        for bit in CLONE_NS_BITS {
            let got = errno_in_child(
                &progs,
                libc::SYS_clone as i64,
                [*bit | libc::SIGCHLD as u64, 0, 0],
            );
            assert_eq!(got, libc::EPERM, "clone with bit {bit:#x}");
        }
    }

    #[test]
    fn ordinary_calls_still_work() {
        let progs = compile(&policy()).unwrap();
        assert_eq!(
            errno_in_child(&progs, libc::SYS_getpid as i64, [0, 0, 0]),
            0
        );
        // PER_LINUX and the query value stay allowed.
        assert_eq!(
            errno_in_child(&progs, libc::SYS_personality as i64, [0xffff_ffff, 0, 0]),
            0
        );
        assert_eq!(
            errno_in_child(&progs, libc::SYS_personality as i64, [0, 0, 0]),
            0
        );
    }

    #[test]
    fn fork_survives_the_filter() {
        let progs = compile(&policy()).unwrap();
        // SAFETY: as above; the grandchild exits immediately.
        unsafe {
            let pid = libc::fork();
            if pid == 0 {
                libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
                if install(&progs).is_err() {
                    libc::_exit(2);
                }
                let c = libc::fork(); // glibc: clone3 -> ENOSYS -> clone
                if c == 0 {
                    libc::_exit(0);
                }
                if c < 0 {
                    libc::_exit(3);
                }
                let mut st = 0;
                libc::waitpid(c, &mut st, 0);
                libc::_exit(if libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0 {
                    0
                } else {
                    4
                });
            }
            let mut st = 0;
            libc::waitpid(pid, &mut st, 0);
            assert!(
                libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0,
                "status {st}"
            );
        }
    }

    #[test]
    fn policy_has_no_duplicates_and_is_documented() {
        let pol = policy();
        let mut names = std::collections::BTreeSet::new();
        for d in &pol {
            assert!(names.insert(d.name), "duplicate {}", d.name);
            assert!(!d.why.is_empty() && !d.group.is_empty());
        }
        assert!(pol
            .iter()
            .any(|d| d.name == "clone3" && d.errno == libc::ENOSYS));
        assert!(pol
            .iter()
            .filter(|d| d.name != "clone3")
            .all(|d| d.errno == libc::EPERM));
    }
}
