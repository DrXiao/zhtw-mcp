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

/// Maximum issues an entry may carry before it is not worth caching.  An
/// entry stores the whole issue list, so a pathological file can dwarf every
/// other entry: one 400 KB file produced 29,796 issues and a 6.7 MB entry,
/// which every later run then had to parse.  Past this many issues, rescanning
/// is cheaper than loading the cached answer, so skip the entry entirely.
const MAX_ENTRY_ISSUES: usize = 2000;

/// Maximum serialized size of the whole cache file.  `MAX_ENTRIES` bounds the
/// count but not the bytes, and the bytes are what every run pays to parse.
/// Oldest entries are evicted until the file fits.
///
/// Sized to sit ABOVE what `MAX_ENTRIES` produces in normal use, not below it.
/// Measured: 993 real entries serialize to 7.7 MB, so a full 2000-entry cache
/// lands near 16 MB.  An 8 MiB cap therefore bound the ordinary case rather
/// than the pathological one, and evicted a quarter of a healthy repo's cache
/// on every run: the next run rescanned those files, re-inserted them, and
/// evicted another quarter, forever.  This is a backstop against entries far
/// larger than average, which is what `MAX_ENTRY_ISSUES` already caps per
/// entry; between them the worst case is bounded without touching normal use.
const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;

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
    /// Whether the fast path may trust this entry's mtime and size alone.
    ///
    /// False when the file was written within a second of being scanned. mtime
    /// has second granularity, so a further rewrite during that same second
    /// leaves the pair unchanged and the entry would answer for content it
    /// never saw. Deciding at store time is what closes the window: a guard
    /// against the clock at lookup time only covers entries that are still
    /// fresh, not the stale pair an old entry already carries.
    ///
    /// Entries written before this field existed default to trusted, which is
    /// exactly the behaviour they had, and they expire within the day.
    #[serde(default = "default_true")]
    fast_path_safe: bool,
}

