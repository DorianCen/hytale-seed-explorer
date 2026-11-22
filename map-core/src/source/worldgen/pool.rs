//! `GeneratorPool` — the explorer's cheap-query worker pool (T08).
//!
//! `ChunkGenerator` is `!Send`/`!Sync` (its whole graph is `Rc`-based, see the
//! T01 audit §3), so exactly one generator lives per worker thread and the
//! main thread talks to them through channels — the same thread-ownership
//! boundary the worldgen repo's `WorldGenService` proves. No `Rc` ever
//! crosses a thread; the borrow checker enforces it for free.
//!
//! Dispatch policy (documented per T08):
//!
//! - **Region requests are band-split**: a rect whose sample rows exceed the
//!   worker count is cut into horizontal bands, one command per worker, and
//!   the replies are merged in band order. Bands keep each worker's column
//!   cache contiguous (T01 §4: the per-instance 50k cache is precious)
//!   while still spreading a tile across the pool.
//! - **Single-point queries** (`biome_at`/`height_at`/`zone_at`) ride the
//!   shared command channel: a free worker picks the command up. They exist
//!   for hover and search, never for tiles.
//! - **Feature placement queries** (T23) are one command, not band-split:
//!   the generator's window-based query keeps one worker's column cache
//!   contiguous, and translating its `Rc`-holding placements happens on that
//!   worker before only plain `Feature` values cross the thread boundary.
//! - **Cancellation** is checked every `CANCEL_CHECK_ROWS` sample rows so a
//!   worker is never held hostage by a scrolled-away tile.
//!
//! Worker threads never panic after reporting ready (the generator only
//! communicates load failures at startup), so blocking reply receives are
//! safe. `Drop` sends one `Shutdown` per worker, joins every handle, and is
//! idempotent.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::error::{SourceError, SourceResult};
use crate::types::{Feature, FeatureFilter, Rect, SpawnPoint, ZoneId};

/// Everything a worker needs to load its own generator. `Send` by
/// construction (plain data); the `Rc<ChunkGenerator>` it produces never
/// leaves the worker thread.
pub(crate) struct LoadSpec {
    /// Path as resolved by `HytaleWorldGenProvider::resolve_path`.
    pub world_path: std::path::PathBuf,
    pub world_name: String,
    pub world_version: String,
    pub assets_root: Option<std::path::PathBuf>,
    pub registries_path: Option<std::path::PathBuf>,
    /// The generator-visible seed (low 32 bits of the world seed, T02).
    pub generator_seed: i32,
}

/// A shared cancellation token; region callers create one per in-flight
/// request, and the tile scheduler (T15) passes its own tokens straight
/// through to cancel tiles that scroll away.
pub(crate) type CancelToken = Arc<AtomicBool>;

pub(crate) fn cancel_token() -> CancelToken {
    Arc::new(AtomicBool::new(false))
}

/// A reply channel for one in-flight region band: carries the band's whole
/// grid (layout + flat values).
type RegionReply<T> = mpsc::Sender<SourceResult<GridOut<T>>>;

/// A reply channel for one in-flight command.
type Reply<T> = mpsc::Sender<SourceResult<T>>;

pub(crate) enum Command {
    BiomeRegion {
        rect: Rect,
        step: u32,
        cancel: CancelToken,
        reply: Reply<GridOut<crate::types::BiomeId>>,
    },
    ZoneRegion {
        rect: Rect,
        step: u32,
        cancel: CancelToken,
        reply: Reply<GridOut<Option<crate::types::ZoneId>>>,
    },
    HeightRegion {
        rect: Rect,
        step: u32,
        cancel: CancelToken,
        reply: Reply<GridOut<i32>>,
    },
    SpawnPoints {
        reply: Reply<Vec<SpawnPoint>>,
    },
    /// One feature-placement query over a rect (T22/T23). The worker
    /// translates placements into plain [`Feature`] values before replying.
    FeatureQuery {
        rect: Rect,
        filter: FeatureFilter,
        reply: Reply<Vec<Feature>>,
    },
    ZoneAt {
        x: i32,
        z: i32,
        reply: Reply<Option<ZoneId>>,
    },
    /// Generate one chunk and extract its compact summary (T09). The caller
    /// caches; the worker never retains the `GeneratedChunk`.
    GenerateSummary {
        cx: i32,
        cz: i32,
        reply: Reply<Arc<crate::source::chunks::ChunkSummary>>,
    },
    /// Generate one chunk and extract one horizontal slice (T09). The reply
    /// carries both products of the single generation: the slice and the
    /// compact summary.
    GenerateSlice {
        cx: i32,
        cz: i32,
        y: i32,
        reply: Reply<(
            crate::source::chunks::ChunkSlice,
            Arc<crate::source::chunks::ChunkSummary>,
        )>,
    },
    Shutdown,
}

