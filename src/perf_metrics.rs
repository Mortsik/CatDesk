//! Bounded, in-memory performance aggregation for the dashboard.
//!
//! Fixed-slot rolling windows keep steady-state memory under 40 KB with no
//! allocation after warm-up. Observation is arithmetic under one short-lived
//! lock — never IO, never await — so request handling is never blocked.
//! Snapshots copy the registry under the lock and sort samples on the stack.

use rand::Rng;
use std::{
    sync::{
        Mutex as StdMutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Request classes. MCP calls carry theirs via `SchedulerTiming::class`; plain
/// HTTP traffic aggregates under "http" and workspace change scans under "scan".
const CLASSES: [&str; CLASS_COUNT] = [
    "control",
    "filesystem",
    "process",
    "browser",
    "general",
    "http",
    SCAN_CLASS_NAME,
];
const CLASS_COUNT: usize = 7;
const CLASS_FILESYSTEM: usize = 1;
const CLASS_GENERAL: usize = 4;
/// The scan class reports its own timing; latency aggregates cover the rest.
pub(crate) const CLASS_SCAN: usize = CLASS_COUNT - 1;
/// Name under which change-scan timings are observed.
pub(crate) const SCAN_CLASS_NAME: &str = "scan";
const HTTP_CLASS_COUNT: usize = CLASS_COUNT - 1;

/// Rolling window: 15 buckets x 60 s covers the last 15 minutes.
const WINDOW_BUCKETS: usize = 15;
const BUCKET_MS: u64 = 60_000;
const WINDOW_MS: u64 = BUCKET_MS * WINDOW_BUCKETS as u64;

/// Tool counters mirror the `request_metadata` whitelist plus "other".
const TOOLS: [&str; TOOL_COUNT] = [
    "catdesk_instruction",
    "run_command",
    "start_command",
    "poll_command",
    "cancel_command",
    "read",
    "read_image",
    "search",
    "write",
    "edit",
    "delete",
    "create_handoff",
    "other",
];
const TOOL_COUNT: usize = 13;
const TOOL_OTHER: usize = TOOL_COUNT - 1;

const ELAPSED_SAMPLES: usize = 32;
const STAGE_SAMPLES: usize = 16;
/// Cache counter pairs: app config, agents text, static data URIs.
const CACHE_KIND_COUNT: usize = 3;
const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
/// 100% of one core in basis points; process CPU never reads higher.
const FULL_CPU_BP: u32 = 10_000;
const CPU_UNKNOWN: u32 = u32::MAX;
const RSS_UNKNOWN: u64 = u64::MAX;

/// One latency sample set: keeps the newest samples with uniform probability,
/// so a saturated bucket still yields honest percentiles from bounded memory.
#[derive(Clone, Copy)]
struct Reservoir<const N: usize> {
    samples: [u32; N],
    count: u32,
}

impl<const N: usize> Reservoir<N> {
    const EMPTY: Self = Self {
        samples: [0; N],
        count: 0,
    };

    fn push(&mut self, value: u64) {
        let value = u32::try_from(value).unwrap_or(u32::MAX);
        if (self.count as usize) < N {
            self.samples[self.count as usize] = value;
        } else {
            // Reservoir sampling: replace an existing sample with probability
            // N / (count + 1). Never grows memory, however hot the bucket is.
            let draw = rand::thread_rng().gen_range(0..=self.count as u64);
            if let Some(slot) = usize::try_from(draw).ok().filter(|slot| *slot < N) {
                self.samples[slot] = value;
            }
        }
        self.count = self.count.saturating_add(1);
    }

    fn filled(&self) -> &[u32] {
        &self.samples[..(self.count as usize).min(N)]
    }
}

#[derive(Clone, Copy)]
struct Bucket {
    bytes: u64,
    count: u32,
    deadlines: u32,
    failures: u32,
    elapsed: Reservoir<ELAPSED_SAMPLES>,
    dispatch: Reservoir<STAGE_SAMPLES>,
    execution: Reservoir<STAGE_SAMPLES>,
}

impl Bucket {
    const EMPTY: Self = Self {
        bytes: 0,
        count: 0,
        deadlines: 0,
        failures: 0,
        elapsed: Reservoir::EMPTY,
        dispatch: Reservoir::EMPTY,
        execution: Reservoir::EMPTY,
    };

    fn observe(&mut self, observation: &Observation) {
        self.count = self.count.saturating_add(1);
        self.bytes = self.bytes.saturating_add(observation.bytes);
        if observation.deadline {
            self.deadlines = self.deadlines.saturating_add(1);
        }
        if observation.failed {
            self.failures = self.failures.saturating_add(1);
        }
        self.elapsed.push(observation.elapsed_ms);
        if let Some(dispatch) = observation.dispatch_ms {
            self.dispatch.push(dispatch);
        }
        if let Some(execution) = observation.execution_ms {
            self.execution.push(execution);
        }
    }
}

impl Default for Bucket {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// Ring of buckets. `base` is the absolute bucket index of `slots[0]`; reads
/// ignore buckets older than the window and a write from outside the window
/// resets the ring (which also absorbs clock steps backwards).
#[derive(Clone, Copy)]
struct ClassWindow {
    base: Option<u64>,
    slots: [Bucket; WINDOW_BUCKETS],
}

impl ClassWindow {
    const EMPTY: Self = Self {
        base: None,
        slots: [Bucket::EMPTY; WINDOW_BUCKETS],
    };

    fn slot_mut(&mut self, now_bucket: u64) -> &mut Bucket {
        let fresh = matches!(self.base, Some(base)
            if now_bucket >= base && now_bucket < base + WINDOW_BUCKETS as u64);
        if !fresh {
            self.slots = [Bucket::EMPTY; WINDOW_BUCKETS];
            self.base = Some(now_bucket);
        }
        &mut self.slots[(now_bucket - self.base.unwrap_or(now_bucket)) as usize]
    }

    fn for_each_fresh(&self, now_bucket: u64, mut visit: impl FnMut(&Bucket)) {
        let Some(base) = self.base else {
            return;
        };
        let oldest = (now_bucket + 1).saturating_sub(WINDOW_BUCKETS as u64);
        for (offset, slot) in self.slots.iter().enumerate() {
            let absolute = base + offset as u64;
            if (oldest..=now_bucket).contains(&absolute) {
                visit(slot);
            }
        }
    }
}

impl Default for ClassWindow {
    fn default() -> Self {
        Self::EMPTY
    }
}

#[derive(Clone, Copy)]
struct ToolCounter {
    count: u64,
    bytes: u64,
    deadlines: u32,
    errors: u32,
}

impl ToolCounter {
    const EMPTY: Self = Self {
        count: 0,
        bytes: 0,
        deadlines: 0,
        errors: 0,
    };
}

#[derive(Clone, Copy)]
struct Registry {
    windows: [ClassWindow; CLASS_COUNT],
    tools: [ToolCounter; TOOL_COUNT],
}

impl Registry {
    const EMPTY: Self = Self {
        windows: [ClassWindow::EMPTY; CLASS_COUNT],
        tools: [ToolCounter::EMPTY; TOOL_COUNT],
    };
}

impl Default for Registry {
    fn default() -> Self {
        Self::EMPTY
    }
}

pub(crate) struct CacheCounters {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
}

impl CacheCounters {
    const EMPTY: Self = Self {
        hits: AtomicU64::new(0),
        misses: AtomicU64::new(0),
    };
}

/// Process-wide registry. Lazy: nothing here touches `diagnostics::init`.
pub(crate) struct PerfMetrics {
    registry: StdMutex<Registry>,
    in_flight: AtomicU64,
    in_flight_max: AtomicU64,
    cache: [CacheCounters; CACHE_KIND_COUNT],
    /// Percent of one core in basis points; `CPU_UNKNOWN` until sampled.
    cpu_bp: AtomicU32,
    /// Resident set size in KiB; `RSS_UNKNOWN` until sampled.
    rss_kb: AtomicU64,
}

impl Default for PerfMetrics {
    fn default() -> Self {
        Self {
            registry: StdMutex::new(Registry::EMPTY),
            in_flight: AtomicU64::new(0),
            in_flight_max: AtomicU64::new(0),
            cache: [CacheCounters::EMPTY; CACHE_KIND_COUNT],
            cpu_bp: AtomicU32::new(CPU_UNKNOWN),
            rss_kb: AtomicU64::new(RSS_UNKNOWN),
        }
    }
}

static PERF: OnceLock<PerfMetrics> = OnceLock::new();

pub(crate) fn global() -> &'static PerfMetrics {
    PERF.get_or_init(PerfMetrics::default)
}

fn registry() -> MutexGuard<'static, Registry> {
    global()
        .registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Which cache a hit/miss belongs to; drives the dashboard CACHE ratio.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CacheKind {
    AppConfig = 0,
    AgentsText = 1,
    DataUri = 2,
}

/// One completed request-class observation.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Observation {
    pub class: &'static str,
    pub elapsed_ms: u64,
    pub dispatch_ms: Option<u64>,
    pub execution_ms: Option<u64>,
    pub deadline: bool,
    pub failed: bool,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClassSnapshot {
    pub count: u64,
    pub deadlines: u64,
    pub failures: u64,
    pub bytes: u64,
    pub p50_ms: Option<u32>,
    pub p95_ms: Option<u32>,
    pub p99_ms: Option<u32>,
    pub dispatch_p95_ms: Option<u32>,
    pub execution_p95_ms: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ToolSnapshot {
    pub name: &'static str,
    pub count: u64,
    pub bytes: u64,
    pub deadlines: u64,
    pub errors: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PerfSnapshot {
    pub window_ms: u64,
    pub classes: [ClassSnapshot; CLASS_COUNT],
    /// Request classes pooled (everything except the scan class).
    pub aggregate: ClassSnapshot,
    pub in_flight: u64,
    pub in_flight_max: u64,
    pub cache_hits: [u64; CACHE_KIND_COUNT],
    pub cache_misses: [u64; CACHE_KIND_COUNT],
    pub cpu_bp: Option<u32>,
    pub rss_kb: Option<u64>,
    pub tools: [ToolSnapshot; TOOL_COUNT],
}

/// Sortable fixed-capacity sample pool; lives on the stack, never allocates.
struct SamplePool<const N: usize> {
    samples: [u32; N],
    len: usize,
}

impl<const N: usize> SamplePool<N> {
    fn empty() -> Self {
        Self {
            samples: [0; N],
            len: 0,
        }
    }

    fn add_reservoir<const M: usize>(&mut self, reservoir: &Reservoir<M>) {
        for sample in reservoir.filled() {
            if self.len < N {
                self.samples[self.len] = *sample;
                self.len += 1;
            }
        }
    }

    fn percentile(&mut self, p: u32) -> Option<u32> {
        percentile(&mut self.samples[..self.len], p)
    }
}

/// Nearest-rank percentile over an ascending-sorted slice.
fn percentile(sorted: &mut [u32], p: u32) -> Option<u32> {
    if sorted.is_empty() {
        return None;
    }
    sorted.sort_unstable();
    let rank = (p as usize * sorted.len() + 99) / 100;
    let rank = rank.clamp(1, sorted.len());
    Some(sorted[rank - 1])
}

pub(crate) fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

pub(crate) fn observe(observation: Observation) {
    observe_at(now_ms(), &observation);
}

pub(crate) fn observe_at(now_ms: u64, observation: &Observation) {
    let Some(class) = CLASSES.iter().position(|name| *name == observation.class) else {
        return;
    };
    let mut registry = registry();
    let window = &mut registry.windows[class];
    window.slot_mut(now_ms / BUCKET_MS).observe(observation);
}

pub(crate) fn tool_index(name: Option<&str>) -> usize {
    name.and_then(|name| TOOLS.iter().position(|tool| *tool == name))
        .unwrap_or(TOOL_OTHER)
}

/// Bump the fixed counter for one tool invocation (no latency reservoirs).
pub(crate) fn observe_tool(tool: usize, bytes: u64, deadline: bool, failed: bool) {
    if tool >= TOOL_COUNT {
        return;
    }
    let mut registry = registry();
    let counter = &mut registry.tools[tool];
    counter.count = counter.count.saturating_add(1);
    counter.bytes = counter.bytes.saturating_add(bytes);
    if deadline {
        counter.deadlines = counter.deadlines.saturating_add(1);
    }
    if failed {
        counter.errors = counter.errors.saturating_add(1);
    }
}

pub(crate) fn record_cache_hit(kind: CacheKind) {
    global().cache[kind as usize]
        .hits
        .fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn record_cache_miss(kind: CacheKind) {
    global().cache[kind as usize]
        .misses
        .fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn begin_in_flight() {
    let depth = global().in_flight.fetch_add(1, Ordering::Relaxed) + 1;
    let mut max = global().in_flight_max.load(Ordering::Relaxed);
    while depth > max {
        match global().in_flight_max.compare_exchange_weak(
            max,
            depth,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(observed) => max = observed,
        }
    }
}

pub(crate) fn end_in_flight() {
    global().in_flight.fetch_sub(1, Ordering::Relaxed);
}

/// Decrements in-flight depth on drop, so cancelled requests cannot leak it.
pub(crate) struct InFlightGuard;

impl InFlightGuard {
    pub(crate) fn new() -> Self {
        begin_in_flight();
        Self
    }
}

impl Default for InFlightGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        end_in_flight();
    }
}

pub(crate) fn snapshot() -> PerfSnapshot {
    snapshot_at(now_ms())
}

pub(crate) fn snapshot_at(now_ms: u64) -> PerfSnapshot {
    let now_bucket = now_ms / BUCKET_MS;
    // Copy the whole registry (about 31 KB) under the lock, then do all
    // percentile work on the stack with the lock released.
    let registry = *registry();
    let mut snapshot = PerfSnapshot {
        window_ms: WINDOW_MS,
        ..PerfSnapshot::default()
    };
    let mut aggregate_elapsed =
        SamplePool::<{ HTTP_CLASS_COUNT * WINDOW_BUCKETS * ELAPSED_SAMPLES }>::empty();
    let mut aggregate_dispatch =
        SamplePool::<{ HTTP_CLASS_COUNT * WINDOW_BUCKETS * STAGE_SAMPLES }>::empty();
    let mut aggregate_execution =
        SamplePool::<{ HTTP_CLASS_COUNT * WINDOW_BUCKETS * STAGE_SAMPLES }>::empty();
    for (class, window) in registry.windows.iter().enumerate() {
        snapshot.classes[class] = class_snapshot(window, now_bucket);
        if class < HTTP_CLASS_COUNT {
            pool_class_samples(
                window,
                now_bucket,
                &mut aggregate_elapsed,
                &mut aggregate_dispatch,
                &mut aggregate_execution,
            );
        }
    }
    snapshot.aggregate = ClassSnapshot {
        p50_ms: aggregate_elapsed.percentile(50),
        p95_ms: aggregate_elapsed.percentile(95),
        p99_ms: aggregate_elapsed.percentile(99),
        dispatch_p95_ms: aggregate_dispatch.percentile(95),
        execution_p95_ms: aggregate_execution.percentile(95),
        ..snapshot.aggregate
    };
    for (slot, counter) in registry.tools.iter().enumerate() {
        snapshot.tools[slot] = ToolSnapshot {
            name: TOOLS[slot],
            count: counter.count,
            bytes: counter.bytes,
            deadlines: counter.deadlines as u64,
            errors: counter.errors as u64,
        };
    }
    for kind in 0..CACHE_KIND_COUNT {
        snapshot.cache_hits[kind] = global().cache[kind].hits.load(Ordering::Relaxed);
        snapshot.cache_misses[kind] = global().cache[kind].misses.load(Ordering::Relaxed);
    }
    snapshot.in_flight = global().in_flight.load(Ordering::Relaxed);
    snapshot.in_flight_max = global().in_flight_max.load(Ordering::Relaxed);
    let cpu = global().cpu_bp.load(Ordering::Relaxed);
    snapshot.cpu_bp = (cpu != CPU_UNKNOWN).then_some(cpu);
    let rss = global().rss_kb.load(Ordering::Relaxed);
    snapshot.rss_kb = (rss != RSS_UNKNOWN).then_some(rss);
    snapshot
}

fn class_snapshot(window: &ClassWindow, now_bucket: u64) -> ClassSnapshot {
    let mut snapshot = ClassSnapshot::default();
    let mut elapsed = SamplePool::<{ WINDOW_BUCKETS * ELAPSED_SAMPLES }>::empty();
    let mut dispatch = SamplePool::<{ WINDOW_BUCKETS * STAGE_SAMPLES }>::empty();
    let mut execution = SamplePool::<{ WINDOW_BUCKETS * STAGE_SAMPLES }>::empty();
    window.for_each_fresh(now_bucket, |bucket| {
        snapshot.count += bucket.count as u64;
        snapshot.deadlines += bucket.deadlines as u64;
        snapshot.failures += bucket.failures as u64;
        snapshot.bytes += bucket.bytes;
        elapsed.add_reservoir(&bucket.elapsed);
        dispatch.add_reservoir(&bucket.dispatch);
        execution.add_reservoir(&bucket.execution);
    });
    snapshot.p50_ms = elapsed.percentile(50);
    snapshot.p95_ms = elapsed.percentile(95);
    snapshot.p99_ms = elapsed.percentile(99);
    snapshot.dispatch_p95_ms = dispatch.percentile(95);
    snapshot.execution_p95_ms = execution.percentile(95);
    snapshot
}

/// Accumulate one class's fresh reservoir samples into aggregate pools.
fn pool_class_samples(
    window: &ClassWindow,
    now_bucket: u64,
    elapsed: &mut SamplePool<{ HTTP_CLASS_COUNT * WINDOW_BUCKETS * ELAPSED_SAMPLES }>,
    dispatch: &mut SamplePool<{ HTTP_CLASS_COUNT * WINDOW_BUCKETS * STAGE_SAMPLES }>,
    execution: &mut SamplePool<{ HTTP_CLASS_COUNT * WINDOW_BUCKETS * STAGE_SAMPLES }>,
) {
    window.for_each_fresh(now_bucket, |bucket| {
        elapsed.add_reservoir(&bucket.elapsed);
        dispatch.add_reservoir(&bucket.dispatch);
        execution.add_reservoir(&bucket.execution);
    });
}

// ── Process CPU / RSS sampler ───────────────────────────────

static SAMPLER_SPAWNED: AtomicBool = AtomicBool::new(false);

/// Spawn the 5 s system sampler exactly once. Best-effort by design: it only
/// ever writes atomics, so a failed read simply leaves the dashboard at "—".
pub(crate) fn spawn_system_sampler() {
    if SAMPLER_SPAWNED.swap(true, Ordering::Relaxed) {
        return;
    }
    tokio::spawn(async move {
        let mut previous: Option<(u64, u64)> = None;
        loop {
            sample_once(&mut previous);
            tokio::time::sleep(SAMPLE_INTERVAL).await;
        }
    });
}

/// Read one sample; `previous` carries (cpu ticks, wall ms) between calls.
fn sample_once(previous: &mut Option<(u64, u64)>) {
    if let Some(rss_kb) = read_process_rss_kb() {
        global().rss_kb.store(rss_kb, Ordering::Relaxed);
    }
    let Some(ticks) = read_process_cpu_ticks() else {
        return;
    };
    let now = now_ms();
    if let Some((previous_ticks, previous_ms)) = *previous {
        let delta_ms = now.saturating_sub(previous_ms);
        if let Some(bp) = cpu_basis_points(previous_ticks, ticks, delta_ms, clk_tck()) {
            global().cpu_bp.store(bp, Ordering::Relaxed);
        }
    }
    *previous = Some((ticks, now));
}

/// Fraction of one core consumed since the previous sample, in basis points.
fn cpu_basis_points(previous_ticks: u64, ticks: u64, delta_ms: u64, clk_tck: u64) -> Option<u32> {
    if delta_ms == 0 || clk_tck == 0 {
        return None;
    }
    let consumed = ticks.saturating_sub(previous_ticks);
    let bp = consumed as u128 * 10_000_000 / (clk_tck as u128 * delta_ms as u128);
    Some((bp as u64).min(FULL_CPU_BP as u64) as u32)
}

#[cfg(unix)]
fn clk_tck() -> u64 {
    static CLK_TCK: OnceLock<u64> = OnceLock::new();
    *CLK_TCK.get_or_init(|| {
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        (ticks.max(1)) as u64
    })
}

/// Total user+system CPU ticks from `/proc/self/stat` (fields 14 and 15).
#[cfg(unix)]
fn read_process_cpu_ticks() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    parse_proc_stat_cpu_ticks(&stat)
}

#[cfg(unix)]
fn read_process_rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    parse_proc_status_rss_kb(&status)
}

/// The comm field may contain spaces and parens, so parse after the last `)`.
#[cfg(unix)]
fn parse_proc_stat_cpu_ticks(stat: &str) -> Option<u64> {
    let mut fields = stat.rsplit_once(')')?.1.split_ascii_whitespace();
    // `fields` starts at the state field (3); utime is field 14, stime 15.
    let utime: u64 = fields.nth(11)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some(utime.saturating_add(stime))
}

#[cfg(unix)]
fn parse_proc_status_rss_kb(status: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?
        .split_ascii_whitespace()
        .next()?
        .parse()
        .ok()
}

#[cfg(windows)]
fn read_process_cpu_ticks() -> Option<u64> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    unsafe {
        let mut creation = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut exit = creation;
        let mut kernel = creation;
        let mut user = creation;
        let handle = GetCurrentProcess();
        if GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) == 0 {
            return None;
        }
        Some(filetime_ticks(&kernel).saturating_add(filetime_ticks(&user)))
    }
}

