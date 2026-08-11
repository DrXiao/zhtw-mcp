// Incremental scan cache for CLI lint.
//
// Two-tier lookup to avoid reading files on cache hit:
//   1. Fast path: stat(file) -> check (path, mtime, size, params) -> return
//      cached ScanOutput without reading the file.
//   2. Slow path (mtime miss): read file, blake3 hash, full cache key check.
//
// TTL-based expiry (default 24h) and MAX_ENTRIES cap prevent unbounded growth.
// Atomic writes via tempfile+rename with flock serialization.
//
// MCP path does NOT use this cache (stateless by design).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::engine::scan::ScanOutput;

/// Default TTL in seconds (24 hours).
const DEFAULT_TTL_SECS: u64 = 24 * 60 * 60;

/// Maximum number of cached entries.  Prevents unbounded growth when
/// scanning large monorepos.  Oldest entries evicted first on overflow.
const MAX_ENTRIES: usize = 2000;

/// BLAKE3 hash of `data`, returned as a 64-char lowercase hex string.
/// ~3-4x faster than SHA-256 thanks to SIMD acceleration.  Non-cryptographic
/// use (local cache keys) makes this a safe choice.
fn blake3_hex(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

fn default_translationese_domain() -> String {
    "general".to_owned()
}

/// Filesystem metadata used for the fast-path cache check.
/// Avoids reading the file and computing a content hash when mtime+size match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileMeta {
    mtime_secs: u64,
    size: u64,
}

/// Scan parameters that affect output (excluding file content).
#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanParams {
    pub ruleset_hash: String,
    pub profile: String,
    pub content_type: String,

    // Currently always "None" because caching is disabled when fix_mode is
    // active. Kept for forward-compatibility.
    pub fix_mode: String,
    // Whether AI detection is active — changes scan results.
    pub detect_ai: bool,
    // Whether translationese detection is active — changes scan results.
    pub detect_translationese: bool,

    // Translationese domain calibration — changes thresholds and serialized
    // reports.
    #[serde(default = "default_translationese_domain")]
    pub translationese_domain: String,

    // AI threshold level (formatted f32) — different multipliers produce
    // different results.
    pub ai_threshold: String,

    // Markdown blockquote-exemption flag — changes which spans get scanned, so
    // cache hits must be invalidated when toggled.
    #[serde(default)]
    pub exempt_blockquotes: bool,

    // Build identity of the scanner itself. `ruleset_hash` covers the rules but
    // not the passes that interpret them, so without this an upgrade that
    // changes a detector keeps serving the old binary's results for every
    // unchanged file until the 24-hour TTL expires. Carries the crate version
    // plus a hash of the scanner sources (see `emit_engine_fingerprint` in
    // build.rs), because a version alone only moves at a release bump and would
    // miss exactly the source-build case this is meant to cover. Entries
    // written before this field existed deserialize with an empty string and
    // therefore miss, which is the intended outcome and is pinned by
    // `legacy_entries_without_engine_version_miss`.
    #[serde(default)]
    pub engine_version: String,
}

/// A single cached entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    file_path: String,
    content_hash: String,
    file_meta: FileMeta,
    params: ScanParams,
    output: ScanOutput,
    input_was_sc: bool,
    #[serde(default)]
    text_char_count: usize,
    timestamp_secs: u64,
}

/// Persistent scan cache backed by a JSON file.
/// Entries are loaded lazily on first access to avoid upfront I/O
/// and deserialization cost when all files are new/modified.
#[derive(Debug)]
pub struct ScanCache {
    path: PathBuf,
    entries: Option<HashMap<String, CacheEntry>>,
    ttl_secs: u64,
    dirty: bool,
}

/// Cached scan result including script classification.
pub struct CacheHit {
    pub output: ScanOutput,
    pub input_was_sc: bool,
    pub text_char_count: usize,
}

/// Result of a cache lookup.
pub enum CacheResult {
    /// Fast-path hit: mtime+size match, no file read needed.
    Hit(Box<CacheHit>),
    /// mtime changed or no entry: caller must read file and call
    /// `check_content`.
    Miss,
}

