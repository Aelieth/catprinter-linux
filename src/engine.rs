//! Job store + the single worker. Every accepted job is rendered (stage 1) then printed with a
//! retry loop bounded by `printer_wait`; the `PrinterView` is what Get-Printer-Attributes reports.
//!
//! Invariants (CUPS `backend/ipp.c`): failures never use `aborted-by-system` / `job-canceled-*`
//! reasons (the backend would report the job as completed); printer-state is never `stopped`.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::ipp::options::JobOptions;
use crate::printer::{Phase, PreparedJob, PrintError, Printer, Progress};
use crate::raster::{GrayPage, Limits};
use crate::render::{self, RenderOptions};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Pending = 3,
    Processing = 5,
    Canceled = 7,
    Aborted = 8,
    Completed = 9,
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobState::Canceled | JobState::Aborted | JobState::Completed
        )
    }
    pub fn as_i32(self) -> i32 {
        self as i32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocFormat {
    PwgRaster,
    Image,
    Unknown,
}

impl DocFormat {
    pub fn sniff(bytes: &[u8]) -> DocFormat {
        if crate::raster::is_raster(bytes) {
            DocFormat::PwgRaster
        } else if crate::render::imagein::is_image(bytes) {
            DocFormat::Image
        } else {
            DocFormat::Unknown
        }
    }
    pub fn mime(self) -> &'static str {
        match self {
            DocFormat::PwgRaster => "image/pwg-raster",
            DocFormat::Image => "image/jpeg",
            DocFormat::Unknown => "application/octet-stream",
        }
    }
}

#[derive(Debug)]
pub struct Job {
    pub id: u32,
    pub opts: JobOptions,
    pub doc: Option<Bytes>,
    pub format: DocFormat,
    pub k_octets: u32,
    pub state: JobState,
    pub reasons: Vec<&'static str>,
    pub message: String,
    pub created: SystemTime,
    pub created_uptime: u32,
    pub processing_uptime: Option<u32>,
    pub completed_uptime: Option<u32>,
    pub impressions: u32,
    pub impressions_completed: u32,
    pub cancel: CancellationToken,
    pub preview: Option<Arc<GrayPage>>,
    /// Create-Job without a document yet.
    pub awaiting_document: bool,
    pub created_at: Instant,
    /// Last time a client did something for this job (Send-Document/Close-Job started or kept
    /// streaming). The stale-Create-Job timer counts from here, not from creation, because the
    /// CUPS backend streams the raster while the filters render it (minutes for a photo PDF).
    pub last_activity: Instant,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrinterState {
    Idle = 3,
    Processing = 4,
    Stopped = 5,
}

#[derive(Debug, Clone)]
pub struct PrinterView {
    pub state: PrinterState,
    pub reasons: Vec<&'static str>,
    pub message: String,
    pub state_changed_uptime: u32,
    pub state_changed_at: SystemTime,
    pub accepting: bool,
    pub current: Option<u32>,
    pub battery: Option<u8>,
    pub last_model: Option<String>,
    /// Why the last job gave up (`offline-report`, `media-empty-error`…). Shown in /health and
    /// on the status page only — NOT as IPP printer-state-reasons: the CUPS backend copies those
    /// onto the queue where they would linger until the next job (nothing polls an idle
    /// printer), so a queue would look "offline" for days after the printer came back.
    pub sticky_reasons: Vec<&'static str>,
    /// When the sticky reasons/message were set (cleared after `error_ttl` or the next success).
    pub sticky_since: Option<Instant>,
}

impl PrinterView {
    fn set(
        &mut self,
        uptime: u32,
        state: PrinterState,
        reasons: &[&'static str],
        message: impl Into<String>,
    ) {
        let message = message.into();
        if self.state != state || self.reasons != reasons || self.message != message {
            self.state = state;
            self.reasons = reasons.to_vec();
            self.message = message;
            self.state_changed_uptime = uptime;
            self.state_changed_at = SystemTime::now();
        }
    }
}

const KEEP_TERMINAL: usize = 100;
const KEEP_PREVIEWS: usize = 5;
/// Job ids count from here (2025-01-01T00:00:00Z) when no persisted counter exists, so ids stay
/// monotonic across restarts: a CUPS backend still polling an old job must never see a *new*
/// job's state under the same id.
const JOB_ID_EPOCH: u64 = 1_735_689_600;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub printer_wait: Duration,
    pub queue_max: usize,
    pub max_document_bytes: usize,
    pub max_copies: u32,
    pub render: RenderOptions,
    pub limits: Limits,
    /// Grace given to the current job on shutdown (once it is printing) before it is cancelled.
    pub shutdown_grace: Duration,
    /// Create-Job with no document activity for this long is aborted (multiple-operation-time-out).
    pub stale_document: Duration,
    /// How long the sticky "gave up" reasons/message stay in /health after a failed job.
    pub error_ttl: Duration,
    /// After cancelling an attempt, how long the driver gets to stop by itself (and release the
    /// Bluetooth link) before its future is dropped.
    pub coop_grace: Duration,
    /// Hard cap on one print attempt (a hung link must not block the queue forever).
    pub attempt_cap: Duration,
    /// Where `next-id` is persisted (systemd `$STATE_DIRECTORY`); None = time-based ids only.
    pub state_dir: Option<PathBuf>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            printer_wait: Duration::from_secs(120),
            queue_max: 16,
            max_document_bytes: 64 * 1024 * 1024,
            max_copies: 10,
            render: RenderOptions::default(),
            limits: Limits::default(),
            shutdown_grace: Duration::from_secs(15),
            stale_document: Duration::from_secs(120),
            error_ttl: Duration::from_secs(600),
            coop_grace: Duration::from_secs(5),
            attempt_cap: Duration::from_secs(15 * 60),
            state_dir: None,
        }
    }
}

/// First job id for this run: max(time-based, persisted counter).
fn initial_job_id(state_dir: Option<&Path>) -> u32 {
    let time_based = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(JOB_ID_EPOCH))
        .unwrap_or(0)
        .clamp(1, i32::MAX as u64 / 2) as u32;
    let persisted = state_dir
        .and_then(|d| std::fs::read_to_string(d.join("next-id")).ok())
        .and_then(|t| t.trim().parse::<u32>().ok())
        .unwrap_or(1);
    time_based.max(persisted).max(1)
}

