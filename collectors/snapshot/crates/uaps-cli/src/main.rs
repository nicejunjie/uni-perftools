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
    ElapsedCollector, HwpcCollector, MpiCollector, PerfCollector, ProcCollector, RawPmuCollector,
    SwCollector, ThreadCollector, TopdownCollector,
};
use uaps_core::{Collector, Snapshot, Target};
use uaps_report::{render_json, Format};

/// Locate the MPI PMPI shim: an explicit `UAPS_MPI_SHIM` override wins,
/// otherwise the copy built by `build.rs` (empty if mpicc was absent).
fn resolve_mpi_shim() -> Result<String> {
    if let Ok(p) = std::env::var("UAPS_MPI_SHIM") {
        if !p.is_empty() && Path::new(&p).exists() {
            return Ok(p);
        }
    }
    // Alongside the executable (survives `make install` / a moved binary, where
    // the compile-time OUT_DIR path below no longer exists).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let beside = dir.join("uaps_mpi.so");
            if beside.exists() {
                return Ok(beside.to_string_lossy().into_owned());
            }
        }
    }
    let built = env!("UAPS_MPI_SHIM_BUILT");
    if !built.is_empty() && Path::new(built).exists() {
        return Ok(built.to_string());
    }
    anyhow::bail!(
        "MPI shim unavailable (no C compiler when uaps was built, or the binary was \
         moved away from its build tree). Build shim/mpi/uaps_mpi.c with a C compiler \
         and set UAPS_MPI_SHIM to its path, or place uaps_mpi.so next to the uaps binary."
    )
}

/// Locate the shared core renderer (`core/cli/upat`) relative to this binary so
/// uaps and upat render through ONE engine. The dev tree and the install layout
/// both keep `core/` a few levels above the uaps binary
/// (`…/collectors/snapshot/target/<profile>/uaps` → `…/core/cli/upat`).
fn find_core_upat() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("UAPS_CORE_UPAT") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent();
    for _ in 0..6 {
        let d = dir?;
        let cand = d.join("core").join("cli").join("upat");
        if cand.exists() {
            return Some(cand);
        }
        dir = d.parent();
    }
    None
}

