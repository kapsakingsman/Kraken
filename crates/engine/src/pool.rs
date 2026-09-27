//! Rendering on several CPU cores: the in-process [`Engine`] plus helper worker processes,
//! each with its own PDFium (PDFium can only be used from one thread per process).
//!
//! Helpers are only used where they pay off. Ordinary pages render in a few milliseconds
//! and all go to the in-process engine; splitting them would cost more than it saves. A
//! page becomes *slow* once one of its tiles took [`SLOW_TILE_MS`] or more (typically a
//! huge image); tiles of slow pages are spread over the helpers, as many as the waiting
//! work needs ([`TARGET_MS`]). Helpers are started ahead ([`RenderPool::prewarm`]) or on
//! demand, free their caches after [`TRIM_AFTER`] idle and stop after [`STOP_AFTER`], and
//! never exceed what the machine can spare ([`PoolConfig::max_helpers`], fewer on battery,
//! none when memory is low).
//!
//! A helper that crashes takes only itself down: its tiles are rendered again elsewhere,
//! and a page that crashed helpers twice is reported as failed instead of being retried.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};

use crate::delivered::Delivered;
use crate::protocol::{self, FromWorker, ToWorker};
use crate::queue::TileQueue;
use crate::{
    DocId, DocInfo, Engine, EngineConfig, EngineError, Tile, TileKey, TileRequest, TileResult,
    system,
};

/// A tile this slow or slower makes its page a candidate for the helpers. Sending a tile
/// to another process and its 1 MB of pixels back costs 1-2 ms, so from here on it pays.
pub const SLOW_TILE_MS: f32 = 25.0;
/// Enough helpers are used for the waiting work on slow pages to finish in about this long.
pub const TARGET_MS: f32 = 150.0;
/// Idle helpers free their caches (parsed pages, decoded images) after this long.
pub const TRIM_AFTER: Duration = Duration::from_secs(10);
/// Idle helpers stop after this long; they are started again when needed.
pub const STOP_AFTER: Duration = Duration::from_secs(60);
/// No helpers are started, and idle ones stop, when less memory than this is free.
pub const MIN_FREE_MEMORY_MB: u64 = 1024;
/// At most this many render processes in total, including the app itself: beyond that,
/// each one adds less speed than memory.
pub const MAX_RENDERERS: usize = 4;

/// Tiles the in-process engine has at once: enough that it never waits for the next one.
const MAIN_CAPACITY: usize = 4;
/// A page whose tiles crashed helpers this often is given up.
const MAX_CRASHES: u32 = 2;
const HOUSEKEEPING: Duration = Duration::from_millis(500);

/// How to start a render worker: a program that calls [`crate::run_worker`].
#[derive(Clone, Debug)]
pub struct WorkerCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

pub struct PoolConfig {
    pub engine: EngineConfig,
    /// `None` renders everything in-process.
    pub worker: Option<WorkerCommand>,
    /// Upper limit on helper processes; see [`PoolConfig::default_max_helpers`].
    pub max_helpers: usize,
    /// Idle helpers free their caches after this long.
    pub trim_after: Duration,
    /// Idle helpers stop after this long.
    pub stop_after: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        PoolConfig {
            engine: EngineConfig::default(),
            worker: None,
            max_helpers: Self::default_max_helpers(),
            trim_after: TRIM_AFTER,
            stop_after: STOP_AFTER,
        }
    }
}

impl PoolConfig {
    /// One renderer per CPU core, keeping one core free for the UI and the rest of the
    /// system, and at most [`MAX_RENDERERS`]; the app itself is one of them.
    pub fn default_max_helpers() -> usize {
        system::cpu_cores()
            .saturating_sub(1)
            .min(MAX_RENDERERS)
            .saturating_sub(1)
    }
}

/// Counters for the HUD and performance reports.
#[derive(Clone, Debug, Default)]
pub struct PoolStatus {
    /// Helper processes running.
    pub helpers: usize,
    /// Helpers rendering right now.
    pub busy_helpers: usize,
    pub tiles_by_helpers: u64,
    /// Tiles stopped part way because nobody wanted them any more.
    pub tiles_cancelled: u64,
    pub helper_crashes: u64,
    /// Why helpers could not be started, if they could not.
    pub helper_error: Option<String>,
}