fn persist_next_id(state_dir: Option<&Path>, next: u32) {
    if let Some(d) = state_dir {
        if let Err(e) = std::fs::write(d.join("next-id"), format!("{next}\n")) {
            tracing::debug!("could not persist next job id: {e}");
        }
    }
}

pub struct Store {
    pub jobs: BTreeMap<u32, Job>,
    pub next_id: u32,
    terminal_order: VecDeque<u32>,
    pub view: PrinterView,
    pub started: Instant,
    pub cfg: EngineConfig,
    pub jobs_total: u64,
    pub config_changed_uptime: u32,
    pub identify_requests: u32,
    /// An Identify is already queued for the worker (requests are coalesced so a burst cannot
    /// fill the work channel and starve print jobs).
    pub identify_pending: bool,
}

impl Store {
    pub fn uptime(&self) -> u32 {
        self.started.elapsed().as_secs().max(1) as u32
    }
    pub fn queued_count(&self) -> u32 {
        self.jobs
            .values()
            .filter(|j| !j.state.is_terminal())
            .count() as u32
    }
    fn prune(&mut self) {
        while self.terminal_order.len() > KEEP_TERMINAL {
            if let Some(id) = self.terminal_order.pop_front() {
                self.jobs.remove(&id);
            }
        }
        // previews: keep the last few only
        let mut with_preview: Vec<u32> = self
            .jobs
            .values()
            .filter(|j| j.preview.is_some())
            .map(|j| j.id)
            .collect();
        with_preview.sort_unstable();
        while with_preview.len() > KEEP_PREVIEWS {
            let id = with_preview.remove(0);
            if let Some(j) = self.jobs.get_mut(&id) {
                j.preview = None;
            }
        }
    }
    fn finish(
        &mut self,
        id: u32,
        state: JobState,
        reasons: &[&'static str],
        message: impl Into<String>,
    ) {
        let up = self.uptime();
        if let Some(j) = self.jobs.get_mut(&id) {
            if j.state.is_terminal() {
                return;
            }
            j.state = state;
            j.reasons = reasons.to_vec();
            j.message = message.into();
            j.completed_uptime = Some(up);
            j.doc = None;
            self.terminal_order.push_back(id);
        }
        self.prune();
    }
    /// Expire the sticky failure state after `error_ttl`; abort Create-Jobs that went quiet.
    pub fn tick(&mut self) {
        if let Some(t) = self.view.sticky_since {
            if t.elapsed() > self.cfg.error_ttl && self.view.current.is_none() {
                let up = self.uptime();
                self.view.set(up, PrinterState::Idle, &[], "");
                self.view.sticky_reasons.clear();
                self.view.sticky_since = None;
            }
        }
        // Create-Job whose client went silent (no Send-Document activity for stale_document)
        let stale_after = self.cfg.stale_document;
        let stale: Vec<u32> = self
            .jobs
            .values()
            .filter(|j| {
                j.awaiting_document
                    && !j.state.is_terminal()
                    && j.last_activity.elapsed() > stale_after
            })
            .map(|j| j.id)
            .collect();
        for id in stale {
            self.finish(
                id,
                JobState::Aborted,
                &["job-completed-with-errors"],
                "No document was sent for this job",
            );
        }
    }
}

#[derive(Debug)]
pub enum Work {
    Print(u32),
    Identify,
}

#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    #[error("printer is busy: {0} jobs already queued")]
    Busy(usize),
    #[error("document too large ({0} bytes)")]
    TooLarge(usize),
    #[error("document format not supported")]
    BadFormat,
    #[error("printer is shutting down")]
    NotAccepting,
    #[error("job {0} not found")]
    NotFound(u32),
    #[error("job {0} already has a document")]
    HasDocument(u32),
    #[error("empty document")]
    Empty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    NotFound,
    AlreadyTerminal,
    Canceled,
}

#[derive(Clone)]
pub struct Engine {
    pub store: Arc<Mutex<Store>>,
    tx: mpsc::Sender<Work>,
    pub shutdown: CancellationToken,
}

impl Engine {
    /// Build the engine and spawn its worker on the current runtime.
    pub fn start(
        cfg: EngineConfig,
        printer: Printer,
        shutdown: CancellationToken,
    ) -> (Engine, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(cfg.queue_max.max(1) + 4);
        let next_id = initial_job_id(cfg.state_dir.as_deref());
        let store = Arc::new(Mutex::new(Store {
            jobs: BTreeMap::new(),
            next_id,
            terminal_order: VecDeque::new(),
            view: PrinterView {
                state: PrinterState::Idle,
                reasons: vec![],
                message: String::new(),
                state_changed_uptime: 1,
                state_changed_at: SystemTime::now(),
                accepting: true,
                current: None,
                battery: None,
                last_model: None,
                sticky_reasons: vec![],
                sticky_since: None,
            },
            started: Instant::now(),
            cfg,
            jobs_total: 0,
            config_changed_uptime: 1,
            identify_requests: 0,
            identify_pending: false,
        }));
        let engine = Engine {
            store: store.clone(),
            tx,
            shutdown: shutdown.clone(),
        };
        let handle = tokio::spawn(worker(store, rx, printer, shutdown));
        (engine, handle)
    }

    pub fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn validate_doc(&self, doc: &Bytes) -> Result<DocFormat, SubmitError> {
        let max = self.store().cfg.max_document_bytes;
        if doc.len() > max {
            let e = SubmitError::TooLarge(doc.len());
            tracing::error!("refused job: {e}");
            return Err(e);
        }
        if doc.is_empty() {
            tracing::error!("refused job: empty document");
            return Err(SubmitError::Empty);
        }
        match DocFormat::sniff(doc) {
            DocFormat::Unknown => {
                tracing::error!("refused job: document format not supported");
                Err(SubmitError::BadFormat)
            }
            f => Ok(f),
        }
    }

