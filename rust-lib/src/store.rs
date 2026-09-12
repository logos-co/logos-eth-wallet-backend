//! The two file primitives the stores share: a clock, and a write that lands by rename so a
//! concurrent reader sees either the old file or the new one and never the truncated middle.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Write `txt` to `tmp`, then move it onto `p`. Cleans up on EITHER failure: a write that
/// fails after creating the file leaves one behind, and those accumulate forever.
pub(crate) fn write_then_rename(tmp: &std::path::Path, p: &std::path::Path, txt: &str) -> bool {
    let ok = std::fs::write(tmp, txt).is_ok() && std::fs::rename(tmp, p).is_ok();
    if !ok {
        let _ = std::fs::remove_file(tmp);
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_that_never_lands_leaves_no_tmp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("aaaa.7.0.tmp");

        // Rename fails: the target is a directory. The tmp file exists by then.
        let target = dir.path().join("occupied");
        std::fs::create_dir(&target).unwrap();
        assert!(!write_then_rename(&tmp, &target, "[]"));
        assert!(!tmp.exists(), "the tmp file outlived a failed rename");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&tmp, "stale").unwrap();
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o444)).unwrap();
            assert!(!write_then_rename(&tmp, &dir.path().join("out.json"), "[]"));
            assert!(!tmp.exists(), "the tmp file outlived a failed write");
        }
    }
}