fn default_true() -> bool {
    true
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
    ///
    /// Load-time pruning, or loading a file already over the byte budget, only
    /// changes memory. `flush` returns early unless `dirty`, and only `put` set
    /// it, so a cache-hit-only run would leave the file untouched and pay to
    /// parse the same garbage on the next run. Marking dirty here makes the
    /// cleanup reach disk.
    fn ensure_loaded(&mut self) -> &mut HashMap<String, CacheEntry> {
        if self.entries.is_none() {
            let (loaded, needs_rewrite) = load_entries(&self.path, self.ttl_secs);
            self.dirty |= needs_rewrite;
            self.entries = Some(loaded);
        }
        self.entries.as_mut().expect("populated directly above")
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
        let now = now_secs();
        let entries = self.ensure_loaded();
        if let Some(entry) = entries.get(&fast_key(file_path, params)) {
            if entry.fast_path_safe
                && now.saturating_sub(entry.timestamp_secs) <= ttl
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
        if output.issues.len() > MAX_ENTRY_ISSUES {
            tracing::debug!(
                "not caching {file_path}: {} issues exceeds {MAX_ENTRY_ISSUES}",
                output.issues.len()
            );
            return;
        }
        let now = now_secs();
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
                timestamp_secs: now,

                // A file written within the last second may be rewritten again
                // inside that same second, leaving mtime and size unchanged.
                fast_path_safe: mtime_secs.saturating_add(1) < now,
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

        // Acquire the lock BEFORE reading the file we are about to replace.
        // `atomic_write` swaps the whole file, so serializing our own snapshot
        // and writing it discards anything another process stored since we
        // loaded. That race predates this cache growing a load-time cleanup,
        // but the cleanup widened it: a run that only read the cache used to
        // leave the file alone, and now it rewrites.
        //
        // Create the cache directory before opening the sidecar. `replace_file`
        // creates it on write, but that is too late: on a first run the
        // directory does not exist yet, opening the lock fails, and the write
        // would then proceed unlocked. Two concurrent first runs would both
        // take that path and one would lose everything the other stored.
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            if let Err(e) = std::fs::create_dir_all(parent) {
                // At warn, like the write failure below: a cache that can never
                // persist otherwise costs a full rescan every run with nothing
                // on screen to explain it.
                tracing::warn!("scan cache dir {} unusable: {e}", parent.display());
                return;
            }
        }

        let lock_path = self.path.with_extension("lock");
        let Ok(lock_file) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&lock_path)
        else {
            // No lock means no way to merge safely, so decline to write at all
            // rather than replace a file another process may be updating.
            tracing::warn!(
                "scan cache lock {} unusable; not writing",
                lock_path.display()
            );
            return;
        };

        // Best-effort lock: if another process holds it, skip this flush but
        // keep dirty=true so Drop retries.
        if lock_file.try_lock_exclusive().is_err() {
            return;
        }

        let ttl = self.ttl_secs;
        let (on_disk, _) = load_entries(&self.path, ttl);
        let entries = self.ensure_loaded();

        // Merge rather than replace. Keys only the other process has are kept,
        // and where both have one the newer scan wins, so neither side loses
        // work it just did.
        //
        // Timestamps have second granularity, so two processes scanning one
        // file in the same second tie and the local entry wins. That needs no
        // finer clock: a lookup revalidates the entry against the file, so a
        // stale winner misses and is rescanned. The fast path compares only
        // mtime and size and so has a same-second blind spot of its own, which
        // `check_fast` closes by declining while the recorded mtime is that
        // fresh.
        for (k, e) in on_disk {
            let ours_wins = entries
                .get(&k)
                .is_some_and(|ours| ours.timestamp_secs >= e.timestamp_secs);
            if !ours_wins {
                entries.insert(k, e);
            }
        }

        // Prune expired entries.
        let now = now_secs();
        entries.retain(|_, e| now.saturating_sub(e.timestamp_secs) <= ttl);

        // Evict oldest entries if over the count cap.
        evict_oldest(entries, entries.len().saturating_sub(MAX_ENTRIES));

        // Serialize once and measure that, rather than measuring every entry
        // separately first. The budget is above what a full cache produces, so
        // the per-entry pass was pure waste on every run: two full
        // serializations to enforce a cap almost never reached. Only when the
        // one serialization comes back over budget is the measuring pass worth
        // its cost, and then the result is serialized again post-eviction.
        let Some(mut bytes) = serialize_entries(entries) else {
            return;
        };
        if bytes.len() > MAX_TOTAL_BYTES {
            evict_to_byte_budget(entries);
            let Some(shrunk) = serialize_entries(entries) else {
                return;
            };
            bytes = shrunk;
        }

        if atomic_write(&self.path, &bytes) {
            self.dirty = false;
        }

        let _ = lock_file.unlock();
    }
}