impl CacheResult {
    /// Extract the cached hit, or None on miss.
    pub fn into_hit(self) -> Option<CacheHit> {
        match self {
            CacheResult::Hit(h) => Some(*h),
            CacheResult::Miss => None,
        }
    }
}

impl ScanCache {
    /// Open (or create) the scan cache at the default location.
    pub fn open_default() -> Self {
        Self::open(default_cache_path())
    }

    /// Open (or create) the scan cache at a specific path.
    /// Entries are NOT loaded until the first lookup; this avoids
    /// deserializing 2000 entries when all files are cache-misses.
    pub fn open(path: PathBuf) -> Self {
        ScanCache {
            path,
            entries: None,
            ttl_secs: DEFAULT_TTL_SECS,
            dirty: false,
        }
    }

    /// Ensure entries are loaded from disk (lazy initialization), and return
    /// them. Returning the reference is what keeps the invariant in the type
    /// rather than in the caller: there is no way to observe `entries` as
    /// `None` after calling this.
    fn ensure_loaded(&mut self) -> &mut HashMap<String, CacheEntry> {
        let (path, ttl) = (&self.path, self.ttl_secs);
        self.entries.get_or_insert_with(|| load_entries(path, ttl))
    }

    /// Get a reference to entries, loading if necessary.
    fn entries(&mut self) -> &HashMap<String, CacheEntry> {
        self.ensure_loaded()
    }

    /// Get a mutable reference to entries, loading if necessary.
    fn entries_mut(&mut self) -> &mut HashMap<String, CacheEntry> {
        self.ensure_loaded()
    }

    /// Fast-path lookup using filesystem metadata (mtime + size).
    /// Avoids reading the file when metadata matches.
    pub fn check_fast(
        &mut self,
        file_path: &str,
        mtime_secs: u64,
        size: u64,
        params: &ScanParams,
    ) -> CacheResult {
        let ttl = self.ttl_secs;
        let entries = self.ensure_loaded();
        if let Some(entry) = entries.get(&fast_key(file_path, params)) {
            if now_secs().saturating_sub(entry.timestamp_secs) <= ttl
                && entry.file_meta.mtime_secs == mtime_secs
                && entry.file_meta.size == size
                && (size == 0 || entry.text_char_count > 0)
            {
                return CacheResult::Hit(Box::new(CacheHit {
                    output: entry.output.clone(),
                    input_was_sc: entry.input_was_sc,
                    text_char_count: entry.text_char_count,
                }));
            }
        }
        CacheResult::Miss
    }

    /// Slow-path lookup using content hash.  Called after file is read.
    /// Returns cached output if content hash matches (mtime changed but
    /// content didn't, e.g. after `touch`).
    pub fn check_content(
        &mut self,
        file_path: &str,
        content: &[u8],
        params: &ScanParams,
    ) -> Option<CacheHit> {
        let ttl = self.ttl_secs;
        let entry = self.entries().get(&fast_key(file_path, params))?;
        if now_secs().saturating_sub(entry.timestamp_secs) > ttl {
            return None;
        }
        if !content.is_empty() && entry.text_char_count == 0 {
            return None;
        }
        (entry.content_hash == blake3_hex(content)).then(|| CacheHit {
            output: entry.output.clone(),
            input_was_sc: entry.input_was_sc,
            text_char_count: entry.text_char_count,
        })
    }

    /// Store a scan result in the cache.
    #[allow(clippy::too_many_arguments)]
    pub fn put(
        &mut self,
        file_path: &str,
        content: &[u8],
        mtime_secs: u64,
        size: u64,
        params: &ScanParams,
        output: ScanOutput,
        input_was_sc: bool,
        text_char_count: usize,
    ) {
        self.entries_mut().insert(
            fast_key(file_path, params),
            CacheEntry {
                file_path: file_path.to_owned(),
                content_hash: blake3_hex(content),
                file_meta: FileMeta { mtime_secs, size },
                params: params.clone(),
                output,
                input_was_sc,
                text_char_count,
                timestamp_secs: now_secs(),
            },
        );
        self.dirty = true;
    }