#[cfg(windows)]
fn filetime_ticks(value: &windows_sys::Win32::Foundation::FILETIME) -> u64 {
    ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64
}

#[cfg(windows)]
fn read_process_rss_kb() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let mut counters = PROCESS_MEMORY_COUNTERS {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            PageFaultCount: 0,
            PeakWorkingSetSize: 0,
            WorkingSetSize: 0,
            QuotaPeakPagedPoolUsage: 0,
            QuotaPagedPoolUsage: 0,
            QuotaPeakNonPagedPoolUsage: 0,
            QuotaNonPagedPoolUsage: 0,
            PagefileUsage: 0,
            PeakPagefileUsage: 0,
        };
        let handle = GetCurrentProcess();
        if GetProcessMemoryInfo(handle, &mut counters, counters.cb) == 0 {
            return None;
        }
        Some((counters.WorkingSetSize / 1024) as u64)
    }
}

// ── Dashboard formatting (pure; unit-tested) ────────────────

/// `p50 12ms p95 180ms p99 640ms ACT 2 DL 1 42KB/s` — after the PERF label.
pub(crate) fn format_perf_line(snapshot: &PerfSnapshot) -> String {
    let aggregate = &snapshot.aggregate;
    format!(
        "p50 {} p95 {} p99 {} ACT {} DL {} {}",
        format_ms(aggregate.p50_ms),
        format_ms(aggregate.p95_ms),
        format_ms(aggregate.p99_ms),
        snapshot.in_flight,
        aggregate.deadlines,
        format_byte_rate(aggregate.bytes, snapshot.window_ms),
    )
}