    /// Print-Job (doc = Some) or Create-Job (doc = None). Returns the job id.
    pub fn create_job(&self, opts: JobOptions, doc: Option<Bytes>) -> Result<u32, SubmitError> {
        let format = match &doc {
            Some(d) => self.validate_doc(d)?,
            None => DocFormat::Unknown,
        };
        let awaiting = doc.is_none();
        let id = {
            let mut s = self.store();
            if !s.view.accepting {
                return Err(SubmitError::NotAccepting);
            }
            let queued = s.queued_count() as usize;
            if queued >= s.cfg.queue_max {
                return Err(SubmitError::Busy(queued));
            }
            let id = s.next_id;
            s.next_id += 1;
            persist_next_id(s.cfg.state_dir.as_deref(), s.next_id);
            s.jobs_total += 1;
            let up = s.uptime();
            let k_octets = doc
                .as_ref()
                .map(|d| d.len().div_ceil(1024) as u32)
                .unwrap_or(0);
            s.jobs.insert(
                id,
                Job {
                    id,
                    opts,
                    doc,
                    format,
                    k_octets,
                    state: JobState::Pending,
                    reasons: vec!["job-incoming"],
                    message: String::new(),
                    created: SystemTime::now(),
                    created_uptime: up,
                    processing_uptime: None,
                    completed_uptime: None,
                    impressions: 0,
                    impressions_completed: 0,
                    cancel: CancellationToken::new(),
                    preview: None,
                    awaiting_document: awaiting,
                    created_at: Instant::now(),
                    last_activity: Instant::now(),
                    model: None,
                },
            );
            id
        };
        if !awaiting {
            self.enqueue(id);
        }
        Ok(id)
    }

    /// A client is doing something for this job (Send-Document/Close-Job started or is still
    /// streaming its body): keeps the stale-Create-Job timer from firing.
    pub fn touch(&self, id: u32) {
        let mut s = self.store();
        if let Some(j) = s.jobs.get_mut(&id) {
            if !j.state.is_terminal() {
                j.last_activity = Instant::now();
            }
        }
    }

    /// Send-Document for a Create-Job job.
    pub fn add_document(&self, id: u32, doc: Bytes) -> Result<(), SubmitError> {
        let format = self.validate_doc(&doc)?;
        {
            let mut s = self.store();
            let j = s.jobs.get_mut(&id).ok_or(SubmitError::NotFound(id))?;
            if j.state.is_terminal() {
                return Err(SubmitError::NotFound(id));
            }
            if !j.awaiting_document {
                return Err(SubmitError::HasDocument(id));
            }
            j.k_octets = doc.len().div_ceil(1024) as u32;
            j.doc = Some(doc);
            j.format = format;
            j.awaiting_document = false;
        }
        self.enqueue(id);
        Ok(())
    }

    fn enqueue(&self, id: u32) {
        {
            let mut s = self.store();
            if let Some(j) = s.jobs.get_mut(&id) {
                if j.state.is_terminal() || j.cancel.is_cancelled() {
                    return;
                }
                j.reasons = vec!["job-queued"];
            }
        }
        if self.tx.try_send(Work::Print(id)).is_err() {
            // channel capacity == queue_max + slack, so this only happens under shutdown
            self.store().finish(
                id,
                JobState::Aborted,
                &["job-completed-with-errors"],
                "printer service is restarting — print again",
            );
        }
    }

    pub fn cancel(&self, id: u32) -> CancelOutcome {
        let mut s = self.store();
        let Some(j) = s.jobs.get(&id) else {
            return CancelOutcome::NotFound;
        };
        if j.state.is_terminal() {
            return CancelOutcome::AlreadyTerminal;
        }
        match j.state {
            JobState::Pending => {
                j.cancel.cancel();
                s.finish(
                    id,
                    JobState::Canceled,
                    &["job-canceled-by-user"],
                    "Canceled",
                );
            }
            _ => {
                // the worker observes the token and finishes the job
                j.cancel.cancel();
                if let Some(j) = s.jobs.get_mut(&id) {
                    j.reasons = vec!["processing-to-stop-point", "job-canceled-by-user"];
                }
            }
        }
        CancelOutcome::Canceled
    }

    /// Cancel-My-Jobs: all non-terminal jobs of `user` (optionally restricted to `ids`).
    pub fn cancel_my_jobs(&self, user: &str, ids: &[u32]) -> Vec<u32> {
        let targets: Vec<u32> = self
            .store()
            .jobs
            .values()
            .filter(|j| {
                !j.state.is_terminal()
                    && j.opts.user == user
                    && (ids.is_empty() || ids.contains(&j.id))
            })
            .map(|j| j.id)
            .collect();
        for id in &targets {
            self.cancel(*id);
        }
        targets
    }

    pub fn identify(&self) {
        let mut s = self.store();
        s.identify_requests += 1;
        if s.identify_pending {
            return;
        }
        s.identify_pending = self.tx.try_send(Work::Identify).is_ok();
    }

    /// Ask for a graceful stop: no new jobs, current job gets a grace period, pending jobs aborted.
    /// No `shutdown` printer-state-reason: the CUPS backend would copy it onto the queue where it
    /// lingers as an alert after the restart; `printer-is-accepting-jobs=false` + the message is
    /// what the backend acts on (it waits).
    pub fn begin_shutdown(&self) {
        let mut s = self.store();
        s.view.accepting = false;
        let up = s.uptime();
        let state = s.view.state;
        s.view.set(up, state, &[], "printer service is stopping");
    }
}

/// The single worker: renders and prints jobs one at a time.
fn lock_store(s: &Arc<Mutex<Store>>) -> std::sync::MutexGuard<'_, Store> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