    /// Flush dirty cache to disk.  Prunes expired and overflow entries
    /// before writing.  Uses tempfile + rename for atomic writes.
    /// Acquires an exclusive flock to prevent concurrent CLI processes
    /// from clobbering each other's writes.
    /// Errors are silently ignored (cache is best-effort).
    pub fn flush(&mut self) {
        if !self.dirty {
            return;
        }

        // Destructured so the entries borrow stays independent of `path`, which
        // the write below needs while `entries` is still live.
        let Self {
            path,
            entries,
            ttl_secs: ttl,
            ..
        } = self;
        let ttl = *ttl;
        let entries = entries.get_or_insert_with(|| load_entries(path, ttl));

        // Prune expired entries.
        let now = now_secs();
        entries.retain(|_, e| now.saturating_sub(e.timestamp_secs) <= ttl);

        // Evict oldest entries if over the cap.
        if entries.len() > MAX_ENTRIES {
            let mut by_time: Vec<(String, u64)> = entries
                .iter()
                .map(|(k, e)| (k.clone(), e.timestamp_secs))
                .collect();
            by_time.sort_by_key(|(_, ts)| *ts);
            let to_remove = entries.len() - MAX_ENTRIES;
            for (k, _) in by_time.into_iter().take(to_remove) {
                entries.remove(&k);
            }
        }

        let entries_vec: Vec<&CacheEntry> = entries.values().collect();
        let Ok(bytes) = serde_json::to_vec(&entries_vec) else {
            return;
        };

        // Acquire exclusive lock on the cache file (or a .lock sidecar) to
        // prevent concurrent CLI processes from clobbering writes.
        let lock_path = self.path.with_extension("lock");
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&lock_path);

        // Best-effort lock: if another process holds it, skip this flush but
        // keep dirty=true so Drop retries.
        let locked = lock_file
            .as_ref()
            .is_ok_and(|f| f.try_lock_exclusive().is_ok());

        // Write without lock if lock file creation failed; skip entirely if
        // lock is held by another process.
        if (locked || lock_file.is_err()) && atomic_write(&self.path, &bytes) {
            self.dirty = false;
        }

        if let Ok(ref f) = lock_file {
            let _ = f.unlock();
        }
    }
}

impl Drop for ScanCache {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Atomic write via [`crate::atomic::replace_file`], reporting success as a
/// bool because the caller only needs to know whether it may clear `dirty`.
///
/// Failure stays non-fatal, but it is logged rather than swallowed: a cache
/// that can never write is otherwise invisible, and the only symptom is
/// every run being slow for no stated reason.
///
/// At `warn` rather than `debug` because the default subscriber filter is
/// `warn`, so anything quieter is dropped unless the user already suspects
/// something and passes `--debug`.  That is the wrong way round for a
/// symptom nobody would think to look for.  Matches how the override and
/// judgment caches report their own store failures.
fn atomic_write(dest: &Path, bytes: &[u8]) -> bool {
    match crate::atomic::replace_file(dest, bytes) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("scan cache write to {} failed: {e}", dest.display());
            false
        }
    }
}

/// Default cache file location: ~/.cache/zhtw-mcp/scan-cache.json
fn default_cache_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("zhtw-mcp")
        .join("scan-cache.json")
}

/// Lookup key combining file path + scan parameters.
/// Hashes directly into blake3 without allocating an intermediate String.
/// One entry per (file, params) tuple — mtime/content validated on lookup.
fn fast_key(file_path: &str, params: &ScanParams) -> String {
    // Serialize the pair rather than listing fields. JSON is self-delimiting,
    // with quoted strings and named keys, so distinct pairs cannot produce the
    // same bytes, and a field added to ScanParams later joins the key without
    // anyone remembering to add it here. Enumerating fields is how the
    // blockquote flag and the effective profile came to be ignored for
    // invalidation, twice, with a stale strict-lint result as the symptom.
    let json = serde_json::to_vec(&(file_path, params)).expect("ScanParams is serializable");
    blake3_hex(&json)[..32].to_string()
}