/// `cpu 3% rss 181MB CACHE 96% SCAN p95 40ms` — after the SYS label.
pub(crate) fn format_system_line(snapshot: &PerfSnapshot) -> String {
    format!(
        "cpu {} rss {} CACHE {} SCAN p95 {}",
        format_cpu(snapshot.cpu_bp),
        format_rss(snapshot.rss_kb),
        format_cache_ratio(&snapshot.cache_hits, &snapshot.cache_misses),
        format_ms(snapshot.classes[CLASS_SCAN].p95_ms),
    )
}

fn format_ms(value: Option<u32>) -> String {
    match value {
        Some(ms) => format!("{ms}ms"),
        None => "—".to_string(),
    }
}

fn format_cpu(basis_points: Option<u32>) -> String {
    match basis_points {
        Some(bp) => format!("{}%", (bp + 50) / 100),
        None => "—".to_string(),
    }
}

fn format_rss(rss_kb: Option<u64>) -> String {
    let Some(rss_kb) = rss_kb else {
        return "—".to_string();
    };
    let mb = rss_kb.saturating_add(512) / 1024;
    if mb >= 1024 {
        format!("{}GB", (mb + 512) / 1024)
    } else {
        format!("{mb}MB")
    }
}

fn format_cache_ratio(hits: &[u64; CACHE_KIND_COUNT], misses: &[u64; CACHE_KIND_COUNT]) -> String {
    let hits: u64 = hits.iter().sum();
    let total = hits + misses.iter().sum::<u64>();
    if total == 0 {
        return "—".to_string();
    }
    format!("{}%", hits * 100 / total)
}

