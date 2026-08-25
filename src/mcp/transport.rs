// JSON-RPC framing for the RMCP stdio server.
//
// RMCP's own stdio transport reads unbounded lines, drops input its parser
// rejects without a reply, and treats anything before `initialize` as a fatal
// handshake error that ends the process. This transport keeps the framing
// contract this server has always had:
//
//   - one line is bounded to MAX_LINE_BYTES, oversize lines are drained and
//     answered -32600 rather than buffered,
//   - malformed UTF-8 and malformed JSON are answered -32700,
//   - valid JSON that is not a valid JSON-RPC request is answered -32600 with
//     the id echoed, so the client can correlate it,
//   - response-shaped messages are discarded in silence, per JSON-RPC 2.0,
//   - a request before `initialize` is answered -32002 and the connection
//     stays up.
//
// Everything past the envelope is RMCP's job.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::Duration;

// Logging is deprecated by SEP-2577 but stays in the spec for now, and this
// server's clients use it; the level is typed rather than stringly so the
// filter, the wire, and `logging/setLevel` cannot disagree.
#[allow(deprecated)]
use rmcp::model::LoggingLevel;
use rmcp::model::RequestId as PeerRequestId;
use rmcp::service::{RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use rmcp::RoleServer;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Stdin};
use tokio::sync::{mpsc, oneshot};

use super::types::{
    parse_jsonrpc_line, JsonRpcResponse, RequestId, TransportError, INVALID_REQUEST,
    SERVER_NOT_INITIALIZED,
};

/// Maximum line length accepted from stdin (4 MiB payload).
/// Prevents memory exhaustion from a stream that never sends a newline.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// Lifecycle state shared between the framing layer and the request handler.
///
/// The framing layer has to answer some messages before RMCP would see them
/// (a pre-init request, a call after `shutdown`), and the handler is what
/// learns that `initialize` and `shutdown` happened. One small shared cell
/// beats routing either concern through the other.
#[derive(Default)]
pub struct Lifecycle {
    initialized: AtomicBool,
    shutdown: AtomicBool,
    /// Receiver for tracing output bound for the client, present once the
    /// client has asked for log notifications.
    logs: std::sync::Mutex<Option<std::sync::mpsc::Receiver<crate::trace::McpLogMessage>>>,
    /// Lowest severity the client asked to receive, as a rank. Everything
    /// below it is dropped rather than sent, which is what `logging/setLevel`
    /// asks for; a client that wants errors should not be reading info.
    log_level: AtomicU8,
    /// The outbound queue, for waiting on it before the process goes.
    ///
    /// Held here because both exit paths need it and only one of them is the
    /// transport: the pre-handshake one is the framing layer's own, the other
    /// is the SDK's notification handler, which owns the judgment cache and so
    /// has to be the one that terminates.
    outbound: std::sync::Mutex<Option<mpsc::UnboundedSender<Outbound>>>,
    /// Requests accepted whose response has not been written yet, by id.
    ///
    /// A set rather than a count: a request can stop being owed a response
    /// without one being sent, when the client cancels it, and a bare counter
    /// cannot express "this particular one is settled" without risking a
    /// double retire that would underflow it.
    in_flight: std::sync::Mutex<std::collections::HashSet<PeerRequestId>>,
}

impl Lifecycle {
    fn mark_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    /// The status `exit` terminates with: 0 when `shutdown` came first, 1
    /// otherwise.
    ///
    /// This reads the same flag the gate reserves, so a client that pipelines
    /// `shutdown` and `exit` without waiting cannot race the two apart.
    pub(crate) fn exit_code(&self) -> i32 {
        if self.shutdown.load(Ordering::Acquire) {
            0
        } else {
            1
        }
    }

    /// Rank a severity so levels can be compared, in the order the spec
    /// defines. Exhaustive on purpose: a level added to the enum should not
    /// quietly fall into a catch-all and be filtered out at every setting.
    #[allow(deprecated)]
    pub(crate) fn log_rank(level: LoggingLevel) -> u8 {
        match level {
            LoggingLevel::Debug => 0,
            LoggingLevel::Info => 1,
            LoggingLevel::Notice => 2,
            LoggingLevel::Warning => 3,
            LoggingLevel::Error => 4,
            LoggingLevel::Critical => 5,
            LoggingLevel::Alert => 6,
            LoggingLevel::Emergency => 7,
        }
    }

    /// Record the level the client asked for. Logs below it stop being sent.
    #[allow(deprecated)]
    pub(crate) fn set_log_level(&self, level: LoggingLevel) {
        self.log_level
            .store(Self::log_rank(level), Ordering::Relaxed);
    }

    /// Start forwarding tracing output to the client.
    ///
    /// Called from two places: a `logging` key in the initialize capabilities,
    /// which is what this server has always honored and which RMCP's typed
    /// `ClientCapabilities` discards, and `logging/setLevel`, which is the
    /// spec's way to ask for the same thing. Repeat calls are no-ops.
    pub(crate) fn enable_logs(&self) {
        let mut slot = self.logs.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        crate::trace::set_mcp_log_sender(Some(tx));
        *slot = Some(rx);
    }

