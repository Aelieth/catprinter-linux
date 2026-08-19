//! IPP Everywhere printer: request validation, operations, printer/job attribute sets.
//! Oracle: /usr/share/cups/ipptool/{ipp-1.1,ipp-2.0,ipp-everywhere}.test and CUPS's `ipp` backend.

pub mod codec;
pub mod media;
pub mod options;

use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use bytes::Bytes;

use crate::engine::{CancelOutcome, Engine, Job, JobState, SubmitError};
use crate::ipp::codec::*;
use crate::ipp::options::JobOptions;

pub const OP_PRINT_JOB: u16 = 0x0002;
pub const OP_VALIDATE_JOB: u16 = 0x0004;
pub const OP_CREATE_JOB: u16 = 0x0005;
pub const OP_SEND_DOCUMENT: u16 = 0x0006;
pub const OP_CANCEL_JOB: u16 = 0x0008;
pub const OP_GET_JOB_ATTRIBUTES: u16 = 0x0009;
pub const OP_GET_JOBS: u16 = 0x000A;
pub const OP_GET_PRINTER_ATTRIBUTES: u16 = 0x000B;
pub const OP_CANCEL_MY_JOBS: u16 = 0x0039;
pub const OP_CLOSE_JOB: u16 = 0x003B;
pub const OP_IDENTIFY_PRINTER: u16 = 0x003C;

pub const OPERATIONS: [i32; 11] = [2, 4, 5, 6, 8, 9, 0xA, 0xB, 0x39, 0x3B, 0x3C];
pub const DOCUMENT_FORMATS: [&str; 4] = [
    "image/pwg-raster",
    "image/jpeg",
    "image/png",
    "application/octet-stream",
];
pub const COMPRESSIONS: [&str; 3] = ["none", "gzip", "deflate"];

/// Static-ish printer identity + shared runtime bits.
#[derive(Clone)]
pub struct PrinterConfig {
    pub name: String,
    pub info: String,
    pub location: String,
    pub make_model: String,
    pub model_label: String,
    pub host: String,
    pub port: u16,
    /// urn:uuid:… (may be replaced when the CUPS queue's uuid is adopted)
    pub uuid: Arc<RwLock<String>>,
    pub resolutions: Vec<u32>,
    pub max_document_kb: i32,
    pub max_copies: i32,
    pub started: SystemTime,
}

/// Wrap a bare IPv6 address in brackets for a URI authority.
fn bracket_host(h: &str) -> String {
    if h.contains(':') && !h.starts_with('[') {
        format!("[{h}]")
    } else {
        h.to_string()
    }
}

impl PrinterConfig {
    pub fn printer_uri(&self) -> String {
        format!("ipp://{}:{}/ipp/print", bracket_host(&self.host), self.port)
    }
    pub fn http_base(&self) -> String {
        format!("http://{}:{}", bracket_host(&self.host), self.port)
    }
    pub fn uuid(&self) -> String {
        self.uuid.read().map(|u| u.clone()).unwrap_or_default()
    }
}

pub struct IppService {
    pub engine: Engine,
    pub cfg: PrinterConfig,
}

/// What the HTTP layer needs back.
pub struct IppOutcome {
    pub body: Bytes,
    /// HTTP status (400 for unparseable requests, else 200).
    pub http_status: u16,
}

impl IppService {
    pub fn new(engine: Engine, cfg: PrinterConfig) -> Self {
        IppService { engine, cfg }
    }

