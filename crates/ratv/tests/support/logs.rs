// Log capture for span tests. Included with `include!` by the tests that use it.
// The includer must import `std::sync::{Arc, Mutex}`.

/// Log text written by a `tracing_subscriber::fmt` subscriber.
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogBuffer {
    /// Collects all `ratv` logs until the guard is dropped. Keep the guard in
    /// a named variable. Only for a test runtime with one thread: the
    /// subscriber is set for this thread, so tasks on other threads do not
    /// use it.
    fn capture() -> (Self, tracing::subscriber::DefaultGuard) {
        use tracing_subscriber::util::SubscriberInitExt;
        // With only one dispatcher, `tracing` asks the dispatcher of the
        // thread that first logs at a place in the code (a callsite) if that
        // place is enabled. A test thread without a dispatcher then disables
        // the place for all threads. A second dispatcher that stays alive
        // stops this.
        static SECOND: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();
        SECOND.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::new()));
        let logs = Self::default();
        let writer = logs.clone();
        let guard = tracing_subscriber::fmt()
            .with_env_filter("ratv=trace")
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish()
            .set_default();
        // A thread that starts to register a callsite before this dispatcher
        // exists can store that the callsite is off, after the rebuild that
        // `set_default` runs.
        // A second rebuild turns such callsites on again.
        tracing::callsite::rebuild_interest_cache();
        (logs, guard)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    /// The innermost `session{...}` span of the first line with the message `message`.
    /// `message` can end with fields. A message with more words after
    /// `message` does not match.
    fn span_of(&self, message: &str) -> String {
        let text = self.text();
        let line = text
            .lines()
            .find(|line| {
                split_line(line).is_some_and(|(_, line_message)| {
                    line_message.strip_prefix(message).is_some_and(|rest| {
                        rest.is_empty()
                            || rest.strip_prefix(' ').is_some_and(|r| {
                                r.split(' ').next().is_some_and(|field| field.contains('='))
                            })
                    })
                })
            })
            .unwrap_or_else(|| panic!("no line with message {message:?} in:\n{text}"));
        session_span(line)
            .unwrap_or_else(|| panic!("no session span in: {line}"))
            .to_owned()
    }

    /// Panics if there are no logs, or if the innermost `session{...}` span
    /// of a line is not one of `spans`.
    fn assert_all_in(&self, spans: &[&str]) {
        let text = self.text();
        assert!(!text.is_empty(), "no logs");
        for line in text.lines() {
            assert!(
                session_span(line).is_some_and(|span| spans.contains(&span)),
                "not in {spans:?}: {line}"
            );
        }
    }

    /// Panics if there are no logs, or if the outermost `session{...}` span
    /// of a line is not one of `spans`.
    fn assert_all_under(&self, spans: &[&str]) {
        let text = self.text();
        assert!(!text.is_empty(), "no logs");
        for line in text.lines() {
            assert!(
                outer_session_span(line).is_some_and(|span| spans.contains(&span)),
                "not under {spans:?}: {line}"
            );
        }
    }
}

/// Splits a log line into its level with spans, and its message with fields.
/// The line is `LEVEL spans: target: message fields`, or
/// `LEVEL target: message fields` without a span. Spans must have fields: a
/// span without fields is not found. `None` if the line has another layout.
fn split_line(line: &str) -> Option<(Option<&str>, &str)> {
    let (spans, after) = match line.find("}: ") {
        Some(end) => (Some(&line[..=end]), &line[end + 3..]),
        None => (None, line.trim_start().split_once(' ')?.1),
    };
    let (_, message) = after.split_once(": ")?;
    Some((spans, message))
}

/// The innermost `session{...}` span of a log line.
fn session_span(line: &str) -> Option<&str> {
    session_span_at(line, |spans| spans.rfind("session{"))
}

/// The outermost `session{...}` span of a log line.
fn outer_session_span(line: &str) -> Option<&str> {
    session_span_at(line, |spans| spans.find("session{"))
}

/// The `session{...}` span of a log line that starts where `find` says.
fn session_span_at(line: &str, find: impl Fn(&str) -> Option<usize>) -> Option<&str> {
    let spans = split_line(line)?.0?;
    let start = find(spans)?;
    let end = start + spans[start..].find('}')?;
    Some(&spans[start..=end])
}

/// The `id` field of a `session{...}` span.
fn session_id(span: &str) -> &str {
    let start = span.find(" id=").expect("id field") + 4;
    span[start..]
        .split([' ', '}'])
        .next()
        .expect("id value")
}
