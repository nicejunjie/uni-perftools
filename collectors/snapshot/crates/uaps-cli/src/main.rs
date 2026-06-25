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
use uaps_core::{Collector, Metric, MetricValue, Snapshot, Target};
use uaps_report::{render_json, Format};

mod aggregate;

/// The HWPC/OS collector set used for every counting pass (single, per-rank, or
/// node-level). Top-down prefers the perf-data-driven engine when it resolves for
/// this CPU; else the hand-coded fallback.
fn build_collectors() -> Vec<Box<dyn Collector>> {
    let mut collectors: Vec<Box<dyn Collector>> = vec![
        Box::new(ElapsedCollector::new()),
        Box::new(ProcCollector::new()),
        Box::new(ThreadCollector::new()),
        Box::new(PerfCollector::new()),
        Box::new(RawPmuCollector::new()),
        Box::new(SwCollector::new()),
    ];
    let hwpc = HwpcCollector::new();
    if hwpc.active() {
        collectors.push(Box::new(hwpc));
    } else {
        collectors.push(Box::new(TopdownCollector::new()));
    }
    collectors
}

/// Run the collector set over a freshly-spawned `program args`, returning the
/// derived snapshot and the child's exit status. `mpi_dir`, when set, LD_PRELOADs
/// the PMPI shim into the child for per-rank MPI timing.
fn collect_process(
    program: &str,
    args: &[String],
    interval_ms: u64,
    mpi_dir: Option<&Path>,
) -> Result<(Snapshot, std::process::ExitStatus)> {
    let mut collectors = build_collectors();
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(dir) = mpi_dir {
        if let Ok(shim) = resolve_mpi_shim() {
            let mut preload = std::env::var("LD_PRELOAD").unwrap_or_default();
            if !preload.is_empty() {
                preload.push(':');
            }
            preload.push_str(&shim);
            cmd.env("LD_PRELOAD", preload);
            cmd.env("UAPS_MPI_OUTDIR", dir);
        }
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
    uaps_core::derive(&mut snapshot);
    Ok((snapshot, status))
}

/// Exit mirroring the target so `uaps` is transparent in pipelines: a non-zero
/// code propagates and a signal becomes 128+signo (like a shell); on success it
/// returns so the caller finishes normally (flushing output).
fn mirror_exit(status: std::process::ExitStatus) {
    use std::os::unix::process::ExitStatusExt;
    match status.code() {
        Some(0) => {}
        Some(code) => std::process::exit(code),
        None => {
            if let Some(sig) = status.signal() {
                std::process::exit(128 + sig);
            }
        }
    }
}

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
        /// whole node instead of just the launched process — the OLD MPI behavior
        /// (measures the launcher node only). The default for a launcher is now
        /// per-rank (APS-style, multi-node). Needs perf_event_paranoid <= 0.
        #[arg(long, short = 'a')]
        system_wide: bool,
        /// APS-style per-rank collection: count only this process and write
        /// snap.<rank>.json into DIR (a shared filesystem path). Used for
        /// `mpirun -n N uaps run --rank-dir DIR -- ./app`; the parent sets this
        /// automatically via UAPS_RANK_DIR when reinjecting.
        #[arg(long)]
        rank_dir: Option<PathBuf>,
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
        Cmd::Run { interval_ms, format, output, mpi, system_wide, rank_dir, argv } => {
            run(argv, interval_ms, format.into(), output, mpi, system_wide, rank_dir)
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
    rank_dir: Option<PathBuf>,
) -> Result<()> {
    let (program, args) = argv.split_first().expect("clap guarantees at least one arg");

    // Node-level counting opens thousands of perf fds on a many-core node; lift the
    // soft fd limit up front so counters don't fail with EMFILE and gap silently.
    uaps_collect::raise_fd_limit();

    // (1) Are we a single MPI rank? Either reinjected by a parent uaps (which set
    // UAPS_RANK_DIR) or invoked APS-style as `mpirun -n N uaps run --rank-dir D`.
    // Count ONLY this process, on THIS node, and drop snap.<rank>.json — the parent
    // / report step aggregates across ranks. This is what makes HW metrics cover
    // every node, not just the launcher's.
    let rank_dir = rank_dir.or_else(|| std::env::var_os("UAPS_RANK_DIR").map(PathBuf::from));
    if let Some(dir) = rank_dir {
        return collect_rank(program, args, interval_ms, &dir);
    }

    let launcher = Path::new(program)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let is_launcher =
        matches!(launcher.as_str(), "mpirun" | "mpiexec" | "orterun" | "srun" | "aprun" | "prun" | "jsrun");

    // (2) Launcher + not explicit node-level (-a): APS-style PER-RANK collection.
    // Reinject `uaps run` per rank so each rank counts itself on its own node, then
    // aggregate across ranks. This is the default for parallel jobs.
    if is_launcher && !system_wide {
        match run_per_rank(program, args, &launcher, interval_ms, &format, &output, &argv)? {
            Some(()) => return Ok(()),     // handled per-rank
            None => {}                     // couldn't locate the app — fall through to node-level
        }
    }

    // (3) Legacy node-level (system-wide) path, kept for `-a` and as the per-rank
    // fallback. MPI launches here measure the launcher node only.
    let mpi = mpi || is_launcher;

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

    let mut collectors = build_collectors();

    let mut cmd = Command::new(program);

    // MPI mode: LD_PRELOAD the PMPI shim and point it at a temp output dir,
    // then aggregate the per-rank files via MpiCollector.
    let is_openmpi = matches!(launcher.as_str(), "mpirun" | "mpiexec" | "orterun");
    if mpi {
        let shim = resolve_mpi_shim()?;
        // Each rank writes its file here, then MpiCollector reads them back. This
        // MUST live on a filesystem visible to every compute node: /tmp is usually
        // node-local, so the launcher-node collector would silently see only the
        // ranks that landed on its node and undercount the job. The working
        // directory is the job's (shared) submit dir on virtually all clusters;
        // fall back to a temp dir only if the cwd is somehow unavailable.
        let base = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let dir = base.join(format!(".uaps_mpi_{}", std::process::id()));
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create MPI output dir {}", dir.display()))?;
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
    mirror_exit(status);
    Ok(())
}

/// Collect a single MPI rank: count ONLY this process, on THIS node (per-process,
/// never system-wide), and write `snap.<rank>.json` into the shared dir. No
/// report — the parent / report step aggregates. This is the per-rank, multi-node
/// half of the APS model. The MPI shim (if any) rides in via the inherited
/// LD_PRELOAD the parent set, so MPI timing is captured per rank too.
fn collect_rank(program: &str, args: &[String], interval_ms: u64, dir: &Path) -> Result<()> {
    let rank = uaps_collect::rank_from_env().unwrap_or(0);
    let (mut snapshot, status) = collect_process(program, args, interval_ms, None)?;
    // Record the job's total rank count so the parent can detect a SHORT aggregate
    // (a node-local rank dir, or crashed ranks) rather than silently undercounting.
    if let Some(ws) = uaps_collect::mpi_world_size_from_env() {
        snapshot.push(Metric {
            key: "mpi_world_size",
            label: "MPI world size".into(),
            value: MetricValue::Int { value: ws, unit: "" },
        });
    }
    std::fs::create_dir_all(dir).ok();
    let path = dir.join(format!("snap.{rank}.json"));
    std::fs::write(&path, render_json(&snapshot))
        .with_context(|| format!("rank {rank}: failed to write {}", path.display()))?;
    mirror_exit(status);
    Ok(())
}

/// Parent of a per-rank (APS-style) run: reinject `uaps run` as each rank so each
/// counts itself on its own node, then aggregate the per-rank snapshots into one
/// job-level report. Returns `Ok(None)` if the app can't be located in the
/// launcher argv (caller falls back to node-level counting).
fn run_per_rank(
    program: &str,
    args: &[String],
    launcher: &str,
    interval_ms: u64,
    format: &Format,
    output: &Option<PathBuf>,
    full_argv: &[String],
) -> Result<Option<()>> {
    let prog_idx = match find_program_index(launcher, args) {
        Some(i) => i,
        None => {
            eprintln!(
                "uaps: could not locate the application in the `{launcher}` command for \
                 per-rank collection — falling back to node-level (launcher-node only). \
                 For per-rank, use `{launcher} … uaps run --rank-dir <shared-dir> -- ./app`."
            );
            return Ok(None);
        }
    };
    let self_exe =
        std::env::current_exe().context("cannot find own path for per-rank reinjection")?;

    // Shared, cross-node-visible dir for the per-rank files (cwd is the job's
    // shared submit dir on virtually all clusters; /tmp would be node-local).
    let base = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
    let dir = base.join(format!(".uaps_rank_{}", std::process::id()));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create per-rank dir {}", dir.display()))?;
    let _guard = TempDir(dir.clone());

    let shim = resolve_mpi_shim().ok(); // MPI timing is best-effort
    let is_openmpi = matches!(launcher, "mpirun" | "mpiexec" | "orterun");

    // Reinjected: <launcher> [launcher opts] [-x ENV…] <self> run -- <app> [args].
    let mut cmd = Command::new(program);
    cmd.args(&args[..prog_idx]);
    if is_openmpi {
        // OpenMPI does not forward the launcher's env to ranks — push ours via -x.
        cmd.arg("-x").arg("UAPS_RANK_DIR");
        if shim.is_some() {
            cmd.arg("-x").arg("LD_PRELOAD").arg("-x").arg("UAPS_MPI_OUTDIR");
        }
    }
    cmd.arg(&self_exe)
        .arg("run")
        .arg("--interval-ms")
        .arg(interval_ms.to_string())
        .arg("--")
        .args(&args[prog_idx..]);

    cmd.env("UAPS_RANK_DIR", &dir);
    if let Some(shim) = &shim {
        let mut preload = std::env::var("LD_PRELOAD").unwrap_or_default();
        if !preload.is_empty() {
            preload.push(':');
        }
        preload.push_str(shim);
        cmd.env("LD_PRELOAD", preload);
        cmd.env("UAPS_MPI_OUTDIR", &dir);
    }

    eprintln!(
        "uaps: per-rank collection (APS-style) — each rank counts itself on its own node"
    );
    let status = cmd
        .status()
        .with_context(|| format!("failed to launch `{program}`"))?;

    // Aggregate the per-rank HW snapshots, then fold in per-rank MPI timing.
    let (mut snapshot, nranks) = aggregate::aggregate(&dir)?;
    if shim.is_some() {
        let mut mpic = MpiCollector::new(dir.clone());
        let _ = mpic.start(&Target { pid: 0 });
        if let Ok(mpi_metrics) = mpic.finish() {
            snapshot.extend(mpi_metrics);
        }
    }
    eprintln!("uaps: aggregated {nranks} rank snapshot(s) across all nodes");

    match format {
        Format::Json => {
            let report = render_json(&snapshot);
            match output {
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
        _ => render_via_core(&snapshot, full_argv, *format, output)?,
    }

    drop(_guard);
    mirror_exit(status);
    Ok(Some(()))
}

/// Locate the application program within a launcher's argv (everything after the
/// launcher, e.g. `["-n","4","./app","arg"]` → index 2). Skips launcher options,
/// consuming a value for the ones that take a separate argument. Returns None if
/// no plausible program token is found (caller then falls back to node-level).
fn find_program_index(launcher: &str, args: &[String]) -> Option<usize> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            return if i + 1 < args.len() { Some(i + 1) } else { None };
        }
        if let Some(stripped) = a.strip_prefix('-') {
            if stripped.is_empty() {
                return None; // a bare "-" is not a program we can wrap
            }
            // `--flag=value` carries its own value; otherwise skip the flag plus
            // however many separate-arg values it consumes (`--mca KEY VAL` = 2).
            if a.contains('=') {
                i += 1;
            } else {
                i += 1 + launcher_flag_value_count(launcher, a);
            }
            continue;
        }
        // First non-flag token. Sanity-check it looks like a program (a path or a
        // bare command name), not a stray flag value we failed to consume.
        return Some(i);
    }
    None
}

/// How many following argv entries `flag` consumes as values. The MCA family takes
/// TWO (`--mca <key> <value>`); most value-flags take one; booleans take none.
fn launcher_flag_value_count(launcher: &str, flag: &str) -> usize {
    if matches!(flag, "--mca" | "--gmca" | "--prtemca" | "--omca") {
        return 2;
    }
    if launcher_flag_takes_value(launcher, flag) {
        1
    } else {
        0
    }
}

/// Whether `flag` consumes the following argv entry as its value, for `launcher`.
/// Conservative: unknown `--flags` are treated as booleans (don't consume), so a
/// missed value-flag surfaces as a fallback-to-node-level rather than a wrong wrap.
fn launcher_flag_takes_value(launcher: &str, flag: &str) -> bool {
    // Common to OpenMPI/MPICH mpirun/mpiexec.
    const MPI: &[&str] = &[
        "-n", "-np", "-c", "-N", "--n", "--np", "--map-by", "--bind-to", "--rank-by",
        "--mca", "--gmca", "--prtemca", "--tune", "-x", "-H", "--host", "--hostfile",
        "--machinefile", "-rf", "--rankfile", "--path", "-wdir", "--wdir", "-am", "--am",
        "-d", "--display", "--output", "--report-bindings",
    ];
    // Slurm srun.
    const SRUN: &[&str] = &[
        "-n", "--ntasks", "-c", "--cpus-per-task", "-N", "--nodes", "--ntasks-per-node",
        "-p", "--partition", "-w", "--nodelist", "--cpu-bind", "--mem", "--mem-per-cpu",
        "-t", "--time", "-A", "--account", "-J", "--job-name", "--gres", "--export",
        "--distribution", "-m", "--label",
    ];
    let table: &[&str] = match launcher {
        "srun" => SRUN,
        "aprun" | "jsrun" | "prun" => MPI, // best-effort; -n/-c style
        _ => MPI,
    };
    table.contains(&flag)
}

#[cfg(test)]
mod tests {
    use super::find_program_index;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn finds_app_after_mpirun_options() {
        // -n consumes a value; ./app is the program.
        assert_eq!(find_program_index("mpirun", &v(&["-n", "4", "./app", "x"])), Some(2));
        // multiple value-flags (the QE command shape).
        assert_eq!(
            find_program_index(
                "mpirun",
                &v(&["-np", "4", "--bind-to", "core", "--map-by", "socket:PE=8", "./pw", "-in", "f"])
            ),
            Some(6)
        );
        // boolean flag (--oversubscribe) must NOT swallow the program.
        assert_eq!(
            find_program_index("mpirun", &v(&["--oversubscribe", "-n", "4", "./app"])),
            Some(3)
        );
        // -x VAR consumes its value.
        assert_eq!(find_program_index("mpirun", &v(&["-x", "FOO", "-n", "2", "app"])), Some(4));
        // --mca KEY VALUE consumes TWO args (the real-world case from container tests).
        assert_eq!(
            find_program_index("mpirun", &v(&["--mca", "btl", "self,tcp", "-n", "2", "./app"])),
            Some(5)
        );
        assert_eq!(
            find_program_index(
                "mpirun",
                &v(&["--allow-run-as-root", "--host", "a,b", "-np", "4", "./app"])
            ),
            Some(5)
        );
    }

    #[test]
    fn finds_app_after_srun_options() {
        // --ntasks=N carries its own value (no consume).
        assert_eq!(find_program_index("srun", &v(&["--ntasks=4", "./app"])), Some(1));
        // separate-value srun flags.
        assert_eq!(find_program_index("srun", &v(&["-n", "4", "-c", "2", "./app"])), Some(4));
    }

    #[test]
    fn handles_explicit_separator_and_missing_program() {
        assert_eq!(find_program_index("mpirun", &v(&["--", "./app"])), Some(1));
        // no program at all (just options) -> None, caller falls back to node-level.
        assert_eq!(find_program_index("mpirun", &v(&["-n", "4"])), None);
        assert_eq!(find_program_index("mpirun", &v(&[])), None);
    }
}