impl Drop for ScanCache {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Remove the `count` oldest entries, in the order `keys_oldest_first` defines.
fn evict_oldest(entries: &mut HashMap<String, CacheEntry>, count: usize) {
    if count == 0 {
        return;
    }
    for k in keys_oldest_first(entries).into_iter().take(count) {
        entries.remove(&k);
    }
}

/// Keys ordered oldest-first, which is the order both caps evict in.
///
/// The key breaks timestamp ties. Every entry written during one run shares the
/// same `now_secs()`, so sorting on the timestamp alone is an all-ties sort
/// over
/// randomized `HashMap` order, and which files survive would change run to run.
///
/// Shared rather than written twice, so the two caps cannot drift apart on
/// which entries a user loses.
fn keys_oldest_first(entries: &HashMap<String, CacheEntry>) -> Vec<String> {
    let mut by_time: Vec<(&str, u64)> = entries
        .iter()
        .map(|(k, e)| (k.as_str(), e.timestamp_secs))
        .collect();
    by_time.sort_unstable_by_key(|&(k, ts)| (ts, k));
    by_time.into_iter().map(|(k, _)| k.to_owned()).collect()
}

/// Drop oldest entries until the map serializes under `MAX_TOTAL_BYTES`.
///
/// Each entry is serialized once to measure it, so the eviction count is known
/// before anything is removed. Returns without touching the map in the common
/// case, where the total already fits.
fn evict_to_byte_budget(entries: &mut HashMap<String, CacheEntry>) {
    // A JSON array of n entries is `[` + entries + `]` with n-1 separating
    // commas, not n. Counting one comma per entry overstates the total by a
    // byte, which is enough to evict an extra entry exactly at the boundary.
    let sizes: HashMap<&str, usize> = entries
        .iter()
        .map(|(k, e)| (k.as_str(), serde_json::to_vec(e).map_or(0, |v| v.len())))
        .collect();
    let mut total: usize = 2 + entries.len().saturating_sub(1) + sizes.values().sum::<usize>();
    if total <= MAX_TOTAL_BYTES {
        return;
    }

    // Ordering comes from `keys_oldest_first`; this pass only adds the size
    // accounting on top of it.
    let mut doomed = Vec::new();
    let mut remaining = entries.len();
    for k in keys_oldest_first(entries) {
        if total <= MAX_TOTAL_BYTES {
            break;
        }

        // Removing an entry drops its bytes and, while more than one remains,
        // the comma that joined it to the rest.
        total -= sizes.get(k.as_str()).copied().unwrap_or(0) + usize::from(remaining > 1);
        remaining -= 1;
        doomed.push(k);
    }
    tracing::debug!(
        "scan cache over {MAX_TOTAL_BYTES} bytes: evicting {} entries",
        doomed.len()
    );
    for k in doomed {
        entries.remove(&k);
    }
}

/// Serialize the map as the flat entry array the cache file holds, or None if
/// serialization fails.  `flush` needs it twice, once to measure and once after
/// a byte-budget eviction.
fn serialize_entries(entries: &HashMap<String, CacheEntry>) -> Option<Vec<u8>> {
    serde_json::to_vec(&entries.values().collect::<Vec<_>>()).ok()
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

/// Load cache entries from disk, and report whether the file needs rewriting.
///
/// Falls back gracefully: a missing or corrupt file yields an empty map. The
/// rewrite flag is what makes load-time pruning and legacy over-budget files
/// visible to `flush`, which otherwise returns early on a read-only run.
fn load_entries(path: &Path, ttl_secs: u64) -> (HashMap<String, CacheEntry>, bool) {
    let Ok(bytes) = std::fs::read(path) else {
        return (HashMap::new(), false);
    };

    let Ok(entries) = serde_json::from_slice::<Vec<CacheEntry>>(&bytes) else {
        return (HashMap::new(), false);
    };

    // Drop expired entries, and oversized ones written before the cap existed,
    // so a cache that already holds one heals instead of being paid for daily.
    let before = entries.len();
    let now = now_secs();
    let kept: HashMap<String, CacheEntry> = entries
        .into_iter()
        .filter(|e| now.saturating_sub(e.timestamp_secs) <= ttl_secs)
        .filter(|e| e.output.issues.len() <= MAX_ENTRY_ISSUES)
        .map(|e| {
            let key = fast_key(&e.file_path, &e.params);
            (key, e)
        })
        .collect();
    let needs_rewrite = before != kept.len() || bytes.len() > MAX_TOTAL_BYTES;
    (kept, needs_rewrite)
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

        // Timestamps must be recent: the TTL prune runs before the count
        // eviction, so a fixed epoch would empty the map and let this test pass
        // without the eviction doing anything.
        let base = now_secs() - (MAX_ENTRIES as u64 + 10);
        for i in 0..MAX_ENTRIES + 10 {
            let name = format!("file_{i}.md");
            cache.put(&name, b"x", 100, 1, &p, empty_output(), false, 1);
            let key = fast_key(&name, &p);
            if let Some(e) = cache.entries_mut().get_mut(&key) {
                e.timestamp_secs = base + i as u64;
            }
        }

        assert!(cache.entries().len() > MAX_ENTRIES);
        cache.flush();
        assert_eq!(cache.entries().len(), MAX_ENTRIES);
        // Oldest go first, so the newest entry must still be there.
        let newest = format!("file_{}.md", MAX_ENTRIES + 9);
        assert!(cache.entries().values().any(|e| e.file_path == newest));
    }

    /// Build a ScanOutput carrying `n` issues, for the entry-size caps.
    fn output_with_issues(n: usize) -> ScanOutput {
        use crate::rules::ruleset::{Issue, IssueType, Severity};
        let mut out = empty_output();
        out.issues = (0..n)
            .map(|i| {
                Issue::new(
                    i,
                    6,
                    "軟件",
                    vec!["軟體".to_owned()],
                    IssueType::CrossStrait,
                    Severity::Warning,
                )
            })
            .collect();
        out
    }

    #[test]
    fn pathological_issue_count_is_not_cached() {
        let dir = TempDir::new().unwrap();
        let mut cache = ScanCache::open(dir.path().join("c.bin"));
        let p = test_params_plain();

        cache.put(
            "huge.md",
            b"x",
            100,
            1,
            &p,
            output_with_issues(MAX_ENTRY_ISSUES + 1),
            false,
            1,
        );
        assert!(cache.entries().is_empty(), "oversized entry was stored");

        cache.put(
            "ok.md",
            b"x",
            100,
            1,
            &p,
            output_with_issues(MAX_ENTRY_ISSUES),
            false,
            1,
        );
        assert_eq!(cache.entries().len(), 1, "entry at the cap was rejected");
    }

    #[test]
    fn oversized_entry_on_disk_is_dropped_on_load() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("c.bin");
        let p = test_params_plain();

        // Write an oversized entry directly, as a cache built before the cap
        // existed would contain.
        let entry = CacheEntry {
            file_path: "legacy.md".into(),
            content_hash: blake3_hex(b"x"),
            file_meta: FileMeta {
                mtime_secs: 100,
                size: 1,
            },
            params: p.clone(),
            output: output_with_issues(MAX_ENTRY_ISSUES + 1),
            input_was_sc: false,
            text_char_count: 1,
            timestamp_secs: now_secs(),
            fast_path_safe: true,
        };
        std::fs::write(&path, serde_json::to_vec(&vec![&entry]).unwrap()).unwrap();
        let before = std::fs::metadata(&path).unwrap().len();

        let mut cache = ScanCache::open(path.clone());
        assert!(
            cache.entries().is_empty(),
            "legacy oversized entry survived load"
        );

        // Dropping it from the map is not the point: the cost this cap exists
        // to remove is the per-run parse of the file, so it has to leave disk.
        // A run that stores nothing new never called put(), so only the load
        // path can mark the cache dirty.
        drop(cache);
        let after = std::fs::metadata(&path).unwrap().len();
        assert!(
            after < before,
            "oversized entry still on disk: {before} -> {after} bytes"
        );
        let (reloaded, _) = load_entries(&path, DEFAULT_TTL_SECS);
        assert!(reloaded.is_empty(), "file still holds the oversized entry");
    }