    /// Queue whatever tracing output has accrued, as `notifications/message`.
    ///
    /// On `Lifecycle` rather than on the transport because the log receiver is
    /// here and because terminating needs it; the transport's own flushing
    /// goes through the same function.
    pub(crate) fn queue_logs(&self) {
        let Some(outbound) = self
            .outbound
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        else {
            return;
        };
        for message in self.drain_logs() {
            let frame = encode(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/message",
                "params": message,
            }));
            if let Ok(frame) = frame {
                let _ = outbound.send(Outbound {
                    frame,
                    answers: None,
                    done: None,
                });
            }
        }
    }

    /// The one way this process terminates on `exit`.
    ///
    /// Both triggers end here: the framing layer answers `exit` before the
    /// handshake, the SDK's notification handler after it. Leaving each to
    /// open-code the sequence is how they came to disagree, with one of them
    /// delivering its last log lines and the other dropping them.
    ///
    /// `before_exit` is whatever the caller owns and the other does not, which
    /// today is the judgment cache the handler holds. It runs after the queue
    /// has drained so that a scan finishing during the drain still counts.
    pub(crate) async fn terminate(&self, before_exit: impl FnOnce()) -> ! {
        self.queue_logs();
        self.drain_outbound().await;
        before_exit();
        std::process::exit(self.exit_code());
    }

    /// Wait for everything already queued to reach the client.
    ///
    /// `process::exit` does not run destructors and does not drain anything,
    /// so a reply queued just before it, a `shutdown` acknowledgement being
    /// the case that matters, would never be written. An empty frame writes
    /// no bytes; it is here to be acknowledged after the ones ahead of it.
    ///
    /// This covers what is already queued, not what is still being computed:
    /// a request in flight when `exit` arrives still loses its response,
    /// because `exit` is unconditional. End of input is the path that waits
    /// for those, through `drain_in_flight`.
    pub(crate) async fn drain_outbound(&self) {
        // Cloned into this future rather than borrowed, and it has to stay
        // that way: holding a sender across the await is what stops a
        // concurrent `close()` from finishing its writer join before this
        // resumes. Hoisting the clone out reintroduces the deadlock family
        // this has already been bitten by twice.
        let Some(outbound) = self
            .outbound
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        else {
            return;
        };
        let (done, written) = oneshot::channel();
        let queued = outbound.send(Outbound {
            frame: Vec::new(),
            answers: None,
            done: Some(done),
        });
        if queued.is_ok() && tokio::time::timeout(FLUSH_TIMEOUT, written).await.is_err() {
            tracing::warn!("exiting with output still queued: the client is not reading");
        }
    }

    /// Install the queue this lifecycle hands frames to.
    fn set_outbound(&self, tx: mpsc::UnboundedSender<Outbound>) {
        *self.outbound.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
    }

    fn close_outbound(&self) {
        self.outbound
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }

    /// Mark one request as accepted, until its response has been written.
    ///
    /// The framing layer consults this at end of input: RMCP ends its service
    /// loop the moment the transport reports EOF, so without this a request
    /// still being served loses its response.
    ///
    /// Both ends of this live in the transport, which is the only layer that
    /// sees a request arrive and its response leave. Counting in the handlers
    /// instead retired a request when the handler returned, which is before
    /// RMCP has serialized and written the response, so the drain could see
    /// nothing outstanding while a reply was still unwritten. It also left
    /// coverage up to each handler remembering to opt in, and three did not.
    pub(crate) fn accept_request(&self, id: PeerRequestId) {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id);
    }

    /// Mark one request as answered, once its response is on the wire.
    ///
    /// A request RMCP never answers stays counted until the drain deadline.
    /// That costs a slower exit rather than a lost response, which is the
    /// direction to err in, and this server answers every request it accepts.
    pub(crate) fn retire_request(&self, id: &PeerRequestId) {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
    }

    /// How many requests are still owed a response.
    fn outstanding(&self) -> usize {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Open the post-handshake request gate after a handshake succeeds.
    pub(crate) fn mark_initialized(&self) {
        self.initialized.store(true, Ordering::Release);
    }

    /// Take whatever tracing output has accrued since the last drain.
    ///
    /// Logs accrue synchronously on the thread serving the request, so a
    /// single drain afterward catches everything for that request.
    pub(crate) fn drain_logs(&self) -> Vec<crate::trace::McpLogMessage> {
        let slot = self.logs.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(rx) = slot.as_ref() else {
            return Vec::new();
        };
        // Drained either way, so a level the client is not reading cannot
        // accumulate in the channel.
        let floor = self.log_level.load(Ordering::Relaxed);
        rx.try_iter()
            .filter(|message| Self::log_rank(message.level) >= floor)
            .collect()
    }
}

/// How much of an unterminated line to discard before giving up on the
/// client. Generous next to the 4 MiB line bound: this is reached only by a
/// stream with no newline in it at all.
const MAX_DRAIN_BYTES: usize = 64 * 1024 * 1024;

/// How long either shutdown path waits for queued output to reach the client.
///
/// Delivery is best effort and termination is not: a client that stops reading
/// its end of the pipe leaves the write unable to complete, and without a
/// bound here that client can keep this process alive indefinitely.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the runtime is given to finish blocking work once the service
/// loop has returned. A separate deadline from `FLUSH_TIMEOUT` even at the
/// same value: that one bounds a write, this one bounds a scan.
pub const BLOCKING_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// How long end of input waits for handlers to finish before giving up on
/// them. Past the longest a handler should take: a sampling round trip is
/// capped at five seconds and a scan is bounded by the text limit.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the drain rechecks. Short enough not to add visible latency to a
/// normal exit, long enough not to spin.
const DRAIN_POLL: Duration = Duration::from_millis(5);

