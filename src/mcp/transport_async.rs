// Async stdio transport for MCP JSON-RPC 2.0 (feature: async-transport).
//
// Replaces the thread+mpsc synchronous transport with a tokio-based event
// loop. The Server struct remains synchronous (Aho-Corasick is CPU-bound);
// async wraps transport I/O only.
//
// Sampling support uses tokio::time::timeout instead of mpsc::recv_timeout.

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::tools::Server;
use super::types::{JsonRpcRequest, JsonRpcResponse, TransportError, INVALID_REQUEST, PARSE_ERROR};

/// Maximum line length (4 MiB), matching the sync transport.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// Run the MCP server on async stdin/stdout until EOF.
///
/// Uses a single-threaded tokio runtime. The Server is borrowed mutably
/// and all tool calls block the event loop (CPU-bound scan is fast enough
/// that this is acceptable for stdio; true concurrency requires
/// RwLock<Scanner> which is deferred).
pub fn run_async_stdio(server: &mut Server) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async_stdio_loop(server))
}

async fn async_stdio_loop(server: &mut Server) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();
    let mut raw_buf: Vec<u8> = Vec::new();
    let mut write_buf: Vec<u8> = Vec::new();

    // See sync transport for why this is unbounded: on_event sends from this
    // same thread during dispatch and we drain it here between requests.
    let (log_tx, log_rx) = std::sync::mpsc::channel::<crate::trace::McpLogMessage>();
    let mut mcp_logging = false;

    loop {
        raw_buf.clear();
        // Bounded read: read raw bytes up to the limit to avoid InvalidData
        // errors when the cap splits a multi-byte UTF-8 codepoint.
        let n = {
            let limited = (&mut reader).take((MAX_LINE_BYTES + 1) as u64);
            tokio::pin!(limited);
            limited.read_until(b'\n', &mut raw_buf).await?
        };
        if n == 0 {
            break; // EOF
        }
        if raw_buf.len() > MAX_LINE_BYTES {
            // Drain the remainder of the oversized line so the next
            // iteration starts at a fresh line boundary.  Use a small
            // scratch buffer instead of appending to raw_buf (which
            // would allow unbounded memory growth).
            if raw_buf.last() != Some(&b'\n') {
                loop {
                    let buf = reader.fill_buf().await?;
                    if buf.is_empty() {
                        break;
                    }
                    if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                        reader.consume(pos + 1);
                        break;
                    }
                    let len = buf.len();
                    reader.consume(len);
                }
            }
            let resp = JsonRpcResponse::error(None, INVALID_REQUEST, "request too large".into());
            if mcp_logging {
                drain_mcp_logs_async(&mut stdout, &log_rx, &mut write_buf).await?;
            }
            send_async(&mut stdout, &resp, &mut write_buf).await?;
            continue;
        }
        let Ok(line) = str::from_utf8(&raw_buf).map(str::trim) else {
            let resp = JsonRpcResponse::error(
                None,
                PARSE_ERROR,
                "request contains malformed UTF-8 character(s)".into(),
            );
            if mcp_logging {
                drain_mcp_logs_async(&mut stdout, &log_rx, &mut write_buf).await?;
            }
            send_async(&mut stdout, &resp, &mut write_buf).await?;
            continue;
        };

        if line.is_empty() {
            continue;
        }

        // Parse and validate the JSON-RPC request via shared parser.
        let mut req = match super::types::parse_jsonrpc_line(line) {
            Ok(r) => r,
            Err(TransportError::StaleResponse) => {
                tracing::debug!("discarding stale response-shaped message (no id)");
                continue;
            }
            Err(e) => {
                tracing::warn!("{e}");
                if mcp_logging {
                    drain_mcp_logs_async(&mut stdout, &log_rx, &mut write_buf).await?;
                }
                if let Some(resp) = e.into_response(None) {
                    send_async(&mut stdout, &resp, &mut write_buf).await?;
                }
                continue;
            }
        };

        // Dispatch synchronously. For tools/call with sampling, we fall back
        // to the synchronous SamplingBridge since the Server is not async.
        // The sampling bridge needs synchronous stdin access, so we use a
        // simple non-sampling dispatch for the async path.
        let resp = dispatch_async(server, &mut req);
        if server.supports_logging() && !mcp_logging {
            crate::trace::set_mcp_log_sender(Some(log_tx.clone()));
            mcp_logging = true;
        }
        // Emit logs accrued during dispatch before the response, matching the
        // sync transport's causal ordering.
        drain_mcp_logs_async(&mut stdout, &log_rx, &mut write_buf).await?;
        if let Some(resp) = resp {
            send_async(&mut stdout, &resp, &mut write_buf).await?;
        }
    }

    crate::trace::set_mcp_log_sender(None);
    Ok(())
}

async fn drain_mcp_logs_async(
    writer: &mut tokio::io::Stdout,
    log_rx: &std::sync::mpsc::Receiver<crate::trace::McpLogMessage>,
    buf: &mut Vec<u8>,
) -> Result<()> {
    let mut wrote = false;
    while let Ok(msg) = log_rx.try_recv() {
        buf.clear();
        serde_json::to_writer(
            &mut *buf,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/message",
                "params": msg,
            }),
        )?;
        buf.push(b'\n');
        writer.write_all(buf).await?;
        wrote = true;
    }
    if wrote {
        writer.flush().await?;
    }
    Ok(())
}

/// Dispatch a request. Sampling is not supported in the async transport
/// (sampling requires synchronous channel access; a full async sampling
/// bridge is a future enhancement).
fn dispatch_async(server: &mut Server, req: &mut JsonRpcRequest) -> Option<JsonRpcResponse> {
    // Pre-init routing (initialize, ping, notifications, rejection).
    if let Some(resp) = server.dispatch_preinit(req) {
        return resp;
    }

    // Async path: tools/call without sampling bridge, all others shared.
    if req.method == "tools/call" {
        Some(server.handle_tools_call(req, None))
    } else {
        server.dispatch_method(req)
    }
}

async fn send_async(
    writer: &mut tokio::io::Stdout,
    resp: &JsonRpcResponse,
    buf: &mut Vec<u8>,
) -> Result<()> {
    buf.clear();
    serde_json::to_writer(&mut *buf, resp)?;
    buf.push(b'\n');
    writer.write_all(buf).await?;
    writer.flush().await?;
    Ok(())
}
