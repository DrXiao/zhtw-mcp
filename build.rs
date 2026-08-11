// Build script: pre-serialize assets/ruleset.json to postcard binary format.
// At runtime, postcard::from_bytes is ~10x faster than serde_json::from_str.

use std::path::Path;

// Mirror the ruleset types needed for deserialization.
// These must match the runtime types in src/rules/ruleset.rs exactly.
#[derive(serde::Serialize, serde::Deserialize)]
struct Ruleset {
    spelling_rules: Vec<SpellingRule>,
    case_rules: Vec<CaseRule>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SpellingRule {
    from: String,
    to: Vec<String>,
    #[serde(rename = "type")]
    rule_type: RuleType,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    english: Option<String>,
    #[serde(default)]
    exceptions: Option<Vec<String>>,
    #[serde(default)]
    context_clues: Option<Vec<String>>,
    #[serde(default)]
    negative_context_clues: Option<Vec<String>>,
    #[serde(default)]
    positional_clues: Option<Vec<String>>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    editorial_confidence: Option<EditorialConfidence>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum EditorialConfidence {
    High,
    Medium,
    Low,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum RuleType {
    PoliticalColoring,
    CrossStrait,
    Typo,
    Confusable,
    Variant,
    AiFiller,
    Translationese,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CaseRule {
    term: String,
    #[serde(default)]
    alternatives: Option<Vec<String>>,
    #[serde(default)]
    disabled: bool,
}

fn main() {
    let ruleset_path = Path::new("assets/ruleset.json");
    println!("cargo:rerun-if-changed={}", ruleset_path.display());

    let json = std::fs::read_to_string(ruleset_path).expect("read assets/ruleset.json");
    let ruleset: Ruleset = serde_json::from_str(&json).expect("parse ruleset.json");

    let bytes = postcard::to_allocvec(&ruleset).expect("postcard serialize");

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let out_path = Path::new(&out_dir).join("ruleset.postcard");
    std::fs::write(&out_path, &bytes).expect("write ruleset.postcard");

    emit_engine_fingerprint();
}

/// Hash the scanner sources into `ZHTW_ENGINE_FINGERPRINT`.
///
/// The scan cache keys on this. A crate version is not enough: it only moves at
/// a release bump, so a source build, a git checkout, or a detector fix within
/// one release would keep serving the previous scanner's results for every
/// unchanged file. Hashing the sources means the key moves exactly when the
/// code that produced the cached answer moves. Cargo.lock is hashed alongside
/// the sources, since the dependencies do as much of the scanning as we do.
///
/// `DefaultHasher::new()` has fixed keys and files are hashed in sorted path
/// order, so the value is stable for a given toolchain. std does not promise
/// the algorithm across releases, so a rustc upgrade can change it. That is the
/// right failure direction for a cache key: the worst case is a cold cache
/// after a toolchain change, never a stale hit. Nothing else depends on this
/// value, so it does not affect reproducible builds of the binary itself.
fn emit_engine_fingerprint() {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    println!("cargo:rerun-if-changed=src");

    // Dependency versions are part of the scanner too: a cargo update can
    // change how pulldown-cmark, aho-corasick or the normalizer behave with an
    // untouched src/ and an unchanged crate version.
    println!("cargo:rerun-if-changed=Cargo.lock");

    let mut files = Vec::new();
    collect_rs_files(Path::new("src"), &mut files);
    files.sort();

    let mut hasher = DefaultHasher::new();
    std::fs::read("Cargo.lock")
        .unwrap_or_default()
        .hash(&mut hasher);
    for path in &files {
        // The path matters as well as the bytes: moving code between modules
        // can change behavior through cfg gating alone.
        path.to_string_lossy().hash(&mut hasher);

        // A file that vanished mid-build still has to produce a stable value
        // rather than abort the build.
        std::fs::read(path).unwrap_or_default().hash(&mut hasher);
    }
    println!(
        "cargo:rustc-env=ZHTW_ENGINE_FINGERPRINT={:016x}",
        hasher.finish()
    );
}

fn collect_rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}
