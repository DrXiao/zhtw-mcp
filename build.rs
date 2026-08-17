// Build script: pre-serialize assets/ruleset.json to postcard binary format. At
// runtime, postcard::from_bytes is ~10x faster than serde_json::from_str.

use std::path::Path;

// The wire-format types, pulled straight from the runtime source rather than
// mirrored here. Postcard is not self-describing: field order and field count
// are the encoding, so a hand-kept second copy corrupts every rule the moment
// it drifts, silently and at runtime. One definition cannot drift.
//
// Everything the include!d file needs is in scope below and nothing else from
// the crate is referenced, which is a constraint on that file rather than on
// this one. See src/rules/schema.rs.
#[allow(dead_code)]
mod schema {
    include!("src/rules/schema.rs");
}
use schema::Ruleset;

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