/// One frame on its way out, and whether writing it answers a request.
struct Outbound {
    frame: Vec<u8>,
    answers: Option<PeerRequestId>,
    /// Signalled once the frame is written, for callers that must not run
    /// ahead of it. RMCP awaits `send`, and on a refused handshake it tears
    /// the session down as soon as that returns: without this the process
    /// could exit with the refusal still queued.
    done: Option<oneshot::Sender<()>>,
}

/// Build the stdio transport for `lifecycle`.
pub fn stdio(lifecycle: Arc<Lifecycle>) -> StdioTransport {
    let (outbound, mut rx) = mpsc::unbounded_channel::<Outbound>();
    // Writing happens here and nowhere else. RMCP polls `receive` inside a
    // select! and drops that future whenever another arm wins, which under a
    // stream of responses is most of the time. A write awaited inside it is
    // dropped with it, losing the reply outright, and a write dropped partway
    // through leaves half a frame on the wire for the next one to run into.
    // Handing frames to a task instead puts them outside anything that gets
    // cancelled, and a queue keeps them in the order they were produced.
    let writer = tokio::spawn({
        let lifecycle = lifecycle.clone();
        async move {
            let mut out = tokio::io::stdout();
            while let Some(item) = rx.recv().await {
                if out.write_all(&item.frame).await.is_ok() {
                    let _ = out.flush().await;
                }
                if let Some(done) = item.done {
                    let _ = done.send(());
                }
                // Retired after the write, not when the handler returned: the
                // response does not exist until it is on the wire, and end of
                // input consults this to decide whether anything is still owed.
                if let Some(id) = &item.answers {
                    lifecycle.retire_request(id);
                }
            }
            let _ = out.flush().await;
        }
    });
    lifecycle.set_outbound(outbound.clone());
    StdioTransport {
        reader: BufReader::new(tokio::io::stdin()),
        outbound,
        writer: Some(writer),
        lifecycle,
        raw: Vec::new(),
        drain_deadline: None,
    }
}

pub struct StdioTransport {
    reader: BufReader<Stdin>,
    outbound: mpsc::UnboundedSender<Outbound>,
    writer: Option<tokio::task::JoinHandle<()>>,
    lifecycle: Arc<Lifecycle>,
    raw: Vec<u8>,
    /// Set on the first end of input, so a drain restarted after cancellation
    /// keeps counting from when it actually began.
    drain_deadline: Option<tokio::time::Instant>,
}

impl StdioTransport {
    /// Queue one frame and forget it. Never awaits, so a caller that is
    /// cancelled mid-call cannot lose it. For the framing layer's own
    /// replies, which are produced inside `receive`.
    fn enqueue(&self, frame: Vec<u8>) {
        let _ = self.outbound.send(Outbound {
            frame,
            answers: None,
            done: None,
        });
    }

    /// Queue one frame and hand back something that completes when it has
    /// been written. For `send`, which RMCP awaits outside `receive`.
    fn enqueue_tracked(
        &self,
        frame: Vec<u8>,
        answers: Option<PeerRequestId>,
    ) -> oneshot::Receiver<()> {
        let (done, written) = oneshot::channel();
        let _ = self.outbound.send(Outbound {
            frame,
            answers,
            done: Some(done),
        });
        written
    }
}

/// Outcome of one bounded line read.
enum ReadLine {
    Line(String),
    Eof,
    TooLong,
    MalformedUtf8,
    /// An unterminated line so long that the discard gave up on finding where
    /// it ends. Nothing further can be framed from this stream.
    Unrecoverable,
}

/// Read one line, bounded to `MAX_LINE_BYTES`.
///
/// UTF-8 is validated exactly once, here, so no caller re-validates or risks a
/// panic on an invalid boundary.
///
/// `raw` is the caller's buffer and it is deliberately not cleared on entry.
/// This future is polled inside RMCP's `select!`, so an in-progress read is
/// dropped whenever another branch becomes ready, which under a stream of
/// outgoing responses happens often. `read_until` appends as it goes and only
/// returns at a delimiter or end of input, so a cancelled read leaves its
/// bytes here for the next call to resume from. Clearing on entry instead
/// discards them, splitting the client's line in two: the request is never
/// answered and the client waits forever.
async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    raw: &mut Vec<u8>,
) -> std::io::Result<ReadLine> {
    // The bound is on the line, not the call, so a resumed read gets what is
    // left of the budget. Saturating because a cancelled read may already
    // hold all of it, in which case there is nothing left to take.
    let budget = (MAX_LINE_BYTES + 1).saturating_sub(raw.len()) as u64;
    reader.take(budget).read_until(b'\n', raw).await?;

    // `read_until` stops at a delimiter, at end of input, or when the budget
    // runs out, and its byte count does not say which. What distinguishes
    // them is the buffer: a delimiter at the end, or the whole budget spent
    // without one.
    if !raw.ends_with(b"\n") && raw.len() > MAX_LINE_BYTES {
        let recovered = drain_until_newline(reader).await?;
        raw.clear();
        // If the discard gave up, the stream has no line boundary left to
        // resynchronize on, so there is nothing to go back to reading.
        return Ok(if recovered {
            ReadLine::TooLong
        } else {
            ReadLine::Unrecoverable
        });
    }
    if raw.is_empty() {
        return Ok(ReadLine::Eof);
    }
    // Either a whole line, or a final one the client left unterminated before
    // closing. The second still parses: a batch caller that writes its last
    // request without a newline and closes gets it answered, and end of input
    // is reported on the next call, once the buffer is empty.
    let line = match std::str::from_utf8(raw) {
        Ok(line) => ReadLine::Line(line.trim().to_owned()),
        Err(_) => ReadLine::MalformedUtf8,
    };
    raw.clear();
    Ok(line)
}