/// Produce the human report (text/HTML) by handing the snapshot to the shared
/// core renderer — the single owner of the roofline, viewpoints and insights.
/// Removes a temp staging directory on drop, so every early-return path (a
/// failed snapshot write, a renderer error) cleans up — not just the success
/// case at the end of `render_via_core`.
struct TempDir(PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn render_via_core(
    snapshot: &Snapshot,
    command: &[String],
    format: Format,
    output: &Option<PathBuf>,
) -> Result<()> {
    // HTML needs a real output directory to write into: with no `-o` we'd render
    // into the temp staging dir and then delete it, producing nothing on a 0 exit.
    // Fail loudly instead.
    if matches!(format, Format::Html) && output.is_none() {
        anyhow::bail!("uaps: --format html requires -o <dir> to write the report into");
    }
    let upat = find_core_upat().context(
        "uaps: shared core renderer (core/cli/upat) not found next to the binary — \
         use `--format json` for the raw snapshot, or set UAPS_CORE_UPAT",
    )?;
    // Stage the snapshot contract in a result dir the core knows how to read.
    // The TempDir guard removes it on every return path below.
    let dir = std::env::temp_dir().join(format!("uaps-render-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let _tmp = TempDir(dir.clone());
    let snap = dir.join("snap.json");
    std::fs::write(&snap, render_json(snapshot))
        .with_context(|| format!("failed to stage snapshot at {}", snap.display()))?;
    // A manifest carries the run command so the core's Run/Software sections show
    // the application, command line, and the target binary's compiler (the core
    // reads `command` from here and resolves the compiler from command[0]).
    let cmd_json: String = command
        .iter()
        .map(|a| {
            format!(
                "\"{}\"",
                a.replace('\\', "\\\\").replace('"', "\\\"")
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let _ = std::fs::write(
        dir.join("manifest.json"),
        format!("{{\"command\": [{cmd_json}]}}\n"),
    );

    let mut cmd = Command::new(&upat);
    cmd.arg("report").arg(&dir).arg("--collector").arg("uaps");
    let result = match format {
        Format::Html => {
            // Guaranteed Some by the guard at the top of this function.
            let outdir = output.clone().expect("html requires -o (checked above)");
            cmd.arg("--format").arg("html").arg("-o").arg(&outdir);
            cmd.status().map(|s| s.success()).unwrap_or(false)
        }
        _ => {
            // Text: the core prints to stdout; route it to -o or to stderr (the
            // target owns stdout), preserving `uaps run`'s pipe-friendly contract.
            match cmd.output() {
                Ok(o) => {
                    // Surface the renderer's own diagnostics: when it exits
                    // non-zero the user otherwise sees only the generic bail!.
                    if !o.status.success() && !o.stderr.is_empty() {
                        use std::io::Write;
                        let _ = std::io::stderr().write_all(&o.stderr);
                    }
                    if !o.status.success() {
                        false
                    } else {
                        match output {
                            Some(path) => std::fs::write(path, &o.stdout).is_ok(),
                            None => {
                                use std::io::Write;
                                let _ = std::io::stderr().write_all(b"\n");
                                std::io::stderr().write_all(&o.stdout).is_ok()
                            }
                        }
                    }
                }
                Err(_) => false,
            }
        }
    };
    // `_tmp` (TempDir guard) removes the staging dir when it drops at function exit.
    if !result {
        anyhow::bail!("uaps: core renderer ({}) failed", upat.display());
    }
    Ok(())
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
        /// Node-level (system-wide) counting: read HW counters per-CPU across the
        /// whole node instead of just the launched process. Required for MPI (it
        /// counts the ranks, not the idle launcher), like Intel APS. Auto-enabled
        /// for MPI launches. Needs perf_event_paranoid <= 0 (or CAP_PERFMON).
        #[arg(long, short = 'a')]
        system_wide: bool,
        /// The target command and its arguments (everything after `--`).
        #[arg(required = true, last = true)]
        argv: Vec<String>,
    },
    /// Attach to an already-running process (coming in a later phase).
    /// Hidden from --help until implemented, but still dispatchable.
    #[command(hide = true)]
    Attach {
        /// PID of the process to profile.
        pid: u32,
    },
    /// Resolve event NAMES to raw perf configs for this host, from the vendored
    /// pmu-events db (`name=0xCONFIG:TYPE`, or `name=GAP` if unknown; TYPE is the
    /// perf_event_attr.type — RAW on x86, a dynamic PMU type on ARM). Lets the
    /// profile collector pick roofline FP/DRAM events data-drivenly instead of
    /// hard-coding raw codes per vendor.
    ResolveEvents {
        /// One or more pmu-events event names (e.g. fp_ret_sse_avx_ops.all).
        #[arg(required = true)]
        names: Vec<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Run { interval_ms, format, output, mpi, system_wide, argv } => {
            run(argv, interval_ms, format.into(), output, mpi, system_wide)
        }
        Cmd::Attach { pid } => {
            anyhow::bail!("`attach` (pid {pid}) is not implemented yet — see roadmap Phase 2+")
        }
        Cmd::ResolveEvents { names } => resolve_events(&names),
    }
}

/// Print `name=0xCONFIG:UNIT` (or `name=GAP`) for each event, resolved once
/// against this host's pmu-events db. The exit code is 0 even with gaps — the
/// caller decides what to do with a gap (e.g. fall back).
fn resolve_events(names: &[String]) -> Result<()> {
    let db = uaps_collect::pmudb::detect();
    for n in names {
        match db.as_ref().and_then(|d| uaps_collect::pmudb::resolve_config_in(d, n)) {
            Some((cfg, ty)) => println!("{n}=0x{cfg:x}:{ty}"),
            None => println!("{n}=GAP"),
        }
    }
    Ok(())
}

fn run(
    argv: Vec<String>,
    interval_ms: u64,
    format: Format,
    output: Option<PathBuf>,
    mpi: bool,
    system_wide: bool,
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

    // Per-process HW counting is meaningless for an MPI launch (it measures the
    // idle launcher, not the ranks), so use node-level (per-CPU) counting — set
    // explicitly with -a, or automatically for MPI. Needs perf_event_paranoid<=0;
    // if the kernel denies it the affected HWPC metrics degrade to gaps.
    let system_wide = system_wide || mpi;
    if system_wide {
        uaps_collect::set_system_wide(true);
        eprintln!(
            "uaps: node-level (system-wide) HW counting{} — needs perf_event_paranoid <= 0",
            if mpi { " [MPI: measuring all ranks on the node]" } else { "" }
        );
    }

    let mut collectors: Vec<Box<dyn Collector>> = vec![
        Box::new(ElapsedCollector::new()),
        Box::new(ProcCollector::new()),
        Box::new(ThreadCollector::new()),
        Box::new(PerfCollector::new()),
        Box::new(RawPmuCollector::new()),
        Box::new(SwCollector::new()),
    ];
    // Top-down: prefer the perf-data-driven engine (vendor-neutral, from the
    // vendored pmu-events) when it resolves for this CPU; else the hand-coded one.
    let hwpc = HwpcCollector::new();
    if hwpc.active() {
        collectors.push(Box::new(hwpc));
    } else {
        collectors.push(Box::new(TopdownCollector::new()));
    }

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

    // JSON is the on-disk contract — emitted here. The human report (text/HTML)
    // is produced by the SHARED core renderer (the one place that owns the
    // roofline, viewpoints and insights for BOTH tiers), so `uaps run` and
    // `upat report --collector uaps` never diverge. We hand the core a snap.json.
    match format {
        Format::Json => {
            let report = render_json(&snapshot);
            match &output {
                Some(path) => {
                    std::fs::write(path, &report)
                        .with_context(|| format!("failed to write snapshot to {}", path.display()))?;
                    eprintln!("uaps: snapshot written to {}", path.display());
                }
                None => {
                    eprintln!();
                    eprint!("{report}");
                }
            }
        }
        _ => render_via_core(&snapshot, &argv, format, &output)?,
    }

    // Mirror the target's exit code so `uaps run` is transparent in pipelines.
    match status.code() {
        Some(0) => {}
        Some(code) => std::process::exit(code),
        None => {
            // Killed by a signal: report it like a shell would (128 + signo) so a
            // crashed target isn't silently seen as success by a CI/make wrapper.
            use std::os::unix::process::ExitStatusExt;
            if let Some(sig) = status.signal() {
                std::process::exit(128 + sig);
            }
        }
    }
    Ok(())
}