async fn worker(
    store: Arc<Mutex<Store>>,
    mut rx: mpsc::Receiver<Work>,
    mut printer: Printer,
    shutdown: CancellationToken,
) {
    loop {
        // biased: once shutdown is requested, never start another job (an unbiased select would
        // pick up queued work half the time and start printing it just to abort it 15 s later).
        let work = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            w = rx.recv() => match w { Some(w) => w, None => break },
        };
        match work {
            Work::Identify => {
                lock_store(&store).identify_pending = false;
                let cancel = CancellationToken::new();
                let deadline = tokio::time::sleep(Duration::from_secs(20));
                tokio::pin!(deadline);
                let res = tokio::select! {
                    r = printer.identify(&cancel) => r,
                    _ = &mut deadline => Err(PrintError::Timeout("identify")),
                    _ = shutdown.cancelled() => Err(PrintError::Cancelled),
                };
                match res {
                    Ok(()) => tracing::info!("identify: done"),
                    Err(e) => tracing::info!("identify: skipped ({e})"),
                }
            }
            Work::Print(id) => {
                // A panic anywhere in the print path (render::pack, BLE) must not kill the single
                // worker: HTTP would keep answering while every job stays pending forever.
                let fut =
                    std::panic::AssertUnwindSafe(run_job(&store, &mut printer, id, &shutdown));
                if futures_util::FutureExt::catch_unwind(fut).await.is_err() {
                    tracing::error!(
                        job = id,
                        "print worker panicked (contained) — job aborted; please report"
                    );
                    let mut s = lock_store(&store);
                    let up = s.uptime();
                    s.finish(
                        id,
                        JobState::Aborted,
                        &["job-completed-with-errors"],
                        "printer service hiccup — print again",
                    );
                    s.view.current = None;
                    s.view.set(up, PrinterState::Idle, &[], "");
                }
            }
        }
    }
    // Shutdown: abort whatever is left pending.
    let mut s = lock_store(&store);
    let pending: Vec<u32> = s
        .jobs
        .values()
        .filter(|j| !j.state.is_terminal())
        .map(|j| j.id)
        .collect();
    for id in pending {
        s.finish(
            id,
            JobState::Aborted,
            &["job-completed-with-errors"],
            "printer service restarted — print again",
        );
    }
}

