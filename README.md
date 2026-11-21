# erl-raster

**Adaptive rasterisation for mobile: measure which engine is faster, then let the device decide.**

`erl-raster` is a small Rust workspace that answers one question, honestly, on the machine it is
running on: *for this piece of drawing, is the CPU or the GPU faster right now, and when is it worth
switching?* It does not replace a graphics engine and it does not intercept one. It rasterises
leaves, measures what that costs, and arbitrates.

Read this document in another language:
[中文](README.zh-CN.md) | [Français](README.fr.md) | [Español](README.es.md) | [Русский](README.ru.md)

- [1. The problem we ran into](#1-the-problem-we-ran-into)
- [2. What ERL Raster is, and what it refuses to be](#2-what-erl-raster-is-and-what-it-refuses-to-be)
- [3. The four traps the design is built around](#3-the-four-traps-the-design-is-built-around)
- [4. The implementation, crate by crate](#4-the-implementation-crate-by-crate)
- [5. The published load](#5-the-published-load)
- [6. Running it](#6-running-it)
- [7. Where the project stands](#7-where-the-project-stands)
- [8. Roadmap](#8-roadmap)
- [9. Why there are no numbers in this file](#9-why-there-are-no-numbers-in-this-file)
- [10. License](#10-license)

---

## 1. The problem we ran into

We build a mobile application in React Native. Scrolling a list of conversations was not smooth, and
opening a conversation took a beat too long before the first frame settled. Nothing in the profile
said "the JavaScript thread", nothing said "the layout", and nothing said "the network". The frames
were being lost where a native application loses them, in the rasterisation, and that is the one part
of the pipeline our stack had never had a reason to look at.

Chasing it produced a finding that is not really about our application at all.

**Both platforms let an application rasterise a piece of content on the CPU or on the GPU, and
neither platform measures which one is faster.**

- On iOS, `CALayer.draw(in:)` gives a CPU context, a `CAMetalLayer` gives a Metal drawable, and
  `shouldRasterize` moves the cost of a subtree into a one-off offscreen rasterisation. These are
  real pipeline choices, and they exist one layer at a time, by hand.
- On Android, `View.setLayerType(LAYER_TYPE_SOFTWARE)` forces a subtree onto the CPU, a
  `SurfaceView` or `TextureView` hands over a surface you own, and `RenderEffect` runs blur and
  colour work on the GPU inside the hardware pipeline. Again, real choices, made by hand, one view
  at a time.

So the lever exists. What does not exist is anything that pulls it. A developer who wants to know
whether a blur should run on this phone's CPU or its GPU has to write a benchmark, run it, hope the
phone was not warm, and then hard-code the answer for a device they will never see again.

And writing that benchmark correctly is surprisingly hard. Our own first attempts were wrong in ways
that produced confident, useless numbers:

- A kernel measured on a buffer it had already touched is up to five times faster than the same
  kernel on a buffer it has just received, so the naive loop measures the cache and calls it the
  compute.
- Running all of one pipeline and then all of the other hands the second the benefit of a clock that
  has ramped up and the first the cost of having ramped it.
- A phone that has throttled benchmarks its throttled self, and a profile taken in that state
  describes a device that no longer exists.
- Switching pipeline is not free. Bind, descriptors, fence, sometimes a transfer. A dispatcher that
  moves on a five percent gain can easily spend more time moving than drawing.

The performance defect that started all of this is now understood and fixed in the application
itself. What it exposed is the gap this repository fills: the arbitration between CPU and GPU is a
decision every rendering stack makes, badly, by hand.

## 2. What ERL Raster is, and what it refuses to be

`erl-raster` is a **rasterisation engine with an arbitration layer**. You describe a batch of drawing
for a frame and hand it over with a target; the library runs it on whichever of its pipelines is
predicted to be fastest, under the conditions the device is actually in.

It is not a framework, not a compositor, and not a replacement for Core Animation, HWUI or Skia. It
cannot intercept the drawing calls of another framework, and it does not try. The compatibility
question is settled by design rather than by hope:

- **An API, never a hook.** No swizzling, no class replacement, no undefined behaviour. That is what
  makes it distributable, on an app store and on a package registry.
- **A permanent, never-removed CPU fallback.** The scalar CPU path is always in the candidate set.
  The worst case of adopting the library is that it runs exactly as fast as the reference.
- **A deterministic reference path.** The CPU reference is integer-only from end to end, so a
  conformance comparison is bit-for-bit rather than approximate. A backend that diverges is wrong,
  not "close enough", and it is switched off rather than quietly tolerated.

The one sentence version: *write the drawing once, and let the arbitration pick the engine.*

## 3. The four traps the design is built around

### 3.1 A benchmark warped by the cache

The fix is not a better loop, it is two measurements and a rule about which one applies.

- **Two regimes, both stored.** `Regime::Hot` reuses the target and keeps the sources resident,
  which describes an operation repeated on data already in flight, like a glyph atlas redrawn every
  frame. `Regime::Cold` allocates a fresh target and scrubs the caches before the iteration, which
  describes the first frame of a screen or a decoded image being resized. In the cold regime a
  sixteen mebibyte buffer is written between iterations, which is above the last level cache of every
  phone and most laptops, and the allocation sits inside the measured region on purpose.
- **The host picks the regime, not the benchmark.** Only the caller knows whether a given frame is
  repeating resident work or receiving new sources. That is what `FrameContext::regime` carries.
- **Warmup is thrown away.** The first iterations of a pipeline contain its first allocation, its
  first shader compilation and the cache fill of everything after. The default harness config
  discards ten iterations before keeping sixty.

### 3.2 The cost of changing pipeline

- **The batch is the unit of decision, not the primitive.** A switch pays for binding, descriptor
  updates and synchronisation; a decision per glyph would cost more than the glyph. A batch is one
  frame's drawing, so a single switch is amortised over everything in it.
- **The switch cost is measured, not estimated.** `measure_switch_costs` runs the same load
  immediately after a load on the other pipeline and subtracts the time it takes after a load on the
  same one, per ordered pair, because the cost is not symmetric. `SwitchCostModel::between` returns
  `None` for a pair that has never been measured, and `None` is not turned into a default: an
  unmeasured move is refused rather than assumed free.
- **At most one performance move per frame.** A second performance move would pay the fixed cost
  twice inside the frame it was meant to improve. A move made because the incumbent *cannot* run the
  batch is not subject to that budget, since it is not churn.
- **Never during a critical window.** The host declares a frame critical while an animation is
  committed, while a target is shared, or while a capture is running. `FrameContext::frozen` carries
  that, and the evidence for a change stops accumulating rather than being thrown away.

### 3.3 Load that changes underneath the profile

- **No attempt to read "GPU utilisation".** It is not exposed to applications on either platform.
  A busy GPU shows up as an unstable submission-to-completion latency instead, and it is that
  variance the project watches, not a hypothetical gauge.
- **The thermal state is part of the profile's identity, not a field beside it.** `SystemState`
  keys a profile on device, platform and thermal state. `ThermalState::permits_absolute_measurement`
  is true only for `Nominal`, and the runtime falls back and distrusts the profile the moment the
  state no longer matches.
- **Hysteresis with written-down numbers.** A challenger must clear the incumbent's cost by the
  measured switch cost plus a margin, default ten percent of the incumbent, and must do it on three
  consecutive evaluations before it takes the place. A regression is an execution landing at more
  than one and a half times its own prediction, four times in a row.

### 3.4 Compatibility

- **Three integration levels, from least to most invasive.** An image producer (`render(op) ->
  surface`) needs no native integration at all and works in a test harness on a desktop. A host view
  is a Fabric component on React Native, a `UIView` over a `CAMetalLayer` on iOS, a `View` over a
  `SurfaceView` on Android, and is reserved for content that changes every frame. Platform adapters
  are small Swift, Objective-C++ and Kotlin layers for what only the platform language can reach:
  the thermal interface, a render effect, a view's layer type.
- **Conformance is verified, not assumed.** Each backend is compared against the integer reference,
  with an explicit per-operation tolerance where the backend declares itself not bit-exact. A
  backend that diverges is disabled, not silently approximated.
- **Nothing is discovered behind the platform's back.** The registry contains what was explicitly
  linked into the build, plus the reference. A GPU that is not linked appears in the report with the
  reason it is missing, rather than with a blank.

## 4. The implementation, crate by crate

The workspace is four crates. The MVP has **no third-party dependency outside the GPU bridge**: the
core, the benchmark and the dispatcher are pure `std`, which keeps the open-source build reproducible
on any machine and keeps the frame path free of code that could pause it.

```
crates/
  erl-raster-core         primitives and the deterministic reference
  erl-render-backends     the implementations, behind one contract
  erl-benchmark           the measurement engine and the device profile
  erl-adaptive-runtime    the dispatcher
```

### 4.1 `erl-raster-core`

Platform-independent rasterisation, and the reference every backend is held to.

- `op` defines what can be asked. `OpKind` is `FillGlyphs`, `Resample` or `Blur`, deliberately three,
  because the three behave differently enough that one answer for all of them would be wrong for two:
  a glyph run is small, scattered and latency-bound; a resample is a streaming read; a blur is a
  two-pass sweep with a scratch buffer. Each kind exposes a `Work` cost in *work units rather than
  time*, because a backend knows what it was asked to do and only the harness can say how long it
  took. `Batch` is the unit of decision, and `OpKindSet` is a three-bit set so the per-frame
  dispatcher can build it in a register and never allocate.
- `reference` is the deterministic path. Integer arithmetic end to end: a floating-point kernel can
  differ between an x86 build machine, an Apple silicon phone and an ARM Android device, which would
  make a conformance test either impossible to write or permanently red. Integers give bit-for-bit
  equality across all three.
- `fixed` holds the fixed-point helpers, written so that an eight-lane NEON register reproduces the
  scalar value exactly. `image` holds `Surface`, a tightly packed four-byte texel buffer, with a
  cheap fingerprint used to say *where* two backends diverged. `glyph` holds `GlyphMask`, `GlyphSpan`
  and `GlyphRun`, with mask reuse the way a text cache uses an atlas. `geom` and `error` are the
  supporting types. `system` defines `SystemState` and `ThermalState`.
- `workload` builds the published load, described in section 5.

The crate carries `#![forbid(unsafe_code)]`. The vectorised kernels that need `unsafe` live in the
backends, behind the architecture check that makes them safe to call.

### 4.2 `erl-render-backends`

One contract, several implementations.

- `Backend` is the trait: `capabilities`, `cost_model`, `prepare`, `execute`, `release`.
  `prepare` is where a pipeline does the work it should only ever do once, so the first *timed*
  execution is representative. `execute` runs a whole batch against a host-memory target and returns
  an `Executed`.
- `Capabilities` describes what a backend *is*: which kinds it serves, the largest surface it takes,
  whether it reproduces the reference byte for byte, and whether re-entering it is costly. A backend
  that serves only part of a batch is not a candidate at all, because splitting a batch across two
  pipelines costs a switch and a composition step, and the MVP does not do it.
- `CostModel` describes what a backend *costs*, in four terms the dispatcher needs independently
  because they scale with different things: a fixed cost per execution, a cost per operation, a cost
  per pixel of work and a cost per byte moved, the last two in thousandths of a nanosecond so a fast
  pipeline is expressible at all. `from_measurement` rebuilds a model from a measured duration, and
  the dedicated fixed-cost probe refines it.
- `CpuBackend::scalar()` is the reference and the fallback. `CpuBackend::neon()` exists only on
  AArch64 and returns `None` elsewhere rather than handing back the scalar backend under a NEON name,
  because a backend that misreports which pipeline it is would poison the profile. Both flavours are
  one type distinguished by a flag, and the vectorised one replaces exactly one thing: the glyph
  compositing row, which is where the measurements put the cost. Resampling is a gather that gains
  little from wide registers, and the box blur's normalisation is an integer division with no
  lane-parallel form on this architecture, so both stay on the reference path and `Capabilities` says
  so.
- `GpuBackend` is the seam plus the device. `GpuSurfaceBridge` is three calls, `device`, `prepare`,
  `execute`, with no lifetime tricks: one implementation per platform, in that platform's language,
  behind a thin FFI layer. The decisive rule is that the target is **host memory**: the bridge
  uploads it, draws, and downloads the result, so the transfer that decides most real cases is
  *inside* the region the harness times. A GPU backend measured on a texture it already owned would
  post a far better number and describe a situation the caller is never in. `GpuBackend::absent`
  exists so a report can say *why* a GPU was not a candidate instead of omitting it silently.
- `wgpu_bridge` is the first real bridge, compiled on macOS only. Four decisions shape it: kernels
  are compute dispatches rather than render passes, because a glyph run has to read the pixel it is
  about to blend and a render pass cannot read its own attachment; everything is a storage buffer
  read and written in place rather than a texture, which removes a staging copy and a de-striding
  pass; the reported `Work` is computed on the host from the same clipping arithmetic the reference
  uses, because counting pixels on device would add a readback every frame to learn what the host
  already knows; and the measurement rule is the seam's, upload and download inside the region.
- `registry` is where the compatibility trap lands. No discovery, no probing. `default_registry()`
  returns the scalar reference first and the vectorised flavour when the architecture has one;
  `registry_with_gpu` appends a linked bridge.

### 4.3 `erl-benchmark`

The measurement engine, and the only thing that outlives a process.

- `stats` refuses to report a mean on its own. Median and ninety-fifth centile over a fixed number
  of iterations, because a mean hides exactly what is visible: the long frames. `is_conclusive`
  guards against reporting a distribution built from too few samples.
- `regime` is the hot and cold split of section 3.1.
- `harness` owns the clock. `HarnessConfig` defaults to ten discarded iterations then sixty kept
  ones, both regimes, and a sixteen mebibyte scrub. `run_trials` runs every pipeline against every
  workload, interleaving the pipelines batch by batch so neither gets the clock's ramp-up.
  `measure_switch_costs` produces the ordered-pair switch costs. A `Trial` reports `Measured`,
  `Unavailable`, `Unsupported` or `Failed`, with a reason attached, so a pipeline that did not run is
  a finding rather than a gap.
- `profile` is what a later session dispatches on. `DeviceProfile` holds the `SystemState` it was
  taken in, the entries, and the switch costs, and writes to a versioned text format that a human can
  read and a diff can review. `fastest_for_kind` answers "who won blur, hot, on this device" from the
  single-kind loads, which is how a decision about a kind can be taken from a measurement of another
  workload that contained it.

### 4.4 `erl-adaptive-runtime`

Where the project stops measuring and starts deciding. It does not contain rendering code: the
pipelines arrive through the `Backend` trait.

- `FrameContext` is the split between what only the host knows and what the library works out. The
  host declares the regime and whether the frame is critical. Everything else, the arbitration
  itself, is the library's, because it is not something an application should be asked to get right.
  `AdaptiveRuntime::render` takes a batch and returns the bytes of its rendering, and the caller
  never names a pipeline.
- `DispatchPolicy` is the hysteresis in numbers: `minimum_gain_percent` (10),
  `required_consecutive_wins` (3), `regression_percent` (150), `regression_window` (4), and
  `switching_enabled`, which pins the incumbent and is the A/B switch the project uses to measure what
  arbitration is worth. `gain_clears_the_bar` is a single inequality on purpose: the gain has to pay
  the move back *and* clear the margin, so a policy cannot be written in which a move is both
  approved and not worth it.
- `Decision` carries the reason it was taken, including the decisions not to move, with the four
  numbers behind it: predicted incumbent cost, predicted challenger cost, measured move cost, and
  consecutive wins. That is what makes the policy testable without a phone, and what makes a
  misbehaving dispatcher debuggable. There are eleven reasons, among them
  `GainBelowSwitchCost`, `SwitchCostUnknown`, `ChallengerNotProven`, `CriticalWindow`,
  `SwitchBudgetSpent`, `StaleProfile`, `IncumbentCannotServe` and `UnavailableFallback`.
- `AdaptiveRuntime` holds five rules. The incumbent is never dropped for being slow, only displaced
  by something better; there is no "no pipeline" state. A challenger must win repeatedly. A move has
  to pay for itself in measured nanoseconds, and an unmeasured move is refused. At most one
  performance move per frame. The profile is only used in the condition it was taken in. A runtime
  built without a permanent fallback is refused at construction rather than tolerated.

## 5. The published load

The benchmark is only comparable with itself if the work never changes, so every workload is fixed,
procedural and free of any font file, image file or asset. Glyph masks come from a seeded generator,
the resample source likewise, and the generator is a seventeen-line `xorshift64*` chosen precisely
because no compiler upgrade can change what it produces. Two runs six months apart, on a laptop and
on a phone, draw exactly the same pixels.

All five draw into the same reference target, 1080 by 2340, a phone in portrait at three times
density, because the target is what a backend has to allocate, clear and hand over, and comparing
across target sizes would fold the target cost into the comparison.

| Workload | What it is for |
|---|---|
| `glyphs.tiny` | Four short masks. The case the GPU loses: submitting costs more than blending. |
| `glyphs.paragraph` | A screen of text. Many small masks over a mostly transparent plane. |
| `resample.card` | A 512 estimate scaled into a card. A streaming read with hard edges, where bilinear and nearest visibly disagree. |
| `blur.panel` | A three-pass box blur over a panel. Read-modify-write with a scratch buffer. |
| `mixed.screen` | All three kinds in one batch. The granularity the dispatcher actually decides at, and the load the switch cost is measured on. |

`glyphs.tiny` exists to be a case the GPU loses and `mixed.screen` exists to be the case where one
decision for the whole batch is either right for all of it or clearly wrong. The shapes are chosen to
make the arbitrage visible, not to look like a real screen.

## 6. Running it

```
cargo test --workspace
cargo build --workspace --release
cargo run --release --bin erl-raster-bench -- --help
```

The bench links whatever pipelines this platform build offers and drives the published loads through
all of them under both regimes.

```
usage: erl-raster-bench [--quick] [--thermal <state>] [--save <path>]

  --quick             run the short configuration: hot regime only, few iterations
  --thermal <state>   nominal, fair, serious, critical or unknown (default: unknown)
  --save <path>       write the device profile to a file when the run finishes

Only a run at nominal thermal pressure produces durations that may be compared with another run.
```

It decides nothing. It prints the pipelines it linked, a durations table with median, ninety-fifth
centile, pixel count and bytes moved per load, regime and pipeline, the list of what could not be
measured with the reason, the fastest pipeline per load and regime with the runner-up's margin, the
fastest pipeline per kind from the single-kind loads, and the measured switch costs per ordered pair.
With `--save` it writes the profile the dispatcher would later read.

Three rules are honoured literally. The clock belongs to the harness and nothing is timed by hand.
Measurement is only absolute at nominal thermal pressure, which is why the thermal state comes from
the command line and not from a guess, and why the report prints a warning when it is not nominal.
The first run on a machine has no baseline and never compares against a stored profile.

A malformed argument is an error, not a silently ignored default, because a report produced under a
configuration nobody chose is a report nobody can interpret.

## 7. Where the project stands

Done and verified:

- The Rust workspace, four crates, contracts stabilised, `cargo test --workspace` green.
- The complete CPU backend: scalar reference and the vectorised compositing row on AArch64, both
  producing identical bytes.
- The integer-only deterministic reference the other backends are held to.
- The published, asset-free workload set.
- The comparative bench with both regimes, the interleaved harness, the discarded warmup, the
  measured switch costs and the versioned device profile.
- The first GPU bridge, on macOS through Metal. It reproduces the scalar reference on the published
  set, verified by test.
- The adaptive runtime: hysteresis, the one-move-per-frame budget, critical windows, the refusal of
  unmeasured switches, the never-removed fallback and the thermal check on the profile.

Not yet:

- The Android bridge. It is the second adapter behind the same `GpuSurfaceBridge` seam, with its own
  platform gate.
- The application integration: a JSI and TurboModule binding generated from the same interface
  definitions, and the host view, so a React Native component can hand a batch to the runtime.
- The zero-copy path: `IOSurface` on iOS and `AHardwareBuffer` on Android, which the seam was already
  shaped for. It is a variant of the pipeline to be measured, not something smuggled into the
  baseline.
- A conformance test runner that compares every backend against the reference across the whole
  published set with the declared tolerances as data rather than as tests.
- Continuous integration compiling the workspace for `aarch64-apple-ios` and `aarch64-linux-android`.

## 8. Roadmap

1. The Android Vulkan bridge, behind the existing seam.
2. The conformance runner, and the tolerance table as data.
3. The React Native host view and its binding.
4. The embedded, quota-limited benchmark that starts *after* the first frame and never before it, so
   a first session is never delayed by measuring it.
5. Multi-resolution and under-load trials, including a simulated competing GPU load, so that trap 3.3
   is provoked on purpose instead of waited for.
6. The zero-copy variants, measured as variants.
7. A published reading of latency, smoothness and energy, against a run without the library.

## 9. Why there are no numbers in this file

Because a duration taken on a warm phone describes a warm phone. The project's own rule is that no
measurement is valid above nominal thermal pressure, and a number copied into a README loses the one
thing that made it meaningful: the condition it was taken in. Numbers belong in the report the bench
prints and in the profile it saves, next to the device, the platform and the thermal state they were
measured at. Run it on your own hardware.

