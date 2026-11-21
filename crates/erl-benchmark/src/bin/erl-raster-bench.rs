//! The comparative bench.
//!
//! This binary exists to ask the one question the library was built to answer, and to ask it in a way
//! that can be trusted: which pipeline is fastest, for which shape of work, on this machine, in this
//! condition. It decides nothing. It links whatever pipelines the platform offers, drives the
//! published loads through all of them under both regimes, measures what moving between them costs,
//! prints what it found and, when asked, writes the profile the dispatcher would later read. The
//! arbitration itself belongs to `erl-adaptive-runtime`, which this crate deliberately does not
//! depend on.
//!
//! Three rules taken from the rest of the project are honoured literally, because a report that broke
//! any of them would be worse than no report at all.
//!
//! **The clock belongs to the harness.** Nothing here times anything by hand. The warmup, the
//! interleaving, the two regimes and the placement of the upload and the download inside the measured
//! region are the harness's, and they stay in one place.
//!
//! **Measurement is only absolute at nominal thermal pressure.** The thermal state is not read from
//! this machine, because no platform adapter exists yet and a guessed reading would be the single
//! value that makes every duration below incomparable with every other one. It comes from
//! `--thermal`, defaults to `unknown`, and the report says so at the top.
//!
//! **The first run on a machine has no baseline.** The report never compares against a stored
//! profile: doing so on numbers taken in another condition is exactly the mistake the profile format
//! was built to prevent.

use std::fmt::Write as _;
use std::path::PathBuf;

use erl_benchmark::{
    measure_switch_costs, run_trials, DeviceProfile, HarnessConfig, SampleStats, SwitchMeasurement,
    Trial, TrialStatus,
};
use erl_raster_core::op::OpKind;
use erl_raster_core::system::{SystemState, ThermalState};
use erl_raster_core::workload::{published_workloads, Workload};
use erl_render_backends::{default_registry, Backend, BackendId, GpuBackend, SwitchCostModel};

#[cfg(any(target_vendor = "apple", target_os = "android"))]
use erl_render_backends::registry_with_gpu;
#[cfg(any(target_os = "macos", target_os = "android"))]
use std::process::Command;

/// The usage line, printed on `--help` and on a malformed command.
const USAGE: &str = "\
erl-raster-bench - measure every pipeline against every published load

usage: erl-raster-bench [--quick] [--thermal <state>] [--save <path>]

  --quick             run the short configuration: hot regime only, few iterations
  --thermal <state>   nominal, fair, serious, critical or unknown (default: unknown)
  --save <path>       write the device profile to a file when the run finishes

Only a run at nominal thermal pressure produces durations that may be compared with another run.";

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.iter().any(|argument| argument == "--help" || argument == "-h") {
        println!("{USAGE}");
        return;
    }

    let options = match Options::parse(arguments.into_iter()) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("erl-raster-bench: {message}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    if let Err(message) = run(&options) {
        eprintln!("erl-raster-bench: {message}");
        std::process::exit(1);
    }
}

/// What the caller asked for.
struct Options {
    config: HarnessConfig,
    thermal: ThermalState,
    save: Option<PathBuf>,
}

impl Options {
    /// Reads the command line, refusing anything it does not understand rather than ignoring it.
    ///
    /// Silent tolerance would make a mistyped flag read as a default, and a report produced under a
    /// configuration nobody chose is a report nobody can interpret.
    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut config = HarnessConfig::default();
        let mut thermal = ThermalState::Unknown;
        let mut save = None;
        let mut arguments = arguments;

        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--quick" => config = HarnessConfig::quick(),
                "--thermal" => {
                    let value = arguments.next().ok_or("--thermal needs a state")?;
                    thermal = ThermalState::from_str(&value);
                    if thermal == ThermalState::Unknown && value != "unknown" {
                        return Err(format!("`{value}` is not a thermal state"));
                    }
                }
                "--save" => {
                    let value = arguments.next().ok_or("--save needs a path")?;
                    save = Some(PathBuf::from(value));
                }
                other => return Err(format!("unknown argument `{other}`")),
            }
        }

        Ok(Self {
            config,
            thermal,
            save,
        })
    }
}

