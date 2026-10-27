//! Landlock confinement for the `__sandbox-exec` helper: restrict this process, then run the command.

use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Command;

use crusty_core::config::SandboxMode;
use landlock::{
    ABI, Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreated,
    RulesetCreatedAttr, RulesetStatus,
};

use crate::Policy;

/// Returns `(filesystem rules, TCP network rules)` supported by the running kernel. Uses
/// hard requirements so an unsupported feature fails instead of being silently dropped.
pub(crate) fn probe() -> (bool, bool) {
    (kernel_supports(false), kernel_supports(true))
}

fn kernel_supports(net: bool) -> bool {
    let fs = Ruleset::default().set_compatibility(CompatLevel::HardRequirement).handle_access(AccessFs::from_all(ABI::V1));
    let rules = fs.and_then(|r| if net { r.handle_access(AccessNet::from_all(ABI::V4)) } else { Ok(r) });
    rules.and_then(|r| r.create()).is_ok()
}

/// Restricts the current process with Landlock as described by `policy_json`, runs `program`
/// with `args` in it, and returns its exit code (128 + signal number when killed by a signal).
///
/// Refuses to run when Landlock is not enforced and the policy sets `fail_if_unavailable`;
/// otherwise warns on stderr and runs the command without confinement.
pub fn run_landlocked(policy_json: &str, program: &str, args: &[String]) -> io::Result<i32> {
    let policy: Policy = serde_json::from_str(policy_json).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    if policy.mode != SandboxMode::Off {
        restrict(&policy)?;
    }
    let status = Command::new(program).args(args).status()?;
    Ok(status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
}

fn restrict(policy: &Policy) -> io::Result<()> {
    let abi = ABI::V4;
    // Allow-list: reads everywhere; writes only in the writable directories, /tmp and /dev/null.
    let ruleset = Ruleset::default().handle_access(AccessFs::from_all(abi)).map_err(to_io)?;
    let ruleset = if policy.network {
        ruleset
    } else {
        // Handled with no allow rules, so every TCP bind and connect is denied.
        ruleset.handle_access(AccessNet::from_all(abi)).map_err(to_io)?
    };
    let mut created = ruleset.create().map_err(to_io)?;
    created = add_path(created, Path::new("/"), AccessFs::from_read(abi))?;
    let temp = [Path::new("/tmp"), Path::new("/dev/null")];
    for dir in policy.writable_dirs().into_iter().chain(temp) {
        if dir.exists() {
            created = add_path(created, dir, write_access(dir, abi))?;
        }
    }

    if let Some(p) = policy.protected_in_writable() {
        eprintln!("crusty sandbox: landlock cannot keep {} read-only inside a writable root; it stays writable", p.display());
    }
    if !policy.network && !kernel_supports(true) {
        eprintln!("crusty sandbox: kernel lacks Landlock ABI v4, so network access is not blocked");
    }

    let status = created.restrict_self().map_err(to_io)?;
    match status.ruleset {
        RulesetStatus::FullyEnforced => Ok(()),
        RulesetStatus::PartiallyEnforced => {
            eprintln!("crusty sandbox: landlock only partly enforced (older kernel ABI)");
            Ok(())
        }
        RulesetStatus::NotEnforced => {
            if policy.fail_if_unavailable {
                return Err(io::Error::other("sandbox unavailable: this kernel does not enforce Landlock"));
            }
            eprintln!("crusty sandbox: landlock not enforced; the command runs unconfined");
            Ok(())
        }
    }
}

/// Full access for directories; file-compatible rights for single files such as `/dev/null`,
/// which the kernel rejects directory-only rights on.
fn write_access(path: &Path, abi: ABI) -> BitFlags<AccessFs> {
    if path.is_dir() { AccessFs::from_all(abi) } else { AccessFs::from_file(abi) }
}

fn add_path(created: RulesetCreated, path: &Path, access: BitFlags<AccessFs>) -> io::Result<RulesetCreated> {
    let fd = PathFd::new(path).map_err(to_io)?;
    created.add_rule(PathBeneath::new(fd, access)).map_err(to_io)
}

fn to_io(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