    #[test]
    fn flush_keeps_entries_another_process_wrote_after_we_loaded() {
        // A second CLI process stores entries while this one holds a snapshot.
        // Flushing used to serialize that snapshot and swap the whole file, so
        // the other process's work vanished. The cleanup-on-load path made this
        // reachable from a run that never stored anything itself.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("c.bin");
        let p = test_params_plain();

        let mut ours = ScanCache::open(path.clone());
        ours.put("ours.md", b"x", 100, 1, &p, empty_output(), false, 1);
        ours.entries(); // take the snapshot

        // Meanwhile, another process writes a disjoint entry and exits.
        {
            let mut theirs = ScanCache::open(path.clone());
            theirs.put("theirs.md", b"y", 100, 1, &p, empty_output(), false, 1);
            theirs.flush();
        }
        let (mid, _) = load_entries(&path, DEFAULT_TTL_SECS);
        assert_eq!(mid.len(), 1, "setup: other process did not land its entry");

        ours.flush();

        let (merged, _) = load_entries(&path, DEFAULT_TTL_SECS);
        let names: std::collections::BTreeSet<&str> =
            merged.values().map(|e| e.file_path.as_str()).collect();
        assert!(
            names.contains("theirs.md"),
            "flush discarded a concurrently written entry: {names:?}"
        );
        assert!(names.contains("ours.md"), "flush lost our own entry");
    }

