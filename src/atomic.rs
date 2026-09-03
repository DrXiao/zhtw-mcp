// Atomic file replacement.
//
// One implementation on purpose. This existed three times: the override store,
// the scan cache, and the judgment cache. The copy that drifted wrote to a
// fixed `.tmp` path, so two processes sharing the destination raced on it, and
// fell back to writing straight over the live file when the rename failed,
// which is the exact truncation the design exists to prevent.

use std::io::Write;
use std::path::Path;

/// Replace `dest` with `bytes`, atomically.
///
/// The temp file is created in the destination's own directory so the
/// rename stays within one filesystem, and it carries a unique name so
/// two processes writing the same target cannot collide on it.  Missing
/// parent directories are created.
///
/// Note that rename semantics detach a symlinked destination and ignore
/// the umask in favor of the temp file's mode; callers that care about
/// either (`baseline.rs` does) should write in place instead.
pub fn replace_file(dest: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    persist_with_retry(tmp, dest)
}

/// Persist `tmp` to `dest`, retrying a transient Windows failure.
///
/// `NamedTempFile::persist` on Windows is one `MoveFileExW` call with no
/// retry of its own, and replacing a file that way is not race-free the way
/// POSIX `rename(2)` is: two callers racing the same `dest` can make one
/// call's `MoveFileExW` fail with `ERROR_ACCESS_DENIED` (surfaced here as
/// `PermissionDenied`) even though neither caller did anything wrong. A short
/// bounded retry clears it; `uv` hit the identical failure persisting temp
/// files and fixed it the same way (astral-sh/uv#9543).
fn persist_with_retry(mut tmp: tempfile::NamedTempFile, dest: &Path) -> std::io::Result<()> {
    const MAX_ATTEMPTS: u32 = 6;
    let mut delay_ms = 10u64;
    let mut attempt = 1;
    loop {
        match tmp.persist(dest) {
            Ok(_) => return Ok(()),
            Err(err) => {
                let retryable = cfg!(windows)
                    && err.error.kind() == std::io::ErrorKind::PermissionDenied
                    && attempt < MAX_ATTEMPTS;
                if !retryable {
                    return Err(err.error);
                }
                tmp = err.file;
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                delay_ms *= 2;
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.json");
        std::fs::write(&path, b"old").unwrap();
        replace_file(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn creates_missing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/f.json");
        replace_file(&path, b"x").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"x");
    }

    #[test]
    fn leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.json");
        replace_file(&path, b"x").unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1, "temp file should be renamed, not left");
    }

    #[test]
    fn concurrent_writers_do_not_share_a_temp_path() {
        // The whole point of a unique temp name: two writers targeting one
        // destination must not collide, and the loser must not truncate.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.json");
        std::thread::scope(|s| {
            for i in 0..8 {
                let path = path.clone();
                s.spawn(move || {
                    let payload = vec![b'a' + i as u8; 4096];
                    replace_file(&path, &payload).unwrap();
                });
            }
        });

        // Whoever won, the file is one writer's payload in full, never a mix or
        // a truncation.
        let got = std::fs::read(&path).unwrap();
        assert_eq!(got.len(), 4096);
        assert!(got.iter().all(|&b| b == got[0]), "torn write");
    }
}
