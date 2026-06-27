//! Filesystem staging primitives shared by the plugin and Lindera-dict
//! staging paths (incremental sha256-diff copy into the data dir).

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::AppResult;

/// True when `dst` exists and its sha256 equals `src`'s. Returns false when
/// `dst` is missing or differs — i.e. "needs (re)copy".
pub(crate) fn file_matches(src: &Path, dst: &Path) -> AppResult<bool> {
    if !dst.exists() {
        return Ok(false);
    }
    Ok(sha256(src)? == sha256(dst)?)
}

pub(crate) fn sha256(path: &Path) -> AppResult<[u8; 32]> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

/// Copy via a sibling `.partial` file + rename so a partial write never
/// leaves a truncated artifact at `dst` for a consumer to load. The suffix
/// is appended (not an extension replace) so `dict.da` → `dict.da.partial`,
/// keeping the temp name unambiguous.
pub(crate) fn copy_atomic(src: &Path, dst: &Path) -> std::io::Result<()> {
    let mut tmp = dst.as_os_str().to_owned();
    tmp.push(".partial");
    let tmp = std::path::PathBuf::from(tmp);
    std::fs::copy(src, &tmp)?;
    std::fs::rename(&tmp, dst)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write(dir: &Path, name: &str, contents: &[u8]) -> std::path::PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(contents).unwrap();
        p
    }

    #[test]
    fn file_matches_false_when_dst_missing_or_differs() {
        let dir = tempdir().unwrap();
        let src = write(dir.path(), "src", b"AAA");
        let dst = dir.path().join("dst");
        assert!(!file_matches(&src, &dst).unwrap());
        write(dir.path(), "dst", b"BBB");
        assert!(!file_matches(&src, &dst).unwrap());
    }

    #[test]
    fn file_matches_true_when_content_identical() {
        let dir = tempdir().unwrap();
        let src = write(dir.path(), "src", b"SAME");
        let dst = write(dir.path(), "dst", b"SAME");
        assert!(file_matches(&src, &dst).unwrap());
    }

    #[test]
    fn copy_atomic_writes_dst_and_cleans_tmp() {
        let dir = tempdir().unwrap();
        let src = write(dir.path(), "model.gguf", b"DATA");
        let dst = dir.path().join("out.gguf");
        copy_atomic(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"DATA");
        // The appended-suffix temp must not linger.
        let tmp = dir.path().join("out.gguf.partial");
        assert!(!tmp.exists());
    }
}
