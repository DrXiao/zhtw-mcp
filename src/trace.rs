use std::collections::BTreeMap;
use std::sync::{mpsc, Mutex, OnceLock};

use serde::Serialize;
use serde_json::{json, Value};
use tracing::field::Field;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::field::Visit;
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, Layer};

/// This crate's own target. `tracing` defaults a target to `module_path!()`,
/// so the crate name is what its own events carry; taking it from Cargo means
/// renaming the package cannot quietly stop log forwarding.
const CRATE_TARGET: &str = env!("CARGO_CRATE_NAME");

/// Whether an event belongs to this crate rather than merely starting with its
/// name: `zhtw_mcp_helper` would be a different crate.
fn is_ours(target: &str) -> bool {
    target
        .strip_prefix(CRATE_TARGET)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
}

static MCP_LOG_TX: OnceLock<Mutex<Option<mpsc::Sender<McpLogMessage>>>> = OnceLock::new();

/// One tracing event on its way to the client as `notifications/message`.
///
/// The level is the SDK's own enum rather than a string: it is what the
/// client asked for in `logging/setLevel`, what the severity filter compares,
/// and what goes on the wire, so keeping it typed means those three agree
/// without a table translating between them at each step.
#[allow(deprecated)]
#[derive(Clone, Debug, Serialize)]
pub struct McpLogMessage {
    pub level: rmcp::model::LoggingLevel,
    pub logger: &'static str,
    pub data: Value,
}

pub fn init(default_level: &str) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(McpLogLayer)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr));
    let _ = tracing::subscriber::set_global_default(subscriber);
}

pub fn set_mcp_log_sender(tx: Option<mpsc::Sender<McpLogMessage>>) {
    // A poisoned lock means a log-forwarding thread panicked while holding it.
    // `McpLogLayer::on_event` already degrades to "no forwarding" in that case,
    // so panicking here would take the server down over a logging side channel.
    // Recover the guard and carry on.
    let slot = MCP_LOG_TX.get_or_init(|| Mutex::new(None));
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = tx;
}

struct McpLogLayer;

impl<S> Layer<S> for McpLogLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        #[allow(deprecated)]
        let level = match *event.metadata().level() {
            Level::ERROR => rmcp::model::LoggingLevel::Error,
            Level::WARN => rmcp::model::LoggingLevel::Warning,
            Level::INFO => rmcp::model::LoggingLevel::Info,
            _ => return,
        };

        // Only this crate's events go to the client. The MCP SDK's own internal
        // tracing is not something a client asked for, and letting it through
        // would put unrelated notifications ahead of the ones a request
        // actually produced.
        //
        // The crate itself or a module inside it, not merely a target that
        // starts with the same letters: `zhtw_mcp_helper` is a different crate
        // and its logs are no more ours than the SDK's.
        if !is_ours(event.metadata().target()) {
            return;
        }
        let Some(tx) = MCP_LOG_TX
            .get()
            .and_then(|slot| slot.lock().ok().and_then(|guard| guard.clone()))
        else {
            return;
        };

        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        let data = json!({
            "message": visitor.message.unwrap_or_else(|| event.metadata().target().to_string()),
            "target": event.metadata().target(),
            "fields": visitor.fields,
        });
        let _ = tx.send(McpLogMessage {
            level,
            logger: "zhtw-mcp",
            data,
        });
    }
}

#[derive(Default)]
struct EventVisitor {
    message: Option<String>,
    fields: BTreeMap<String, Value>,
}

impl Visit for EventVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let value = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(value);
        } else {
            self.fields
                .insert(field.name().to_string(), Value::String(value));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.fields
                .insert(field.name().to_string(), Value::String(value.to_string()));
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.insert(field.name().to_string(), json!(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.insert(field.name().to_string(), json!(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields.insert(field.name().to_string(), json!(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One at a time: the sender these tests install is process-global, so
    /// two of them running at once would each receive the other's events.
    static SENDER: Mutex<()> = Mutex::new(());

    /// Collect what the layer forwards for one closure's worth of events.
    ///
    /// The subscriber is installed for this thread only, so events from tests
    /// running beside this one do not reach the layer.
    fn forwarded(emit: impl FnOnce()) -> Vec<McpLogMessage> {
        let _installed = SENDER.lock().unwrap_or_else(|e| e.into_inner());
        let (tx, rx) = mpsc::channel();
        set_mcp_log_sender(Some(tx));
        let subscriber = tracing_subscriber::registry().with(McpLogLayer);
        tracing::subscriber::with_default(subscriber, emit);
        set_mcp_log_sender(None);
        rx.try_iter().collect()
    }

    #[test]
    fn an_event_arrives_as_message_target_and_typed_fields() {
        // The payload a client actually renders. Nothing else asserts its
        // shape, so a change to the visitor would otherwise only show up as a
        // client displaying nothing.
        let sent = forwarded(|| {
            tracing::info!(
                count = 3u64,
                signed = -1i64,
                ok = true,
                name = "zhtw",
                "scan done"
            );
        });

        assert_eq!(sent.len(), 1, "one event, one notification");
        let data = &sent[0].data;
        assert_eq!(data["message"], "scan done");
        assert_eq!(data["target"], "zhtw_mcp::trace::tests");

        // Numbers and booleans stay typed rather than being stringified, which
        // is the whole reason the visitor implements more than record_debug.
        assert_eq!(data["fields"]["count"], 3);
        assert_eq!(data["fields"]["signed"], -1);
        assert_eq!(data["fields"]["ok"], true);
        assert_eq!(data["fields"]["name"], "zhtw");
    }

    #[test]
    #[allow(deprecated)]
    fn levels_map_and_anything_below_info_is_dropped() {
        {
            let sent = forwarded(|| {
                tracing::error!("bad");
                tracing::warn!("iffy");
                tracing::info!("fine");
                tracing::debug!("noise");
                tracing::trace!("more noise");
            });
            let levels: Vec<_> = sent.iter().map(|m| m.level).collect();
            assert_eq!(
                levels,
                vec![
                    rmcp::model::LoggingLevel::Error,
                    rmcp::model::LoggingLevel::Warning,
                    rmcp::model::LoggingLevel::Info,
                ],
                "debug and trace are not what a client asked for"
            );
        }
    }

    #[test]
    fn another_crates_events_are_not_forwarded() {
        // The SDK's own tracing would otherwise arrive interleaved with the
        // notifications a request produced.
        let sent = forwarded(|| {
            tracing::info!(target: "rmcp::service", "sdk internals");
            // Shares our first eleven characters and is still not ours.
            tracing::info!(target: "zhtw_mcp_helper", "a different crate");
            tracing::info!(target: "zhtw_mcp", "the crate itself");
            tracing::info!(target: "zhtw_mcp::engine", "a module of ours");
        });
        let targets: Vec<_> = sent.iter().map(|m| m.data["target"].clone()).collect();
        assert_eq!(targets, vec!["zhtw_mcp", "zhtw_mcp::engine"]);
    }

    #[test]
    fn a_message_less_event_falls_back_to_its_target() {
        let sent = forwarded(|| tracing::info!(rule = "ZY5"));
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].data["message"], "zhtw_mcp::trace::tests");
        assert_eq!(sent[0].data["fields"]["rule"], "ZY5");
    }
}