/// Consume and discard bytes until a newline or EOF, so the line after an
/// oversize one still parses.
async fn drain_until_newline<R: AsyncBufRead + Unpin>(reader: &mut R) -> std::io::Result<bool> {
    let mut discarded = 0usize;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(true);
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                reader.consume(pos + 1);
                return Ok(true);
            }
            None => {
                let len = available.len();
                reader.consume(len);
                discarded = discarded.saturating_add(len);
                // A line that never ends is not a line. Without a bound this
                // reads forever, and since a client has one stream, it would
                // never get to send anything else either.
                if discarded > MAX_DRAIN_BYTES {
                    return Ok(false);
                }
            }
        }
    }
}

/// Serialize one message as a newline-terminated JSON-RPC frame.
fn encode(message: &impl serde::Serialize) -> Result<Vec<u8>, serde_json::Error> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    Ok(line)
}

/// The request a `notifications/cancelled` refers to, if it is one.
fn cancelled_request_id(
    notification: &rmcp::model::JsonRpcNotification<rmcp::model::ClientNotification>,
) -> Option<PeerRequestId> {
    match &notification.notification {
        rmcp::model::ClientNotification::CancelledNotification(cancelled) => {
            cancelled.params.request_id.clone()
        }
        _ => None,
    }
}

/// Wait for accepted requests to produce their responses.
///
/// Every request is counted by `receive` before it is handed to RMCP, so by
/// the time end of input is seen here, everything dispatched before it is
/// already counted. Nothing has to be yielded to first: there is no window
/// between a request being dispatched and its being registered.
///
/// Free-standing and taking only the lifecycle, so the waiting can be tested
/// without a transport, a subprocess, or a lint slow enough to still be
/// running when end of input lands.
async fn drain_in_flight(lifecycle: &Lifecycle, deadline: &mut Option<tokio::time::Instant>) {
    // Kept by the caller for the same reason the read buffer is: this runs
    // inside a future RMCP cancels, and a deadline restarted on every
    // cancellation is not a deadline. A handler that keeps producing peer
    // traffic would otherwise hold the process open indefinitely.
    let deadline = *deadline.get_or_insert_with(|| tokio::time::Instant::now() + DRAIN_TIMEOUT);
    loop {
        let outstanding = lifecycle.outstanding();
        if outstanding == 0 {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!("end of input with {outstanding} request(s) still running");
            return;
        }
        tokio::time::sleep(DRAIN_POLL).await;
    }
}

impl StdioTransport {
    /// Emit any pending tracing output as `notifications/message`.
    fn flush_logs(&self) {
        self.lifecycle.queue_logs();
    }

    /// Write a framing-level response directly, bypassing RMCP.
    ///
    /// Whatever this request logged goes out first: clients read causality
    /// from that order.
    fn reply(&self, response: JsonRpcResponse) {
        self.flush_logs();
        if let Ok(frame) = encode(&response) {
            self.enqueue(frame);
        }
    }
}

/// What the framing layer does with an envelope before RMCP sees it.
enum Gate {
    /// Hand it to RMCP.
    Forward,
    /// Answer here.
    Reply(Box<JsonRpcResponse>),
    /// Drop in silence, which is what a notification gets when there is
    /// nothing to say back.
    Drop,
    /// Terminate. The status is read at the exit, from the same flag the
    /// handler path consults, rather than carried from here.
    Exit,
}

/// One-shot helper: a request is answered, a notification is dropped.
fn answer(id: Option<RequestId>, response: impl FnOnce(RequestId) -> JsonRpcResponse) -> Gate {
    match id {
        Some(id) => Gate::Reply(Box::new(response(id))),
        None => Gate::Drop,
    }
}