/// What a worker reports when it is ready: worker 0 attaches the catalogs it
/// extracted from its generator (built once, on the worker thread, then sent
/// to the main thread as plain data); every worker reports the generator's
/// chunk bounds (identical across workers).
pub(crate) struct WorkerReady {
    pub catalogs: Option<crate::source::worldgen::SourceCatalogs>,
    pub min_chunk: i32,
    pub max_chunk: i32,
}

/// The startup summary the pool hands back to the main thread.
pub(crate) struct PoolReady {
    pub catalogs: crate::source::worldgen::SourceCatalogs,
    pub min_chunk: i32,
    pub max_chunk: i32,
}

/// The merged result of a region request: grid layout plus flat row-major
/// values. `WorldgenSource` converts this into the public grid types.
pub(crate) struct GridOut<T> {
    pub origin_x: i32,
    pub origin_z: i32,
    pub step: u32,
    pub width: u32,
    pub height: u32,
    pub values: Vec<T>,
}

impl<T> GridOut<T> {
    pub fn new(
        rect: Rect,
        step: u32,
        width: u32,
        height: u32,
        values: Vec<T>,
    ) -> SourceResult<Self> {
        let expected = (width * height) as usize;
        if values.len() != expected {
            return Err(SourceError::Internal(format!(
                "a region merge produced {} samples where {expected} were expected",
                values.len()
            )));
        }
        Ok(Self {
            origin_x: rect.min_x,
            origin_z: rect.min_z,
            step,
            width,
            height,
            values,
        })
    }
}

/// The pool handle owned by the main thread.
pub(crate) struct GeneratorPool {
    command_tx: Sender<Command>,
    workers: Vec<JoinHandle<()>>,
    worker_count: usize,
    shutting_down: AtomicBool,
}

impl GeneratorPool {
    /// Spawns `workers` threads, each loading its own generator. Blocks
    /// until every worker reports ready; the first failure tears the pool
    /// down and propagates as `SourceError::Load`. Worker 0's catalogs come
    /// back with the pool.
    pub fn start(spec: &LoadSpec, workers: usize) -> SourceResult<(Self, PoolReady)> {
        let (command_tx, command_rx) = unbounded::<Command>();
        let (ready_tx, ready_rx) = mpsc::channel::<SourceResult<WorkerReady>>();
        let command_rx = Arc::new(command_rx);
        let mut handles = Vec::with_capacity(workers);
        let mut spawn_failures = 0usize;

        for index in 0..workers {
            let worker_spec = WorkerSpec {
                load: LoadSpec {
                    world_path: spec.world_path.clone(),
                    world_name: spec.world_name.clone(),
                    world_version: spec.world_version.clone(),
                    assets_root: spec.assets_root.clone(),
                    registries_path: spec.registries_path.clone(),
                    generator_seed: spec.generator_seed,
                },
                attach_catalogs: index == 0,
            };
            let command_rx = Arc::clone(&command_rx);
            let worker_ready_tx = ready_tx.clone();
            match std::thread::Builder::new()
                .name(format!("map-core-generator-{index}"))
                .spawn(move || worker_main(worker_spec, command_rx, worker_ready_tx))
            {
                Ok(handle) => handles.push(handle),
                Err(error) => {
                    // Report through the ready channel so the collection
                    // loop below sees it uniformly.
                    let _ = ready_tx.send(Err(SourceError::Load(format!(
                        "could not spawn generator worker {index}: {error}"
                    ))));
                    spawn_failures += 1;
                    // A thread that never started must not be waited for;
                    // stop spawning further workers.
                    break;
                }
            }
        }

        let started = handles.len();
        let mut pool = GeneratorPool {
            command_tx,
            workers: handles,
            worker_count: started,
            shutting_down: AtomicBool::new(false),
        };

        if started == 0 {
            pool.shutdown();
            return match ready_rx.recv() {
                Ok(Err(error)) => Err(error),
                _ => Err(SourceError::Load(
                    "no generator worker could start".to_string(),
                )),
            };
        }

        // Every started worker reports once; a spawn failure adds one extra
        // (error) report. The first error wins and tears the pool down.
        let expected_reports = started + spawn_failures;
        let mut catalogs: Option<crate::source::worldgen::SourceCatalogs> = None;
        let mut bounds: Option<(i32, i32)> = None;
        for _ in 0..expected_reports {
            match ready_rx.recv() {
                Ok(Ok(ready)) => {
                    bounds.get_or_insert((ready.min_chunk, ready.max_chunk));
                    if catalogs.is_none() {
                        catalogs = ready.catalogs;
                    }
                }
                Ok(Err(error)) => {
                    pool.shutdown();
                    return Err(error);
                }
                // A worker died before reporting: the channel closes only
                // when ALL senders drop, so treat it as a failed worker.
                Err(_) => {
                    pool.shutdown();
                    return Err(SourceError::Load(
                        "a generator worker died before reporting ready".to_string(),
                    ));
                }
            }
        }
        let (min_chunk, max_chunk) = bounds.ok_or_else(|| {
            pool.shutdown();
            SourceError::Internal("no generator worker reported its chunk bounds".to_string())
        })?;
        match catalogs {
            Some(catalogs) => Ok((
                pool,
                PoolReady {
                    catalogs,
                    min_chunk,
                    max_chunk,
                },
            )),
            None => {
                pool.shutdown();
                Err(SourceError::Internal(
                    "the catalog worker never reported its catalogs".to_string(),
                ))
            }
        }
    }

