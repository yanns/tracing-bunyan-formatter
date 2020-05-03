use crate::storage_layer::JsonStorage;
use serde::ser::{SerializeMap, Serializer};
use serde_json::Value;
use std::fmt;
use std::io::Write;
use tracing::{Event, Id, Subscriber};
use tracing_core::metadata::Level;
use tracing_log::AsLog;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::SpanRef;
use tracing_subscriber::Layer;

/// Keys for core fields of the Bunyan format (https://github.com/trentm/node-bunyan#core-fields)
const BUNYAN_VERSION: &str = "v";
const LEVEL: &str = "level";
const NAME: &str = "name";
const HOSTNAME: &str = "hostname";
const PID: &str = "pid";
const TIME: &str = "time";
const MESSAGE: &str = "msg";
const _SOURCE: &str = "src";

/// Convert from log levels to Bunyan's levels.
fn to_bunyan_level(level: &Level) -> u16 {
    match level.as_log() {
        log::Level::Error => 50,
        log::Level::Warn => 40,
        log::Level::Info => 30,
        log::Level::Debug => 20,
        log::Level::Trace => 10,
    }
}

/// This layer is exclusively concerned with formatting information using the [Bunyan format](https://github.com/trentm/node-bunyan).
/// It relies on the upstream `JsonStorageLayer` to get access to the fields attached to
/// each span.
pub struct BunyanFormattingLayer<W: MakeWriter + 'static> {
    make_writer: W,
    pid: u32,
    hostname: String,
    bunyan_version: u8,
    name: String,
}

impl<W: MakeWriter + 'static> BunyanFormattingLayer<W> {
    /// Create a new `BunyanFormattingLayer`.
    ///
    /// You have to specify:
    /// - a `name`, which will be attached to all formatted records according to the [Bunyan format](https://github.com/trentm/node-bunyan#log-record-fields);
    /// - a `make_writer`, which will be used to get a `Write` instance to write formatted records to.
    ///
    /// ## Using stdout
    /// ```rust
    /// use tracing_bunyan_formatter::BunyanFormattingLayer;
    ///
    /// let formatting_layer = BunyanFormattingLayer::new("tracing_example".into(), std::io::stdout);
    /// ```
    ///
    /// If you prefer, you can use closure syntax:
    /// ```rust
    /// use tracing_bunyan_formatter::BunyanFormattingLayer;
    ///
    /// let formatting_layer = BunyanFormattingLayer::new("tracing_example".into(), || std::io::stdout());
    /// ```
    pub fn new(name: String, make_writer: W) -> Self {
        Self {
            make_writer,
            name,
            pid: std::process::id(),
            hostname: gethostname::gethostname().to_string_lossy().into_owned(),
            bunyan_version: 0,
        }
    }

    fn serialize_bunyan_core_fields(
        &self,
        map_serializer: &mut impl SerializeMap<Error = serde_json::Error>,
        message: &str,
        level: &Level,
    ) -> Result<(), std::io::Error> {
        map_serializer.serialize_entry(BUNYAN_VERSION, &self.bunyan_version)?;
        map_serializer.serialize_entry(NAME, &self.name)?;
        map_serializer.serialize_entry(MESSAGE, &message)?;
        map_serializer.serialize_entry(LEVEL, &to_bunyan_level(level))?;
        map_serializer.serialize_entry(HOSTNAME, &self.hostname)?;
        map_serializer.serialize_entry(PID, &self.pid)?;
        map_serializer.serialize_entry(TIME, &chrono::Utc::now().to_rfc3339())?;
        Ok(())
    }

    /// Given a span, it serialised it to a in-memory buffer (vector of bytes).
    fn serialize_span<S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>>(
        &self,
        span: &SpanRef<S>,
        ty: Type,
    ) -> Result<Vec<u8>, std::io::Error> {
        let mut buffer = Vec::new();
        let mut serializer = serde_json::Serializer::new(&mut buffer);
        let mut map_serializer = serializer.serialize_map(None)?;
        let message = format_span_context(&span, ty);
        self.serialize_bunyan_core_fields(&mut map_serializer, &message, span.metadata().level())?;

        let extensions = span.extensions();
        if let Some(visitor) = extensions.get::<JsonStorage>() {
            for (key, value) in visitor.values() {
                map_serializer.serialize_entry(key, value)?;
            }
        }
        map_serializer.end()?;
        Ok(buffer)
    }

    /// Given an in-memory buffer holding a complete serialised record, flush it to the writer
    /// returned by self.make_writer.
    ///
    /// We add a trailing new-line at the end of the serialised record.
    ///
    /// If we write directly to the writer returned by self.make_writer in more than one go
    /// we can end up with broken/incoherent bits and pieces of those records when
    /// running multi-threaded/concurrent programs.
    fn emit(&self, mut buffer: Vec<u8>) -> Result<(), std::io::Error> {
        buffer.write_all(b"\n")?;
        self.make_writer.make_writer().write_all(&buffer)
    }
}

