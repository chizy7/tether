//! The seccomp filter the `process` backend installs on Linux.
//!
//! It is a deny list: every system call is allowed except the ones below,
//! which fail with `EPERM`. Those are the calls a build or test command has no
//! business making and that reach furthest into the host: tracing other
//! processes, mounting, loading kernel modules, changing namespaces, setting
//! the clock. With `Network::None` it also refuses to create internet
//! sockets, which is what cuts a command off from the network.
//!
//! A deny list cannot make this a security boundary, and it is not one. A
//! kernel bug or a call missing from the list gets around it.

use std::collections::BTreeMap;

use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

use crate::SandboxError;

/// Refused outright, whatever the arguments. System call numbers are `i64`
/// on every architecture the filter supports, all of them 64-bit.
const DENIED: &[i64] = &[
    // Reading and writing other processes.
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    // Mounts and the filesystem root.
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_chroot,
    libc::SYS_open_tree,
    libc::SYS_move_mount,
    libc::SYS_fsopen,
    libc::SYS_fsconfig,
    libc::SYS_fsmount,
    libc::SYS_fspick,
    libc::SYS_open_by_handle_at,
    // Namespaces.
    libc::SYS_setns,
    libc::SYS_unshare,
    // The kernel itself.
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_kexec_load,
    libc::SYS_reboot,
    libc::SYS_swapon,
    libc::SYS_swapoff,
    libc::SYS_acct,
    libc::SYS_quotactl,
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
    libc::SYS_userfaultfd,
    // Setting the clock. Reading it, and the adjtimex calls that programs
    // use to read clock state, stay allowed.
    libc::SYS_settimeofday,
    libc::SYS_clock_settime,
    // Kernel keyrings.
    libc::SYS_keyctl,
    libc::SYS_add_key,
    libc::SYS_request_key,
    // io_uring can do most of the above, sockets included, without the
    // system calls this filter looks at.
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
];

/// Socket families that reach beyond this machine. Unix sockets stay
/// allowed: local tools use them.
const NETWORK_FAMILIES: &[libc::c_int] = &[libc::AF_INET, libc::AF_INET6, libc::AF_PACKET];

fn failed(e: impl std::fmt::Display) -> SandboxError {
    SandboxError::Other(format!("building the seccomp filter: {e}"))
}

/// Compile the filter. Done once per sandbox, before any command is started,
/// because a forked child must not allocate.
pub(crate) fn filter(block_network: bool) -> Result<BpfProgram, SandboxError> {
    // An empty rule list matches the call whatever its arguments.
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> =
        DENIED.iter().map(|call| (*call, Vec::new())).collect();
    if block_network {
        let mut families = Vec::new();
        for family in NETWORK_FAMILIES {
            let is_family =
                SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, *family as u64)
                    .map_err(failed)?;
            families.push(SeccompRule::new(vec![is_family]).map_err(failed)?);
        }
        rules.insert(libc::SYS_socket, families);
    }

    let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(failed)?;
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(failed)?;
    BpfProgram::try_from(filter).map_err(failed)
}