fn run(options: &Options) -> Result<(), String> {
    let workloads = published_workloads().map_err(|error| error.to_string())?;
    let mut backends = pipelines();
    let state = host_state(options.thermal);

    let trials = run_trials(&mut backends, &workloads, &options.config);

    // The switch cost is a property of binding a pipeline, not of the pixels drawn, so it is measured
    // once on the load that drives every kind. Measuring it on each load would multiply the run for a
    // number that would come out the same.
    let (switches, switch_costs) = match representative(&workloads) {
        Some(workload) => measure_switch_costs(&mut backends, workload, &options.config),
        None => (Vec::new(), SwitchCostModel::new()),
    };

    let profile = DeviceProfile::build(state, &trials, &workloads, switch_costs);

    print!(
        "{}",
        report(&backends, &workloads, &trials, &switches, &profile, options)
    );

    if let Some(path) = &options.save {
        profile
            .save(path)
            .map_err(|error| format!("could not write {}: {error}", path.display()))?;
        println!("\nprofile written to {}", path.display());
    }

    Ok(())
}

/// The pipelines this run compares, in report order.
///
/// The GPU is linked when this platform has a bridge for it and named as absent when it does not, so
/// that the report says *why* it was not a candidate rather than leaving a reader to wonder whether it
/// was forgotten. The CPU reference is always present: the library must never be able to make a
/// machine worse than it would have been without it.
fn pipelines() -> Vec<Box<dyn Backend>> {
    let mut backends = default_registry();

    #[cfg(all(feature = "gpu", any(target_vendor = "apple", target_os = "android")))]
    {
        match erl_render_backends::WgpuBridge::create() {
            Ok(bridge) => backends = registry_with_gpu(Box::new(bridge)),
            Err(error) => {
                eprintln!("erl-raster-bench: the GPU bridge is not linked: {error}");
                backends.push(Box::new(GpuBackend::absent(
                    gpu_bridge_id(),
                    "the graphics device could not be opened on this machine",
                )));
            }
        }
    }

    #[cfg(not(all(feature = "gpu", any(target_vendor = "apple", target_os = "android"))))]
    {
        backends.push(Box::new(GpuBackend::absent(gpu_bridge_id(), GPU_ABSENT_REASON)));
    }

    backends
}

/// Why the GPU entry is absent, when the build itself cannot carry one.
#[cfg(not(all(feature = "gpu", any(target_vendor = "apple", target_os = "android"))))]
const GPU_ABSENT_REASON: &str = if cfg!(feature = "gpu") {
    "no GPU bridge is compiled for this platform"
} else {
    "this bench was built without the graphics bridge"
};

/// The identifier the GPU entry carries, whether or not a device opened.
///
/// It is the identifier the linked bridge would have reported, so a reader can compare a report from a
/// machine whose driver refused the instance with one from a machine where it did not, and see the
/// same name in the same place.
fn gpu_bridge_id() -> BackendId {
    #[cfg(target_os = "android")]
    {
        BackendId::GpuVulkan
    }
    #[cfg(not(target_os = "android"))]
    {
        BackendId::GpuMetal
    }
}

/// The load the switch cost is measured on.
///
/// `mixed.screen` is preferred because it contains every kind, which is the situation in which a
/// caller would actually weigh moving between two pipelines.
fn representative(workloads: &[Workload]) -> Option<&Workload> {
    workloads
        .iter()
        .find(|workload| workload.name == "mixed.screen")
        .or_else(|| workloads.last())
}

/// The conditions this profile belongs to.
///
/// The thermal state is the caller's, not this program's. See the module documentation.
fn host_state(thermal: ThermalState) -> SystemState {
    SystemState::new(host_device(), host_platform(), thermal)
}

/// The model of the machine, as the platform names it.
fn host_device() -> String {
    #[cfg(target_os = "android")]
    if let Some(model) = android_property("ro.product.model") {
        // The model alone ("Pixel 8") does not say which vendor built it, and two phones from
        // different vendors can share a model name. The pair is what a reader needs to know which
        // driver produced these numbers.
        return match android_property("ro.product.manufacturer") {
            Some(vendor) => format!("{vendor} {model}"),
            None => model,
        };
    }
    #[cfg(target_os = "macos")]
    if let Some(model) = command_line("sysctl", &["-n", "hw.model"]) {
        return model;
    }
    #[cfg(target_os = "linux")]
    if let Ok(model) = std::fs::read_to_string("/sys/class/dmi/id/product_name") {
        let model = model.trim();
        if !model.is_empty() {
            return model.to_string();
        }
    }
    "unknown".to_string()
}