/// Renders tiles like [`Engine`], on as many processes as useful. Same calling pattern:
/// [`RenderPool::open`], [`RenderPool::set_wanted`], then read [`RenderPool::results`].
pub struct RenderPool {
    main: Arc<Engine>,
    events: Sender<Event>,
    results: Receiver<TileResult>,
    status: Arc<Mutex<PoolStatus>>,
    thread: Option<JoinHandle<()>>,
}

impl RenderPool {
    /// `waker` is called after new results arrive, like [`Engine::start_with_waker`].
    pub fn start(
        config: PoolConfig,
        waker: impl Fn() + Send + 'static,
    ) -> Result<Self, EngineError> {
        let main = Arc::new(Engine::start(config.engine)?);
        let (events, event_rx) = unbounded();
        let (result_tx, results) = unbounded();
        let status = Arc::new(Mutex::new(PoolStatus::default()));

        // The engine's results reach the scheduler like any other event.
        {
            let engine_results = main.results().clone();
            let events = events.clone();
            thread::spawn(move || {
                for result in engine_results {
                    if events.send(Event::MainResult(result)).is_err() {
                        return;
                    }
                }
            });
        }

        let scheduler = Scheduler {
            main: Arc::clone(&main),
            main_slot: Slot::default(),
            helpers: Vec::new(),
            next_helper: 0,
            queue: TileQueue::default(),
            generation: 0,
            delivered: Delivered::default(),
            docs: HashMap::new(),
            page_ms: HashMap::new(),
            crashes: HashMap::new(),
            given_up: HashSet::new(),
            results: result_tx,
            waker: Box::new(waker),
            events: events.clone(),
            worker: config.worker,
            max_helpers: config.max_helpers,
            trim_after: config.trim_after,
            stop_after: config.stop_after,
            status: Arc::clone(&status),
            counters: PoolStatus::default(),
        };
        let thread = thread::Builder::new()
            .name("render-pool".into())
            .spawn(move || scheduler.run(event_rx))?;
        Ok(RenderPool {
            main,
            events,
            results,
            status,
            thread: Some(thread),
        })
    }

    /// Opens a PDF (in-process; helpers open it in the background). Blocks until it is
    /// open, so call it from a background thread for large files.
    pub fn open(
        &self,
        path: impl Into<PathBuf>,
        password: Option<&str>,
    ) -> Result<DocInfo, EngineError> {
        let path = path.into();
        let info = self.main.open(&path, password)?;
        let _ = self.events.send(Event::Opened {
            doc: info.id,
            path,
            password: password.map(str::to_owned),
        });
        Ok(info)
    }

    pub fn close(&self, doc: DocId) {
        let _ = self.events.send(Event::Close(doc));
    }

    /// See [`Engine::set_wanted`].
    pub fn set_wanted(
        &self,
        generation: u64,
        requests: Vec<TileRequest>,
        results_read: Option<u64>,
    ) {
        let _ = self.events.send(Event::Wanted {
            generation,
            requests,
            results_read,
        });
    }

    /// Starts the helpers now, so they have the open documents ready when a slow page
    /// comes up. Call it once the first page is on screen, not before: starting processes
    /// competes with startup for the CPU.
    pub fn prewarm(&self) {
        let _ = self.events.send(Event::Prewarm);
    }

    pub fn results(&self) -> &Receiver<TileResult> {
        &self.results
    }

    pub fn status(&self) -> PoolStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Kills every helper process, as if they had crashed. For tests.
    #[doc(hidden)]
    pub fn kill_helpers(&self) {
        let _ = self.events.send(Event::KillHelpers);
    }
}

