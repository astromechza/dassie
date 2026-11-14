//! Dassie host daemon and CLI.
//!
//! One binary. `dassie vm-run` runs one VM under its systemd unit
//! (ADR 0012). Later, `dassie daemon` runs the reconcile loop, and the other
//! subcommands are clients that talk to the daemon over its Unix socket. The
//! daemon owns the store (desired state), systemd units and D-Bus, and Caddy
//! config, and composes `dassie-vm`, `dassie-image` and `dassie-proto` into
//! app-level operations.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

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
}

#[derive(Debug, Args)]
struct VmRunArgs {
    /// Firecracker `--config-file`. Its vsock `uds_path` names the VM's
    /// sockets.
    #[arg(long)]
    config: PathBuf,
    /// The Firecracker binary.
    #[arg(long, default_value = "firecracker")]
    firecracker: PathBuf,
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::VmRun(args) => ExitCode::from(runner::vm_run(&args.config, &args.firecracker)),
    }
}