    #[test]
    fn fast_path_declines_while_the_recorded_mtime_is_this_second() {
        // mtime has second granularity, so a file rewritten to the same length
        // within the second it was scanned matches on mtime and size and would
        // serve the previous content's issues. The fast path must decline and
        // let the caller fall back to the content hash, which reads the file.
        let dir = TempDir::new().unwrap();
        let mut cache = ScanCache::open(dir.path().join("c.bin"));
        let p = test_params_plain();
        let now = now_secs();

        cache.put("fresh.md", b"x", now, 1, &p, empty_output(), false, 1);
        assert!(
            matches!(cache.check_fast("fresh.md", now, 1, &p), CacheResult::Miss),
            "fast path trusted an mtime from this second"
        );

        // An older mtime is outside the window and still hits.
        let old = now - 60;
        cache.put("settled.md", b"x", old, 1, &p, empty_output(), false, 1);
        assert!(
            matches!(
                cache.check_fast("settled.md", old, 1, &p),
                CacheResult::Hit(_)
            ),
            "fast path stopped working for settled files"
        );
    }

    #[test]
    fn an_entry_stored_while_fresh_stays_off_the_fast_path() {
        // The window a clock guard at lookup time cannot close: the file was
        // written and scanned inside one second, so a later rewrite in that
        // same second leaves mtime and size unchanged. Time passing does not
        // make the recorded pair trustworthy, so the entry must keep failing
        // the fast path and force a content check for as long as it lives.
        let dir = TempDir::new().unwrap();
        let mut cache = ScanCache::open(dir.path().join("c.bin"));
        let p = test_params_plain();
        let now = now_secs();

        cache.put("fresh.md", b"x", now, 1, &p, empty_output(), false, 1);
        let key = fast_key("fresh.md", &p);

        // Age the entry by an hour. Its mtime is still the second it was
        // written in, so nothing about it became safer.
        if let Some(e) = cache.entries_mut().get_mut(&key) {
            e.timestamp_secs -= 3600;
        }
        assert!(
            matches!(cache.check_fast("fresh.md", now, 1, &p), CacheResult::Miss),
            "an entry stored inside the write second became fast-path trusted"
        );
    }

    #[test]
    fn flush_creates_the_cache_directory_before_taking_the_lock() {
        // On a first run the cache directory does not exist. Opening the lock
        // sidecar used to fail there and the write proceeded unlocked, so two
        // concurrent first runs could each replace the other's file.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing").join("deeper").join("c.bin");
        let p = test_params_plain();

        let mut cache = ScanCache::open(path.clone());
        cache.put("a.md", b"x", 100, 1, &p, empty_output(), false, 1);
        cache.flush();

        assert!(path.exists(), "cache file was not written");
        assert!(
            path.with_extension("lock").exists(),
            "no lock sidecar, so the write was taken unlocked"
        );
        let (loaded, _) = load_entries(&path, DEFAULT_TTL_SECS);
        assert_eq!(loaded.len(), 1);
    }

