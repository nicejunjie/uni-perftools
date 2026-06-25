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
    /// Aggregate a per-rank results dir into one snapshot report (like `aps-report`).
    /// For the launcher-agnostic APS-style flow: `mpirun -n N uaps run -- ./app`
    /// (each rank writes snap.<rank>.json into ./uaps_result), then `uaps report
    /// ./uaps_result`.
    Report {
        /// The per-rank results directory (holds snap.<rank>.json).
        result_dir: PathBuf,
        /// Output format.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
        /// Write the report to a file instead of stdout/stderr.
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
}

/// APS-parity shorthand: a bare `uaps ./app …` (so `mpirun -n N uaps ./app` works
/// just like `mpirun -n N aps ./app`) is rewritten to `uaps run -- ./app …`. Only
/// triggers when the first token is a plain program (not a subcommand or a flag).
fn normalize_argv() -> Vec<String> {
    const SUBS: &[&str] = &["run", "attach", "resolve-events", "report", "help"];
    let v: Vec<String> = std::env::args().collect();
    if v.len() >= 2 {
        let a = v[1].as_str();
        if !a.starts_with('-') && !SUBS.contains(&a) {
            let mut out = vec![v[0].clone(), "run".into(), "--".into()];
            out.extend_from_slice(&v[1..]);
            return out;
        }
    }
    v
}

fn main() -> Result<()> {
    let cli = Cli::parse_from(normalize_argv());
    match cli.command {
        Cmd::Run { interval_ms, format, output, mpi, system_wide, rank_dir, argv } => {
            run(argv, interval_ms, format.into(), output, mpi, system_wide, rank_dir)
        }
        Cmd::Attach { pid } => {
            anyhow::bail!("`attach` (pid {pid}) is not implemented yet — see roadmap Phase 2+")
        }
        Cmd::ResolveEvents { names } => resolve_events(&names),
        Cmd::Report { result_dir, format, output } => report(&result_dir, format.into(), &output),
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

    let launcher = Path::new(program)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let is_launcher =
        matches!(launcher.as_str(), "mpirun" | "mpiexec" | "orterun" | "srun" | "aprun" | "prun" | "jsrun");

    // APS-style per-rank collection: uaps was placed INSIDE the launcher
    // (`mpirun -n N uaps ./app`) — detected by an explicit --rank-dir, or by a rank
    // env when the target isn't itself a launcher. Count this process, write
    // snap.<rank>.json (+ MPI shim timing) into the shared results dir; `uaps report
    // <dir>` aggregates it (like aps-report). Launcher-agnostic: no flag-parsing,
    // no `-x` env injection — works with ANY launcher that sets a rank env.
    let rank_dir = rank_dir.or_else(|| std::env::var_os("UAPS_RANK_DIR").map(PathBuf::from));
    if rank_dir.is_some() || (!is_launcher && uaps_collect::rank_from_env().is_some()) {
        let dir = rank_dir.unwrap_or_else(default_result_dir);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create results dir {}", dir.display()))?;
        if uaps_collect::rank_from_env() == Some(0) {
            eprintln!(
                "uaps: per-rank (APS-style) collection into {} — aggregate with: uaps report {}",
                dir.display(),
                dir.display()
            );
        }
        return collect_rank(program, args, interval_ms, Some(&dir), Some(&dir));
    }

    // An MPI launcher handed directly to uaps. uaps profiles MPI per-rank like APS:
    // place it INSIDE the launcher, don't wrap it (wrapping would need
    // launcher-specific arg-parsing + env propagation — not portable). `-a` still
    // gives a quick node-level snapshot of the launcher's node.
    if is_launcher && !system_wide {
        anyhow::bail!(
            "uaps profiles MPI per-rank like Intel APS — place it INSIDE the launcher:\n  \
             {launcher} … uaps ./app       (each rank writes ./uaps_result/)\n  \
             uaps report uaps_result        (aggregate, like aps-report)\n\
             Or `uaps run -a -- {launcher} …` for a node-level (launcher-node) snapshot."
        );
    }

    // Single process, or node-level (-a). With -a on a launcher, also LD_PRELOAD the
    // shim so node-level MPI timing is captured. Needs perf_event_paranoid<=0.
    let mpi = mpi || (is_launcher && system_wide);
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

/// Collect a single MPI rank (APS-style): count ONLY this process, on THIS node
/// (per-process, never system-wide), and write `snap.<rank>.json` into the shared
/// results `dir` for `uaps report` to aggregate. `shim_dir`, when set, LD_PRELOADs
/// the PMPI shim into the child so per-rank MPI timing is captured too.
fn collect_rank(
    program: &str,
    args: &[String],
    interval_ms: u64,
    dir: Option<&Path>,
    shim_dir: Option<&Path>,
) -> Result<()> {
    let rank = uaps_collect::rank_from_env().unwrap_or(0);
    let (mut snapshot, status) = collect_process(program, args, interval_ms, shim_dir)?;
    // Record the job's total rank count so `uaps report` can flag a SHORT aggregate
    // (e.g. crashed ranks) rather than silently undercounting.
    if let Some(ws) = uaps_collect::mpi_world_size_from_env() {
        snapshot.push(Metric {
            key: "mpi_world_size",
            label: "MPI world size".into(),
            value: MetricValue::Int { value: ws, unit: "" },
        });
    }
    let json = render_json(&snapshot);
    // Write snap.<rank>.json into the results dir for `uaps report` to aggregate;
    // rank 0 also records the command so the report can show the app + compiler.
    if let Some(dir) = dir {
        std::fs::create_dir_all(dir).ok();
        let _ = std::fs::write(dir.join(format!("snap.{rank}.json")), &json);
        if rank == 0 {
            let cmd: Vec<&str> =
                std::iter::once(program).chain(args.iter().map(String::as_str)).collect();
            let _ = std::fs::write(dir.join("cmdline"), cmd.join("\n"));
        }
    }
    mirror_exit(status);
    Ok(())
}

/// Default per-rank results dir for the APS-style flow: `./uaps_result` under the
/// job's working directory (the shared submit dir on a cluster). All ranks of one
/// launch share it; override with `--rank-dir` for concurrent jobs.
fn default_result_dir() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("uaps_result")
}

/// `aps-report` for uaps: aggregate a per-rank results dir (snap.<rank>.json plus
/// any MPI shim files) into one snapshot and render it.
fn report(result_dir: &Path, format: Format, output: &Option<PathBuf>) -> Result<()> {
    let (mut snapshot, nranks) = aggregate::aggregate(result_dir)
        .with_context(|| format!("no per-rank snapshots in {}", result_dir.display()))?;
    // Fold in per-rank MPI timing if the shim left rank_*.txt (does NOT delete dir).
    snapshot.extend(MpiCollector::new(result_dir.to_path_buf()).metrics());
    eprintln!("uaps: aggregated {nranks} rank snapshot(s) from {}", result_dir.display());

    let command: Vec<String> = std::fs::read_to_string(result_dir.join("cmdline"))
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_else(|_| vec![result_dir.display().to_string()]);

    match format {
        Format::Json => {
            let json = render_json(&snapshot);
            match output {
                Some(p) => {
                    std::fs::write(p, &json)
                        .with_context(|| format!("failed to write {}", p.display()))?;
                    eprintln!("uaps: snapshot written to {}", p.display());
                }
                None => {
                    eprintln!();
                    eprint!("{json}");
                }
            }
        }
        _ => render_via_core(&snapshot, &command, format, output)?,
    }
    Ok(())
}