/// The platform and its version.
fn host_platform() -> String {
    #[cfg(target_os = "android")]
    if let Some(release) = android_property("ro.build.version.release") {
        return format!("Android {release}");
    }
    #[cfg(target_os = "macos")]
    if let Some(version) = command_line("sw_vers", &["-productVersion"]) {
        return format!("macOS {version}");
    }
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}

/// A system property, as the platform's own tool reports it.
///
/// Android has no file to read for these: they are held by the property service and reachable through
/// `getprop`, which is why the absolute path is used. A process whose `PATH` omits the system binaries
/// would otherwise find nothing and report `unknown` for a device it is running on.
#[cfg(target_os = "android")]
fn android_property(name: &str) -> Option<String> {
    command_line("/system/bin/getprop", &[name])
}

/// The first line of a command's standard output, or `None` when it cannot be run or answers nothing.
#[cfg(any(target_os = "macos", target_os = "android"))]
fn command_line(program: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(program).args(arguments).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_string())
}

/// The report, assembled as one block of text.
fn report(
    backends: &[Box<dyn Backend>],
    workloads: &[Workload],
    trials: &[Trial],
    switches: &[SwitchMeasurement],
    profile: &DeviceProfile,
    options: &Options,
) -> String {
    let mut out = String::new();

    let _ = writeln!(out, "erl-raster bench");
    let _ = writeln!(out, "  device     {}", profile.state.device);
    let _ = writeln!(out, "  platform   {}", profile.state.platform);
    let _ = writeln!(out, "  thermal    {}", profile.state.thermal);
    let _ = writeln!(
        out,
        "  regimes    {}",
        options
            .config
            .regimes
            .iter()
            .map(|regime| regime.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let _ = writeln!(
        out,
        "  sampling   {} kept after {} discarded, per pipeline, regime and load",
        options.config.iterations, options.config.warmup
    );
    let _ = writeln!(
        out,
        "  flush      {} written between cold iterations",
        bytes(options.config.scrub_bytes as u64)
    );
    let _ = writeln!(out, "  pipelines");
    for backend in backends {
        let capabilities = backend.capabilities();
        let _ = writeln!(
            out,
            "    {:<11} {}",
            backend.id().as_str(),
            capabilities.device_name
        );
        let _ = writeln!(out, "    {:<11} {}", "", capabilities.notes);
    }

    if !profile.state.thermal.permits_absolute_measurement() {
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "  note  the device is not at nominal thermal pressure, so the durations below describe"
        );
        let _ = writeln!(
            out,
            "        this machine in the state it is in now. They are comparable within this report"
        );
        let _ = writeln!(
            out,
            "        and with nothing else. Re-run with `--thermal nominal` once it has cooled."
        );
    }

    let measured: Vec<&Trial> = trials
        .iter()
        .filter(|trial| trial.status.stats().is_some())
        .collect();
    let skipped: Vec<&Trial> = trials
        .iter()
        .filter(|trial| trial.status.stats().is_none())
        .collect();

    let _ = writeln!(out);
    if measured.is_empty() {
        let _ = writeln!(out, "durations\n  nothing ran");
    } else {
        let load_width = measured
            .iter()
            .map(|trial| trial.workload.len())
            .max()
            .unwrap_or(0)
            .max("load".len());
        let pipeline_width = measured
            .iter()
            .map(|trial| trial.backend.as_str().len())
            .max()
            .unwrap_or(0)
            .max("pipeline".len());

        let _ = writeln!(out, "durations");
        let _ = writeln!(
            out,
            "  {:<load_width$}  {:<6}  {:<pipeline_width$}  {:>10}  {:>10}  {:>12}  {:>10}",
            "load", "regime", "pipeline", "median", "p95", "pixels", "moved"
        );
        for trial in &measured {
            let Some(stats) = trial.status.stats() else {
                continue;
            };
            let Some(workload) = workloads.iter().find(|item| item.name == trial.workload) else {
                continue;
            };
            let TrialStatus::Measured {
                transferred_bytes, ..
            } = &trial.status
            else {
                continue;
            };
            let _ = writeln!(
                out,
                "  {:<load_width$}  {:<6}  {:<pipeline_width$}  {:>10}  {:>10}  {:>12}  {:>10}{}",
                trial.workload,
                trial.regime.as_str(),
                trial.backend.as_str(),
                duration(stats.median_ns),
                duration(stats.p95_ns),
                workload.batch.work().pixels(),
                bytes(*transferred_bytes),
                if stats.is_conclusive() { "" } else { "  (few)" }
            );
        }
    }

    if !skipped.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "not measured");
        for trial in &skipped {
            let reason = match &trial.status {
                TrialStatus::Unavailable { reason } => format!("unavailable: {reason}"),
                TrialStatus::Unsupported { reason } => format!("unsupported: {reason}"),
                TrialStatus::Failed { reason } => format!("failed: {reason}"),
                TrialStatus::Measured { .. } => continue,
            };
            let _ = writeln!(
                out,
                "  {} on {} {}: {reason}",
                trial.backend.as_str(),
                trial.workload,
                trial.regime.as_str()
            );
        }
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "fastest pipeline per load and regime");
    for workload in workloads {
        for regime in &options.config.regimes {
            let mut ranking: Vec<(BackendId, u64)> = trials
                .iter()
                .filter(|trial| trial.workload == workload.name && trial.regime == *regime)
                .filter_map(|trial| {
                    trial
                        .status
                        .stats()
                        .map(|stats: &SampleStats| (trial.backend, stats.median_ns))
                })
                .collect();
            ranking.sort_by_key(|(backend, cost)| (*cost, *backend));

            let Some((winner, cost)) = ranking.first().copied() else {
                let _ = writeln!(
                    out,
                    "  {:<20} {:<6} nothing measured",
                    workload.name,
                    regime.as_str()
                );
                continue;
            };

            let margin = match ranking.get(1) {
                Some((runner_up, slower)) if cost > 0 => {
                    format!(
                        "{runner_up} is {:.0}% slower",
                        (*slower as f64 / cost as f64 - 1.0) * 100.0
                    )
                }
                Some((runner_up, _)) => format!("{runner_up} is not slower"),
                None => "nothing to compare it with".to_string(),
            };

            let _ = writeln!(
                out,
                "  {:<20} {:<6} {:<11} {:>10}   ({margin})",
                workload.name,
                regime.as_str(),
                winner.as_str(),
                duration(cost)
            );
        }
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "fastest pipeline per kind, from the single-kind loads");
    for kind in OpKind::ALL {
        for regime in &options.config.regimes {
            match profile.fastest_for_kind(kind, *regime) {
                Some(backend) => {
                    let _ = writeln!(
                        out,
                        "  {:<12} {:<6} {}",
                        kind.as_str(),
                        regime.as_str(),
                        backend.as_str()
                    );
                }
                None => {
                    let _ = writeln!(
                        out,
                        "  {:<12} {:<6} no single-kind load was measured",
                        kind.as_str(),
                        regime.as_str()
                    );
                }
            }
        }
    }

    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "switch cost, median of one pipeline run after another minus after the same one"
    );
    if switches.is_empty() {
        let _ = writeln!(out, "  no ordered pair of pipelines could both run the load");
    }
    for measurement in switches {
        let _ = writeln!(
            out,
            "  {:<11} -> {:<11} {:>10}   (after the other {}, after the same {})",
            measurement.from.as_str(),
            measurement.to.as_str(),
            duration(measurement.cost_ns),
            duration(measurement.after_other_ns),
            duration(measurement.after_same_ns)
        );
    }

    out
}

/// A duration in the unit a reader of frame times thinks in.
fn duration(nanoseconds: u64) -> String {
    if nanoseconds < 1_000 {
        format!("{nanoseconds} ns")
    } else if nanoseconds < 1_000_000 {
        format!("{:.1} us", nanoseconds as f64 / 1_000.0)
    } else {
        format!("{:.2} ms", nanoseconds as f64 / 1_000_000.0)
    }
}

/// A byte count in the unit that keeps it short.
fn bytes(count: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    let count = count as f64;
    if count < KIB {
        format!("{count:.0} B")
    } else if count < MIB {
        format!("{:.1} KiB", count / KIB)
    } else {
        format!("{:.1} MiB", count / MIB)
    }
}