    #[test]
    fn flush_keeps_the_newer_of_two_entries_for_one_key() {
        // Both processes scanned the same file. The later scan is the one worth
        // keeping, whichever side holds it.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("c.bin");
        let p = test_params_plain();

        let mut ours = ScanCache::open(path.clone());
        ours.put("same.md", b"x", 100, 1, &p, empty_output(), false, 1);
        let key = fast_key("same.md", &p);
        let stale = now_secs() - 500;
        if let Some(e) = ours.entries_mut().get_mut(&key) {
            e.timestamp_secs = stale;
        }

        {
            let mut theirs = ScanCache::open(path.clone());
            theirs.put("same.md", b"y", 200, 2, &p, empty_output(), false, 9);
            theirs.flush();
        }

        ours.flush();

        let (merged, _) = load_entries(&path, DEFAULT_TTL_SECS);
        let e = merged.get(&key).expect("entry survived");
        assert_eq!(
            e.text_char_count, 9,
            "older snapshot overwrote the newer scan"
        );
    }

    #[test]
    fn byte_cap_does_not_bind_on_a_normal_full_cache() {
        // The regression this pins: an 8 MiB budget sat BELOW what a healthy
        // MAX_ENTRIES-sized cache produces, so every run evicted a quarter of a
        // repo that was doing nothing wrong, and the next run rescanned it. A
        // full cache of ordinary documents must survive flush intact.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("c.bin");
        let mut cache = ScanCache::open(path);
        let p = test_params_plain();

        let base = now_secs() - MAX_ENTRIES as u64;
        for i in 0..MAX_ENTRIES {
            let name = format!("file_{i}.md");
            cache.put(&name, b"x", 100, 1, &p, output_with_issues(20), false, 1);
            let key = fast_key(&name, &p);
            if let Some(e) = cache.entries_mut().get_mut(&key) {
                e.timestamp_secs = base + i as u64;
            }
        }
        cache.flush();

        assert_eq!(
            cache.entries().len(),
            MAX_ENTRIES,
            "byte cap evicted from a cache that is merely full, not oversized"
        );
    }

    #[test]
    fn total_size_cap_evicts_oldest() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("c.bin");
        let mut cache = ScanCache::open(path.clone());
        let p = test_params_plain();

        // Each entry sits under the per-entry cap; together they blow the file
        // budget, which is exactly the case MAX_ENTRIES alone does not catch.
        let base = now_secs() - 300;
        for i in 0..300 {
            let name = format!("file_{i}.md");
            cache.put(&name, b"x", 100, 1, &p, output_with_issues(1000), false, 1);
            let key = fast_key(&name, &p);
            if let Some(e) = cache.entries_mut().get_mut(&key) {
                e.timestamp_secs = base + i as u64;
            }
        }
        cache.flush();

        let written = std::fs::metadata(&path).unwrap().len() as usize;
        assert!(
            written <= MAX_TOTAL_BYTES,
            "cache file {written} bytes exceeds {MAX_TOTAL_BYTES}"
        );
        // Newest survive: eviction is oldest-first, not arbitrary.
        assert!(cache
            .entries()
            .values()
            .any(|e| e.file_path == "file_299.md"));
    }

    #[test]
    fn legacy_total_size_over_budget_is_rewritten_on_cache_hit_only_run() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("c.bin");
        let p = test_params_plain();
        let base = now_secs() - 300;
        let entries: Vec<CacheEntry> = (0..300)
            .map(|i| CacheEntry {
                file_path: format!("file_{i}.md"),
                content_hash: blake3_hex(b"x"),
                file_meta: FileMeta {
                    mtime_secs: 100,
                    size: 1,
                },
                params: p.clone(),
                output: output_with_issues(1000),
                input_was_sc: false,
                text_char_count: 1,
                timestamp_secs: base + i,
                fast_path_safe: true,
            })
            .collect();
        std::fs::write(&path, serde_json::to_vec(&entries).unwrap()).unwrap();
        let before = std::fs::metadata(&path).unwrap().len() as usize;
        assert!(
            before > MAX_TOTAL_BYTES,
            "test cache did not exceed byte cap: {before}"
        );

        let mut cache = ScanCache::open(path.clone());
        assert_eq!(cache.entries().len(), entries.len());
        drop(cache);

        let after = std::fs::metadata(&path).unwrap().len() as usize;
        assert!(
            after <= MAX_TOTAL_BYTES,
            "legacy cache stayed over byte cap: {before} -> {after}"
        );
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