async fn run_job(
    store: &Arc<Mutex<Store>>,
    printer: &mut Printer,
    id: u32,
    shutdown: &CancellationToken,
) {
    let lock = || lock_store(store);
    // ---- take the job
    let (doc, format, opts, cancel, cfg) = {
        let mut s = lock();
        let up = s.uptime();
        let cfg = s.cfg.clone();
        let Some(j) = s.jobs.get_mut(&id) else { return };
        if j.state.is_terminal() {
            return;
        }
        if j.cancel.is_cancelled() {
            s.finish(
                id,
                JobState::Canceled,
                &["job-canceled-by-user"],
                "Canceled",
            );
            return;
        }
        if shutdown.is_cancelled() {
            s.finish(
                id,
                JobState::Aborted,
                &["job-completed-with-errors"],
                "printer service restarted — print again",
            );
            return;
        }
        j.state = JobState::Processing;
        j.reasons = vec!["job-printing"];
        j.processing_uptime = Some(up);
        let doc = j.doc.take().unwrap_or_default();
        let out = (doc, j.format, j.opts.clone(), j.cancel.clone(), cfg);
        s.view.current = Some(id);
        s.view.sticky_since = None;
        s.view.set(
            up,
            PrinterState::Processing,
            &[],
            format!("Preparing job {id}"),
        );
        out
    };
    // ---- stage 1: decode + prepare (blocking, bounded)
    let render_opts = opts.render_options(&cfg.render, format == DocFormat::Image);
    let limits = cfg.limits;
    let render_for_prep = render_opts.clone();
    let prep = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::task::spawn_blocking(move || -> Result<(render::GrayStrip, u32), String> {
            let pages: Vec<GrayPage> = match format {
                DocFormat::PwgRaster => {
                    crate::raster::decode(&doc, &limits).map_err(|e| e.to_string())?
                }
                DocFormat::Image => vec![render::imagein::load(&doc).map_err(|e| e.to_string())?],
                DocFormat::Unknown => return Err("unsupported document format".into()),
            };
            let n = pages.len() as u32;
            let strip = render::prepare(pages, &render_for_prep).map_err(|e| e.to_string())?;
            Ok((strip, n))
        }),
    )
    .await;
    let (strip, pages) = match prep {
        Ok(Ok(Ok(v))) => v,
        Ok(Ok(Err(msg))) => {
            tracing::error!(job = id, "refused job: {msg}");
            let mut s = lock();
            let up = s.uptime();
            s.finish(id, JobState::Aborted, &["document-format-error"], msg);
            s.view.current = None;
            s.view.set(up, PrinterState::Idle, &[], "");
            return;
        }
        Ok(Err(join)) => {
            let mut s = lock();
            let up = s.uptime();
            s.finish(
                id,
                JobState::Aborted,
                &["document-format-error"],
                format!("render crashed: {join}"),
            );
            s.view.current = None;
            s.view.set(up, PrinterState::Idle, &[], "");
            return;
        }
        Err(_) => {
            let mut s = lock();
            let up = s.uptime();
            s.finish(
                id,
                JobState::Aborted,
                &["document-format-error"],
                "rendering took too long",
            );
            s.view.current = None;
            s.view.set(up, PrinterState::Idle, &[], "");
            return;
        }
    };
    {
        let mut s = lock();
        if let Some(j) = s.jobs.get_mut(&id) {
            j.impressions = pages.max(1) * opts.copies.max(1);
            j.preview = Some(Arc::new(strip.as_page()));
        }
    }
    tracing::info!(
        job = id,
        quality = opts.print_quality,
        color_mode = %opts.color_mode,
        preset = render_opts.preset.name(),
        tone = render_opts.tone.name(),
        layout = ?strip.layout,
        pages,
        strip = format_args!("{}x{}", strip.width, strip.height),
        "prepared print"
    );
    let job = PreparedJob {
        id,
        name: opts.job_name.clone(),
        strip,
        opts: opts.render_options(&cfg.render, format == DocFormat::Image),
        copies: opts.copies.max(1),
    };

    // ---- stage 2: print with retries until printer_wait elapses
    let started = Instant::now();
    let deadline = started + cfg.printer_wait;
    let mut attempt: u32 = 0;
    let mut by_shutdown = false;
    let mut last_log: Option<Instant> = None;
    // Phase reported by the driver (Searching … Printing), so shutdown can cancel immediately
    // while nothing has reached the paper yet and only wait `shutdown_grace` once it prints.
    let phase = Arc::new(AtomicU8::new(Phase::Searching as u8));
    let outcome: Result<crate::printer::PrintReport, PrintError> = loop {
        attempt += 1;
        let store2 = store.clone();
        let phase2 = phase.clone();
        let mut on_progress = move |p: Progress| {
            phase2.store(p.phase as u8, Ordering::Relaxed);
            let mut s = store2.lock().unwrap_or_else(|e| e.into_inner());
            let up = s.uptime();
            let msg = match p.phase {
                Phase::Printing => format!("Printing job {id} — {}%", p.percent),
                _ => p.message.clone(),
            };
            s.view.set(up, PrinterState::Processing, &[], msg);
            if let Some(j) = s.jobs.get_mut(&id) {
                j.impressions_completed = (j.impressions as u64 * p.percent as u64 / 100) as u32;
                j.message = p.message;
            }
        };
        // The driver gets a child token: cancelling the job cancels it, and shutdown or the
        // per-attempt cap cancel it explicitly. In every case the driver is given `coop_grace`
        // to stop by itself — and release the Bluetooth link cleanly — before its future is
        // dropped (dropping mid-connect leaves cleanup to best-effort Drop impls).
        let attempt_token = cancel.child_token();
        phase.store(Phase::Searching as u8, Ordering::Relaxed);
        let print_fut = printer.print(&job, &attempt_token, &mut on_progress);
        tokio::pin!(print_fut);
        let mut capped = false;
        let first = tokio::select! {
            r = &mut print_fut => Some(r),
            _ = cancel.cancelled() => None,
            _ = shutdown_wait(shutdown, &phase, cfg.shutdown_grace) => { by_shutdown = true; None }
            _ = tokio::time::sleep(cfg.attempt_cap) => { capped = true; None }
        };
        let res = match first {
            Some(r) => r,
            None => {
                attempt_token.cancel();
                match tokio::time::timeout(cfg.coop_grace, &mut print_fut).await {
                    Ok(Ok(rep)) => Ok(rep),
                    Ok(Err(e)) if capped && matches!(e, PrintError::Cancelled) => {
                        Err(PrintError::Timeout("printing (attempt took too long)"))
                    }
                    Ok(Err(e)) => Err(e),
                    Err(_) if capped => {
                        Err(PrintError::Timeout("printing (attempt took too long)"))
                    }
                    Err(_) => Err(PrintError::Cancelled),
                }
            }
        };
        match res {
            Ok(rep) => break Ok(rep),
            Err(PrintError::Cancelled) => break Err(PrintError::Cancelled),
            Err(e) => {
                let now = Instant::now();
                let retry = e.retryable() && now < deadline && !shutdown.is_cancelled();
                let waited = now.duration_since(started).as_secs();
                {
                    let mut s = lock();
                    let up = s.uptime();
                    let msg = if retry {
                        format!(
                            "{} (waited {waited} s of {} s)",
                            e.kid_message(),
                            cfg.printer_wait.as_secs()
                        )
                    } else {
                        e.kid_message()
                    };
                    s.view.set(
                        up,
                        PrinterState::Processing,
                        e.printer_reasons(),
                        msg.clone(),
                    );
                    if let Some(j) = s.jobs.get_mut(&id) {
                        j.message = msg;
                    }
                }
                if !retry {
                    break Err(e);
                }
                // One journal line per minute while waiting, not one per attempt.
                if last_log.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
                    tracing::info!(
                        job = id,
                        attempt,
                        waited_s = waited,
                        "print attempt failed: {e}; retrying until the printer is found"
                    );
                    last_log = Some(now);
                } else {
                    tracing::debug!(job = id, attempt, "print attempt failed: {e}; retrying");
                }
                let backoff = Duration::from_secs(match attempt {
                    1 => 3,
                    2 => 5,
                    3 => 10,
                    _ => 15,
                });
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {},
                    _ = cancel.cancelled() => break Err(PrintError::Cancelled),
                    _ = shutdown.cancelled() => { by_shutdown = true; break Err(PrintError::Cancelled) },
                }
            }
        }
    };

    // ---- terminal state + printer view
    let mut s = lock();
    let up = s.uptime();
    s.view.current = None;
    match outcome {
        Ok(rep) => {
            tracing::info!(job = id, model = %rep.model, lines = rep.lines, copies = rep.copies, elapsed = ?rep.elapsed, confirmed = rep.complete_confirmed, "printed");
            if let Some(j) = s.jobs.get_mut(&id) {
                j.impressions_completed = j.impressions;
                j.model = Some(rep.model.clone());
            }
            s.view.battery = rep.battery.or(s.view.battery);
            s.view.last_model = Some(rep.model.clone());
            s.finish(id, JobState::Completed, &["job-completed-successfully"], "");
            s.view.set(up, PrinterState::Idle, &[], "");
            s.view.sticky_reasons.clear();
            s.view.sticky_since = None;
        }
        Err(PrintError::Cancelled) => {
            let by_shutdown = by_shutdown || (shutdown.is_cancelled() && !cancel.is_cancelled());
            if by_shutdown {
                s.finish(
                    id,
                    JobState::Aborted,
                    &["job-completed-with-errors"],
                    "printer service restarted — print again",
                );
            } else {
                s.finish(
                    id,
                    JobState::Canceled,
                    &["job-canceled-by-user"],
                    "Canceled",
                );
            }
            s.view.set(up, PrinterState::Idle, &[], "");
        }
        Err(e) => {
            if matches!(e, PrintError::Render(_) | PrintError::Io(_)) {
                tracing::error!(job = id, "refused job: {e}");
            } else {
                tracing::warn!(job = id, "job failed: {e}");
            }
            let (reasons, sticky_reasons): (&[&str], &[&str]) = match &e {
                PrintError::NotFound
                | PrintError::ConnectFailed { .. }
                | PrintError::NoBluetoothd
                | PrintError::AdapterOff => (&["service-off-line"], &["offline-report"]),
                PrintError::Render(_) => (&["document-format-error"], &[]),
                PrintError::Condition(c) if c.error.as_deref() == Some("no-paper") => {
                    (&["job-completed-with-errors"], &["media-empty-error"])
                }
                _ => (&["job-completed-with-errors"], &["other-error"]),
            };
            let msg = match &e {
                PrintError::NotFound | PrintError::ConnectFailed { .. } => {
                    format!("Cat printer not found for {} min — job {id} stopped. Turn it on and print again.", cfg.printer_wait.as_secs().div_ceil(60))
                }
                other => other.kid_message(),
            };
            s.finish(id, JobState::Aborted, reasons, msg.clone());
            // Live IPP reasons go back to none; the "why" stays visible in /health for a while.
            s.view.set(
                up,
                PrinterState::Idle,
                &[],
                if sticky_reasons.is_empty() {
                    String::new()
                } else {
                    msg
                },
            );
            s.view.sticky_reasons = sticky_reasons.to_vec();
            s.view.sticky_since = if sticky_reasons.is_empty() {
                None
            } else {
                Some(Instant::now())
            };
        }
    }
}