/// Decide the fate of one envelope.
fn gate(lifecycle: &Lifecycle, request: &super::types::JsonRpcRequest) -> Gate {
    let method = request.method.as_str();
    let id = request.id.clone();

    // exit is honored regardless of lifecycle state. After the handshake the
    // handler runs it, because it owns the judgment cache that has to be
    // flushed before the process goes. Before the handshake RMCP would end the
    // session rather than deliver it, and no scan has run, so there is nothing
    // to flush and the transport can honor it directly.
    if method == "exit" {
        if lifecycle.initialized.load(Ordering::Relaxed) {
            return Gate::Forward;
        }
        return Gate::Exit;
    }
    if lifecycle.shutdown.load(Ordering::Acquire) {
        tracing::warn!("rejecting {method} after shutdown");
        return answer(id, |id| {
            JsonRpcResponse::error(Some(id), INVALID_REQUEST, "server is shutting down".into())
        });
    }
    // A notification carrying an id is a client bug that used to be reported
    // rather than silently reinterpreted.
    if method.starts_with("notifications/") && id.is_some() {
        return answer(id, |id| {
            JsonRpcResponse::error(
                Some(id),
                INVALID_REQUEST,
                format!("{method} must be sent as a notification (no id)"),
            )
        });
    }
    // Answered here rather than forwarded, for two reasons: the handler runs
    // asynchronously, so a pipelined request could otherwise pass this gate
    // while it waits to run, and before the handshake RMCP would treat it as a
    // failed initialize and end the session.
    if method == "shutdown" {
        tracing::info!("shutdown requested");
        lifecycle.mark_shutdown();
        return answer(id, |id| {
            JsonRpcResponse::success(Some(id), serde_json::json!({}))
        });
    }
    // Discovery is by definition pre-handshake: a client asks what this server
    // speaks before committing to a revision. Its handler opens the gate after
    // it has successfully produced the discovery response.
    if method == "server/discover" {
        return Gate::Forward;
    }
    if method == "initialize" {
        // A `logging` key in the client capabilities is this server's own
        // extension, and RMCP's typed capabilities drop it before the handler
        // ever sees it, so it is read here off the raw envelope.
        if request.params.pointer("/capabilities/logging").is_some() {
            lifecycle.enable_logs();
        }
        return Gate::Forward;
    }
    if lifecycle.initialized.load(Ordering::Relaxed) {
        return Gate::Forward;
    }

    // Pre-init. RMCP ends the session on anything but initialize, so these are
    // answered here instead.
    match method {
        "ping" => answer(id, |id| {
            JsonRpcResponse::success(Some(id), serde_json::json!({}))
        }),
        _ if method.starts_with("notifications/") => Gate::Drop,
        _ => {
            tracing::warn!("rejecting {method} before initialization");
            answer(id, |id| {
                JsonRpcResponse::error(
                    Some(id),
                    SERVER_NOT_INITIALIZED,
                    "server not initialized".into(),
                )
            })
        }
    }
}

