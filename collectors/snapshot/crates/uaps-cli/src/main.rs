//! `uaps` command-line entry point.
//!
//! Phase 0: launch a target command, run it to completion under the collector
//! harness, and print the snapshot. The orchestration here (start all
//! collectors → run target → finish all collectors → render) is the stable
//! shape later phases extend; only the set of collectors grows.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use uaps_collect::{
    ElapsedCollector, MpiCollector, PerfCollector, ProcCollector, RawPmuCollector, ThreadCollector,
    TopdownCollector,
};
use uaps_core::{Collector, Snapshot, Target};
use uaps_report::{render, Format};

/// Locate the MPI PMPI shim: an explicit `UAPS_MPI_SHIM` override wins,
/// otherwise the copy built by `build.rs` (empty if mpicc was absent).
fn resolve_mpi_shim() -> Result<String> {
    if let Ok(p) = std::env::var("UAPS_MPI_SHIM") {
        if !p.is_empty() && Path::new(&p).exists() {
            return Ok(p);
        }
    }
    let built = env!("UAPS_MPI_SHIM_BUILT");
    if !built.is_empty() && Path::new(built).exists() {
        return Ok(built.to_string());
    }
    anyhow::bail!(
        "MPI shim unavailable (mpicc was missing when uaps was built). \
         Build shim/mpi/uaps_mpi.c with mpicc and set UAPS_MPI_SHIM to its path."
    )
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
    Html,
}

impl From<OutputFormat> for Format {
    fn from(f: OutputFormat) -> Self {
        match f {
            OutputFormat::Text => Format::Text,
            OutputFormat::Json => Format::Json,
            OutputFormat::Html => Format::Html,
        }
    }
}

#[derive(Parser)]
#[command(name = "uaps", version, about = "Universal Application Performance Snapshot")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Launch a command and profile it until it exits: `uaps run -- ./app args`.
    Run {
        /// Sampling interval in milliseconds for periodic collectors.
        #[arg(long, default_value_t = 20)]
        interval_ms: u64,
        /// Output format.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
        /// Write the report to a file instead of stdout.
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// Profile MPI: LD_PRELOAD the PMPI shim so per-rank MPI time and
        /// imbalance are collected. Run as `uaps run --mpi -- mpirun -n N ./app`.
        #[arg(long)]
        mpi: bool,
        /// The target command and its arguments (everything after `--`).
        #[arg(required = true, last = true)]
        argv: Vec<String>,
    },
    /// Attach to an already-running process (coming in a later phase).
    Attach {
        /// PID of the process to profile.
        pid: u32,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Run { interval_ms, format, output, mpi, argv } => {
            run(argv, interval_ms, format.into(), output, mpi)
        }
        Cmd::Attach { pid } => {
            anyhow::bail!("`attach` (pid {pid}) is not implemented yet — see roadmap Phase 2+")
        }
    }
}

fn run(
    argv: Vec<String>,
    interval_ms: u64,
    format: Format,
    output: Option<PathBuf>,
    mpi: bool,
) -> Result<()> {
    let (program, args) = argv.split_first().expect("clap guarantees at least one arg");

    // APS-style auto-detect: if the target is an MPI launcher, enable MPI mode
    // even without --mpi (so `uaps run -- mpirun -n N ./app` just works).
    let launcher = Path::new(program)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mpi = mpi
        || matches!(launcher.as_str(), "mpirun" | "mpiexec" | "orterun" | "srun" | "aprun" | "prun" | "jsrun");

    let mut collectors: Vec<Box<dyn Collector>> = vec![
        Box::new(ElapsedCollector::new()),
        Box::new(ProcCollector::new()),
        Box::new(ThreadCollector::new()),
        Box::new(PerfCollector::new()),
        Box::new(RawPmuCollector::new()),
        Box::new(TopdownCollector::new()),
    ];

    let mut cmd = Command::new(program);

    // MPI mode: LD_PRELOAD the PMPI shim and point it at a temp output dir,
    // then aggregate the per-rank files via MpiCollector.
    let is_openmpi = matches!(launcher.as_str(), "mpirun" | "mpiexec" | "orterun");
    if mpi {
        let shim = resolve_mpi_shim()?;
        let dir = std::env::temp_dir().join(format!("uaps_mpi_{}", std::process::id()));
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create MPI temp dir {}", dir.display()))?;
        let mut preload = std::env::var("LD_PRELOAD").unwrap_or_default();
        if !preload.is_empty() {
            preload.push(':');
        }
        preload.push_str(&shim);
        cmd.env("UAPS_MPI_OUTDIR", &dir);
        cmd.env("LD_PRELOAD", &preload);
        // OpenMPI does not forward the launcher's env to ranks — inject -x so the
        // shim + outdir reach every rank. (srun/aprun forward env by default.)
        if is_openmpi {
            cmd.arg("-x").arg("LD_PRELOAD").arg("-x").arg("UAPS_MPI_OUTDIR");
        }
        cmd.args(args);
        collectors.push(Box::new(MpiCollector::new(dir)));
    } else {
        cmd.args(args);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to launch `{program}`"))?;

    let target = Target { pid: child.id() };
    for collector in &mut collectors {
        collector
            .start(&target)
            .with_context(|| format!("collector `{}` failed to start", collector.name()))?;
    }

    // Sampling loop: poll for exit; between polls, let periodic collectors
    // take a sample. Per-sample errors are non-fatal (the process may have
    // exited between the exit check and the read).
    let interval = Duration::from_millis(interval_ms.max(1));
    let status = loop {
        if let Some(status) = child.try_wait().context("failed polling target process")? {
            break status;
        }
        for collector in &mut collectors {
            let _ = collector.sample();
        }
        std::thread::sleep(interval);
    };

    let mut snapshot = Snapshot::default();
    for collector in &mut collectors {
        let metrics = collector
            .finish()
            .with_context(|| format!("collector `{}` failed to finish", collector.name()))?;
        snapshot.extend(metrics);
    }

    // Turn raw counts into APS-style derived metrics (CPI, cache-miss rate, …).
    uaps_core::derive(&mut snapshot);
    let insights = uaps_core::insights(&snapshot);

    let report = render(&snapshot, &insights, format);
    match &output {
        Some(path) => {
            std::fs::write(path, &report)
                .with_context(|| format!("failed to write report to {}", path.display()))?;
            eprintln!("uaps: report written to {}", path.display());
        }
        None => {
            eprintln!();
            print!("{report}");
        }
    }

    // Mirror the target's exit code so `uaps run` is transparent in pipelines.
    if let Some(code) = status.code() {
        if code != 0 {
            std::process::exit(code);
        }
    }
    Ok(())
}