fn format_byte_rate(bytes: u64, window_ms: u64) -> String {
    if window_ms == 0 {
        return "0B/s".to_string();
    }
    let per_second = bytes.saturating_mul(1000) / window_ms;
    if per_second >= 1024 * 1024 {
        format!("{}MB/s", (per_second + 512) / (1024 * 1024))
    } else if per_second >= 1024 {
        format!("{}KB/s", (per_second + 512) / 1024)
    } else {
        format!("{per_second}B/s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(class: &'static str, elapsed_ms: u64) -> Observation {
        Observation {
            class,
            elapsed_ms,
            ..Observation::default()
        }
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let mut empty: [u32; 0] = [];
        assert_eq!(percentile(&mut empty, 50), None);
        let mut single = [5u32];
        assert_eq!(percentile(&mut single, 50), Some(5));
        let mut three = [30u32, 10, 20];
        assert_eq!(percentile(&mut three, 50), Some(20));
        assert_eq!(percentile(&mut three, 95), Some(30));
        assert_eq!(percentile(&mut three, 99), Some(30));
        let mut hundred: Vec<u32> = (1..=100).collect();
        assert_eq!(percentile(&mut hundred, 50), Some(50));
        assert_eq!(percentile(&mut hundred, 95), Some(95));
        assert_eq!(percentile(&mut hundred, 99), Some(99));
    }

    #[test]
    fn observations_expire_after_fifteen_buckets() {
        let now = 1_700_000_000_000u64;
        observe_at(
            now,
            &Observation {
                dispatch_ms: Some(3),
                execution_ms: Some(7),
                bytes: 128,
                ..observation("filesystem", 10)
            },
        );
        let fresh = snapshot_at(now);
        let filesystem = &fresh.classes[CLASS_FILESYSTEM];
        assert_eq!(filesystem.count, 1);
        assert_eq!(filesystem.p50_ms, Some(10));
        assert_eq!(filesystem.p95_ms, Some(10));
        assert_eq!(filesystem.dispatch_p95_ms, Some(3));
        assert_eq!(filesystem.execution_p95_ms, Some(7));
        assert_eq!(filesystem.bytes, 128);

        // The oldest bucket survives until it is 15 buckets old, then expires.
        let still_fresh = snapshot_at(now + WINDOW_MS - BUCKET_MS);
        assert_eq!(still_fresh.classes[CLASS_FILESYSTEM].count, 1);
        let expired = snapshot_at(now + WINDOW_MS);
        assert_eq!(expired.classes[CLASS_FILESYSTEM].count, 0);
        assert_eq!(expired.classes[CLASS_FILESYSTEM].p50_ms, None);
        assert_eq!(expired.aggregate.p50_ms, None);
    }

    #[test]
    fn reservoirs_stay_bounded_under_saturation() {
        let now = 1_700_000_000_000u64;
        for elapsed in 0..10_000u64 {
            observe_at(now, &observation("general", elapsed % 1000));
        }
        let snapshot = snapshot_at(now);
        let general = &snapshot.classes[CLASS_GENERAL];
        assert_eq!(general.count, 10_000, "counts must not be truncated");
        assert!(general.p50_ms.is_some());
        assert!(general.p99_ms.is_some());
        let slots = global().registry.lock().unwrap();
        let bucket = &slots.windows[CLASS_GENERAL].slots[0];
        assert_eq!(
            bucket.elapsed.count, 10_000,
            "count must remember every push"
        );
        assert_eq!(
            bucket.elapsed.filled().len(),
            ELAPSED_SAMPLES,
            "reservoir must cap retained samples"
        );
    }

    #[test]
    fn deadline_failure_and_bytes_are_counted_per_class() {
        let now = 1_700_000_000_000u64;
        observe_at(
            now,
            &Observation {
                deadline: true,
                failed: true,
                bytes: 42,
                ..observation("process", 5_000)
            },
        );
        observe_at(now, &observation("process", 10));
        let snapshot = snapshot_at(now);
        let process = &snapshot.classes[2];
        assert_eq!(process.count, 2);
        assert_eq!(process.deadlines, 1);
        assert_eq!(process.failures, 1);
        assert_eq!(process.bytes, 42);
    }

    #[test]
    fn unknown_classes_are_ignored() {
        // Routing rejects unknown names outright, so nothing lands anywhere
        // (the global registry is shared with tests running in parallel).
        assert_eq!(
            CLASSES.iter().position(|name| *name == "mystery-class"),
            None
        );
        observe_at(now_ms(), &observation("mystery-class", 1)); // must not panic
    }

    #[test]
    fn clock_step_backwards_resets_the_window() {
        let now = 1_700_000_000_000u64;
        observe_at(now, &observation("http", 1));
        assert_eq!(snapshot_at(now).classes[5].count, 1);
        // A much older timestamp must not resurrect the stale bucket.
        assert_eq!(snapshot_at(now - 10 * BUCKET_MS).classes[5].count, 0);
        observe_at(now - 10 * BUCKET_MS, &observation("http", 2));
        let reset = snapshot_at(now - 10 * BUCKET_MS);
        assert_eq!(reset.classes[5].count, 1);
        assert_eq!(reset.classes[5].p50_ms, Some(2));
    }

    #[test]
    fn tool_counters_track_whitelist_and_other() {
        assert_eq!(tool_index(Some("read")), 5);
        assert_eq!(tool_index(Some("create_handoff")), 11);
        assert_eq!(tool_index(Some("mystery-tool")), TOOL_OTHER);
        assert_eq!(tool_index(None), TOOL_OTHER);

        let before = snapshot().tools[5].count;
        observe_tool(5, 64, false, false);
        observe_tool(TOOL_OTHER, 0, true, true);
        let after = snapshot();
        assert_eq!(after.tools[5].count, before + 1);
        assert_eq!(after.tools[5].bytes, 64);
        assert_eq!(after.tools[TOOL_OTHER].deadlines, 1);
        assert_eq!(after.tools[TOOL_OTHER].errors, 1);
        assert_eq!(after.tools.len(), TOOL_COUNT);
    }

    #[test]
    fn cache_counters_accumulate_per_kind() {
        let before = snapshot();
        record_cache_hit(CacheKind::AppConfig);
        record_cache_miss(CacheKind::AppConfig);
        record_cache_hit(CacheKind::DataUri);
        let after = snapshot();
        assert_eq!(
            after.cache_hits[CacheKind::AppConfig as usize],
            before.cache_hits[CacheKind::AppConfig as usize] + 1
        );
        assert_eq!(
            after.cache_misses[CacheKind::AppConfig as usize],
            before.cache_misses[CacheKind::AppConfig as usize] + 1
        );
        assert_eq!(
            after.cache_hits[CacheKind::DataUri as usize],
            before.cache_hits[CacheKind::DataUri as usize] + 1
        );
    }

    #[test]
    fn in_flight_depth_and_max_track_requests() {
        begin_in_flight();
        begin_in_flight();
        let during = snapshot();
        assert_eq!(during.in_flight, 2);
        assert!(during.in_flight_max >= 2, "max must observe depth 2");
        end_in_flight();
        end_in_flight();
        assert_eq!(snapshot().in_flight, 0);
    }

    #[test]
    fn in_flight_guard_restores_depth_on_drop() {
        begin_in_flight();
        {
            let _guard = InFlightGuard::new();
            assert_eq!(snapshot().in_flight, 2);
        }
        assert_eq!(snapshot().in_flight, 1);
        end_in_flight();
    }

    #[test]
    fn registry_stays_under_the_memory_budget() {
        let steady_state = std::mem::size_of::<PerfMetrics>();
        assert!(
            steady_state < 40 * 1024,
            "steady-state registry must stay under 40 KB, saw {steady_state} bytes"
        );
        // One 60 s bucket: counters plus three fixed reservoirs.
        assert_eq!(
            std::mem::size_of::<Bucket>(),
            std::mem::size_of::<u64>()
                + 3 * std::mem::size_of::<u32>()
                + std::mem::size_of::<Reservoir<ELAPSED_SAMPLES>>()
                + 2 * std::mem::size_of::<Reservoir<STAGE_SAMPLES>>()
        );
    }

    #[test]
    fn cpu_basis_points_convert_ticks_to_fraction_of_a_core() {
        assert_eq!(cpu_basis_points(100, 200, 1_000, 100), Some(FULL_CPU_BP));
        assert_eq!(cpu_basis_points(100, 150, 1_000, 100), Some(5_000));
        assert_eq!(cpu_basis_points(100, 200, 0, 100), None);
        assert_eq!(cpu_basis_points(100, 200, 1_000, 0), None);
        // Two busy cores on one core's clock still caps at 100%.
        assert_eq!(cpu_basis_points(0, 20_000, 1_000, 100), Some(FULL_CPU_BP));
    }

    #[cfg(unix)]
    #[test]
    fn proc_parsers_read_cpu_ticks_and_rss() {
        // Real /proc/self/stat shape: pid (comm with spaces) state ppid pgrp
        // session tty_nr tpgid flags minflt cminflt majflt cmajflt utime stime.
        let stat = "12345 (catdesk ser ver) R 1 2 3 4 5 6 7 8 9 10 4242 58 0 0 0 0 0 0 0";
        assert_eq!(parse_proc_stat_cpu_ticks(stat), Some(4242 + 58));
        assert_eq!(parse_proc_stat_cpu_ticks("12 (bash) S 1 2"), None);
        let status = "Name:\tcatdesk\nVmRSS:\t    185344 kB\nVmSize:\t 999999 kB\n";
        assert_eq!(parse_proc_status_rss_kb(status), Some(185_344));
        assert_eq!(parse_proc_status_rss_kb("Name:\tcatdesk\n"), None);
    }
}