/// Resolves when the current attempt should be cancelled because of shutdown: immediately while
/// the driver is still searching/connecting/preparing (nothing on paper yet), after `grace` once
/// it is printing (let a short strip finish).
async fn shutdown_wait(shutdown: &CancellationToken, phase: &AtomicU8, grace: Duration) {
    shutdown.cancelled().await;
    if phase.load(Ordering::Relaxed) < Phase::Printing as u8 {
        return;
    }
    tokio::time::sleep(grace).await;
}

/// Snapshot for /health.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Health {
    pub version: &'static str,
    pub uptime_s: u64,
    pub printer_state: &'static str,
    pub reasons: Vec<&'static str>,
    pub message: String,
    pub accepting: bool,
    pub queue_depth: u32,
    pub current_job: Option<u32>,
    pub jobs_total: u64,
    pub battery: Option<u8>,
    pub last_model: Option<String>,
    pub last_job: Option<LastJob>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LastJob {
    pub id: u32,
    pub state: &'static str,
    pub message: String,
    pub name: String,
}

pub fn state_name(s: JobState) -> &'static str {
    match s {
        JobState::Pending => "pending",
        JobState::Processing => "processing",
        JobState::Canceled => "canceled",
        JobState::Aborted => "aborted",
        JobState::Completed => "completed",
    }
}