    /// Handle one IPP request body; always returns an IPP response (or a 400 for garbage).
    ///
    /// Panics are contained here: a panic while *parsing* (the `ipp` crate slices some
    /// language-tagged values unchecked) is a malformed request → `client-error-bad-request`; a
    /// panic while *dispatching* is our bug → `server-error-busy`, which the CUPS backend merely
    /// retries after a pause (it would STOP the queue on `not-found`, downgrade the protocol on
    /// `bad-request`, or mark the job completed on `internal-error`).
    pub fn handle(&self, body: Bytes) -> IppOutcome {
        use std::panic::{catch_unwind, AssertUnwindSafe};
        let peek = peek_header(&body);
        let (ver, _op, id) = peek.unwrap_or((0x0101, 0, 1));
        let ver = if matches!(ver >> 8, 1 | 2) {
            ver
        } else {
            0x0101
        };
        let req = match parse_contained(body) {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("bad IPP request: {e}");
                let r = Resp::new(ver, Status::ClientErrorBadRequest, id)
                    .status_message("malformed IPP request");
                return IppOutcome {
                    body: r.into_bytes(),
                    http_status: if peek.is_some() { 200 } else { 400 },
                };
            }
        };
        let resp = match catch_unwind(AssertUnwindSafe(|| self.dispatch(&req))) {
            Ok(r) => r,
            Err(_) => {
                tracing::error!(
                    op = format_args!("0x{:04x}", req.op),
                    "IPP handler panicked (contained; please report)"
                );
                Resp::new(ver, Status::ServerErrorBusy, id)
                    .status_message("printer service hiccup — please retry")
            }
        };
        IppOutcome {
            body: resp.into_bytes(),
            http_status: 200,
        }
    }

    fn dispatch(&self, req: &Request) -> Resp {
        let ver = req.version;
        let id = req.request_id;
        // RFC 8011 §4.1.8: version we do not support
        if !matches!(req.version_major(), 1 | 2) {
            return Resp::new(0x0101, Status::ServerErrorVersionNotSupported, id)
                .status_message("IPP version not supported");
        }
        // §4.1.1 request-id must be > 0
        if id == 0 {
            return Resp::new(ver, Status::ClientErrorBadRequest, id)
                .status_message("request-id must be positive");
        }
        // §4.1.4 operation attributes: group first, charset then natural-language first
        if req.first_group_tag() != Some(Group::OperationAttributes) {
            return Resp::new(ver, Status::ClientErrorBadRequest, id)
                .status_message("missing operation attributes");
        }
        let names = req.operation_attr_names();
        if names.first().map(String::as_str) != Some("attributes-charset")
            || names.get(1).map(String::as_str) != Some("attributes-natural-language")
        {
            return Resp::new(ver, Status::ClientErrorBadRequest, id).status_message(
                "attributes-charset and attributes-natural-language must come first",
            );
        }
        // target: printer-uri (or job-uri for job ops)
        let has_printer_uri = req
            .get(Some(Group::OperationAttributes), "printer-uri")
            .is_some();
        let has_job_uri = req
            .get(Some(Group::OperationAttributes), "job-uri")
            .is_some();
        if !has_printer_uri && !has_job_uri {
            return Resp::new(ver, Status::ClientErrorBadRequest, id)
                .status_message("missing printer-uri");
        }
        match req.op {
            OP_GET_PRINTER_ATTRIBUTES => self.get_printer_attributes(req),
            OP_VALIDATE_JOB => self.validate_job(req),
            OP_PRINT_JOB => self.print_job(req),
            OP_CREATE_JOB => self.create_job(req),
            OP_SEND_DOCUMENT => self.send_document(req),
            OP_CANCEL_JOB => self.cancel_job(req),
            OP_CANCEL_MY_JOBS => self.cancel_my_jobs(req),
            OP_CLOSE_JOB => self.close_job(req),
            OP_GET_JOB_ATTRIBUTES => self.get_job_attributes(req),
            OP_GET_JOBS => self.get_jobs(req),
            OP_IDENTIFY_PRINTER => self.identify_printer(req),
            op => {
                tracing::debug!("unsupported operation 0x{op:04x}");
                Resp::new(ver, Status::ServerErrorOperationNotSupported, id)
                    .status_message("operation not supported")
            }
        }
    }

    // ------------------------------------------------------------------ helpers

    fn job_id_from(&self, req: &Request) -> Option<u32> {
        if let Some(i) = req.get_int(Some(Group::OperationAttributes), "job-id") {
            return u32::try_from(i).ok();
        }
        let uri = req.get_str(Some(Group::OperationAttributes), "job-uri")?;
        uri.rsplit('/').next()?.parse().ok()
    }

    fn requested(req: &Request) -> Vec<String> {
        req.get_strs(Some(Group::OperationAttributes), "requested-attributes")
    }

    /// Document checks shared by Print-Job / Validate-Job / Send-Document.
    fn check_document_format(&self, req: &Request) -> Result<(), Resp> {
        if let Some(f) = req.get_str(Some(Group::OperationAttributes), "document-format") {
            if !DOCUMENT_FORMATS.iter().any(|d| d.eq_ignore_ascii_case(&f)) {
                let mut r = Resp::new(
                    req.version,
                    Status::ClientErrorDocumentFormatNotSupported,
                    req.request_id,
                )
                .status_message("document format not supported");
                r.add(Group::UnsupportedAttributes, "document-format", v_mime(&f));
                return Err(r);
            }
        }
        if let Some(c) = req.get_str(Some(Group::OperationAttributes), "compression") {
            if !COMPRESSIONS.iter().any(|d| d.eq_ignore_ascii_case(&c)) {
                let mut r = Resp::new(
                    req.version,
                    Status::ClientErrorCompressionNotSupported,
                    req.request_id,
                )
                .status_message("compression not supported");
                r.add(Group::UnsupportedAttributes, "compression", v_kw(&c));
                return Err(r);
            }
        }
        Ok(())
    }

    /// Decompress the payload if `compression` says so.
    fn document_bytes(&self, req: &Request) -> Result<Bytes, Resp> {
        use std::io::Read;
        let comp = req
            .get_str(Some(Group::OperationAttributes), "compression")
            .unwrap_or_else(|| "none".into());
        // The CUPS `ipp` backend gzips *every* raster job once gzip is advertised, so this is the
        // hot path — and an unbounded inflate of a 64 MB body is a multi-GB allocation. Inflate at
        // most max_document_bytes + 1 and reject anything larger the same way an oversize body is
        // rejected (the backend cancels the job cleanly on request-value-too-long).
        let max = self.engine.store().cfg.max_document_bytes;
        let too_large = || {
            Resp::new(
                req.version,
                Status::ClientErrorRequestValueTooLong,
                req.request_id,
            )
            .status_message("document too large for this printer")
        };
        let comp_err = |what: &str, e: std::io::Error| {
            Resp::new(
                req.version,
                Status::ClientErrorCompressionError,
                req.request_id,
            )
            .status_message(&format!("{what}: {e}"))
        };
        let inflate = |mut d: Box<dyn Read + '_>| -> Result<Vec<u8>, std::io::Error> {
            let mut v = Vec::new();
            d.by_ref().take(max as u64 + 1).read_to_end(&mut v)?;
            Ok(v)
        };
        let out = match comp.to_ascii_lowercase().as_str() {
            "none" => req.payload.clone(),
            "gzip" => {
                let v = inflate(Box::new(flate2::read::MultiGzDecoder::new(
                    &req.payload[..],
                )))
                .map_err(|e| comp_err("gzip", e))?;
                if v.len() > max {
                    return Err(too_large());
                }
                Bytes::from(v)
            }
            "deflate" => {
                let v = match inflate(Box::new(flate2::read::ZlibDecoder::new(&req.payload[..]))) {
                    Ok(v) => v,
                    // some clients send raw deflate
                    Err(_) => inflate(Box::new(flate2::read::DeflateDecoder::new(
                        &req.payload[..],
                    )))
                    .map_err(|e| comp_err("deflate", e))?,
                };
                if v.len() > max {
                    return Err(too_large());
                }
                Bytes::from(v)
            }
            other => {
                return Err(Resp::new(
                    req.version,
                    Status::ClientErrorCompressionNotSupported,
                    req.request_id,
                )
                .status_message(&format!("compression {other} not supported")))
            }
        };
        Ok(out)
    }

    fn submit_error(&self, req: &Request, e: SubmitError) -> Resp {
        tracing::error!("refused job: {e}");
        let (status, msg) = match e {
            SubmitError::Busy(_) | SubmitError::NotAccepting => {
                (Status::ServerErrorBusy, "printer is busy, try again")
            }
            SubmitError::TooLarge(_) => {
                (Status::ClientErrorRequestValueTooLong, "document too large")
            }
            SubmitError::BadFormat => (
                Status::ClientErrorDocumentFormatNotSupported,
                "document format not supported",
            ),
            SubmitError::Empty => (Status::ClientErrorBadRequest, "empty document"),
            SubmitError::NotFound(_) => (Status::ClientErrorNotFound, "job not found"),
            SubmitError::HasDocument(_) => (
                Status::ServerErrorMultipleDocumentJobsNotSupported,
                "job already has a document",
            ),
        };
        Resp::new(req.version, status, req.request_id).status_message(msg)
    }

    fn job_response(&self, req: &Request, id: u32) -> Resp {
        let mut r = Resp::new(req.version, Status::SuccessfulOk, req.request_id);
        let s = self.engine.store();
        if let Some(j) = s.jobs.get(&id) {
            let up = s.uptime();
            let mut g = r.push_group(Group::JobAttributes);
            self.write_job_attrs(&mut g, j, up, &s.view, None);
        }
        r
    }

    // ------------------------------------------------------------------ operations

    fn validate_job(&self, req: &Request) -> Resp {
        if let Err(r) = self.check_document_format(req) {
            return r;
        }
        Resp::new(req.version, Status::SuccessfulOk, req.request_id)
    }

    fn print_job(&self, req: &Request) -> Resp {
        if let Err(r) = self.check_document_format(req) {
            return r;
        }
        let doc = match self.document_bytes(req) {
            Ok(d) => d,
            Err(r) => return r,
        };
        let opts = JobOptions::from_request(req, self.cfg.max_copies as u32);
        match self.engine.create_job(opts, Some(doc)) {
            Ok(id) => self.job_response(req, id),
            Err(e) => self.submit_error(req, e),
        }
    }

    fn create_job(&self, req: &Request) -> Resp {
        let opts = JobOptions::from_request(req, self.cfg.max_copies as u32);
        match self.engine.create_job(opts, None) {
            Ok(id) => self.job_response(req, id),
            Err(e) => self.submit_error(req, e),
        }
    }

    fn send_document(&self, req: &Request) -> Resp {
        let Some(id) = self.job_id_from(req) else {
            return Resp::new(req.version, Status::ClientErrorBadRequest, req.request_id)
                .status_message("missing job-id");
        };
        let Some(last) = req.get_bool(Some(Group::OperationAttributes), "last-document") else {
            return Resp::new(req.version, Status::ClientErrorBadRequest, req.request_id)
                .status_message("missing last-document");
        };
        if let Err(r) = self.check_document_format(req) {
            return r;
        }
        let exists = {
            let s = self.engine.store();
            s.jobs
                .get(&id)
                .map(|j| (j.state.is_terminal(), j.awaiting_document))
        };
        match exists {
            None => {
                return Resp::new(req.version, Status::ClientErrorNotFound, req.request_id)
                    .status_message("job not found")
            }
            Some((true, _)) => {
                return Resp::new(req.version, Status::ClientErrorNotPossible, req.request_id)
                    .status_message("job is already complete")
            }
            Some((false, false)) if !req.payload.is_empty() => {
                return Resp::new(
                    req.version,
                    Status::ServerErrorMultipleDocumentJobsNotSupported,
                    req.request_id,
                )
                .status_message("only one document per job")
            }
            _ => {}
        }
        if req.payload.is_empty() {
            // Empty last-document closes the job; a job with no document is aborted.
            if last {
                let mut s = self.engine.store();
                if let Some(j) = s.jobs.get(&id) {
                    if j.awaiting_document {
                        let _ = j;
                        s.jobs.get_mut(&id).unwrap().awaiting_document = false;
                        drop(s);
                        self.engine.cancel(id);
                    }
                }
            }
            return self.job_response(req, id);
        }
        let doc = match self.document_bytes(req) {
            Ok(d) => d,
            Err(r) => return r,
        };
        match self.engine.add_document(id, doc) {
            Ok(()) => self.job_response(req, id),
            Err(e) => self.submit_error(req, e),
        }
    }

    fn cancel_job(&self, req: &Request) -> Resp {
        let Some(id) = self.job_id_from(req) else {
            return Resp::new(req.version, Status::ClientErrorBadRequest, req.request_id)
                .status_message("missing job-id");
        };
        match self.engine.cancel(id) {
            CancelOutcome::NotFound => {
                Resp::new(req.version, Status::ClientErrorNotFound, req.request_id)
                    .status_message("job not found")
            }
            CancelOutcome::AlreadyTerminal => {
                Resp::new(req.version, Status::ClientErrorNotPossible, req.request_id)
                    .status_message("job is already complete")
            }
            CancelOutcome::Canceled => Resp::new(req.version, Status::SuccessfulOk, req.request_id),
        }
    }

    fn cancel_my_jobs(&self, req: &Request) -> Resp {
        let user = req
            .get_str(Some(Group::OperationAttributes), "requesting-user-name")
            .unwrap_or_else(|| "anonymous".into());
        let ids: Vec<u32> = req
            .get_ints(Some(Group::OperationAttributes), "job-ids")
            .into_iter()
            .filter_map(|i| u32::try_from(i).ok())
            .collect();
        let _ = self.engine.cancel_my_jobs(&user, &ids);
        Resp::new(req.version, Status::SuccessfulOk, req.request_id)
    }

    fn close_job(&self, req: &Request) -> Resp {
        let Some(id) = self.job_id_from(req) else {
            return Resp::new(req.version, Status::ClientErrorBadRequest, req.request_id)
                .status_message("missing job-id");
        };
        let awaiting = {
            let s = self.engine.store();
            match s.jobs.get(&id) {
                None => {
                    return Resp::new(req.version, Status::ClientErrorNotFound, req.request_id)
                        .status_message("job not found")
                }
                Some(j) => j.awaiting_document && !j.state.is_terminal(),
            }
        };
        if awaiting {
            self.engine.cancel(id);
        }
        Resp::new(req.version, Status::SuccessfulOk, req.request_id)
    }

    fn identify_printer(&self, req: &Request) -> Resp {
        let actions = req.get_strs(None, "identify-actions");
        let bad: Vec<String> = actions
            .iter()
            .filter(|a| a.as_str() != "flash")
            .cloned()
            .collect();
        self.engine.identify();
        if !bad.is_empty() {
            // We only know how to "flash" (feed a bit of paper); substitute and say so.
            let mut r = Resp::new(
                req.version,
                Status::SuccessfulOkIgnoredOrSubstitutedAttributes,
                req.request_id,
            );
            r.add(
                Group::UnsupportedAttributes,
                "identify-actions",
                v_kws(&bad.iter().map(String::as_str).collect::<Vec<_>>()),
            );
            return r;
        }
        Resp::new(req.version, Status::SuccessfulOk, req.request_id)
    }

    fn get_job_attributes(&self, req: &Request) -> Resp {
        let Some(id) = self.job_id_from(req) else {
            return Resp::new(req.version, Status::ClientErrorBadRequest, req.request_id)
                .status_message("missing job-id");
        };
        let requested = Self::requested(req);
        let mut r = Resp::new(req.version, Status::SuccessfulOk, req.request_id);
        let s = self.engine.store();
        match s.jobs.get(&id) {
            None => Resp::new(req.version, Status::ClientErrorNotFound, req.request_id)
                .status_message("job not found"),
            Some(j) => {
                let up = s.uptime();
                let filter = job_filter(&requested, true);
                let mut g = r.push_group(Group::JobAttributes);
                self.write_job_attrs(&mut g, j, up, &s.view, filter.as_ref());
                r
            }
        }
    }

    fn get_jobs(&self, req: &Request) -> Resp {
        let which = req
            .get_str(Some(Group::OperationAttributes), "which-jobs")
            .unwrap_or_else(|| "not-completed".into());
        let limit = req
            .get_int(Some(Group::OperationAttributes), "limit")
            .filter(|l| *l > 0)
            .map(|l| l as usize);
        let my_jobs = req
            .get_bool(Some(Group::OperationAttributes), "my-jobs")
            .unwrap_or(false);
        let user = req
            .get_str(Some(Group::OperationAttributes), "requesting-user-name")
            .unwrap_or_else(|| "anonymous".into());
        let ids: Vec<u32> = req
            .get_ints(Some(Group::OperationAttributes), "job-ids")
            .into_iter()
            .filter_map(|i| u32::try_from(i).ok())
            .collect();
        let requested = Self::requested(req);
        let filter = job_filter(&requested, false);
        let mut r = Resp::new(req.version, Status::SuccessfulOk, req.request_id);
        let s = self.engine.store();
        let up = s.uptime();
        let mut jobs: Vec<&Job> = s
            .jobs
            .values()
            .filter(|j| match which.as_str() {
                "completed" => j.state.is_terminal(),
                "aborted" => j.state == JobState::Aborted,
                "canceled" => j.state == JobState::Canceled,
                "all" => true,
                _ => !j.state.is_terminal(),
            })
            .filter(|j| !my_jobs || j.opts.user == user)
            .filter(|j| ids.is_empty() || ids.contains(&j.id))
            .collect();
        if which == "completed" || which == "aborted" || which == "canceled" {
            jobs.sort_by_key(|j| std::cmp::Reverse(j.id));
        } else {
            jobs.sort_by_key(|j| j.id);
        }
        if let Some(l) = limit {
            jobs.truncate(l);
        }
        for j in jobs {
            let mut g = r.push_group(Group::JobAttributes);
            self.write_job_attrs(&mut g, j, up, &s.view, filter.as_ref());
        }
        r
    }

    /// Job attribute set. `filter` = None means "everything".
    fn write_job_attrs(
        &self,
        g: &mut GroupWriter<'_>,
        j: &Job,
        up: u32,
        view: &crate::engine::PrinterView,
        filter: Option<&Vec<String>>,
    ) {
        let want = |n: &str| filter.is_none_or(|f| f.iter().any(|x| x == n));
        let uri = format!("{}/{}", self.cfg.printer_uri(), j.id);
        macro_rules! put {
            ($n:literal, $v:expr) => {
                if want($n) {
                    g.add($n, $v);
                }
            };
        }
        put!("job-id", v_int(j.id as i32));
        put!("job-uri", v_uri(&uri));
        put!("job-printer-uri", v_uri(&self.cfg.printer_uri()));
        put!(
            "job-more-info",
            v_uri(&format!("{}/jobs/{}", self.cfg.http_base(), j.id))
        );
        put!("job-name", v_name(&j.opts.job_name));
        put!("job-originating-user-name", v_name(&j.opts.user));
        put!("job-state", v_enum(j.state.as_i32()));
        put!(
            "job-state-reasons",
            v_kws(if j.reasons.is_empty() {
                &["none"]
            } else {
                &j.reasons
            })
        );
        put!("job-state-message", v_text(&j.message));
        put!("job-printer-state-message", v_text(&view.message));
        put!(
            "job-printer-state-reasons",
            v_kws(if view.reasons.is_empty() {
                &["none"]
            } else {
                &view.reasons
            })
        );
        put!(
            "number-of-documents",
            v_int(if j.awaiting_document { 0 } else { 1 })
        );
        put!("time-at-creation", v_int(j.created_uptime as i32));
        put!(
            "time-at-processing",
            j.processing_uptime
                .map(|t| v_int(t as i32))
                .unwrap_or_else(v_no_value)
        );
        put!(
            "time-at-completed",
            j.completed_uptime
                .map(|t| v_int(t as i32))
                .unwrap_or_else(v_no_value)
        );
        put!("date-time-at-creation", v_datetime(j.created));
        put!("job-printer-up-time", v_int(up as i32));
        put!("job-k-octets", v_int(j.k_octets as i32));
        put!("job-impressions", v_int(j.impressions as i32));
        put!(
            "job-impressions-completed",
            v_int(j.impressions_completed as i32)
        );
        put!("job-media-sheets", v_int(j.impressions as i32));
        put!(
            "job-media-sheets-completed",
            v_int(j.impressions_completed as i32)
        );
        put!(
            "job-uuid",
            v_uri(&format!(
                "urn:uuid:{}",
                uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, uri.as_bytes())
            ))
        );
        put!("copies", v_int(j.opts.copies as i32));
        put!("print-quality", v_enum(j.opts.print_quality));
        put!("print-color-mode", v_kw(&j.opts.color_mode));
        if let Some(m) = &j.opts.media {
            let name = m
                .name
                .clone()
                .or_else(|| media::find_size(m.x_hmm, m.y_hmm).map(|s| s.name.to_string()))
                .unwrap_or_else(|| format!("custom_{}x{}mm", m.x_hmm / 100, m.y_hmm / 100));
            put!("media", v_kw(&name));
        }
        if let Some(f) = &j.opts.document_format {
            put!("document-format", v_mime(f));
        }
    }

    fn get_printer_attributes(&self, req: &Request) -> Resp {
        let requested = Self::requested(req);
        let want_all = requested.is_empty() || requested.iter().any(|r| r == "all");
        let want_desc = want_all || requested.iter().any(|r| r == "printer-description");
        let want_tmpl = want_all || requested.iter().any(|r| r == "job-template");
        let want_media_db = requested.iter().any(|r| r == "media-col-database");
        let named: Vec<&str> = requested
            .iter()
            .map(String::as_str)
            .filter(|r| {
                !matches!(
                    *r,
                    "all" | "printer-description" | "job-template" | "media-col-database"
                )
            })
            .collect();
        let want = |name: &str, group: Grp| -> bool {
            match group {
                Grp::Desc if want_desc => true,
                Grp::Tmpl if want_tmpl => true,
                _ => named.iter().any(|n| n.eq_ignore_ascii_case(name)),
            }
        };
        let mut r = Resp::new(req.version, Status::SuccessfulOk, req.request_id);
        let (view, up, queued, config_up) = {
            let s = self.engine.store();
            (
                s.view.clone(),
                s.uptime(),
                s.queued_count(),
                s.config_changed_uptime,
            )
        };
        let uri = self.cfg.printer_uri();
        let http = self.cfg.http_base();
        let now = SystemTime::now();
        let res: Vec<i32> = self.cfg.resolutions.iter().map(|d| *d as i32).collect();
        let res_default = *res.iter().min().unwrap_or(&203);
        let mut put = |name: &str, group: Grp, v: ipp::value::IppValue| {
            if want(name, group) {
                r.add(Group::PrinterAttributes, name, v);
            }
        };
        use Grp::*;
        // ---- identity / description
        put("printer-uri-supported", Desc, v_uri(&uri));
        put("uri-security-supported", Desc, v_kw("none"));
        put("uri-authentication-supported", Desc, v_kw("none"));
        put("printer-name", Desc, v_name(&self.cfg.name));
        put("printer-info", Desc, v_text(&self.cfg.info));
        put("printer-location", Desc, v_text(&self.cfg.location));
        put("printer-make-and-model", Desc, v_text(&self.cfg.make_model));
        put("printer-more-info", Desc, v_uri(&format!("{http}/")));
        put("printer-supply-info-uri", Desc, v_uri(&format!("{http}/")));
        put("printer-uuid", Desc, v_uri(&self.cfg.uuid()));
        put(
            "printer-device-id",
            Desc,
            v_text(&format!(
                "MFG:Cat Printer;MDL:{};CMD:PWGRaster;CLS:PRINTER;",
                self.cfg.model_label
            )),
        );
        put("printer-geo-location", Desc, v_unknown());
        put("printer-organization", Desc, v_text(""));
        put("printer-organizational-unit", Desc, v_text(""));
        put(
            "printer-icons",
            Desc,
            v_uris(&[
                &format!("{http}/icons/48.png"),
                &format!("{http}/icons/128.png"),
                &format!("{http}/icons/512.png"),
            ]),
        );
        put(
            "printer-strings-uri",
            Desc,
            v_uri(&format!("{http}/strings/en.strings")),
        );
        put("printer-strings-languages-supported", Desc, v_lang("en"));
        put("printer-state", Desc, v_enum(view.state as i32));
        put(
            "printer-state-reasons",
            Desc,
            v_kws(if view.reasons.is_empty() {
                &["none"]
            } else {
                &view.reasons
            }),
        );
        put("printer-state-message", Desc, v_text(&view.message));
        put(
            "printer-state-change-time",
            Desc,
            v_int(view.state_changed_uptime as i32),
        );
        put(
            "printer-state-change-date-time",
            Desc,
            v_datetime(view.state_changed_at),
        );
        put("printer-config-change-time", Desc, v_int(config_up as i32));
        put(
            "printer-config-change-date-time",
            Desc,
            v_datetime(self.cfg.started),
        );
        put("printer-is-accepting-jobs", Desc, v_bool(view.accepting));
        put("queued-job-count", Desc, v_int(queued as i32));
        put("printer-up-time", Desc, v_int(up as i32));
        put("printer-current-time", Desc, v_datetime(now));
        put("ipp-versions-supported", Desc, v_kws(&["1.1", "2.0"]));
        put("ipp-features-supported", Desc, v_kw("ipp-everywhere"));
        put("operations-supported", Desc, v_enums(&OPERATIONS));
        put("charset-configured", Desc, v_charset("utf-8"));
        put("charset-supported", Desc, v_charset("utf-8"));
        put("natural-language-configured", Desc, v_lang("en"));
        put("generated-natural-language-supported", Desc, v_lang("en"));
        put(
            "printer-get-attributes-supported",
            Desc,
            v_kw("document-format"),
        );
        put("pdl-override-supported", Desc, v_kw("attempted"));
        put("compression-supported", Desc, v_kws(&COMPRESSIONS));
        put("preferred-attributes-supported", Desc, v_bool(false));
        put("job-ids-supported", Desc, v_bool(true));
        put(
            "which-jobs-supported",
            Desc,
            v_kws(&["completed", "not-completed", "aborted", "canceled", "all"]),
        );
        put("multiple-document-jobs-supported", Desc, v_bool(false));
        put("multiple-operation-time-out", Desc, v_int(120));
        put(
            "multiple-operation-time-out-action",
            Desc,
            v_kw("abort-job"),
        );
        put(
            "overrides-supported",
            Desc,
            v_kws(&["document-number", "pages"]),
        );
        put(
            "job-k-octets-supported",
            Desc,
            v_range(0, self.cfg.max_document_kb),
        );
        put("pages-per-minute", Desc, v_int(1));
        put("color-supported", Desc, v_bool(false));
        put("document-format-default", Desc, v_mime("image/pwg-raster"));
        put(
            "document-format-supported",
            Desc,
            v_mimes(&DOCUMENT_FORMATS),
        );
        put("pwg-raster-document-type-supported", Desc, v_kw("sgray_8"));
        put(
            "pwg-raster-document-resolution-supported",
            Desc,
            v_array(res.iter().map(|d| v_res(*d)).collect()),
        );
        put("pwg-raster-document-sheet-back", Desc, v_kw("normal"));
        put("printer-resolution-default", Desc, v_res(res_default));
        put(
            "printer-resolution-supported",
            Desc,
            v_array(res.iter().map(|d| v_res(*d)).collect()),
        );
        put("identify-actions-default", Desc, v_kw("flash"));
        put("identify-actions-supported", Desc, v_kw("flash"));
        put(
            "job-creation-attributes-supported",
            Desc,
            v_kws(&[
                "copies",
                "document-format",
                "ipp-attribute-fidelity",
                "job-name",
                "media",
                "media-col",
                "multiple-document-handling",
                "orientation-requested",
                "output-bin",
                "print-color-mode",
                "print-content-optimize",
                "print-quality",
                "print-scaling",
                "printer-resolution",
                "sides",
                "finishings",
                "overrides",
            ]),
        );
        put(
            "media-col-supported",
            Desc,
            v_kws(&[
                "media-size",
                "media-size-name",
                "media-source",
                "media-type",
                "media-left-margin",
                "media-right-margin",
                "media-top-margin",
                "media-bottom-margin",
            ]),
        );
        put(
            "media-left-margin-supported",
            Desc,
            v_int(media::MARGIN_LEFT_RIGHT_HMM),
        );
        put(
            "media-right-margin-supported",
            Desc,
            v_int(media::MARGIN_LEFT_RIGHT_HMM),
        );
        put(
            "media-top-margin-supported",
            Desc,
            v_int(media::MARGIN_TOP_BOTTOM_HMM),
        );
        put(
            "media-bottom-margin-supported",
            Desc,
            v_int(media::MARGIN_TOP_BOTTOM_HMM),
        );
        put("media-source-supported", Desc, v_kw(media::MEDIA_SOURCE));
        put(
            "media-type-supported",
            Desc,
            v_kws(&["stationery", "labels"]),
        );
        put("media-size-supported", Desc, media::media_size_supported());
        put("media-ready", Desc, v_kw(media::TAPE.name));
        put(
            "media-col-ready",
            Desc,
            media::media_col(Some(&media::TAPE)),
        );
        // printer-supply: paper (unknown level) + battery
        let batt = view.battery.map(|b| b as i32).unwrap_or(-2);
        put(
            "printer-supply",
            Desc,
            v_array(vec![
                v_octets(b"index=1;class=supplyThatIsConsumed;type=other;unit=percent;maxcapacity=100;level=-2;colorantname=none;"),
                v_octets(format!("index=2;class=supplyThatIsConsumed;type=other;unit=percent;maxcapacity=100;level={batt};colorantname=none;").as_bytes()),
            ]),
        );
        put(
            "printer-supply-description",
            Desc,
            v_array(vec![
                v_text("Thermal paper roll"),
                v_text("Printer battery"),
            ]),
        );
        // ---- job template
        put("copies-default", Tmpl, v_int(1));
        put(
            "copies-supported",
            Tmpl,
            v_range(1, self.cfg.max_copies.max(1)),
        );
        put("finishings-default", Tmpl, v_enum(3));
        put("finishings-supported", Tmpl, v_enum(3));
        put("media-default", Tmpl, v_kw(media::TAPE.name));
        put("media-supported", Tmpl, v_kws(&media::media_names()));
        put(
            "media-col-default",
            Tmpl,
            media::media_col(Some(&media::TAPE)),
        );
        put(
            "multiple-document-handling-default",
            Tmpl,
            v_kw("single-document"),
        );
        put(
            "multiple-document-handling-supported",
            Tmpl,
            v_kw("single-document"),
        );
        put("orientation-requested-default", Tmpl, v_enum(3));
        put(
            "orientation-requested-supported",
            Tmpl,
            v_enums(&[3, 4, 5, 6]),
        );
        put("output-bin-default", Tmpl, v_kw("face-up"));
        put("output-bin-supported", Tmpl, v_kw("face-up"));
        put("page-ranges-supported", Tmpl, v_bool(false));
        put("print-color-mode-default", Tmpl, v_kw("bi-level"));
        put(
            "print-color-mode-supported",
            Tmpl,
            v_kws(&["bi-level", "monochrome"]),
        );
        // IPP Everywhere wants these; only `auto` so CUPS does not grow a second Text/Photo/Graphics
        // menu. ensure-queue also strips any leftover *print-content-optimize OpenUI from the PPD.
        put("print-content-optimize-default", Tmpl, v_kw("auto"));
        put("print-content-optimize-supported", Tmpl, v_kw("auto"));
        put("print-quality-default", Tmpl, v_enum(4));
        put("print-quality-supported", Tmpl, v_enums(&[3, 4, 5]));
        put("print-rendering-intent-default", Tmpl, v_kw("auto"));
        put("print-rendering-intent-supported", Tmpl, v_kw("auto"));
        put("print-scaling-default", Tmpl, v_kw("auto"));
        put(
            "print-scaling-supported",
            Tmpl,
            v_kws(&["auto", "fit", "fill", "none"]),
        );
        put("sides-default", Tmpl, v_kw("one-sided"));
        put("sides-supported", Tmpl, v_kw("one-sided"));
        put("number-up-default", Tmpl, v_int(1));
        put("number-up-supported", Tmpl, v_int(1));
        put("job-priority-default", Tmpl, v_int(50));
        put("job-priority-supported", Tmpl, v_int(1));
        put("job-hold-until-default", Tmpl, v_kw("no-hold"));
        put("job-hold-until-supported", Tmpl, v_kw("no-hold"));
        put("job-sheets-default", Tmpl, v_kw("none"));
        put("job-sheets-supported", Tmpl, v_kw("none"));
        if want_media_db
            || named
                .iter()
                .any(|n| n.eq_ignore_ascii_case("media-col-database"))
        {
            r.add(
                Group::PrinterAttributes,
                "media-col-database",
                media::media_col_database(),
            );
        }
        r
    }
}

