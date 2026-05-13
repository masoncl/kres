//! Token/cost accounting, per-role and per-model.
//!
//! The `/cost` printed per-role accumulated token
//! usage from every API round. Same shape here, made concurrency-safe
//! by keeping the accumulator under a Mutex.

use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UsageEntry {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub calls: u64,
}

impl UsageEntry {
    pub fn add(&mut self, other: &UsageEntry) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(other.cache_creation_input_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(other.cache_read_input_tokens);
        self.calls = self.calls.saturating_add(other.calls);
    }
}

/// Key under which we accumulate: (role, model).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UsageKey {
    pub role: String,
    pub model: String,
}

#[derive(Debug, Default)]
pub struct UsageTracker {
    inner: Mutex<BTreeMap<UsageKey, UsageEntry>>,
}

impl UsageTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(
        &self,
        role: impl Into<String>,
        model: impl Into<String>,
        input_tokens: u64,
        output_tokens: u64,
        cache_creation_input_tokens: u64,
        cache_read_input_tokens: u64,
    ) {
        let key = UsageKey {
            role: role.into(),
            model: model.into(),
        };
        let mut guard = self.inner.lock().unwrap();
        let entry = guard.entry(key).or_default();
        entry.add(&UsageEntry {
            input_tokens,
            output_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            calls: 1,
        });
    }

    pub fn snapshot(&self) -> Vec<(UsageKey, UsageEntry)> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    }

    pub fn totals(&self) -> UsageEntry {
        let g = self.inner.lock().unwrap();
        let mut total = UsageEntry::default();
        for v in g.values() {
            total.add(v);
        }
        total
    }

    pub fn reset(&self) {
        self.inner.lock().unwrap().clear();
    }
}

pub fn format_usage_summary(
    usage: &UsageTracker,
    label: &str,
    empty_message: Option<&str>,
) -> Option<String> {
    let snap = usage.snapshot();
    if snap.is_empty() {
        return empty_message.map(str::to_string);
    }
    let total = usage.totals();
    let mut out = format!("{label} ({} call(s) total):", total.calls);
    for (k, e) in &snap {
        out.push_str(&format!(
            "\n  {:>4}/{:<24}  {:>4}x  in={:>9}  out={:>9}  cache_create={:>9}  cache_read={:>9}",
            k.role,
            k.model,
            e.calls,
            format_token_count(e.input_tokens),
            format_token_count(e.output_tokens),
            format_token_count(e.cache_creation_input_tokens),
            format_token_count(e.cache_read_input_tokens),
        ));
    }
    out.push_str(&format!(
        "\n  total         {:>4}x  in={:>9}  out={:>9}  cache_create={:>9}  cache_read={:>9}",
        total.calls,
        format_token_count(total.input_tokens),
        format_token_count(total.output_tokens),
        format_token_count(total.cache_creation_input_tokens),
        format_token_count(total.cache_read_input_tokens),
    ));
    Some(out)
}

pub fn format_token_count(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    if n < 1_000_000 {
        return format!("{:.1}k", n as f64 / 1_000.0);
    }
    format!("{:.2}M", n as f64 / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_sums() {
        let t = UsageTracker::new();
        t.record("fast", "opus-4-7", 100, 20, 0, 0);
        t.record("fast", "opus-4-7", 50, 10, 0, 0);
        t.record("slow", "opus-4-7", 1000, 500, 0, 0);
        let snap = t.snapshot();
        assert_eq!(snap.len(), 2);
        let fast = snap.iter().find(|(k, _)| k.role == "fast").unwrap().1;
        assert_eq!(fast.input_tokens, 150);
        assert_eq!(fast.output_tokens, 30);
        assert_eq!(fast.calls, 2);
        let total = t.totals();
        assert_eq!(total.input_tokens, 1150);
        assert_eq!(total.output_tokens, 530);
        assert_eq!(total.calls, 3);
    }

    #[test]
    fn formats_usage_summary() {
        let t = UsageTracker::new();
        t.record("fast", "claude", 12_300, 400, 1000, 9000);

        let out = format_usage_summary(&t, "usage", None).unwrap();

        assert!(out.contains("usage (1 call(s) total):"));
        assert!(out.contains("fast/claude"));
        assert!(out.contains("in=    12.3k"));
        assert!(out.contains("cache_create=     1.0k"));
        assert!(out.contains("cache_read=     9.0k"));
    }

    #[test]
    fn formats_empty_usage_only_when_requested() {
        let t = UsageTracker::new();

        assert_eq!(format_usage_summary(&t, "usage", None), None);
        assert_eq!(
            format_usage_summary(&t, "usage", Some("empty")),
            Some("empty".to_string())
        );
    }

    #[test]
    fn reset_clears() {
        let t = UsageTracker::new();
        t.record("x", "m", 10, 10, 0, 0);
        t.reset();
        assert_eq!(t.totals().calls, 0);
    }

    #[test]
    fn concurrent_records_do_not_lose_counts() {
        use std::sync::Arc;
        use std::thread;
        let t = Arc::new(UsageTracker::new());
        let mut hs = vec![];
        for _ in 0..8 {
            let t2 = t.clone();
            hs.push(thread::spawn(move || {
                for _ in 0..100 {
                    t2.record("fast", "m", 1, 1, 0, 0);
                }
            }));
        }
        for h in hs {
            h.join().unwrap();
        }
        assert_eq!(t.totals().calls, 800);
    }
}