impl Transport<RoleServer> for StdioTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        // Only a reply retires a request. A notification answers nothing, and
        // a server-initiated request (sampling) is this server asking, not
        // answering.
        let answers = match &item {
            TxJsonRpcMessage::<RoleServer>::Response(response) => Some(response.id.clone()),
            // An error response may carry no id, when nothing could be
            // correlated; there is then no request for it to retire.
            TxJsonRpcMessage::<RoleServer>::Error(error) => error.id.clone(),
            _ => None,
        };
        let queued = match encode(&item) {
            Ok(frame) => Ok(self.enqueue_tracked(frame, answers)),
            Err(e) => {
                // Nothing will be written, so nothing will retire this one
                // later. Leaving it outstanding would hold end of input open
                // for a response that is never coming.
                if let Some(id) = &answers {
                    self.lifecycle.retire_request(id);
                }
                Err(std::io::Error::from(e))
            }
        };
        async move {
            // Resolves when the frame is on the wire, or immediately if the
            // writer is already gone, which means nothing more will be written
            // anyway.
            let _ = queued?.await;
            Ok(())
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        loop {
            // Framing-level logs (a parse error, an oversize line) have no
            // response to ride along with, so they go out before the next read
            // blocks. Request-scoped logs are flushed by the handler instead,
            // which is what keeps them ahead of their own response.
            self.flush_logs();
            let line = match read_line(&mut self.reader, &mut self.raw).await {
                Ok(ReadLine::Line(line)) => line,
                Ok(ReadLine::Eof) => {
                    // Stdin closing means the client stopped sending, not that
                    // it stopped listening: a batch caller writes its requests,
                    // closes the write half, and waits for the answers.
                    // Reporting EOF now would end RMCP's service loop with
                    // handlers still running and their responses unwritten.
                    drain_in_flight(&self.lifecycle, &mut self.drain_deadline).await;
                    self.flush_logs();
                    return None;
                }
                Ok(ReadLine::Unrecoverable) => {
                    tracing::error!(
                        "no line boundary within {MAX_DRAIN_BYTES} bytes, closing the session"
                    );
                    self.reply(JsonRpcResponse::error(
                        None,
                        INVALID_REQUEST,
                        "request too large".into(),
                    ));
                    return None;
                }
                Ok(ReadLine::TooLong) => {
                    tracing::warn!("request exceeds {MAX_LINE_BYTES} bytes");
                    self.reply(JsonRpcResponse::error(
                        None,
                        INVALID_REQUEST,
                        "request too large".into(),
                    ));
                    continue;
                }
                Ok(ReadLine::MalformedUtf8) => {
                    self.reply(JsonRpcResponse::error(
                        None,
                        super::types::PARSE_ERROR,
                        "invalid UTF-8 in request".into(),
                    ));
                    continue;
                }
                Err(e) => {
                    tracing::error!("stdin read failed: {e}");
                    return None;
                }
            };
            if line.is_empty() {
                continue;
            }

            // Validate the envelope before RMCP sees it. The line is parsed
            // twice, once here and once by RMCP's own deserializer; that costs
            // one JSON pass per message and buys a single definition of what a
            // well-formed request is.
            let request = match parse_jsonrpc_line(&line) {
                Ok(request) => request,
                Err(TransportError::PeerResponse) => {
                    // A reply to a request this server sent: sampling today,
                    // and whatever server-to-client request comes next. RMCP's
                    // peer owns the ids it has outstanding, so the reply has to
                    // reach it; answering nothing here leaves the request
                    // waiting for its timeout and the sampled answer lost. A
                    // reply carrying no id, or one whose envelope RMCP cannot
                    // model, has nothing to correlate against and is dropped,
                    // which is what the peer would do with it anyway.
                    // Only once a session exists. Before the handshake this
                    // server has asked the client nothing, so there is no id
                    // for a reply to match, and RMCP reads any non-request
                    // arriving there as a failed handshake and ends the
                    // session. Dropping it costs nothing and keeps a stray
                    // line from taking the connection down.
                    if !self.lifecycle.initialized.load(Ordering::Relaxed) {
                        continue;
                    }
                    match serde_json::from_str(&line) {
                        Ok(message) => return Some(message),
                        Err(_) => continue,
                    }
                }
                Err(e) => {
                    tracing::warn!("{e}");
                    if let Some(response) = e.into_response(None) {
                        self.reply(response);
                    }
                    continue;
                }
            };

            match gate(&self.lifecycle, &request) {
                Gate::Forward => {}
                Gate::Reply(response) => {
                    self.reply(*response);
                    continue;
                }
                Gate::Drop => continue,
                Gate::Exit => {
                    tracing::info!("exit notification before initialize, terminating");
                    // Nothing to do beyond terminating: no scan has run before
                    // the handshake, so there is no cache to flush.
                    self.lifecycle.terminate(|| {}).await;
                }
            }

            match serde_json::from_str::<RxJsonRpcMessage<RoleServer>>(&line) {
                Ok(message) => {
                    // Counted here rather than in the handlers, so every
                    // request is covered whether or not its handler knows
                    // about the drain. A notification has nothing to answer.
                    match &message {
                        RxJsonRpcMessage::<RoleServer>::Request(request) => {
                            self.lifecycle.accept_request(request.id.clone());
                        }
                        // A cancelled request will never be answered, so it
                        // stops being owed one here. Without this, end of
                        // input waits out its whole deadline for a response
                        // that by definition is not coming.
                        RxJsonRpcMessage::<RoleServer>::Notification(notification) => {
                            if let Some(id) = cancelled_request_id(notification) {
                                self.lifecycle.retire_request(&id);
                            }
                        }
                        _ => {}
                    }
                    return Some(message);
                }
                Err(e) => {
                    // A valid JSON-RPC envelope RMCP cannot model: report it
                    // rather than dropping the client's request on the floor.
                    tracing::warn!("unsupported message shape: {e}");
                    if request.id.is_some() {
                        self.reply(JsonRpcResponse::error(
                            request.id,
                            INVALID_REQUEST,
                            e.to_string(),
                        ));
                    }
                }
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        // Dropping the queue ends the writer task once it has drained, so
        // whatever was queued still reaches the client before the process
        // goes. Waiting on the task is what makes that ordering hold.
        self.lifecycle.close_outbound();
        self.outbound = mpsc::unbounded_channel().0;
        if let Some(writer) = self.writer.take() {
            // Bounded for the same reason the exit drain is: with both senders
            // dropped the writer ends once it has drained, but a client that
            // has stopped reading leaves it blocked in a write it cannot
            // finish, and this await is what end of input returns through.
            if tokio::time::timeout(FLUSH_TIMEOUT, writer).await.is_err() {
                tracing::warn!("closing with output still queued: the client is not reading");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lifecycle() -> Lifecycle {
        Lifecycle::default()
    }

    fn req(method: &str, id: Option<i64>) -> super::super::types::JsonRpcRequest {
        super::super::types::JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: id.map(RequestId::Int),
            method: method.into(),
            params: serde_json::json!({}),
        }
    }

    #[test]
    fn pre_init_request_is_rejected_not_fatal() {
        let lc = lifecycle();
        let Gate::Reply(response) = gate(&lc, &req("tools/list", Some(1))) else {
            panic!("a pre-init request must be answered, not dropped");
        };
        assert_eq!(response.error.unwrap().code, SERVER_NOT_INITIALIZED);
    }

    #[test]
    fn pre_init_notification_is_dropped() {
        let lc = lifecycle();
        assert!(matches!(
            gate(&lc, &req("notifications/initialized", None)),
            Gate::Drop
        ));
    }

    #[test]
    fn pre_init_ping_is_answered() {
        let lc = lifecycle();
        let Gate::Reply(response) = gate(&lc, &req("ping", Some(1))) else {
            panic!("pre-init ping must be answered");
        };
        assert!(response.result.is_some());
    }

    #[test]
    fn successful_initialize_opens_the_gate() {
        let lc = lifecycle();
        assert!(matches!(
            gate(&lc, &req("initialize", Some(1))),
            Gate::Forward
        ));
        lc.mark_initialized();
        assert!(matches!(
            gate(&lc, &req("tools/list", Some(2))),
            Gate::Forward
        ));
    }

    #[test]
    fn notification_with_id_is_rejected() {
        let lc = lifecycle();
        lc.initialized.store(true, Ordering::Relaxed);
        let Gate::Reply(response) = gate(&lc, &req("notifications/cancelled", Some(3))) else {
            panic!("an id-bearing notification must be answered");
        };
        assert_eq!(response.error.unwrap().code, INVALID_REQUEST);
    }

    #[test]
    fn after_shutdown_everything_but_exit_is_rejected() {
        let lc = lifecycle();
        lc.initialized.store(true, Ordering::Relaxed);
        lc.mark_shutdown();
        assert!(matches!(gate(&lc, &req("exit", None)), Gate::Forward));
        let Gate::Reply(response) = gate(&lc, &req("tools/list", Some(4))) else {
            panic!("a post-shutdown request must be answered");
        };
        assert_eq!(response.error.unwrap().code, INVALID_REQUEST);
    }

    #[test]
    fn successful_discover_opens_the_gate() {
        // Discovery precedes the handshake, so the gate has to let it reach
        // RMCP, and RMCP serves the session from there.
        let lc = lifecycle();
        assert!(matches!(
            gate(&lc, &req("server/discover", Some(1))),
            Gate::Forward
        ));
        lc.mark_initialized();
        assert!(matches!(
            gate(&lc, &req("tools/list", Some(2))),
            Gate::Forward
        ));
    }

    #[test]
    fn pre_init_exit_terminates_rather_than_forwarding() {
        // Forwarding it would reach RMCP as a failed handshake, which ends the
        // session with the wrong status and an error the client did not cause.
        // The gate decides that this terminates; what status it terminates
        // with is read at the exit, from the same flag either path consults.
        let lc = lifecycle();
        assert!(matches!(gate(&lc, &req("exit", None)), Gate::Exit));
        assert_eq!(lc.exit_code(), 1);
        lc.mark_shutdown();
        assert!(matches!(gate(&lc, &req("exit", None)), Gate::Exit));
        assert_eq!(lc.exit_code(), 0);
    }

    #[test]
    fn pre_init_shutdown_is_answered_not_forwarded() {
        // Forwarding it would reach RMCP as a failed handshake and end the
        // session, which is the one thing the pre-init gate exists to prevent.
        let lc = lifecycle();
        let Gate::Reply(response) = gate(&lc, &req("shutdown", Some(1))) else {
            panic!("a pre-init shutdown must be answered here");
        };
        assert!(response.result.is_some());
        assert_eq!(lc.exit_code(), 0);
    }

    #[test]
    fn shutdown_as_a_notification_sets_the_flag_without_replying() {
        let lc = lifecycle();
        lc.initialized.store(true, Ordering::Relaxed);
        assert!(matches!(gate(&lc, &req("shutdown", None)), Gate::Drop));
        assert_eq!(lc.exit_code(), 0);
    }

    #[test]
    fn exit_code_is_one_without_shutdown() {
        assert_eq!(lifecycle().exit_code(), 1);
    }

    #[test]
    fn shutdown_closes_the_gate_before_its_handler_runs() {
        let lc = lifecycle();
        lc.initialized.store(true, Ordering::Relaxed);
        assert!(matches!(
            gate(&lc, &req("shutdown", Some(1))),
            Gate::Reply(_)
        ));
        let Gate::Reply(response) = gate(&lc, &req("tools/call", Some(2))) else {
            panic!("a pipelined request after shutdown must be answered here");
        };
        assert_eq!(response.error.unwrap().code, INVALID_REQUEST);
    }

    #[tokio::test]
    async fn oversize_line_is_drained_and_the_next_line_parses() {
        let big = "x".repeat(MAX_LINE_BYTES + 10);
        let input = format!("{big}\n{{\"jsonrpc\":\"2.0\"}}\n");
        let mut reader = BufReader::new(input.as_bytes());
        let mut raw = Vec::new();

        assert!(matches!(
            read_line(&mut reader, &mut raw).await.unwrap(),
            ReadLine::TooLong
        ));
        let ReadLine::Line(next) = read_line(&mut reader, &mut raw).await.unwrap() else {
            panic!("the line after an oversize one must still parse");
        };
        assert_eq!(next, "{\"jsonrpc\":\"2.0\"}");
    }

    #[tokio::test(start_paused = true)]
    async fn end_of_input_waits_for_an_accepted_request() {
        let lifecycle = Lifecycle::default();
        lifecycle.accept_request(PeerRequestId::Number(1));

        let mut deadline = None;
        let drain = std::pin::pin!(drain_in_flight(&lifecycle, &mut deadline));
        // Well past DRAIN_POLL, and with time paused it costs no wall clock.
        let waited = tokio::time::timeout(DRAIN_TIMEOUT / 2, drain).await;
        assert!(waited.is_err(), "a request still running holds the drain");

        lifecycle.retire_request(&PeerRequestId::Number(1));
        tokio::time::timeout(DRAIN_TIMEOUT, drain_in_flight(&lifecycle, &mut None))
            .await
            .expect("the drain returns once the response exists");
    }

    #[tokio::test(start_paused = true)]
    async fn end_of_input_gives_up_on_a_request_that_never_finishes() {
        let lifecycle = Lifecycle::default();
        lifecycle.accept_request(PeerRequestId::Number(1));

        // Bounded, so a wedged handler cannot keep the process alive.
        tokio::time::timeout(DRAIN_TIMEOUT * 2, drain_in_flight(&lifecycle, &mut None))
            .await
            .expect("the drain gives up at DRAIN_TIMEOUT");
    }

    #[tokio::test]
    async fn a_final_frame_without_its_newline_is_still_answered() {
        // A caller that writes its last request and closes without a trailing
        // newline gets it answered. Nothing distinguishes that from a line
        // still being written except end of input, so it is only delivered
        // once the client has stopped sending.
        let (client, mut server) = tokio::io::duplex(64);
        let mut reader = BufReader::new(client);
        let mut raw = Vec::new();

        server.write_all(b"{\"jsonrpc\":\"2.0\"}").await.unwrap();
        drop(server);

        let ReadLine::Line(line) = read_line(&mut reader, &mut raw).await.unwrap() else {
            panic!("an unterminated final frame is a request, not a broken one");
        };
        assert_eq!(line, "{\"jsonrpc\":\"2.0\"}");
        // And end of input follows, so the loop still terminates.
        assert!(matches!(
            read_line(&mut reader, &mut raw).await.unwrap(),
            ReadLine::Eof
        ));
    }

    #[tokio::test]
    async fn a_line_at_exactly_the_limit_is_not_too_long() {
        // The limit is on the content, so the newline that terminates a
        // maximum-length line does not push it over.
        let (client, mut server) = tokio::io::duplex(MAX_LINE_BYTES + 64);
        let mut reader = BufReader::new(client);
        let mut raw = Vec::new();

        let body = "x".repeat(MAX_LINE_BYTES);
        tokio::spawn(async move {
            server.write_all(body.as_bytes()).await.unwrap();
            server.write_all(b"\n").await.unwrap();
        });

        let ReadLine::Line(line) = read_line(&mut reader, &mut raw).await.unwrap() else {
            panic!("a line of exactly MAX_LINE_BYTES fits");
        };
        assert_eq!(line.len(), MAX_LINE_BYTES);
    }

    #[tokio::test]
    async fn a_cancelled_read_of_an_oversize_line_is_still_too_long() {
        // The resumed read has no budget left. That must report the line as
        // oversize, not mistake an empty read for end of input and hang up.
        let (client, mut server) = tokio::io::duplex(MAX_LINE_BYTES + 64);
        let mut reader = BufReader::new(client);
        let mut raw = vec![b'x'; MAX_LINE_BYTES + 1];

        server
            .write_all(b"tail-of-the-oversize-line\n{\"jsonrpc\":\"2.0\"}\n")
            .await
            .unwrap();
        assert!(matches!(
            read_line(&mut reader, &mut raw).await.unwrap(),
            ReadLine::TooLong
        ));

        let ReadLine::Line(next) = read_line(&mut reader, &mut raw).await.unwrap() else {
            panic!("the line after an oversize one still parses");
        };
        assert_eq!(next, "{\"jsonrpc\":\"2.0\"}");
    }

    #[tokio::test]
    async fn a_cancelled_read_resumes_instead_of_losing_the_line() {
        // RMCP polls `receive` inside a select!, so a read in progress is
        // dropped whenever a response becomes ready. The bytes already taken
        // have to survive that, or the client's request is split in two and
        // never answered. This is the failure that hung CI: it needs an
        // outgoing response to land mid-read, so it is timing-dependent in
        // the server and deterministic only here.
        let (client, mut server) = tokio::io::duplex(64);
        let mut reader = BufReader::new(client);
        let mut raw = Vec::new();

        server.write_all(b"{\"jsonrpc\":").await.unwrap();
        // Drop the read future mid-line, exactly as the select! would.
        let cancelled =
            tokio::time::timeout(Duration::from_millis(20), read_line(&mut reader, &mut raw)).await;
        assert!(cancelled.is_err(), "the read must still be waiting");
        assert!(!raw.is_empty(), "the bytes taken so far are kept");

        server.write_all(b"\"2.0\"}\n").await.unwrap();
        let ReadLine::Line(line) = read_line(&mut reader, &mut raw).await.unwrap() else {
            panic!("the resumed read must produce the whole line");
        };
        assert_eq!(line, "{\"jsonrpc\":\"2.0\"}");
        assert!(raw.is_empty(), "a consumed line leaves the buffer empty");
    }

    #[tokio::test]
    async fn malformed_utf8_is_reported_not_dropped() {
        let mut reader = BufReader::new(&b"\xff\xfe\n"[..]);
        let mut raw = Vec::new();
        assert!(matches!(
            read_line(&mut reader, &mut raw).await.unwrap(),
            ReadLine::MalformedUtf8
        ));
    }

    #[tokio::test]
    async fn empty_input_is_eof() {
        let mut reader = BufReader::new(&b""[..]);
        let mut raw = Vec::new();
        assert!(matches!(
            read_line(&mut reader, &mut raw).await.unwrap(),
            ReadLine::Eof
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn draining_after_the_queue_is_closed_returns_rather_than_waiting() {
        // The shape of the bug this has been bitten by: a drain that waits on
        // a writer which can no longer answer. With the sender taken there is
        // nothing to enqueue against, so it has to return, not block.
        let lifecycle = Lifecycle::default();
        let (outbound, _rx) = mpsc::unbounded_channel();
        lifecycle.set_outbound(outbound);
        lifecycle.close_outbound();

        tokio::time::timeout(Duration::from_secs(1), lifecycle.drain_outbound())
            .await
            .expect("a closed queue has nothing to wait for");
    }

    #[tokio::test(start_paused = true)]
    async fn draining_gives_up_rather_than_waiting_on_a_writer_that_cannot_write() {
        // Nothing consumes the queue here, which is what a client that has
        // stopped reading its end of the pipe looks like from in here.
        let lifecycle = Lifecycle::default();
        let (outbound, _rx) = mpsc::unbounded_channel();
        lifecycle.set_outbound(outbound);

        tokio::time::timeout(FLUSH_TIMEOUT * 2, lifecycle.drain_outbound())
            .await
            .expect("the drain is bounded, so termination does not depend on the client");
    }
}
