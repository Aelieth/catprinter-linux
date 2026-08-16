//! Job store + the single worker. Every accepted job is rendered (stage 1) then printed with a
//! retry loop bounded by `printer_wait`; the `PrinterView` is what Get-Printer-Attributes reports.
//!
//! Invariants (CUPS `backend/ipp.c`): failures never use `aborted-by-system` / `job-canceled-*`
//! reasons (the backend would report the job as completed); printer-state is never `stopped`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::ipp::options::JobOptions;
use crate::printer::{PreparedJob, PrintError, Printer, Progress};
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
    /// When the sticky "gave up" reasons were set (cleared after ERROR_TTL or the next success).
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

const ERROR_TTL: Duration = Duration::from_secs(600);
const KEEP_TERMINAL: usize = 100;
const KEEP_PREVIEWS: usize = 5;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub printer_wait: Duration,
    pub queue_max: usize,
    pub max_document_bytes: usize,
    pub max_copies: u32,
    pub render: RenderOptions,
    pub limits: Limits,
    /// Grace given to the current job on shutdown before it is cancelled.
    pub shutdown_grace: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            printer_wait: Duration::from_secs(600),
            queue_max: 16,
            max_document_bytes: 64 * 1024 * 1024,
            max_copies: 10,
            render: RenderOptions::default(),
            limits: Limits::default(),
            shutdown_grace: Duration::from_secs(15),
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
    /// Live queue depth (pending + processing).
    pub identify_requests: u32,
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
    /// Expire the sticky failure state after ERROR_TTL.
    pub fn tick(&mut self) {
        if let Some(t) = self.view.sticky_since {
            if t.elapsed() > ERROR_TTL && self.view.current.is_none() {
                let up = self.uptime();
                self.view.set(up, PrinterState::Idle, &[], "");
                self.view.sticky_since = None;
            }
        }
        // Create-Job that never got a document
        let stale: Vec<u32> = self
            .jobs
            .values()
            .filter(|j| {
                j.awaiting_document
                    && !j.state.is_terminal()
                    && j.created_at.elapsed() > Duration::from_secs(120)
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
        let store = Arc::new(Mutex::new(Store {
            jobs: BTreeMap::new(),
            next_id: 1,
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
                sticky_since: None,
            },
            started: Instant::now(),
            cfg,
            jobs_total: 0,
            config_changed_uptime: 1,
            identify_requests: 0,
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
            return Err(SubmitError::TooLarge(doc.len()));
        }
        if doc.is_empty() {
            return Err(SubmitError::Empty);
        }
        match DocFormat::sniff(doc) {
            DocFormat::Unknown => Err(SubmitError::BadFormat),
            f => Ok(f),
        }
    }

    /// Print-Job (doc = Some) or Create-Job (doc = None). Returns the job id.
    pub fn create_job(&self, opts: JobOptions, doc: Option<Bytes>) -> Result<u32, SubmitError> {
        let format = match &doc {
            Some(d) => self.validate_doc(d)?,
            None => DocFormat::Unknown,
        };
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
            s.jobs_total += 1;
            let up = s.uptime();
            let k_octets = doc
                .as_ref()
                .map(|d| d.len().div_ceil(1024) as u32)
                .unwrap_or(0);
            let awaiting = doc.is_none();
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
                    model: None,
                },
            );
            id
        };
        if !self.store().jobs[&id].awaiting_document {
            self.enqueue(id);
        }
        Ok(id)
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
        self.store().identify_requests += 1;
        let _ = self.tx.try_send(Work::Identify);
    }

    /// Ask for a graceful stop: no new jobs, current job gets a grace period, pending jobs aborted.
    pub fn begin_shutdown(&self) {
        let mut s = self.store();
        s.view.accepting = false;
        let up = s.uptime();
        s.view.set(
            up,
            PrinterState::Idle,
            &["shutdown"],
            "printer service is stopping",
        );
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
        let work = tokio::select! {
            w = rx.recv() => match w { Some(w) => w, None => break },
            _ = shutdown.cancelled() => break,
        };
        match work {
            Work::Identify => {
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
                run_job(&store, &mut printer, id, &shutdown).await;
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
            let strip = render::prepare(pages, &render_opts).map_err(|e| e.to_string())?;
            Ok((strip, n))
        }),
    )
    .await;
    let (strip, pages) = match prep {
        Ok(Ok(Ok(v))) => v,
        Ok(Ok(Err(msg))) => {
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
    let hard_deadline = deadline + Duration::from_secs(900);
    let mut attempt: u32 = 0;
    let mut last_err: Option<PrintError> = None;
    let outcome: Result<crate::printer::PrintReport, PrintError> = loop {
        attempt += 1;
        let store2 = store.clone();
        let mut on_progress = move |p: Progress| {
            let mut s = store2.lock().unwrap_or_else(|e| e.into_inner());
            let up = s.uptime();
            let msg = match p.phase {
                crate::printer::Phase::Printing => format!("Printing job {id} — {}%", p.percent),
                _ => p.message.clone(),
            };
            s.view.set(up, PrinterState::Processing, &[], msg);
            if let Some(j) = s.jobs.get_mut(&id) {
                j.impressions_completed = (j.impressions as u64 * p.percent as u64 / 100) as u32;
                j.message = p.message;
            }
        };
        let res = tokio::select! {
            r = printer.print(&job, &cancel, &mut on_progress) => r,
            _ = cancel.cancelled() => Err(PrintError::Cancelled),
            _ = shutdown_grace(shutdown, cfg.shutdown_grace) => Err(PrintError::Cancelled),
        };
        match res {
            Ok(rep) => break Ok(rep),
            Err(PrintError::Cancelled) => break Err(PrintError::Cancelled),
            Err(e) => {
                let now = Instant::now();
                let retry = e.retryable() && now < deadline && now < hard_deadline;
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
                tracing::info!(job = id, attempt, "print attempt failed: {e}; retrying");
                last_err = Some(e);
                let backoff = Duration::from_secs(match attempt {
                    1 => 3,
                    2 => 5,
                    3 => 10,
                    _ => 15,
                });
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {},
                    _ = cancel.cancelled() => break Err(PrintError::Cancelled),
                    _ = shutdown.cancelled() => break Err(PrintError::Cancelled),
                }
            }
        }
    };
    let _ = last_err;

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
            s.view.sticky_since = None;
        }
        Err(PrintError::Cancelled) => {
            let by_shutdown = shutdown.is_cancelled() && !cancel.is_cancelled();
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
            tracing::warn!(job = id, "job failed: {e}");
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
            s.view.set(
                up,
                PrinterState::Idle,
                sticky_reasons,
                if sticky_reasons.is_empty() {
                    String::new()
                } else {
                    msg
                },
            );
            s.view.sticky_since = if sticky_reasons.is_empty() {
                None
            } else {
                Some(Instant::now())
            };
        }
    }
}

/// Resolves `grace` after shutdown was requested (gives the current job time to finish naturally).
async fn shutdown_grace(shutdown: &CancellationToken, grace: Duration) {
    shutdown.cancelled().await;
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
        Health {
            version: crate::VERSION,
            uptime_s: self.started.elapsed().as_secs(),
            printer_state: match self.view.state {
                PrinterState::Idle => "idle",
                PrinterState::Processing => "processing",
                PrinterState::Stopped => "stopped",
            },
            reasons: self.view.reasons.clone(),
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
