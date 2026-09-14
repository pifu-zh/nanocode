//! Terminal spinner — Rust port of `src/utils/streaming.ts`.
//! Braille frames at 80ms, 52 random verbs, gold-colored, right-aligned stats.

use rand::Rng;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const INTERVAL_MS: u64 = 80;

pub const SPINNER_VERBS: &[&str] = &[
    "Analyzing", "Architecting", "Bootstrapping", "Calculating", "Cerebrating",
    "Compiling", "Composing", "Computing", "Considering", "Constructing",
    "Crafting", "Debugging", "Deciphering", "Deliberating", "Designing",
    "Encoding", "Engineering", "Evaluating", "Exploring", "Formulating",
    "Generating", "Hypothesizing", "Implementing", "Inspecting", "Integrating",
    "Interpreting", "Investigating", "Iterating", "Mapping", "Navigating",
    "Optimizing", "Orchestrating", "Parsing", "Planning", "Pondering",
    "Processing", "Prototyping", "Querying", "Reasoning", "Refactoring",
    "Resolving", "Reviewing", "Searching", "Solving", "Structuring",
    "Synthesizing", "Thinking", "Transforming", "Understanding", "Validating",
    "Wrangling", "Writing", "Zigzagging",
];

pub struct SpinnerHandle {
    stopped: Arc<AtomicBool>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for SpinnerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

impl SpinnerHandle {
    pub fn stop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            join.abort();
        }
        if std::io::stdout().is_terminal() {
            print!("\r\x1b[K");
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    }
}

/// Start a spinner with a random verb (or an explicit message).
pub fn spinner(message: Option<&str>) -> SpinnerHandle {
    let verb = message
        .map(String::from)
        .unwrap_or_else(|| format!("{}…", SPINNER_VERBS[rand::thread_rng().gen_range(0..SPINNER_VERBS_LEN)]));
    const SPINNER_VERBS_LEN: usize = 53;

    let stopped = Arc::new(AtomicBool::new(false));
    let stopped_clone = stopped.clone();
    let is_tty = std::io::stdout().is_terminal();

    let join = tokio::spawn(async move {
        let mut frame_index = 0usize;
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(INTERVAL_MS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        if is_tty {
            print!("\r\x1b[K  \x1b[38;2;230;190;80m{} {verb}\x1b[0m", FRAMES[0]);
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
        loop {
            if stopped_clone.load(Ordering::SeqCst) {
                return;
            }
            interval.tick().await;
            if stopped_clone.load(Ordering::SeqCst) {
                return;
            }
            frame_index += 1;
            if is_tty {
                print!(
                    "\r\x1b[K  \x1b[38;2;230;190;80m{} {verb}\x1b[0m",
                    FRAMES[frame_index % FRAMES.len()]
                );
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
        }
    });

    SpinnerHandle { stopped, join: Some(join) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spinner_starts_and_stops() {
        {
            let mut s = spinner(Some("Testing…"));
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            s.stop();
        }
        // no panic = good; cleanup flushed
    }

    #[test]
    fn verbs_present() {
        assert_eq!(SPINNER_VERBS.len(), 53);
        assert!(SPINNER_VERBS.contains(&"Pondering"));
    }
}