#[derive(Clone, Copy)]
enum Grp {
    Desc,
    Tmpl,
}

/// Which job attributes to return. Get-Jobs default = job-id + job-uri only; Get-Job-Attributes default = all.
fn job_filter(requested: &[String], default_all: bool) -> Option<Vec<String>> {
    if requested.is_empty() {
        return if default_all {
            None
        } else {
            Some(vec!["job-id".into(), "job-uri".into()])
        };
    }
    if requested
        .iter()
        .any(|r| r == "all" || r == "job-description" || r == "job-template")
    {
        return None;
    }
    Some(requested.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineConfig;
    use crate::printer::fake::FakePrinter;
    use crate::printer::Printer;
    use tokio_util::sync::CancellationToken;

    fn service(dir: &std::path::Path) -> IppService {
        service_with(dir, EngineConfig::default())
    }

    fn service_with(dir: &std::path::Path, cfg: EngineConfig) -> IppService {
        let printer = Printer::Fake(FakePrinter::new(dir).unwrap());
        let (engine, _h) = Engine::start(cfg, printer, CancellationToken::new());
        let cfg = PrinterConfig {
            name: "CatPrinter".into(),
            info: "Cat Printer".into(),
            location: "here".into(),
            make_model: "Cat Printer MXW01".into(),
            model_label: "MXW01".into(),
            host: "127.0.0.1".into(),
            port: 8095,
            uuid: Arc::new(RwLock::new(
                "urn:uuid:12345678-1234-5678-1234-567812345678".into(),
            )),
            resolutions: vec![203],
            max_document_kb: 65536,
            max_copies: 10,
            started: SystemTime::now(),
        };
        IppService::new(engine, cfg)
    }

    fn build_req(
        version: u16,
        op: u16,
        id: i32,
        op_attrs: Vec<(&str, ipp::value::IppValue)>,
        payload: &[u8],
    ) -> Bytes {
        // Reuse Resp as a generic message builder (header op field == status field).
        let mut b = Vec::new();
        b.extend(version.to_be_bytes());
        b.extend(op.to_be_bytes());
        b.extend(id.to_be_bytes());
        b.push(0x01);
        for (n, v) in op_attrs {
            let a = ipp::attribute::IppAttribute::new(ipp::value::IppName::new_truncated(n), v);
            b.extend(a.to_bytes());
        }
        b.push(0x03);
        b.extend(payload);
        Bytes::from(b)
    }

    fn std_ops(extra: Vec<(&str, ipp::value::IppValue)>) -> Vec<(&str, ipp::value::IppValue)> {
        let mut v = vec![
            ("attributes-charset", v_charset("utf-8")),
            ("attributes-natural-language", v_lang("en")),
            ("printer-uri", v_uri("ipp://127.0.0.1:8095/ipp/print")),
        ];
        v.extend(extra);
        v
    }

    fn status_of(bytes: &Bytes) -> u16 {
        u16::from_be_bytes([bytes[2], bytes[3]])
    }

    #[tokio::test]
    async fn validation_rules() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path());
        // request-id 0
        let out = svc.handle(build_req(
            0x0101,
            OP_GET_PRINTER_ATTRIBUTES,
            0,
            std_ops(vec![]),
            b"",
        ));
        assert_eq!(status_of(&out.body), 0x0400);
        // version 0.0
        let out = svc.handle(build_req(
            0x0000,
            OP_GET_PRINTER_ATTRIBUTES,
            1,
            std_ops(vec![]),
            b"",
        ));
        assert_eq!(status_of(&out.body), 0x0503);
        // wrong order
        let out = svc.handle(build_req(
            0x0101,
            OP_GET_PRINTER_ATTRIBUTES,
            1,
            vec![
                ("attributes-natural-language", v_lang("en")),
                ("attributes-charset", v_charset("utf-8")),
                ("printer-uri", v_uri("ipp://x/ipp/print")),
            ],
            b"",
        ));
        assert_eq!(status_of(&out.body), 0x0400);
        // no printer-uri
        let out = svc.handle(build_req(
            0x0101,
            OP_GET_PRINTER_ATTRIBUTES,
            1,
            vec![
                ("attributes-charset", v_charset("utf-8")),
                ("attributes-natural-language", v_lang("en")),
            ],
            b"",
        ));
        assert_eq!(status_of(&out.body), 0x0400);
        // unknown op
        let out = svc.handle(build_req(0x0200, 0x0010, 1, std_ops(vec![]), b""));
        assert_eq!(status_of(&out.body), 0x0501);
        // good
        let out = svc.handle(build_req(
            0x0200,
            OP_GET_PRINTER_ATTRIBUTES,
            1,
            std_ops(vec![(
                "requested-attributes",
                v_kws(&["all", "media-col-database"]),
            )]),
            b"",
        ));
        assert_eq!(status_of(&out.body), 0x0000);
        let back = parse(out.body).unwrap();
        assert!(back
            .get(Some(Group::PrinterAttributes), "media-col-database")
            .is_some());
        assert_eq!(
            back.get_strs(Some(Group::PrinterAttributes), "ipp-features-supported"),
            vec!["ipp-everywhere"]
        );
        assert!(back
            .get_ints(Some(Group::PrinterAttributes), "operations-supported")
            .contains(&0x3C));
        // requested-attributes filter
        let out = svc.handle(build_req(
            0x0101,
            OP_GET_PRINTER_ATTRIBUTES,
            1,
            std_ops(vec![(
                "requested-attributes",
                v_kw("printer-uri-supported"),
            )]),
            b"",
        ));
        let back = parse(out.body).unwrap();
        assert!(back
            .get(Some(Group::PrinterAttributes), "printer-uri-supported")
            .is_some());
        assert!(back
            .get(Some(Group::PrinterAttributes), "printer-name")
            .is_none());
        assert!(back
            .get(Some(Group::PrinterAttributes), "media-col-database")
            .is_none());
    }

    #[tokio::test]
    async fn document_and_tone_attributes_for_cups() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path());
        let out = svc.handle(build_req(
            0x0200,
            OP_GET_PRINTER_ATTRIBUTES,
            1,
            std_ops(vec![(
                "requested-attributes",
                v_kws(&["all", "media-col-database"]),
            )]),
            b"",
        ));
        assert_eq!(status_of(&out.body), 0x0000);
        let back = parse(out.body).unwrap();
        let colors = back.get_strs(Some(Group::PrinterAttributes), "print-color-mode-supported");
        assert!(
            colors.iter().any(|c| c == "bi-level") && colors.iter().any(|c| c == "monochrome"),
            "{colors:?}"
        );
        assert_eq!(
            back.get_str(Some(Group::PrinterAttributes), "print-color-mode-default")
                .as_deref(),
            Some("bi-level")
        );
        let rasters = back.get_strs(
            Some(Group::PrinterAttributes),
            "pwg-raster-document-type-supported",
        );
        assert_eq!(rasters, vec!["sgray_8"], "{rasters:?}");
        assert_eq!(
            back.get_ints(Some(Group::PrinterAttributes), "print-quality-supported"),
            vec![3, 4, 5]
        );
        assert_eq!(
            back.media_database_xy(media::A4.name),
            Some((media::A4.x_hmm, media::A4.y_hmm))
        );
        assert_eq!(
            back.media_database_xy(media::LETTER.name),
            Some((media::LETTER.x_hmm, media::LETTER.y_hmm))
        );
        assert_eq!(media::A4.x_hmm, 21000);
        assert_eq!(media::LETTER.x_hmm, 21590);
        assert_eq!(media::A4.y_hmm, 29700);
        assert_eq!(media::LETTER.y_hmm, 27940);
    }

    #[tokio::test]
    async fn minidoc_a4_fake_job_keeps_default_style_and_sheet() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path());
        let pwg = include_bytes!("../../tests/fixtures/tiny-roll48.pwg");
        let mc = Coll::new()
            .add(
                "media-size",
                Coll::new()
                    .add("x-dimension", v_int(media::A4.x_hmm))
                    .add("y-dimension", v_int(media::A4.y_hmm))
                    .build(),
            )
            .add("media-size-name", v_kw(media::A4.name))
            .build();
        let mut body = Vec::new();
        body.extend(0x0200u16.to_be_bytes());
        body.extend(OP_PRINT_JOB.to_be_bytes());
        body.extend(1i32.to_be_bytes());
        body.push(0x01);
        for (n, v) in std_ops(vec![
            ("document-format", v_mime("image/pwg-raster")),
            ("job-name", v_name("Homework")),
        ]) {
            let a = ipp::attribute::IppAttribute::new(ipp::value::IppName::new_truncated(n), v);
            body.extend(a.to_bytes());
        }
        body.push(0x02);
        for (n, v) in [
            ("media-col", mc),
            ("print-quality", v_enum(4)),
            ("print-color-mode", v_kw("bi-level")),
        ] {
            let a = ipp::attribute::IppAttribute::new(ipp::value::IppName::new_truncated(n), v);
            body.extend(a.to_bytes());
        }
        body.push(0x03);
        body.extend(pwg.iter().copied());
        let out = svc.handle(bytes::Bytes::from(body));
        assert_eq!(status_of(&out.body), 0x0000, "Print-Job rejected");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        let json_path = loop {
            let found = std::fs::read_dir(dir.path()).unwrap().flatten().find(|e| {
                let n = e.file_name();
                let n = n.to_string_lossy();
                n.starts_with("job-") && n.ends_with(".json")
            });
            if found.is_some() || std::time::Instant::now() >= deadline {
                break found;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        let json_path = json_path.expect("fake printer wrote no job-*.json").path();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
        assert_eq!(json["preset"], "default", "{json}");
        assert_eq!(json["layout"], "Sheet", "{json}");
        assert_eq!(json["width"], 384, "{json}");
        assert_eq!(json["tone"], "blackwhite", "{json}");
        assert_eq!(json["mode"], "1bpp", "{json}");
    }

    #[tokio::test]
    async fn print_job_lifecycle_with_fake_printer() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path());
        // Unsupported format
        let out = svc.handle(build_req(
            0x0101,
            OP_PRINT_JOB,
            5,
            std_ops(vec![("document-format", v_mime("application/pdf"))]),
            b"%PDF-1.4",
        ));
        assert_eq!(status_of(&out.body), 0x040A);
        // Garbage bytes with octet-stream
        let out = svc.handle(build_req(
            0x0101,
            OP_PRINT_JOB,
            6,
            std_ops(vec![(
                "document-format",
                v_mime("application/octet-stream"),
            )]),
            b"hello",
        ));
        assert_eq!(status_of(&out.body), 0x040A);
        // Get-Jobs default: no jobs yet
        let out = svc.handle(build_req(0x0101, OP_GET_JOBS, 7, std_ops(vec![]), b""));
        assert_eq!(status_of(&out.body), 0x0000);
        // Create-Job + Send-Document without last-document → bad request
        let out = svc.handle(build_req(
            0x0101,
            OP_CREATE_JOB,
            8,
            std_ops(vec![("job-name", v_name("j"))]),
            b"",
        ));
        let back = parse(out.body).unwrap();
        let id = back.get_int(Some(Group::JobAttributes), "job-id").unwrap();
        assert_eq!(
            back.get_int(Some(Group::JobAttributes), "job-state"),
            Some(3)
        );
        let out = svc.handle(build_req(
            0x0101,
            OP_SEND_DOCUMENT,
            9,
            std_ops(vec![("job-id", v_int(id))]),
            b"RaS2",
        ));
        assert_eq!(status_of(&out.body), 0x0400);
        // Cancel it
        let out = svc.handle(build_req(
            0x0101,
            OP_CANCEL_JOB,
            10,
            std_ops(vec![("job-id", v_int(id))]),
            b"",
        ));
        assert_eq!(status_of(&out.body), 0x0000);
        let out = svc.handle(build_req(
            0x0101,
            OP_CANCEL_JOB,
            11,
            std_ops(vec![("job-id", v_int(id))]),
            b"",
        ));
        assert_eq!(
            status_of(&out.body),
            0x0404,
            "cancel of terminal job → not-possible"
        );
        let out = svc.handle(build_req(
            0x0101,
            OP_CANCEL_JOB,
            12,
            std_ops(vec![("job-id", v_int(999))]),
            b"",
        ));
        assert_eq!(status_of(&out.body), 0x0406);
        // Get-Jobs completed lists it, default attrs = job-id, job-uri only
        let out = svc.handle(build_req(
            0x0101,
            OP_GET_JOBS,
            13,
            std_ops(vec![("which-jobs", v_kw("completed"))]),
            b"",
        ));
        let back = parse(out.body).unwrap();
        assert_eq!(back.get_int(Some(Group::JobAttributes), "job-id"), Some(id));
        assert!(back.get(Some(Group::JobAttributes), "job-state").is_none());
        // Get-Job-Attributes → full set
        let out = svc.handle(build_req(
            0x0101,
            OP_GET_JOB_ATTRIBUTES,
            14,
            std_ops(vec![("job-id", v_int(id))]),
            b"",
        ));
        let back = parse(out.body).unwrap();
        assert_eq!(
            back.get_int(Some(Group::JobAttributes), "job-state"),
            Some(7)
        );
        assert!(back
            .get(Some(Group::JobAttributes), "job-printer-up-time")
            .is_some());
        // Identify with unsupported action
        let out = svc.handle(build_req(
            0x0101,
            OP_IDENTIFY_PRINTER,
            15,
            std_ops(vec![("identify-actions", v_kw("sound"))]),
            b"",
        ));
        assert_eq!(status_of(&out.body), 0x0001);
        let out = svc.handle(build_req(
            0x0101,
            OP_IDENTIFY_PRINTER,
            16,
            std_ops(vec![]),
            b"",
        ));
        assert_eq!(status_of(&out.body), 0x0000);
        // Validate-Job ok
        let out = svc.handle(build_req(
            0x0101,
            OP_VALIDATE_JOB,
            17,
            std_ops(vec![("document-format", v_mime("image/pwg-raster"))]),
            b"",
        ));
        assert_eq!(status_of(&out.body), 0x0000);
        // Cancel-My-Jobs / Close-Job on unknown job
        let out = svc.handle(build_req(
            0x0101,
            OP_CANCEL_MY_JOBS,
            18,
            std_ops(vec![("requesting-user-name", v_name("kid"))]),
            b"",
        ));
        assert_eq!(status_of(&out.body), 0x0000);
        let out = svc.handle(build_req(
            0x0101,
            OP_CLOSE_JOB,
            19,
            std_ops(vec![("job-id", v_int(4242))]),
            b"",
        ));
        assert_eq!(status_of(&out.body), 0x0406);
    }

    #[tokio::test]
    async fn long_multibyte_job_name_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path());
        // job-name as text (≤ 1023 bytes parses fine) but longer than the 255-byte name limit we
        // echo it with, and every 255-byte cut lands inside a 2-byte char.
        let long = "é".repeat(400);
        let raster = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/tiny-roll48.pwg"
        ))
        .unwrap();
        let out = svc.handle(build_req(
            0x0200,
            OP_PRINT_JOB,
            21,
            std_ops(vec![
                ("requesting-user-name", v_name("kid")),
                ("job-name", v_text(&long)),
                ("document-format", v_mime("image/pwg-raster")),
            ]),
            &raster,
        ));
        assert_eq!(status_of(&out.body), 0x0000);
        let back = parse(out.body).unwrap();
        let echoed = back
            .get_str(Some(Group::JobAttributes), "job-name")
            .unwrap();
        assert!(echoed.len() <= 255 && echoed.chars().all(|c| c == 'é'));
    }

    #[tokio::test]
    async fn crate_parser_panic_is_contained() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path());
        // textWithLanguage whose inner language length exceeds the value: the crate slices
        // unchecked; we must answer bad-request instead of dropping the connection.
        let mut b = Vec::new();
        b.extend(0x0200u16.to_be_bytes());
        b.extend(OP_GET_PRINTER_ATTRIBUTES.to_be_bytes());
        b.extend(9i32.to_be_bytes());
        b.push(0x01);
        for (n, v) in [
            ("attributes-charset", v_charset("utf-8")),
            ("attributes-natural-language", v_lang("en")),
            ("printer-uri", v_uri("ipp://127.0.0.1:8095/ipp/print")),
        ] {
            let a = ipp::attribute::IppAttribute::new(ipp::value::IppName::new_truncated(n), v);
            b.extend(a.to_bytes());
        }
        b.push(0x35); // textWithLanguage
        b.extend((8u16).to_be_bytes());
        b.extend(b"job-name");
        b.extend((4u16).to_be_bytes());
        b.extend([0x00, 0x05, b'e', b'n']); // claims 5 language bytes, has 2
        b.push(0x03);
        let out = svc.handle(Bytes::from(b));
        assert_eq!(out.http_status, 200);
        assert_eq!(status_of(&out.body), 0x0400);
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    #[tokio::test]
    async fn gzip_bomb_is_rejected_with_request_value_too_long() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = EngineConfig {
            max_document_bytes: 1024 * 1024,
            ..EngineConfig::default()
        };
        let svc = service_with(dir.path(), cfg);
        let bomb = gzip(&vec![0u8; 8 * 1024 * 1024]); // 8 MiB of zeros → a few KiB
        assert!(bomb.len() < 64 * 1024);
        let out = svc.handle(build_req(
            0x0200,
            OP_PRINT_JOB,
            31,
            std_ops(vec![
                ("compression", v_kw("gzip")),
                ("document-format", v_mime("image/pwg-raster")),
            ]),
            &bomb,
        ));
        assert_eq!(status_of(&out.body), 0x0409);
        // deflate path too
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut e, &vec![0u8; 8 * 1024 * 1024]).unwrap();
        let z = e.finish().unwrap();
        let out = svc.handle(build_req(
            0x0200,
            OP_PRINT_JOB,
            32,
            std_ops(vec![
                ("compression", v_kw("deflate")),
                ("document-format", v_mime("image/pwg-raster")),
            ]),
            &z,
        ));
        assert_eq!(status_of(&out.body), 0x0409);
    }

    #[tokio::test]
    async fn gzip_pwg_roundtrip_prints() {
        let dir = tempfile::tempdir().unwrap();
        let svc = service(dir.path());
        let raster = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/tiny-roll48.pwg"
        ))
        .unwrap();
        let out = svc.handle(build_req(
            0x0200,
            OP_PRINT_JOB,
            41,
            std_ops(vec![
                ("compression", v_kw("gzip")),
                ("document-format", v_mime("image/pwg-raster")),
            ]),
            &gzip(&raster),
        ));
        assert_eq!(status_of(&out.body), 0x0000);
        let back = parse(out.body).unwrap();
        let id = back.get_int(Some(Group::JobAttributes), "job-id").unwrap();
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let out = svc.handle(build_req(
                0x0200,
                OP_GET_JOB_ATTRIBUTES,
                42,
                std_ops(vec![("job-id", v_int(id))]),
                b"",
            ));
            let back = parse(out.body).unwrap();
            if back.get_int(Some(Group::JobAttributes), "job-state") == Some(9) {
                let png = std::fs::read_dir(dir.path())
                    .unwrap()
                    .flatten()
                    .any(|e| e.file_name().to_string_lossy().ends_with(".png"));
                assert!(png, "fake printer wrote no png");
                return;
            }
        }
        panic!("gzip job did not complete");
    }
}
