//! The file primitives the stores share: a clock, and writes that land by rename so a
//! concurrent reader sees either the old file or the new one and never the truncated middle.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Write `bytes` to `tmp`, 0600 before a byte lands, then move it onto `p`. Cleans up on EITHER
/// failure: a write that fails after creating the file leaves one behind, and those accumulate.
pub(crate) fn write_private_then_rename(tmp: &std::path::Path, p: &std::path::Path, bytes: &[u8]) -> bool {
    let ok = write_private(tmp, bytes).is_ok() && std::fs::rename(tmp, p).is_ok();
    if !ok {
        let _ = std::fs::remove_file(tmp);
    }
    ok
}

fn write_private(p: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut f = options.open(p)?;
    // A stale file keeps its old mode through `create`.
    #[cfg(unix)]
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(bytes)?;
    f.sync_all()
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
        assert!(!write_private_then_rename(&tmp, &target, b"[]"));
        assert!(!tmp.exists(), "the tmp file outlived a failed rename");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&tmp, "stale").unwrap();
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o444)).unwrap();
            assert!(!write_private_then_rename(&tmp, &dir.path().join("out.json"), b"[]"));
            assert!(!tmp.exists(), "the tmp file outlived a failed write");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_private_write_lands_0600_even_over_a_stale_tmp_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (tmp, out) = (dir.path().join("box.tmp"), dir.path().join("box.json"));
        std::fs::write(&tmp, "stale").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(write_private_then_rename(&tmp, &out, b"{}"));
        let mode = std::fs::metadata(&out).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!tmp.exists());
    }
}
