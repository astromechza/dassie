//! Dassie host daemon and CLI.
//!
//! One binary. `dassie vm-run` runs one VM under its systemd unit
//! (ADR 0012), and `dassie guest-drive` builds a VM's guest drive
//! (ADR 0015). Later, `dassie daemon` runs the reconcile loop, and the other
//! subcommands are clients that talk to the daemon over its Unix socket. The
//! daemon owns the store (desired state), systemd units and D-Bus, and Caddy
//! config, and composes `dassie-vm`, `dassie-image` and `dassie-proto` into
//! app-level operations.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

mod guest_drive;
mod runner;

#[derive(Debug, Parser)]
#[command(version, about = "dassie: micro-VM app host")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run one boot of one VM as a systemd unit's main process.
    ///
    /// Reports the guest's events to the journal and `READY=1` to systemd.
    /// A SIGTERM stops the VM gracefully. Exits with the app's exit status:
    /// 0 after a requested stop, 196 if no `exited` event arrived, and 197
    /// if the VM could not run (ADR 0012).
    VmRun(VmRunArgs),
    /// Build a VM's guest drive from its `guest.json` on stdin.
    ///
    /// The JSON gets the same strict checks init runs at boot, so a config
    /// the guest would refuse is refused here. The same config always
    /// builds the same bytes (ADR 0015).
    GuestDrive(GuestDriveArgs),
}

#[derive(Debug, Args)]
struct GuestDriveArgs {
    /// The erofs to write, replaced atomically.
    #[arg(long)]
    out: PathBuf,
    /// The `mkfs.erofs` to run (erofs-utils 1.8 or later).
    #[arg(long, default_value = "mkfs.erofs")]
    mkfs: PathBuf,
}

#[derive(Debug, Args)]
struct VmRunArgs {
    /// Firecracker `--config-file`, with host paths. Unjailed, its vsock
    /// `uds_path` names the VM's sockets.
    #[arg(long)]
    config: PathBuf,
    /// The Firecracker binary.
    #[arg(long, default_value = "firecracker")]
    firecracker: PathBuf,
    /// Boot in the jailer as this uid (ADR 0013). Needs --gid and --id.
    #[arg(long, requires_all = ["gid", "id"])]
    uid: Option<u32>,
    /// The jail's gid.
    #[arg(long, requires = "uid")]
    gid: Option<u32>,
    /// The jail's id: the instance name, 1-64 letters, digits or -.
    #[arg(long, requires = "uid")]
    id: Option<String>,
    /// The jailer binary.
    #[arg(long, default_value = "jailer")]
    jailer: PathBuf,
    /// Where jails are built. Must be on the same filesystem as the VM's
    /// assets, which are hard-linked in.
    #[arg(long, default_value = dassie_vm::jail::DEFAULT_CHROOT_BASE)]
    chroot_base: PathBuf,
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::GuestDrive(args) => {
            match guest_drive::build(std::io::stdin().lock(), &args.out, args.mkfs) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("dassie guest-drive: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::VmRun(args) => {
            let jail = match (args.uid, args.gid, args.id) {
                (Some(uid), Some(gid), Some(id)) => Some(runner::JailArgs {
                    jailer: args.jailer,
                    chroot_base: args.chroot_base,
                    id,
                    uid,
                    gid,
                }),
                _ => None,
            };
            ExitCode::from(runner::vm_run(&runner::Launch {
                config: args.config,
                firecracker: args.firecracker,
                jail,
            }))
        }
    }
}