impl Drop for RenderPool {
    fn drop(&mut self) {
        let _ = self.events.send(Event::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

enum Event {
    Wanted {
        generation: u64,
        requests: Vec<TileRequest>,
        results_read: Option<u64>,
    },
    Opened {
        doc: DocId,
        path: PathBuf,
        password: Option<String>,
    },
    Close(DocId),
    Prewarm,
    MainResult(TileResult),
    Helper(u64, FromWorker),
    HelperGone(u64),
    KillHelpers,
    Shutdown,
}

/// Tiles handed to one renderer and not finished yet.
#[derive(Default)]
struct Slot {
    assigned: Vec<TileRequest>,
    /// The renderer's own generation counter, bumped with every new assignment.
    generation: u64,
    /// The assignment changed and must be sent.
    dirty: bool,
}

struct Helper {
    id: u64,
    child: Child,
    stdin: Option<BufWriter<ChildStdin>>,
    slot: Slot,
    /// Documents this helper has open (it opens them in the background after starting).
    ready: HashSet<DocId>,
    idle_since: Instant,
    trimmed: bool,
}

impl Helper {
    fn send(&mut self, message: &ToWorker) {
        let failed = match &mut self.stdin {
            Some(stdin) => protocol::write_to_worker(stdin, message).is_err(),
            None => true,
        };
        if failed {
            // It died; its reader thread reports that and the tiles are handed out again.
            self.stdin = None;
        }
    }
}

struct DocSource {
    path: PathBuf,
    password: Option<String>,
}

struct Scheduler {
    main: Arc<Engine>,
    main_slot: Slot,
    helpers: Vec<Helper>,
    next_helper: u64,
    /// Wanted tiles not handed to any renderer yet.
    queue: TileQueue,
    /// The caller's generation.
    generation: u64,
    delivered: Delivered,
    docs: HashMap<DocId, DocSource>,
    /// Slowest tile seen per page, in milliseconds. The slowest, not the latest: a page's
    /// quick drafts must not hide that its final tiles are slow.
    page_ms: HashMap<(DocId, u32), f32>,
    crashes: HashMap<(DocId, u32), u32>,
    /// Pages that crashed helpers [`MAX_CRASHES`] times.
    given_up: HashSet<(DocId, u32)>,
    results: Sender<TileResult>,
    waker: Box<dyn Fn() + Send>,
    events: Sender<Event>,
    worker: Option<WorkerCommand>,
    max_helpers: usize,
    trim_after: Duration,
    stop_after: Duration,
    status: Arc<Mutex<PoolStatus>>,
    counters: PoolStatus,
}

impl Scheduler {
    fn run(mut self, events: Receiver<Event>) {
        loop {
            // Without helpers there is nothing to look after, so no timer: an idle app
            // must not wake up.
            let event = if self.helpers.is_empty() {
                events.recv().map_err(|_| RecvTimeoutError::Disconnected)
            } else {
                events.recv_timeout(HOUSEKEEPING)
            };
            let mut woke = false;
            match event {
                Ok(event) => {
                    if !self.handle(event, &mut woke) {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            // Handle everything that arrived meanwhile before deciding who renders what.
            let mut stop = false;
            while let Ok(event) = events.try_recv() {
                if !self.handle(event, &mut woke) {
                    stop = true;
                    break;
                }
            }
            if stop {
                break;
            }
            self.housekeeping();
            self.assign();
            self.flush_assignments();
            self.publish_status();
            if woke {
                (self.waker)();
            }
        }
        self.stop_all_helpers();
    }

    /// Returns `false` to stop.
    fn handle(&mut self, event: Event, woke: &mut bool) -> bool {
        match event {
            Event::Wanted {
                generation,
                requests,
                results_read,
            } => self.set_wanted(generation, requests, results_read, woke),
            Event::Opened {
                doc,
                path,
                password,
            } => {
                let message = ToWorker::Open {
                    doc,
                    path: path.clone(),
                    password: password.clone(),
                };
                for helper in &mut self.helpers {
                    helper.send(&message);
                }
                self.docs.insert(doc, DocSource { path, password });
            }
            Event::Close(doc) => self.close(doc),
            Event::Prewarm => {
                let want = self.max_helpers_now();
                while self.helpers.len() < want && self.spawn_helper() {}
            }
            Event::MainResult(result) => {
                let outcome = result.tile;
                self.finished(None, result.key, outcome, woke);
            }
            Event::Helper(id, message) => self.on_helper_message(id, message, woke),
            Event::HelperGone(id) => self.helper_gone(id, woke),
            Event::KillHelpers => {
                for helper in &mut self.helpers {
                    let _ = helper.child.kill();
                }
            }
            Event::Shutdown => return false,
        }
        true
    }

    fn set_wanted(
        &mut self,
        generation: u64,
        requests: Vec<TileRequest>,
        results_read: Option<u64>,
        woke: &mut bool,
    ) {
        if generation < self.generation {
            return;
        }
        self.generation = generation;
        self.queue.drop_older_than(generation);
        let wanted: HashSet<TileKey> = requests.iter().map(|r| r.key).collect();

        // Tiles being rendered that nobody wants any more are taken back; the renderer
        // stops them part way.
        for slot in self.slots_mut() {
            let before = slot.assigned.len();
            slot.assigned.retain(|a| wanted.contains(&a.key));
            slot.dirty |= slot.assigned.len() != before;
        }

        for request in requests {
            let request = TileRequest {
                generation,
                ..request
            };
            if self.delivered.is_unread(&request, results_read) {
                continue;
            }
            if self.given_up.contains(&(request.key.doc, request.key.page)) {
                self.send_result(
                    &request,
                    Err(EngineError::Worker(
                        "this page crashed the renderer repeatedly".into(),
                    )),
                    woke,
                );
                continue;
            }
            // Already being rendered: keep it there, with the new priority and generation.
            let mut in_flight = false;
            for slot in self.slots_mut() {
                if let Some(a) = slot.assigned.iter_mut().find(|a| a.key == request.key) {
                    if a.quality == request.quality {
                        a.generation = request.generation;
                        a.priority = request.priority;
                        in_flight = true;
                    }
                    break;
                }
            }
            if !in_flight {
                self.queue.push(request);
            }
        }
    }

    fn close(&mut self, doc: DocId) {
        self.queue.remove_doc(doc);
        for slot in self.slots_mut() {
            let before = slot.assigned.len();
            slot.assigned.retain(|a| a.key.doc != doc);
            slot.dirty |= slot.assigned.len() != before;
        }
        self.main.close(doc);
        for helper in &mut self.helpers {
            helper.ready.remove(&doc);
            helper.send(&ToWorker::Close(doc));
        }
        self.docs.remove(&doc);
        self.page_ms.retain(|(d, _), _| *d != doc);
        self.crashes.retain(|(d, _), _| *d != doc);
        self.given_up.retain(|(d, _)| *d != doc);
    }

    fn on_helper_message(&mut self, id: u64, message: FromWorker, woke: &mut bool) {
        match message {
            FromWorker::Opened { doc, error } => {
                if let Some(helper) = self.helpers.iter_mut().find(|h| h.id == id)
                    && error.is_none()
                    && self.docs.contains_key(&doc)
                {
                    helper.ready.insert(doc);
                }
            }
            FromWorker::Tile {
                key,
                width,
                height,
                draft,
                render_time,
                rgba,
                ..
            } => {
                let tile = Tile {
                    width,
                    height,
                    rgba,
                    render_time,
                    draft,
                };
                self.finished(Some(id), key, Ok(tile), woke);
            }
            FromWorker::Failed {
                key,
                cancelled,
                message,
                ..
            } => {
                let error = if cancelled {
                    EngineError::Cancelled
                } else {
                    EngineError::Worker(message)
                };
                self.finished(Some(id), key, Err(error), woke);
            }
        }
    }

    /// A renderer (`helper`, or the in-process engine for `None`) is done with a tile.
    fn finished(
        &mut self,
        helper: Option<u64>,
        key: TileKey,
        outcome: Result<Tile, EngineError>,
        woke: &mut bool,
    ) {
        let slot = match helper {
            None => &mut self.main_slot,
            Some(id) => match self.helpers.iter_mut().find(|h| h.id == id) {
                Some(h) => &mut h.slot,
                None => return,
            },
        };
        let Some(pos) = slot.assigned.iter().position(|a| a.key == key) else {
            // Taken back meanwhile: nobody wants it.
            if matches!(outcome, Err(EngineError::Cancelled)) {
                self.counters.tiles_cancelled += 1;
            }
            return;
        };
        let request = slot.assigned.remove(pos);
        if slot.assigned.is_empty()
            && let Some(id) = helper
            && let Some(h) = self.helpers.iter_mut().find(|h| h.id == id)
        {
            h.idle_since = Instant::now();
        }
        match outcome {
            // Stopped although still assigned (it was taken back and handed out again in
            // between): render it again.
            Err(EngineError::Cancelled) => self.queue.push(request),
            outcome => {
                if let Ok(tile) = &outcome {
                    let ms = tile.render_time.as_secs_f32() * 1000.0;
                    let slowest = self.page_ms.entry((key.doc, key.page)).or_default();
                    *slowest = slowest.max(ms);
                    if helper.is_some() {
                        self.counters.tiles_by_helpers += 1;
                    }
                }
                self.send_result(&request, outcome, woke);
            }
        }
    }

    fn send_result(
        &mut self,
        request: &TileRequest,
        tile: Result<Tile, EngineError>,
        woke: &mut bool,
    ) {
        self.delivered
            .record(request.key, request.quality, tile.is_ok());
        let _ = self.results.send(TileResult {
            key: request.key,
            generation: request.generation,
            tile,
        });
        *woke = true;
    }

    fn helper_gone(&mut self, id: u64, woke: &mut bool) {
        let Some(pos) = self.helpers.iter().position(|h| h.id == id) else {
            return;
        };
        let mut helper = self.helpers.remove(pos);
        let _ = helper.child.kill();
        let _ = helper.child.wait();
        if helper.slot.assigned.is_empty() {
            return; // stopped while idle, or crashed with nothing to blame
        }
        self.counters.helper_crashes += 1;
        for request in helper.slot.assigned {
            let page = (request.key.doc, request.key.page);
            let crashes = self.crashes.entry(page).or_default();
            *crashes += 1;
            if *crashes >= MAX_CRASHES {
                self.given_up.insert(page);
                self.send_result(
                    &request,
                    Err(EngineError::Worker(
                        "this page crashed the renderer repeatedly".into(),
                    )),
                    woke,
                );
            } else {
                self.queue.push(request);
            }
        }
    }

    fn is_slow(page_ms: &HashMap<(DocId, u32), f32>, key: &TileKey) -> bool {
        page_ms
            .get(&(key.doc, key.page))
            .is_some_and(|ms| *ms >= SLOW_TILE_MS)
    }

    /// Hands queued tiles to renderers, most urgent first.
    fn assign(&mut self) {
        let pending = self.queue.sorted(self.generation);
        if pending.is_empty() {
            return;
        }

        // How many renderers the waiting slow work needs to be done in about TARGET_MS.
        let slow_ms: f32 = pending
            .iter()
            .chain(self.main_slot.assigned.iter())
            .chain(self.helpers.iter().flat_map(|h| h.slot.assigned.iter()))
            .filter_map(|r| {
                let ms = *self.page_ms.get(&(r.key.doc, r.key.page))?;
                (ms >= SLOW_TILE_MS).then_some(ms)
            })
            .sum();
        let renderers_needed = (slow_ms / TARGET_MS).ceil() as usize;
        let helpers_needed = renderers_needed.saturating_sub(1);
        if helpers_needed > self.helpers.len() {
            let allowed = self.max_helpers_now();
            while self.helpers.len() < helpers_needed.min(allowed) && self.spawn_helper() {}
        }

        let mut busy_helpers = self
            .helpers
            .iter()
            .filter(|h| !h.slot.assigned.is_empty())
            .count();
        for request in pending {
            // One renderer per tile at a time. (A draft still rendering while its final
            // version is wanted: the final waits for the draft.)
            let key = request.key;
            if std::iter::once(&self.main_slot)
                .chain(self.helpers.iter().map(|h| &h.slot))
                .any(|slot| slot.assigned.iter().any(|a| a.key == key))
            {
                continue;
            }
            let slow = Self::is_slow(&self.page_ms, &request.key);
            let main_slow = self
                .main_slot
                .assigned
                .iter()
                .filter(|a| Self::is_slow(&self.page_ms, &a.key))
                .count();
            // The in-process engine takes any tile, but only one slow one at a time so
            // ordinary tiles are never stuck behind several slow ones.
            if self.main_slot.assigned.len() < MAIN_CAPACITY && (!slow || main_slow == 0) {
                self.queue.take_if(&request.key, |_| true);
                self.main_slot.assigned.push(request);
                self.main_slot.dirty = true;
                continue;
            }
            if !slow || busy_helpers >= helpers_needed {
                continue;
            }
            let doc = request.key.doc;
            if let Some(helper) = self
                .helpers
                .iter_mut()
                .find(|h| h.slot.assigned.is_empty() && h.stdin.is_some() && h.ready.contains(&doc))
            {
                self.queue.take_if(&request.key, |_| true);
                helper.slot.assigned.push(request);
                helper.slot.dirty = true;
                helper.trimmed = false;
                busy_helpers += 1;
            }
        }
    }

    /// Sends every changed assignment to its renderer.
    fn flush_assignments(&mut self) {
        if self.main_slot.dirty {
            let slot = &mut self.main_slot;
            slot.dirty = false;
            slot.generation += 1;
            self.main
                .set_wanted(slot.generation, slot.assigned.clone(), None);
        }
        for helper in &mut self.helpers {
            if helper.slot.dirty {
                helper.slot.dirty = false;
                helper.slot.generation += 1;
                let message = ToWorker::Wanted {
                    generation: helper.slot.generation,
                    requests: helper.slot.assigned.clone(),
                };
                helper.send(&message);
            }
        }
    }

    /// Frees memory of idle helpers, and stops them when idle long, on battery or when
    /// memory runs low.
    fn housekeeping(&mut self) {
        if self.helpers.is_empty() {
            return;
        }
        let allowed = self.max_helpers_now();
        let now = Instant::now();
        let mut stop = Vec::new();
        let mut running = self.helpers.len();
        for helper in &mut self.helpers {
            if !helper.slot.assigned.is_empty() {
                continue;
            }
            let idle = now.duration_since(helper.idle_since);
            if idle >= self.stop_after || running > allowed {
                stop.push(helper.id);
                running -= 1;
            } else if idle >= self.trim_after && !helper.trimmed {
                helper.send(&ToWorker::Trim);
                helper.trimmed = true;
            }
        }
        for id in stop {
            if let Some(pos) = self.helpers.iter().position(|h| h.id == id) {
                let helper = self.helpers.remove(pos);
                stop_helper(helper);
            }
        }
    }

    /// Helpers the machine can spare right now.
    fn max_helpers_now(&self) -> usize {
        if self.worker.is_none() {
            return 0;
        }
        if system::free_memory_mb().is_some_and(|mb| mb < MIN_FREE_MEMORY_MB) {
            return 0;
        }
        if system::on_battery() {
            return self.max_helpers.min(1);
        }
        self.max_helpers
    }

    /// Starts a helper and has it open the documents. Returns `false` if that failed.
    fn spawn_helper(&mut self) -> bool {
        let Some(worker) = &self.worker else {
            return false;
        };
        let mut command = Command::new(&worker.program);
        command
            .args(&worker.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            use windows_sys::Win32::System::Threading::{
                BELOW_NORMAL_PRIORITY_CLASS, CREATE_NO_WINDOW,
            };
            // Below the app's priority, so helpers never take the CPU from the UI.
            command.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                // Do not try again for every tile.
                self.counters.helper_error = Some(format!("{}: {e}", worker.program.display()));
                self.worker = None;
                return false;
            }
        };
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            return false;
        };
        let id = self.next_helper;
        self.next_helper += 1;
        let events = self.events.clone();
        thread::spawn(move || {
            let mut input = BufReader::new(stdout);
            while let Ok(Some(message)) = protocol::read_from_worker(&mut input) {
                if events.send(Event::Helper(id, message)).is_err() {
                    return;
                }
            }
            let _ = events.send(Event::HelperGone(id));
        });
        let mut helper = Helper {
            id,
            child,
            stdin: Some(BufWriter::new(stdin)),
            slot: Slot::default(),
            ready: HashSet::new(),
            idle_since: Instant::now(),
            trimmed: false,
        };
        for (doc, source) in &self.docs {
            helper.send(&ToWorker::Open {
                doc: *doc,
                path: source.path.clone(),
                password: source.password.clone(),
            });
        }
        self.helpers.push(helper);
        true
    }

    fn stop_all_helpers(&mut self) {
        for helper in self.helpers.drain(..) {
            stop_helper(helper);
        }
    }

    fn slots_mut(&mut self) -> impl Iterator<Item = &mut Slot> {
        std::iter::once(&mut self.main_slot).chain(self.helpers.iter_mut().map(|h| &mut h.slot))
    }

    fn publish_status(&mut self) {
        self.counters.helpers = self.helpers.len();
        self.counters.busy_helpers = self
            .helpers
            .iter()
            .filter(|h| !h.slot.assigned.is_empty())
            .count();
        if let Ok(mut status) = self.status.lock() {
            *status = self.counters.clone();
        }
    }
}

/// Closing its stdin makes a helper exit; one that does not is killed.
fn stop_helper(mut helper: Helper) {
    helper.stdin = None;
    thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = helper.child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = helper.child.kill();
        let _ = helper.child.wait();
    });
}