/// Extract mtime (seconds since epoch) from filesystem metadata.
pub fn mtime_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Load cache entries from disk.
/// Falls back gracefully: if the file is missing or corrupt,
/// returns an empty map.
fn load_entries(path: &Path, ttl_secs: u64) -> HashMap<String, CacheEntry> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return HashMap::new(),
    };

    let entries: Vec<CacheEntry> = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };

    let now = now_secs();
    entries
        .into_iter()
        .filter(|e| now.saturating_sub(e.timestamp_secs) <= ttl_secs)
        .map(|e| {
            let key = fast_key(&e.file_path, &e.params);
            (key, e)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::scan::ScanOutput;
    use crate::engine::zhtype::ChineseType;
    use tempfile::TempDir;

    fn empty_output() -> ScanOutput {
        ScanOutput {
            issues: vec![],
            detected_script: ChineseType::Traditional,
            ai_signature: None,
            translationese_signature: None,
            coverage: None,
            oral_density: None,
            quality_flags: Vec::new(),
        }
    }

    fn test_params() -> ScanParams {
        ScanParams {
            ruleset_hash: "rh".into(),
            profile: "base".into(),
            content_type: "md".into(),
            fix_mode: "none".into(),
            detect_ai: false,
            detect_translationese: false,
            translationese_domain: "general".into(),
            ai_threshold: "1.0".into(),
            exempt_blockquotes: false,
            engine_version: "test".into(),
        }
    }

    fn test_params_plain() -> ScanParams {
        ScanParams {
            ruleset_hash: "rh".into(),
            profile: "base".into(),
            content_type: "plain".into(),
            fix_mode: "none".into(),
            detect_ai: false,
            detect_translationese: false,
            translationese_domain: "general".into(),
            ai_threshold: "1.0".into(),
            exempt_blockquotes: false,
            engine_version: "test".into(),
        }
    }

    /// Put `base` into a fresh cache, confirm it hits, then confirm `variant`
    /// misses on both the fast and the content path. Lookup never compares the
    /// stored params, so a field absent from the key shows up here as the
    /// variant hitting the base entry.
    fn assert_variant_misses(base: &ScanParams, variant: &ScanParams) {
        let dir = TempDir::new().unwrap();
        let mut cache = ScanCache::open(dir.path().join("c.bin"));

        cache.put("a.md", b"hello", 1000, 5, base, empty_output(), false, 5);
        assert!(matches!(
            cache.check_fast("a.md", 1000, 5, base),
            CacheResult::Hit(_)
        ));
        assert!(matches!(
            cache.check_fast("a.md", 1000, 5, variant),
            CacheResult::Miss
        ));
        assert!(cache.check_content("a.md", b"hello", variant).is_none());
    }

    #[test]
    fn fast_path_hit() {
        let dir = TempDir::new().unwrap();
        let mut cache = ScanCache::open(dir.path().join("c.bin"));
        let p = test_params();

        cache.put("a.md", b"hello", 1000, 5, &p, empty_output(), false, 5);

        // Same mtime+size = fast hit.
        assert!(matches!(
            cache.check_fast("a.md", 1000, 5, &p),
            CacheResult::Hit(_)
        ));

        // Different mtime = miss.
        assert!(matches!(
            cache.check_fast("a.md", 2000, 5, &p),
            CacheResult::Miss
        ));

        // Different size = miss.
        assert!(matches!(
            cache.check_fast("a.md", 1000, 99, &p),
            CacheResult::Miss
        ));

        // Different profile = miss (different entry entirely).
        let strict = ScanParams {
            profile: "strict".into(),
            ..p.clone()
        };
        assert!(matches!(
            cache.check_fast("a.md", 1000, 5, &strict),
            CacheResult::Miss
        ));
    }

    #[test]
    fn detect_ai_changes_cache_key() {
        let p = test_params();
        assert_variant_misses(
            &p,
            &ScanParams {
                detect_ai: true,
                ..p.clone()
            },
        );
    }

    #[test]
    fn legacy_entries_without_engine_version_miss() {
        // The whole invalidation scheme rests on old entries deserializing with
        // empty defaults and therefore missing. Write a cache file that
        // predates both new fields and check that, rather than trusting the
        // #[serde(default)] annotations to stay put.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("legacy.json");
        let p = test_params();

        // Build a current entry, then strip the two fields back out of the
        // serialized form to reproduce a file written by an older binary.
        {
            let mut cache = ScanCache::open(path.clone());
            cache.put("a.md", b"hello", 1000, 5, &p, empty_output(), false, 5);
            cache.flush();
        }
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let mut stripped = 0;
        for entry in doc.as_array_mut().expect("cache file is an array") {
            if let Some(params) = entry.get_mut("params").and_then(|p| p.as_object_mut()) {
                params.remove("engine_version");
                params.remove("exempt_blockquotes");
                stripped += 1;
            }
        }
        assert!(stripped > 0, "test did not find a params object to strip");
        std::fs::write(&path, serde_json::to_string(&doc).unwrap()).unwrap();

        let mut cache = ScanCache::open(path);

        // The entry must still LOAD, which is what #[serde(default)] buys.
        // Without it the file fails to parse, load_entries swallows the error
        // and returns an empty map, and the Miss below would hold for the wrong
        // reason. Looking it up with legacy-shaped params must therefore Hit.
        let legacy_shaped = ScanParams {
            engine_version: String::new(),
            exempt_blockquotes: false,
            ..p.clone()
        };
        assert!(
            matches!(
                cache.check_fast("a.md", 1000, 5, &legacy_shaped),
                CacheResult::Hit(_)
            ),
            "legacy entry did not load: the serde defaults are gone"
        );

        // And current params miss it, because the key now carries both fields.
        assert!(matches!(
            cache.check_fast("a.md", 1000, 5, &p),
            CacheResult::Miss
        ));
        assert!(cache.check_content("a.md", b"hello", &p).is_none());
    }

    #[test]
    fn exempt_blockquotes_changes_cache_key() {
        // The flag changes which spans are scanned, so a warm cache must not
        // answer for the other setting. It used to: the field was on ScanParams
        // but absent from the key, so a plain lint followed by an exempt one
        // returned the first run's issues.
        let p = test_params();
        assert_variant_misses(
            &p,
            &ScanParams {
                exempt_blockquotes: true,
                ..p.clone()
            },
        );
    }

    #[test]
    fn profile_config_changes_cache_key() {
        // The key carries the whole effective ProfileConfig, not the name the
        // user asked for. The relaxed flag rewrites that config without
        // changing the name, so keying on the name let a relaxed run answer for
        // a strict one and a strict gate report clean. Derive both strings the
        // way build_lint_setup does, so this pins the real mechanism rather
        // than two arbitrary strings that happen to differ.
        use crate::rules::ruleset::Profile;
        let strict = Profile::Strict.config();
        let relaxed = strict.with_relaxed();
        assert_ne!(
            format!("{strict:?}"),
            format!("{relaxed:?}"),
            "with_relaxed must change the config it is keyed on"
        );

        let p = ScanParams {
            profile: format!("{strict:?}"),
            ..test_params()
        };
        assert_variant_misses(
            &p,
            &ScanParams {
                profile: format!("{relaxed:?}"),
                ..p.clone()
            },
        );
    }

    #[test]
    fn engine_version_changes_cache_key() {
        // ruleset_hash covers the rules, not the passes that interpret them.
        // Without this an upgrade that changes a detector keeps serving the old
        // binary's results for every unchanged file until the TTL expires.
        let p = test_params();
        assert_variant_misses(
            &p,
            &ScanParams {
                engine_version: "0.2.0".into(),
                ..p.clone()
            },
        );
    }

    #[test]
    fn ai_threshold_changes_cache_key() {
        let p = ScanParams {
            detect_ai: true,
            ai_threshold: "1.0".into(),
            ..test_params()
        };
        for threshold in ["0.5", "1.5"] {
            assert_variant_misses(
                &p,
                &ScanParams {
                    ai_threshold: threshold.into(),
                    ..p.clone()
                },
            );
        }
    }

    #[test]
    fn translationese_domain_changes_cache_key() {
        let p = ScanParams {
            detect_translationese: true,
            translationese_domain: "general".into(),
            ..test_params()
        };
        assert_variant_misses(
            &p,
            &ScanParams {
                translationese_domain: "technical".into(),
                ..p.clone()
            },
        );
    }

    #[test]
    fn slow_path_content_check() {
        let dir = TempDir::new().unwrap();
        let mut cache = ScanCache::open(dir.path().join("c.bin"));
        let p = test_params_plain();

        cache.put("b.md", b"data", 1000, 4, &p, empty_output(), false, 4);

        // Same content despite mtime miss: slow-path hit.
        assert!(cache.check_content("b.md", b"data", &p).is_some());

        // Different content: slow-path miss.
        assert!(cache.check_content("b.md", b"changed", &p).is_none());
    }

    #[test]
    fn cache_persists_to_disk() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("c.bin");
        let p = test_params_plain();

        {
            let mut cache = ScanCache::open(path.clone());
            cache.put("f.md", b"x", 100, 1, &p, empty_output(), false, 1);
            cache.flush();
        }

        let mut cache = ScanCache::open(path);
        assert!(matches!(
            cache.check_fast("f.md", 100, 1, &p),
            CacheResult::Hit(_)
        ));
    }

    #[test]
    fn expired_entries_pruned() {
        let dir = TempDir::new().unwrap();
        let mut cache = ScanCache::open(dir.path().join("c.bin"));
        let p = test_params_plain();
        cache.put("e.md", b"x", 100, 1, &p, empty_output(), false, 1);
        for entry in cache.entries_mut().values_mut() {
            entry.timestamp_secs = 0;
        }
        assert!(matches!(
            cache.check_fast("e.md", 100, 1, &p),
            CacheResult::Miss
        ));
    }

    #[test]
    fn overflow_evicts_oldest() {
        let dir = TempDir::new().unwrap();
        let mut cache = ScanCache::open(dir.path().join("c.bin"));
        let p = test_params_plain();

        for i in 0..MAX_ENTRIES + 10 {
            let name = format!("file_{i}.md");
            cache.put(&name, b"x", 100, 1, &p, empty_output(), false, 1);
            let key = fast_key(&name, &p);
            if let Some(e) = cache.entries_mut().get_mut(&key) {
                e.timestamp_secs = i as u64 + 1_700_000_000;
            }
        }

        assert!(cache.entries().len() > MAX_ENTRIES);
        cache.flush();
        assert!(cache.entries().len() <= MAX_ENTRIES);
    }

    #[test]
    fn char_count_survives_cache_hits() {
        let dir = TempDir::new().unwrap();
        let mut cache = ScanCache::open(dir.path().join("c.bin"));
        let p = test_params_plain();

        cache.put(
            "chars.md",
            "甲乙丙".as_bytes(),
            1000,
            9,
            &p,
            empty_output(),
            false,
            3,
        );

        let fast_hit = cache
            .check_fast("chars.md", 1000, 9, &p)
            .into_hit()
            .unwrap();
        assert_eq!(fast_hit.text_char_count, 3);

        let content_hit = cache
            .check_content("chars.md", "甲乙丙".as_bytes(), &p)
            .unwrap();
        assert_eq!(content_hit.text_char_count, 3);
    }

    #[test]
    fn legacy_entries_without_char_count_miss_until_refreshed() {
        let dir = TempDir::new().unwrap();
        let mut cache = ScanCache::open(dir.path().join("c.bin"));
        let p = test_params_plain();

        cache.put("legacy.md", b"abc", 1000, 3, &p, empty_output(), false, 0);

        assert!(matches!(
            cache.check_fast("legacy.md", 1000, 3, &p),
            CacheResult::Miss
        ));
        assert!(cache.check_content("legacy.md", b"abc", &p).is_none());
    }
}