impl Store {
    pub fn health(&self) -> Health {
        let last = self
            .jobs
            .values()
            .filter(|j| j.state.is_terminal())
            .max_by_key(|j| j.id)
            .map(|j| LastJob {
                id: j.id,
                state: state_name(j.state),
                message: j.message.clone(),
                name: j.opts.job_name.clone(),
            });
        let mut reasons = self.view.reasons.clone();
        for r in &self.view.sticky_reasons {
            if !reasons.contains(r) {
                reasons.push(r);
            }
        }
        Health {
            version: crate::VERSION,
            uptime_s: self.started.elapsed().as_secs(),
            printer_state: match self.view.state {
                PrinterState::Idle => "idle",
                PrinterState::Processing => "processing",
                PrinterState::Stopped => "stopped",
            },
            reasons,
            message: self.view.message.clone(),
            accepting: self.view.accepting,
            queue_depth: self.queued_count(),
            current_job: self.view.current,
            jobs_total: self.jobs_total,
            battery: self.view.battery,
            last_model: self.view.last_model.clone(),
            last_job: last,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipp::options::JobOptions;
    use crate::printer::fake::FakePrinter;

    fn raster() -> Bytes {
        Bytes::from(
            std::fs::read(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/tiny-roll48.pwg"
            ))
            .unwrap(),
        )
    }

    fn cfg() -> EngineConfig {
        EngineConfig {
            printer_wait: Duration::from_secs(1),
            shutdown_grace: Duration::from_millis(300),
            stale_document: Duration::from_millis(200),
            error_ttl: Duration::from_millis(300),
            coop_grace: Duration::from_secs(2),
            ..EngineConfig::default()
        }
    }

    struct Rig {
        engine: Engine,
        worker: tokio::task::JoinHandle<()>,
        shutdown: CancellationToken,
        dir: tempfile::TempDir,
    }

    fn rig(cfg: EngineConfig, state: &str) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("state"), state).unwrap();
        let printer = Printer::Fake(FakePrinter::new(dir.path()).unwrap());
        let shutdown = CancellationToken::new();
        let (engine, worker) = Engine::start(cfg, printer, shutdown.clone());
        Rig {
            engine,
            worker,
            shutdown,
            dir,
        }
    }

    fn opts(name: &str) -> JobOptions {
        JobOptions {
            job_name: name.into(),
            user: "kid".into(),
            ..JobOptions::default()
        }
    }

    async fn wait_terminal(engine: &Engine, id: u32, max: Duration) -> Job {
        let t0 = Instant::now();
        loop {
            {
                let s = engine.store();
                if let Some(j) = s.jobs.get(&id) {
                    if j.state.is_terminal() {
                        return Job {
                            id: j.id,
                            opts: j.opts.clone(),
                            doc: None,
                            format: j.format,
                            k_octets: j.k_octets,
                            state: j.state,
                            reasons: j.reasons.clone(),
                            message: j.message.clone(),
                            created: j.created,
                            created_uptime: j.created_uptime,
                            processing_uptime: j.processing_uptime,
                            completed_uptime: j.completed_uptime,
                            impressions: j.impressions,
                            impressions_completed: j.impressions_completed,
                            cancel: j.cancel.clone(),
                            preview: None,
                            awaiting_document: j.awaiting_document,
                            created_at: j.created_at,
                            last_activity: j.last_activity,
                            model: j.model.clone(),
                        };
                    }
                }
            }
            assert!(t0.elapsed() < max, "job {id} not terminal after {max:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_state(engine: &Engine, id: u32, state: JobState, max: Duration) {
        let t0 = Instant::now();
        while engine.store().jobs.get(&id).map(|j| j.state) != Some(state) {
            assert!(t0.elapsed() < max, "job {id} never reached {state:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn print_job_completes() {
        let r = rig(cfg(), "ok");
        let id = r.engine.create_job(opts("hello"), Some(raster())).unwrap();
        let j = wait_terminal(&r.engine, id, Duration::from_secs(10)).await;
        assert_eq!(j.state, JobState::Completed);
        assert_eq!(j.reasons, vec!["job-completed-successfully"]);
        assert_eq!(j.impressions, 1);
        assert_eq!(j.impressions_completed, 1);
        let s = r.engine.store();
        assert_eq!(s.view.state, PrinterState::Idle);
        assert!(s.view.reasons.is_empty());
        assert!(s.view.sticky_reasons.is_empty());
        assert!(s
            .view
            .last_model
            .as_deref()
            .unwrap_or("")
            .starts_with("MXW01"));
    }

    #[tokio::test]
    async fn printer_off_gives_up_then_ttl_clears() {
        let r = rig(cfg(), "off");
        let id = r.engine.create_job(opts("x"), Some(raster())).unwrap();
        // while retrying: processing + connecting-to-device + kid message
        wait_state(&r.engine, id, JobState::Processing, Duration::from_secs(5)).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        {
            let s = r.engine.store();
            assert_eq!(s.view.state, PrinterState::Processing);
            assert!(s.view.reasons.contains(&"connecting-to-device"));
            assert!(s.view.message.contains("Cat printer not found"));
        }
        let j = wait_terminal(&r.engine, id, Duration::from_secs(20)).await;
        assert_eq!(j.state, JobState::Aborted);
        assert_eq!(j.reasons, vec!["service-off-line"]);
        assert!(j.message.contains("job"), "{}", j.message);
        {
            let s = r.engine.store();
            // live IPP reasons are clean; the sticky "why" is only for /health
            assert_eq!(s.view.state, PrinterState::Idle);
            assert!(s.view.reasons.is_empty());
            assert_eq!(s.view.sticky_reasons, vec!["offline-report"]);
            assert!(s.health().reasons.contains(&"offline-report"));
            assert!(!s.view.message.is_empty());
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        r.engine.store().tick();
        let s = r.engine.store();
        assert!(s.view.sticky_reasons.is_empty());
        assert!(s.view.message.is_empty());
    }

    #[tokio::test]
    async fn flaky_printer_retries_then_prints() {
        let r = rig(
            EngineConfig {
                printer_wait: Duration::from_secs(60),
                ..cfg()
            },
            "flaky:1",
        );
        let id = r.engine.create_job(opts("x"), Some(raster())).unwrap();
        let j = wait_terminal(&r.engine, id, Duration::from_secs(20)).await;
        assert_eq!(j.state, JobState::Completed);
    }

    #[tokio::test]
    async fn cancel_pending_job_is_immediate_and_processing_job_stops_cooperatively() {
        let r = rig(cfg(), "slow");
        let a = r.engine.create_job(opts("a"), Some(raster())).unwrap();
        let b = r.engine.create_job(opts("b"), Some(raster())).unwrap();
        wait_state(&r.engine, a, JobState::Processing, Duration::from_secs(5)).await;
        // b is pending: cancel is immediate
        assert_eq!(r.engine.cancel(b), CancelOutcome::Canceled);
        let jb = wait_terminal(&r.engine, b, Duration::from_secs(1)).await;
        assert_eq!(jb.state, JobState::Canceled);
        assert_eq!(jb.reasons, vec!["job-canceled-by-user"]);
        assert_eq!(r.engine.cancel(b), CancelOutcome::AlreadyTerminal);
        assert_eq!(r.engine.cancel(999), CancelOutcome::NotFound);
        // a is printing (slow = 15 s): cancel → the driver itself returns Cancelled
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(r.engine.cancel(a), CancelOutcome::Canceled);
        let ja = wait_terminal(&r.engine, a, Duration::from_secs(5)).await;
        assert_eq!(ja.state, JobState::Canceled);
        assert!(
            r.dir.path().join(format!("job-{a}-cancelled.txt")).exists(),
            "driver did not observe the cancel token itself"
        );
        assert_eq!(r.engine.store().view.state, PrinterState::Idle);
    }

    #[tokio::test]
    async fn shutdown_aborts_current_and_pending_and_starts_nothing_new() {
        let r = rig(cfg(), "slow");
        let mut ids = vec![];
        for i in 0..6 {
            ids.push(
                r.engine
                    .create_job(opts(&format!("j{i}")), Some(raster()))
                    .unwrap(),
            );
        }
        wait_state(
            &r.engine,
            ids[0],
            JobState::Processing,
            Duration::from_secs(5),
        )
        .await;
        r.engine.begin_shutdown();
        r.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(10), r.worker)
            .await
            .expect("worker stops")
            .unwrap();
        {
            let s = r.engine.store();
            for id in &ids {
                let j = &s.jobs[id];
                assert_eq!(j.state, JobState::Aborted, "job {id}");
                assert!(j.message.contains("print again"), "{}", j.message);
                assert!(
                    j.processing_uptime.is_none() || *id == ids[0],
                    "job {id} was started"
                );
            }
            assert!(!s.view.accepting);
        }
        assert!(matches!(
            r.engine.create_job(opts("late"), Some(raster())),
            Err(SubmitError::NotAccepting)
        ));
    }

    #[tokio::test]
    async fn stale_create_job_aborted_by_tick_but_touch_keeps_it() {
        let r = rig(cfg(), "ok");
        let a = r.engine.create_job(opts("a"), None).unwrap();
        let b = r.engine.create_job(opts("b"), None).unwrap();
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_millis(120)).await;
            r.engine.touch(b);
            r.engine.store().tick();
        }
        assert_eq!(r.engine.store().jobs[&a].state, JobState::Aborted);
        assert_eq!(r.engine.store().jobs[&b].state, JobState::Pending);
        r.engine.add_document(b, raster()).unwrap();
        let j = wait_terminal(&r.engine, b, Duration::from_secs(10)).await;
        assert_eq!(j.state, JobState::Completed);
    }

    #[tokio::test]
    async fn prune_keeps_100_terminal_and_5_previews_and_queue_full_is_busy() {
        let r = rig(
            EngineConfig {
                queue_max: 2,
                ..cfg()
            },
            "slow",
        );
        let a = r.engine.create_job(opts("a"), Some(raster())).unwrap();
        let _b = r.engine.create_job(opts("b"), Some(raster())).unwrap();
        assert!(matches!(
            r.engine.create_job(opts("c"), Some(raster())),
            Err(SubmitError::Busy(2))
        ));
        assert_eq!(r.engine.cancel(a), CancelOutcome::Canceled);
        // pruning: fill with cancelled pending jobs
        drop(r);
        let r = rig(
            EngineConfig {
                queue_max: 500,
                ..cfg()
            },
            "off",
        );
        let mut last = 0;
        for i in 0..130 {
            let id = r
                .engine
                .create_job(opts(&format!("p{i}")), Some(raster()))
                .unwrap();
            r.engine.cancel(id);
            last = id;
        }
        let s = r.engine.store();
        let terminal = s.jobs.values().filter(|j| j.state.is_terminal()).count();
        assert!(terminal <= KEEP_TERMINAL + 1, "{terminal}");
        assert!(s.jobs.contains_key(&last));
        assert!(s.jobs.values().filter(|j| j.preview.is_some()).count() <= KEEP_PREVIEWS);
    }

    #[tokio::test]
    async fn identify_is_coalesced() {
        let r = rig(cfg(), "slow");
        let a = r.engine.create_job(opts("a"), Some(raster())).unwrap();
        wait_state(&r.engine, a, JobState::Processing, Duration::from_secs(5)).await;
        for _ in 0..12 {
            r.engine.identify();
        }
        // a burst never fills the work channel: new jobs still queue instead of aborting
        let b = r.engine.create_job(opts("b"), Some(raster())).unwrap();
        assert_eq!(r.engine.store().jobs[&b].state, JobState::Pending);
        assert_eq!(r.engine.store().identify_requests, 12);
        assert!(r.engine.store().identify_pending);
    }

    #[tokio::test]
    async fn panicking_job_does_not_kill_worker() {
        let r = rig(cfg(), "panic");
        let a = r.engine.create_job(opts("boom"), Some(raster())).unwrap();
        let j = wait_terminal(&r.engine, a, Duration::from_secs(10)).await;
        assert_eq!(j.state, JobState::Aborted);
        assert!(j.message.contains("hiccup"), "{}", j.message);
        assert!(!r.worker.is_finished());
        std::fs::write(r.dir.path().join("state"), "ok").unwrap();
        let b = r.engine.create_job(opts("after"), Some(raster())).unwrap();
        let j = wait_terminal(&r.engine, b, Duration::from_secs(10)).await;
        assert_eq!(j.state, JobState::Completed);
        assert_eq!(r.engine.store().view.state, PrinterState::Idle);
    }

    #[tokio::test]
    async fn job_ids_are_monotonic_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let mk = || {
            rig(
                EngineConfig {
                    state_dir: Some(dir.path().to_path_buf()),
                    ..cfg()
                },
                "ok",
            )
        };
        let r1 = mk();
        let a = r1.engine.create_job(opts("a"), Some(raster())).unwrap();
        let b = r1.engine.create_job(opts("b"), Some(raster())).unwrap();
        assert!(a > 1_000_000, "time-based ids: {a}");
        assert_eq!(b, a + 1);
        drop(r1);
        let r2 = mk();
        let c = r2.engine.create_job(opts("c"), Some(raster())).unwrap();
        assert!(c > b, "{c} > {b}");
        // and without a state dir the ids still never go backwards in practice
        let r3 = rig(cfg(), "ok");
        let d = r3.engine.create_job(opts("d"), Some(raster())).unwrap();
        assert!(d >= a);
    }

    #[derive(Clone)]
    struct Cap(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Cap {
        type Writer = CapW;
        fn make_writer(&'a self) -> Self::Writer {
            CapW(self.0.clone())
        }
    }
    struct CapW(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for CapW {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn over_max_pages_is_aborted_not_completed() {
        let cap = Cap(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
        let logged = cap.0.clone();
        let sub = tracing_subscriber::fmt()
            .with_writer(cap)
            .with_max_level(tracing::Level::ERROR)
            .with_target(false)
            .finish();
        let _guard = tracing::subscriber::set_default(sub);
        let r = rig(
            EngineConfig {
                limits: crate::raster::Limits {
                    max_pages: 0,
                    ..crate::raster::Limits::default()
                },
                ..cfg()
            },
            "ok",
        );
        let id = r.engine.create_job(opts("long"), Some(raster())).unwrap();
        let j = wait_terminal(&r.engine, id, Duration::from_secs(10)).await;
        let text = String::from_utf8_lossy(&logged.lock().unwrap()).into_owned();
        assert!(
            text.contains("refused job"),
            "error-level refuse log missing: {text}"
        );
        assert_eq!(j.state, JobState::Aborted);
        assert!(
            !j.reasons.contains(&"job-completed-successfully"),
            "{:?}",
            j.reasons
        );
        assert_eq!(j.reasons, vec!["document-format-error"]);
        assert!(
            j.message.contains("page") || j.message.contains("pages"),
            "{}",
            j.message
        );
        assert!(
            !r.dir.path().join(format!("job-{id}.json")).exists(),
            "refused job must not write a success artifact"
        );
    }

    #[tokio::test]
    async fn unwritable_fake_dest_fails_job() {
        let cap = Cap(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
        let logged = cap.0.clone();
        let sub = tracing_subscriber::fmt()
            .with_writer(cap)
            .with_max_level(tracing::Level::ERROR)
            .with_target(false)
            .finish();
        let _guard = tracing::subscriber::set_default(sub);
        let dest = tempfile::tempdir().unwrap();
        let not_a_dir = dest.path().join("blocked");
        std::fs::write(&not_a_dir, b"not a directory").unwrap();
        let printer = Printer::Fake(FakePrinter::new(&not_a_dir).unwrap());
        let shutdown = CancellationToken::new();
        let (engine, _worker) = Engine::start(cfg(), printer, shutdown);
        let id = engine.create_job(opts("nope"), Some(raster())).unwrap();
        let j = wait_terminal(&engine, id, Duration::from_secs(10)).await;
        let text = String::from_utf8_lossy(&logged.lock().unwrap()).into_owned();
        assert!(
            text.contains("refused job") && text.contains("fake printer could not write"),
            "error-level write failure missing: {text}"
        );
        assert_eq!(j.state, JobState::Aborted);
        assert!(
            !j.reasons.contains(&"job-completed-successfully"),
            "{:?}",
            j.reasons
        );
        let pngs: Vec<_> = std::fs::read_dir(dest.path())
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "png"))
            .collect();
        assert!(pngs.is_empty(), "no success PNG on an unwritable dest");
    }
}