    pub fn worker_count(&self) -> usize {
        self.worker_count
    }

    /// Sends one command; workers pull from the shared queue.
    fn send(&self, command: Command) -> SourceResult<()> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(SourceError::Internal(
                "the generator pool is shutting down and accepts no commands".to_string(),
            ));
        }
        self.command_tx
            .send(command)
            .map_err(|_| SourceError::Internal("a generator worker channel is closed".to_string()))
    }

    /// Splits `rect` into at most `worker_count` horizontal bands (by sample
    /// rows); single-band regions stay whole. Bands come back in ascending z
    /// order.
    fn split_bands(&self, rect: Rect, step: u32) -> Vec<Rect> {
        let (_, samples_z) = rect.sample_dims(step);
        let bands = self.worker_count.min(samples_z as usize).max(1);
        if bands == 1 {
            return vec![rect];
        }
        let rows_per_band = (samples_z as usize).div_ceil(bands) as u32;
        let step_i = step as i32;
        let mut out = Vec::with_capacity(bands);
        let mut row_start = 0u32;
        while row_start < samples_z {
            let row_end = samples_z.min(row_start + rows_per_band);
            let min_z = rect.min_z + row_start as i32 * step_i;
            // The band's last sampled z, clamped into the original rect.
            let last_sample_z = min_z + (row_end - row_start) as i32 * step_i - step_i;
            out.push(Rect {
                min_x: rect.min_x,
                max_x: rect.max_x,
                min_z,
                max_z: rect.max_z.min(last_sample_z),
            });
            row_start = row_end;
        }
        out
    }

    /// Requests a biome grid over `rect`, band-split across the pool. The
    /// cancellation token is caller-owned (T15 wires it to tiles scrolling
    /// out of view; workers check it between row batches).
    pub fn biome_region(
        &self,
        rect: Rect,
        step: u32,
        cancel: CancelToken,
    ) -> SourceResult<GridOut<crate::types::BiomeId>> {
        self.region(rect, step, cancel, |rect, step, cancel, reply| {
            Command::BiomeRegion {
                rect,
                step,
                cancel,
                reply,
            }
        })
    }

    /// Requests a zone grid over `rect`, band-split across the pool (T19's
    /// debug layer; same cheap per-sample zone lookup as `zone_at`).
    pub fn zone_region(
        &self,
        rect: Rect,
        step: u32,
        cancel: CancelToken,
    ) -> SourceResult<GridOut<Option<crate::types::ZoneId>>> {
        self.region(rect, step, cancel, |rect, step, cancel, reply| {
            Command::ZoneRegion {
                rect,
                step,
                cancel,
                reply,
            }
        })
    }

    /// Requests a height grid over `rect`, band-split across the pool.
    pub fn height_region(
        &self,
        rect: Rect,
        step: u32,
        cancel: CancelToken,
    ) -> SourceResult<GridOut<i32>> {
        self.region(rect, step, cancel, |rect, step, cancel, reply| {
            Command::HeightRegion {
                rect,
                step,
                cancel,
                reply,
            }
        })
    }

    /// The shared band-split machinery: one command per band, replies merged
    /// in band order into a grid covering the whole rect.
    fn region<T, F>(
        &self,
        rect: Rect,
        step: u32,
        cancel: CancelToken,
        make: F,
    ) -> SourceResult<GridOut<T>>
    where
        F: Fn(Rect, u32, CancelToken, RegionReply<T>) -> Command,
    {
        let bands = self.split_bands(rect, step);
        let mut replies = Vec::with_capacity(bands.len());
        for band in &bands {
            let (tx, rx) = mpsc::channel();
            let command = make(*band, step, Arc::clone(&cancel), tx);
            self.send(command)?;
            replies.push(rx);
        }
        let (samples_x, samples_z) = rect.sample_dims(step);
        let mut values = Vec::with_capacity((samples_x * samples_z) as usize);
        for (band_index, rx) in replies.iter().enumerate() {
            let part: GridOut<T> = rx.recv().map_err(|_| {
                SourceError::Internal("a generator worker dropped its reply channel".to_string())
            })??;
            let (part_w, part_h) = (part.width, part.height);
            let expected = (part_w * part_h) as usize;
            if part.values.len() != expected || part.step != step {
                return Err(SourceError::Internal(format!(
                    "band {band_index} returned a malformed grid ({} values, step {})",
                    part.values.len(),
                    part.step
                )));
            }
            values.extend(part.values);
        }
        GridOut::new(rect, step, samples_x, samples_z, values)
    }

    /// Single-point queries ride the shared channel to a free worker.
    pub fn spawn_points(&self) -> SourceResult<Vec<SpawnPoint>> {
        let (tx, rx) = mpsc::channel();
        self.send(Command::SpawnPoints { reply: tx })?;
        rx.recv().map_err(|_| {
            SourceError::Internal("the spawn-point reply channel closed".to_string())
        })?
    }

    /// Sends one feature-placement query and blocks for its reply. Feature
    /// queries are one command (not band-split): the generator's query is
    /// internally window-based and a single worker keeps its column cache
    /// contiguous. The caller is expected to be a background thread; the
    /// overlay never calls this from the UI thread.
    pub fn features_in(
        &self,
        rect: Rect,
        filter: FeatureFilter,
    ) -> SourceResult<Vec<crate::types::Feature>> {
        let (tx, rx) = mpsc::channel();
        self.send(Command::FeatureQuery {
            rect,
            filter,
            reply: tx,
        })?;
        rx.recv()
            .map_err(|_| SourceError::Internal("the feature reply channel closed".to_string()))?
    }

    pub fn zone_at(&self, x: i32, z: i32) -> SourceResult<Option<ZoneId>> {
        let (tx, rx) = mpsc::channel();
        self.send(Command::ZoneAt { x, z, reply: tx })?;
        rx.recv()
            .map_err(|_| SourceError::Internal("the zone reply channel closed".to_string()))?
    }

    /// Requests one chunk summary: generation + extraction on a worker. The
    /// caller caches the result; the worker never retains the chunk.
    pub fn generate_summary(
        &self,
        cx: i32,
        cz: i32,
    ) -> SourceResult<Arc<crate::source::chunks::ChunkSummary>> {
        let (tx, rx) = mpsc::channel();
        self.send(Command::GenerateSummary { cx, cz, reply: tx })?;
        rx.recv()
            .map_err(|_| SourceError::Internal("the summary reply channel closed".to_string()))?
    }

    /// Requests one horizontal slice: generation + extraction on a worker.
    /// The reply carries the slice and the summary (one generation, two
    /// products); the caller decides what to cache.
    pub fn generate_slice(
        &self,
        cx: i32,
        cz: i32,
        y: i32,
    ) -> SourceResult<(
        crate::source::chunks::ChunkSlice,
        Arc<crate::source::chunks::ChunkSummary>,
    )> {
        let (tx, rx) = mpsc::channel();
        self.send(Command::GenerateSlice {
            cx,
            cz,
            y,
            reply: tx,
        })?;
        rx.recv()
            .map_err(|_| SourceError::Internal("the slice reply channel closed".to_string()))?
    }

    /// Idempotent: sends one `Shutdown` per worker and joins every handle.
    pub fn shutdown(&mut self) {
        if self.shutting_down.swap(true, Ordering::AcqRel) {
            return;
        }
        for _ in 0..self.worker_count {
            // Errors are fine: a worker may already be blocked in recv; the
            // join below is the real synchronization.
            let _ = self.command_tx.send(Command::Shutdown);
        }
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for GeneratorPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Per-worker startup spec: the load data plus whether this worker extracts
/// the catalogs for its ready report.
struct WorkerSpec {
    load: LoadSpec,
    attach_catalogs: bool,
}

/// The worker thread main: load, report ready, serve commands until
/// `Shutdown`. The generator never leaves this thread.
fn worker_main(
    spec: WorkerSpec,
    command_rx: Arc<Receiver<Command>>,
    ready_tx: mpsc::Sender<SourceResult<WorkerReady>>,
) {
    let generator = match crate::source::worldgen::load_generator(&spec.load) {
        Ok(generator) => generator,
        Err(error) => {
            // Report and die: the pool constructor sees the error and tears
            // the other workers down.
            let _ = ready_tx.send(Err(error));
            return;
        }
    };
    let ready = if spec.attach_catalogs {
        match crate::source::worldgen::build_catalogs(&generator) {
            Ok(catalogs) => WorkerReady {
                catalogs: Some(catalogs),
                min_chunk: generator.get_min_chunk_coord(),
                max_chunk: generator.get_max_chunk_coord(),
            },
            Err(error) => {
                let _ = ready_tx.send(Err(error));
                return;
            }
        }
    } else {
        WorkerReady {
            catalogs: None,
            min_chunk: generator.get_min_chunk_coord(),
            max_chunk: generator.get_max_chunk_coord(),
        }
    };
    // The chunk-generation state (registries + reusable resource) every
    // worker needs for T09's summary/slice commands. Built before the ready
    // report so a failure tears the pool down uniformly.
    let mut gen_state = match crate::source::worldgen::init_gen_state(&generator) {
        Ok(gen_state) => gen_state,
        Err(error) => {
            let _ = ready_tx.send(Err(error));
            return;
        }
    };

    if ready_tx.send(Ok(ready)).is_err() {
        // The pool gave up on startup; nothing left to do.
        return;
    }

    let seed = spec.load.generator_seed;
    while let Ok(command) = command_rx.recv() {
        match command {
            Command::BiomeRegion {
                rect,
                step,
                cancel,
                reply,
            } => {
                let result =
                    crate::source::worldgen::biome_region(&generator, seed, rect, step, &cancel);
                let _ = send_reply(reply, result);
            }
            Command::HeightRegion {
                rect,
                step,
                cancel,
                reply,
            } => {
                let result =
                    crate::source::worldgen::height_region(&generator, seed, rect, step, &cancel);
                let _ = send_reply(reply, result);
            }
            Command::ZoneRegion {
                rect,
                step,
                cancel,
                reply,
            } => {
                let result =
                    crate::source::worldgen::zone_region(&generator, seed, rect, step, &cancel);
                let _ = send_reply(reply, result);
            }
            Command::SpawnPoints { reply } => {
                let result = crate::source::worldgen::spawn_points(&generator, seed);
                let _ = send_reply(reply, result);
            }
            Command::FeatureQuery {
                rect,
                filter,
                reply,
            } => {
                let result =
                    crate::source::worldgen::feature_query(&generator, seed, rect, &filter);
                let _ = send_reply(reply, result);
            }
            Command::ZoneAt { x, z, reply } => {
                let result = crate::source::worldgen::zone_at(&generator, seed, x, z);
                let _ = send_reply(reply, result);
            }
            Command::GenerateSummary { cx, cz, reply } => {
                let result = crate::source::worldgen::generate_summary(
                    &generator,
                    &mut gen_state,
                    seed,
                    cx,
                    cz,
                );
                let _ = send_reply(reply, result);
            }
            Command::GenerateSlice { cx, cz, y, reply } => {
                let result = crate::source::worldgen::generate_slice(
                    &generator,
                    &mut gen_state,
                    seed,
                    cx,
                    cz,
                    y,
                );
                let _ = send_reply(reply, result);
            }
            Command::Shutdown => return,
        }
    }
}

fn send_reply<T>(reply: Reply<T>, result: SourceResult<T>) -> SourceResult<()> {
    reply
        .send(result)
        .map_err(|_| SourceError::Internal("the caller dropped its reply channel".to_string()))
}
