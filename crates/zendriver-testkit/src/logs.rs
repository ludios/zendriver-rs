//! Capture `tracing` output so a test can assert on what was logged, e.g.
//! that a clean shutdown logged no error.

use std::cell::OnceCell;
use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, SubscriberExt as _};
use tracing_subscriber::{EnvFilter, Layer};

/// One captured event.
#[derive(Clone, Debug)]
pub struct LogLine {
    pub level: Level,
    /// The event's target, usually its module path, e.g. `zendriver::browser`.
    pub target: String,
    pub message: String,
    /// The other fields, rendered as `name=value` separated by spaces.
    pub fields: String,
}

impl fmt::Display for LogLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {}", self.level, self.target, self.message)?;
        if !self.fields.is_empty() {
            write!(f, " {}", self.fields)?;
        }
        Ok(())
    }
}

/// Everything one thread has logged at `DEBUG` and above.
#[derive(Clone, Debug, Default)]
struct Buffer(Arc<Mutex<Vec<LogLine>>>);

#[derive(Default)]
struct LineVisitor {
    message: String,
    fields: String,
}

impl Visit for LineVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            // A `format_args!` whose `Debug` is the plain text.
            self.message = format!("{value:?}");
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(self.fields, "{}={value:?}", field.name());
        }
    }
}

impl<S: Subscriber> Layer<S> for Buffer {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut visitor = LineVisitor::default();
        event.record(&mut visitor);
        let line = LogLine {
            level: *event.metadata().level(),
            target: event.metadata().target().to_string(),
            message: visitor.message,
            fields: visitor.fields,
        };
        // A poisoned buffer means another thread panicked mid-push; that
        // panic is the one worth reading, so don't add a second.
        if let Ok(mut lines) = self.0.lock() {
            lines.push(line);
        }
    }
}

thread_local! {
    static THREAD_BUFFER: OnceCell<Buffer> = const { OnceCell::new() };
}

impl Buffer {
    /// This thread's buffer. The first call makes it the thread's subscriber
    /// for good, so captures never have to be undone in order. If `RUST_LOG`
    /// is set, events are also printed to the test output, filtered by it.
    fn of_this_thread() -> Self {
        THREAD_BUFFER.with(|cell| {
            cell.get_or_init(|| {
                let buffer = Self::default();
                let echo = std::env::var_os("RUST_LOG").map(|_| {
                    tracing_subscriber::fmt::layer()
                        .with_test_writer()
                        .with_filter(EnvFilter::from_default_env())
                });
                let subscriber = tracing_subscriber::registry()
                    .with(buffer.clone().with_filter(LevelFilter::DEBUG))
                    .with(echo);
                std::mem::forget(tracing::subscriber::set_default(subscriber));
                buffer
            })
            .clone()
        })
    }

    fn lines(&self) -> std::sync::MutexGuard<'_, Vec<LogLine>> {
        self.0.lock().expect("log buffer")
    }
}

/// What one thread logged at `DEBUG` and above, from some point on.
///
/// Thread-local, so it sees a task only if that task runs on this thread:
/// the caller must be on a current-thread runtime.
#[derive(Clone, Debug)]
pub struct Logs {
    buffer: Buffer,
    /// Where in `buffer` this view starts.
    from: usize,
}

impl Logs {
    /// What this thread logs from now on. Like [`crate::launch_for_test`],
    /// it sees a task only if the task runs on this thread.
    pub fn from_now_on() -> Self {
        let buffer = Buffer::of_this_thread();
        let from = buffer.lines().len();
        Self { buffer, from }
    }

    /// Every captured line, oldest first.
    pub fn lines(&self) -> Vec<LogLine> {
        self.buffer.lines()[self.from..].to_vec()
    }

    /// Captured lines at `level` or more severe: `at_least(Level::WARN)` is
    /// the warnings and errors.
    pub fn at_least(&self, level: Level) -> Vec<LogLine> {
        // `tracing` orders levels by verbosity: ERROR < WARN < ... < TRACE.
        let mut lines = self.lines();
        lines.retain(|line| line.level <= level);
        lines
    }

    /// Panic, listing them, if any lines at `level` or more severe were
    /// captured.
    pub fn assert_none_at_least(&self, level: Level) {
        let lines = self.at_least(level);
        assert!(
            lines.is_empty(),
            "expected nothing logged at {level} or above, got:\n{}",
            lines.iter().map(|l| format!("  {l}\n")).collect::<String>(),
        );
    }
}