/// The type of record we are dealing with: entering a span, exiting a span, an event.
#[derive(Clone, Debug)]
pub enum Type {
    EnterSpan,
    ExitSpan,
    Event,
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let repr = match self {
            Type::EnterSpan => "START",
            Type::ExitSpan => "END",
            Type::Event => "EVENT",
        };
        write!(f, "{}", repr)
    }
}

/// Ensure consistent formatting of the span context.
///
/// Example: "[AN_INTERESTING_SPAN - START]"
fn format_span_context<S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>>(
    span: &SpanRef<S>,
    ty: Type,
) -> String {
    format!("[{} - {}]", span.metadata().name().to_uppercase(), ty)
}

/// Ensure consistent formatting of event message.
///
/// Examples:
/// - "[AN_INTERESTING_SPAN - EVENT] My event message" (for an event with a parent span)
/// - "My event message" (for an event without a parent span)
fn format_event_message<S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>>(
    current_span: &Option<SpanRef<S>>,
    event: &Event,
    event_visitor: &JsonStorage<'_>,
) -> String {
    // Extract the "message" field, if provided. Fallback to the target, if missing.
    let mut message = event_visitor
        .values()
        .get("message")
        .map(|v| match v {
            Value::String(s) => Some(s.as_str()),
            _ => None,
        })
        .flatten()
        .unwrap_or_else(|| event.metadata().target())
        .to_owned();

    // If the event is in the context of a span, prepend the span name to the message.
    if let Some(span) = &current_span {
        message = format!("{} {}", format_span_context(span, Type::Event), message);
    }

    message
}

impl<S, W> Layer<S> for BunyanFormattingLayer<W>
where
    S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    W: MakeWriter + 'static,
{
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        // Events do not necessarily happen in the context of a span, hence lookup_current
        // returns an `Option<SpanRef<_>>` instead of a `SpanRef<_>`.
        let current_span = ctx.lookup_current();

        let mut event_visitor = JsonStorage::default();
        event.record(&mut event_visitor);

        // Opting for a closure to use the ? operator and get more linear code.
        let format = || {
            let mut buffer = Vec::new();

            let mut serializer = serde_json::Serializer::new(&mut buffer);
            let mut map_serializer = serializer.serialize_map(None)?;

            let message = format_event_message(&current_span, event, &event_visitor);
            self.serialize_bunyan_core_fields(
                &mut map_serializer,
                &message,
                event.metadata().level(),
            )?;

            // Add all the other fields associated with the event, expect the message we already used.
            for (key, value) in event_visitor
                .values()
                .iter()
                .filter(|(&key, _)| key != "message")
            {
                map_serializer.serialize_entry(key, value)?;
            }

            // Add all the fields from the current span, if we have one.
            if let Some(span) = &current_span {
                let extensions = span.extensions();
                if let Some(visitor) = extensions.get::<JsonStorage>() {
                    for (key, value) in visitor.values() {
                        map_serializer.serialize_entry(key, value)?;
                    }
                }
            }
            map_serializer.end()?;
            Ok(buffer)
        };

        let result: std::io::Result<Vec<u8>> = format();
        if let Ok(formatted) = result {
            let _ = self.emit(formatted);
        }
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        let span = ctx.span(id).expect("Span not found, this is a bug");
        if let Ok(serialized) = self.serialize_span(&span, Type::EnterSpan) {
            let _ = self.emit(serialized);
        }
    }

    fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
        let span = ctx.span(id).expect("Span not found, this is a bug");
        if let Ok(serialized) = self.serialize_span(&span, Type::ExitSpan) {
            let _ = self.emit(serialized);
        }
    }
}
