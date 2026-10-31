//! Capture `tracing` output so a test can assert on what was logged, e.g.
//! that a clean shutdown logged no error.

use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::subscriber::DefaultGuard;
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

/// Events captured at `DEBUG` and above.
#[derive(Clone, Debug, Default)]
pub struct Logs(Arc<Mutex<Vec<LogLine>>>);

impl Logs {
    /// Every captured line, oldest first.
    pub fn lines(&self) -> Vec<LogLine> {
        self.0.lock().expect("log buffer").clone()
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

    /// Capture this thread's events into `self` until the guard drops. If
    /// `RUST_LOG` is set, also print them to the test output, filtered by it.
    ///
    /// Thread-local, so it sees a task only if that task runs on this
    /// thread; the caller must be on a current-thread runtime.
    pub(crate) fn capture(&self) -> DefaultGuard {
        let echo = std::env::var_os("RUST_LOG").map(|_| {
            tracing_subscriber::fmt::layer()
                .with_test_writer()
                .with_filter(EnvFilter::from_default_env())
        });
        let subscriber = tracing_subscriber::registry()
            .with(self.clone().with_filter(LevelFilter::DEBUG))
            .with(echo);
        tracing::subscriber::set_default(subscriber)
    }
}

impl<S: Subscriber> Layer<S> for Logs {
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
