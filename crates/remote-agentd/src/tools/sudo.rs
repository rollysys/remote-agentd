#![allow(dead_code)] // sudo helpers are public API for future tool additions
//! sudo helper module — permission-aware file operations for boards where
//! the daemon runs as a normal user but target files are root-owned.
//!
//! Design: every filesystem-mutating operation has two implementations:
//!   - direct (non-sudo): use `std::fs`
//!   - sudo: shell out to `sudo -n <cmd>` (requires NOPASSWD sudoers config)
//!
//! We do NOT try to wrap arbitrary closures — Rust's `fs` calls are library
//! functions, not subprocesses, so `sudo` can't elevate them. Instead each
//! helper is an explicit function that picks the right path.
//!
//! `sudo -n` (non-interactive) is used everywhere so a missing NOPASSWD entry
//! fails fast instead of hanging on a password prompt (which would deadlock
//! the MCP stdio loop).

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, Result};

/// Metadata snapshot used by fetch/put/edit to capture and restore file
/// ownership + permissions across sudo boundaries.
#[derive(Debug, Clone)]
pub struct SudoMeta {
    pub size: u64,
    pub is_dir: bool,
    /// POSIX permission bits as a raw octal u32 (e.g. 0o755).
    pub mode: u32,
    /// Owner username (resolved from uid).
    pub owner: String,
    /// Group name (resolved from gid).
    pub group: String,
    /// Numeric uid (fallback if name resolution fails).
    pub uid: u32,
    /// Numeric gid (fallback if name resolution fails).
    pub gid: u32,
}

// ─────────────────────────────────────────────────────────────────────────
// Metadata queries — always work without sudo for files the daemon can stat,
// and via `sudo -n stat` for root-owned files.
// ─────────────────────────────────────────────────────────────────────────

/// Stat a path and return rich metadata (mode/owner/group/size/is_dir).
///
/// Works without sudo for world-readable or daemon-owned paths; for
/// root-owned paths the caller should pass `sudo: true` to shell out to
/// `sudo -n stat`.
pub fn file_metadata(path: &Path) -> Result<SudoMeta> {
    // Fast path: direct stat (no sudo). Works for most files.
    match fs::metadata(path) {
        Ok(m) => Ok(meta_from_fs(&m)),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(anyhow!(
            "Permission denied statting {} (daemon uid cannot access; pass sudo:true)",
            path.display()
        )),
        Err(e) => Err(anyhow!("stat {} failed: {}", path.display(), e)),
    }
}

/// sudo-aware stat: uses `sudo -n stat` to read metadata for root-owned files.
pub fn file_metadata_sudo(path: &Path) -> Result<SudoMeta> {
    // `stat -c` on Linux (coreutils), `stat -f` on macOS. We target Linux
    // boards primarily, so use the GNU format. macOS fallback via `ls -ldn`.
    let out = Command::new("sudo")
        .arg("-n")
        .arg("stat")
        .arg("-c")
        .arg("%s %f %u %g %A %U %G")
        .arg(path)
        .output()
        .map_err(|e| anyhow!("failed to spawn `sudo stat`: {}", e))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!("`sudo stat {}` failed: {}", path.display(), err.trim()));
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_stat_output(&stdout)
}

/// Parse `stat -c "%s %f %u %g %A %U %G"` output:
/// size, raw_mode_hex, uid, gid, perm_str, owner, group
fn parse_stat_output(s: &str) -> Result<SudoMeta> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 7 {
        return Err(anyhow!("unexpected stat output: {:?}", s));
    }
    let size: u64 = parts[0]
        .parse()
        .map_err(|e| anyhow!("stat size parse: {}", e))?;
    let uid: u32 = parts[2]
        .parse()
        .map_err(|e| anyhow!("stat uid parse: {}", e))?;
    let gid: u32 = parts[3]
        .parse()
        .map_err(|e| anyhow!("stat gid parse: {}", e))?;
    // parts[4] is the symbolic perm like `drwxr-xr-x`; convert to octal.
    let mode = perm_str_to_octal(parts[4])?;
    // Determine is_dir from the perm string's leading char.
    let is_dir = parts[4].starts_with('d');
    Ok(SudoMeta {
        size,
        is_dir,
        mode,
        owner: parts[5].to_string(),
        group: parts[6].to_string(),
        uid,
        gid,
    })
}

/// Convert a symbolic permission string like `drwxr-xr-x` to octal u32.
///
/// Handles setuid (`s` in owner exec), setgid (`s` in group exec), and
/// sticky (`t` in other exec) bits, mapping them to 0o4000/0o2000/0o1000.
fn perm_str_to_octal(perm: &str) -> Result<u32> {
    // Take the last 9 chars (owner/group/other, 3 each).
    let s = if perm.len() >= 10 { &perm[1..] } else { perm };
    if s.len() < 9 {
        return Err(anyhow!("perm string too short: {}", perm));
    }
    let bytes = s.as_bytes();
    let mut mode: u32 = 0;
    for (i, triplet) in bytes.chunks(3).enumerate() {
        if triplet.len() < 3 {
            return Err(anyhow!("perm triplet short: {:?}", triplet));
        }
        let mut bits: u32 = 0;
        if triplet[0] == b'r' {
            bits |= 0o4;
        }
        if triplet[1] == b'w' {
            bits |= 0o2;
        }
        // Execute bit: 'x', or 's'/'t' (which also set special bits).
        if triplet[2] == b'x' || triplet[2] == b's' || triplet[2] == b't' {
            bits |= 0o1;
        }
        mode |= bits << (6 - i * 3);
    }
    // Setuid: 's' in owner exec position (triplet 0, byte 2).
    if bytes[2] == b's' {
        mode |= 0o4000;
    }
    // Setgid: 's' in group exec position (triplet 1, byte 5).
    if bytes[5] == b's' {
        mode |= 0o2000;
    }
    // Sticky: 't' in other exec position (triplet 2, byte 8).
    if bytes[8] == b't' {
        mode |= 0o1000;
    }
    Ok(mode)
}

#[cfg(unix)]
fn meta_from_fs(m: &fs::Metadata) -> SudoMeta {
    use std::os::unix::fs::MetadataExt;
    let mode = m.mode();
    // Resolve uid/gid to names via /etc/passwd + /etc/group. Fall back to
    // numeric strings if resolution fails (common in containers).
    let owner = uid_to_name(m.uid()).unwrap_or_else(|| m.uid().to_string());
    let group = gid_to_name(m.gid()).unwrap_or_else(|| m.gid().to_string());
    SudoMeta {
        size: m.len(),
        is_dir: m.is_dir(),
        mode,
        owner,
        group,
        uid: m.uid(),
        gid: m.gid(),
    }
}

#[cfg(not(unix))]
fn meta_from_fs(m: &fs::Metadata) -> SudoMeta {
    SudoMeta {
        size: m.len(),
        is_dir: m.is_dir(),
        mode: 0o644,
        owner: String::from("unknown"),
        group: String::from("unknown"),
        uid: 0,
        gid: 0,
    }
}

#[cfg(unix)]
fn uid_to_name(uid: u32) -> Option<String> {
    // Minimal: use `getent passwd` to avoid a libc dep. Cache-free; this is
    // only called for metadata display, not hot paths.
    let out = Command::new("getent")
        .arg("passwd")
        .arg(uid.to_string())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    line.split(':').next().map(String::from)
}

#[cfg(unix)]
fn gid_to_name(gid: u32) -> Option<String> {
    let out = Command::new("getent")
        .arg("group")
        .arg(gid.to_string())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    line.split(':').next().map(String::from)
}

// ─────────────────────────────────────────────────────────────────────────
// Permission-aware filesystem mutators.
// ─────────────────────────────────────────────────────────────────────────

/// Create directory (and parents). sudo path uses `sudo -n mkdir -p`.
pub fn mkdir_all(path: &Path, sudo: bool) -> Result<()> {
    if !sudo {
        fs::create_dir_all(path)
            .map_err(|e| anyhow!("mkdir -p {} failed: {}", path.display(), e))
    } else {
        let st = Command::new("sudo")
            .arg("-n")
            .arg("mkdir")
            .arg("-p")
            .arg(path)
            .status()
            .map_err(|e| anyhow!("failed to spawn sudo mkdir: {}", e))?;
        if !st.success() {
            return Err(anyhow!("sudo mkdir -p {} failed", path.display()));
        }
        Ok(())
    }
}

/// Rename (atomic move). sudo path uses `sudo -n mv`.
pub fn rename(src: &Path, dst: &Path, sudo: bool) -> Result<()> {
    if !sudo {
        fs::rename(src, dst)
            .map_err(|e| anyhow!("rename {} → {} failed: {}", src.display(), dst.display(), e))
    } else {
        let st = Command::new("sudo")
            .arg("-n")
            .arg("mv")
            .arg(src)
            .arg(dst)
            .status()
            .map_err(|e| anyhow!("failed to spawn sudo mv: {}", e))?;
        if !st.success() {
            return Err(anyhow!("sudo mv {} → {} failed", src.display(), dst.display()));
        }
        Ok(())
    }
}

/// Create an empty file. sudo path uses `sudo -n touch`.
pub fn touch(path: &Path, sudo: bool) -> Result<()> {
    if !sudo {
        fs::File::create(path)
            .map_err(|e| anyhow!("touch {} failed: {}", path.display(), e))?;
        Ok(())
    } else {
        let st = Command::new("sudo")
            .arg("-n")
            .arg("touch")
            .arg(path)
            .status()
            .map_err(|e| anyhow!("failed to spawn sudo touch: {}", e))?;
        if !st.success() {
            return Err(anyhow!("sudo touch {} failed", path.display()));
        }
        Ok(())
    }
}

/// Remove a file. sudo path uses `sudo -n rm -f`.
pub fn remove_file(path: &Path, sudo: bool) -> Result<()> {
    if !sudo {
        fs::remove_file(path)
            .map_err(|e| anyhow!("rm {} failed: {}", path.display(), e))
    } else {
        let st = Command::new("sudo")
            .arg("-n")
            .arg("rm")
            .arg("-f")
            .arg(path)
            .status()
            .map_err(|e| anyhow!("failed to spawn sudo rm: {}", e))?;
        if !st.success() {
            return Err(anyhow!("sudo rm {} failed", path.display()));
        }
        Ok(())
    }
}

/// Apply mode (chmod) and optionally owner (chown) to a path.
/// `owner` may be "user", "user:group", or None to skip chown.
pub fn set_owner_mode(path: &Path, mode: u32, owner: Option<&str>, sudo: bool) -> Result<()> {
    // chmod
    if !sudo {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(mode))
                .map_err(|e| anyhow!("chmod {:o} {} failed: {}", mode, path.display(), e))?;
        }
        #[cfg(not(unix))]
        {
            let _ = mode; // no-op on non-unix
        }
    } else {
        let mode_str = format!("{:o}", mode);
        let st = Command::new("sudo")
            .arg("-n")
            .arg("chmod")
            .arg(&mode_str)
            .arg(path)
            .status()
            .map_err(|e| anyhow!("failed to spawn sudo chmod: {}", e))?;
        if !st.success() {
            return Err(anyhow!("sudo chmod {:o} {} failed", mode, path.display()));
        }
    }

    // chown
    if let Some(owner_spec) = owner {
        if !owner_spec.is_empty() {
            if !sudo {
                // Non-sudo chown usually fails unless daemon is root; try anyway
                // and warn on failure rather than hard-erroring (best-effort).
                #[cfg(unix)]
                {
                    if let Some((u, g)) = parse_owner_spec(owner_spec) {
                        use std::os::unix::fs::chown;
                        // chown is best-effort without sudo
                        let _ = chown(path, u, g);
                    }
                }
            } else {
                let st = Command::new("sudo")
                    .arg("-n")
                    .arg("chown")
                    .arg(owner_spec)
                    .arg(path)
                    .status()
                    .map_err(|e| anyhow!("failed to spawn sudo chown: {}", e))?;
                if !st.success() {
                    return Err(anyhow!("sudo chown {} {} failed", owner_spec, path.display()));
                }
            }
        }
    }
    Ok(())
}

/// Parse "user", "user:group", or "user:group" into (Option<uid>, Option<gid>).
#[cfg(unix)]
fn parse_owner_spec(spec: &str) -> Option<(Option<u32>, Option<u32>)> {
    // no OsStrExt needed — we use getent for name resolution
    let mut parts = spec.splitn(2, ':');
    let user = parts.next()?;
    let group = parts.next();
    let uid = name_to_uid(user);
    let gid = group.and_then(name_to_gid);
    Some((uid, gid))
}

#[cfg(unix)]
fn name_to_uid(name: &str) -> Option<u32> {
    let out = Command::new("getent")
        .arg("passwd")
        .arg(name)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    // passwd format: name:passwd:uid:gid:gecos:home:shell
    line.split(':').nth(2).and_then(|s| s.parse().ok())
}

#[cfg(unix)]
fn name_to_gid(name: &str) -> Option<u32> {
    let out = Command::new("getent")
        .arg("group")
        .arg(name)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    line.split(':').nth(2).and_then(|s| s.parse().ok())
}

/// Capture metadata of a path (sudo-aware) for later restoration.
/// Returns None if the path doesn't exist yet.
pub fn capture_meta(path: &Path, sudo: bool) -> Option<SudoMeta> {
    if sudo {
        file_metadata_sudo(path).ok()
    } else {
        file_metadata(path).ok()
    }
}

/// Read a file's contents. sudo path uses `sudo -n cat`.
pub fn read_file(path: &Path, sudo: bool) -> Result<String> {
    if !sudo {
        fs::read_to_string(path)
            .map_err(|e| anyhow!("read {} failed: {}", path.display(), e))
    } else {
        let out = Command::new("sudo")
            .arg("-n")
            .arg("cat")
            .arg(path)
            .output()
            .map_err(|e| anyhow!("failed to spawn sudo cat: {}", e))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(anyhow!("sudo cat {} failed: {}", path.display(), err.trim()));
        }
        String::from_utf8(out.stdout)
            .map_err(|e| anyhow!("sudo cat {} returned non-utf8: {}", path.display(), e))
    }
}

/// Write a file's contents. sudo path uses `sudo -n tee`.
pub fn write_file(path: &Path, content: &str, sudo: bool) -> Result<()> {
    if !sudo {
        fs::write(path, content.as_bytes())
            .map_err(|e| anyhow!("write {} failed: {}", path.display(), e))
    } else {
        use std::io::Write;
        let mut child = Command::new("sudo")
            .arg("-n")
            .arg("tee")
            .arg(path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| anyhow!("failed to spawn sudo tee: {}", e))?;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin
                .write_all(content.as_bytes())
                .map_err(|e| anyhow!("failed to write to sudo tee: {}", e))?;
        }
        let out = child
            .wait_with_output()
            .map_err(|e| anyhow!("sudo tee wait failed: {}", e))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(anyhow!("sudo tee {} failed: {}", path.display(), err.trim()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── perm_str_to_octal ──────────────────────────────────────────────

    #[test]
    fn perm_str_to_octal_regular() {
        assert_eq!(perm_str_to_octal("rwxr-xr-x").unwrap(), 0o755);
        assert_eq!(perm_str_to_octal("rw-r--r--").unwrap(), 0o644);
        assert_eq!(perm_str_to_octal("rwxrwxrwx").unwrap(), 0o777);
        assert_eq!(perm_str_to_octal("--------x").unwrap(), 0o001);
        assert_eq!(perm_str_to_octal("r--------").unwrap(), 0o400);
    }

    #[test]
    fn perm_str_to_octal_with_dir_prefix() {
        // `stat -c %A` includes a leading type char for directories/files.
        assert_eq!(perm_str_to_octal("drwxr-xr-x").unwrap(), 0o755);
        assert_eq!(perm_str_to_octal("-rw-r--r--").unwrap(), 0o644);
        assert_eq!(perm_str_to_octal("drwx------").unwrap(), 0o700);
    }

    #[test]
    fn perm_str_to_octal_setuid_setgid_sticky() {
        // 's' in owner exec position = setuid, 's' in group = setgid,
        // 't' in other = sticky. All imply execute bit.
        assert_eq!(perm_str_to_octal("rwsr-xr-x").unwrap(), 0o4755);
        assert_eq!(perm_str_to_octal("rwxr-sr-x").unwrap(), 0o2755);
        assert_eq!(perm_str_to_octal("rwxr-xr-t").unwrap(), 0o1755);
    }

    #[test]
    fn perm_str_to_octal_too_short() {
        assert!(perm_str_to_octal("rwx").is_err());
        assert!(perm_str_to_octal("rw-").is_err());
    }

    // ── parse_stat_output ──────────────────────────────────────────────

    #[test]
    fn parse_stat_output_file() {
        // Format: "%s %f %u %g %A %U %G"
        // size=12345 raw_mode_hex=81a4 uid=1000 gid=1000 perm=-rw-r--r--
        // owner=alice group=alice
        let out = "12345 81a4 1000 1000 -rw-r--r-- alice alice";
        let meta = parse_stat_output(out).unwrap();
        assert_eq!(meta.size, 12345);
        assert_eq!(meta.mode, 0o644);
        assert_eq!(meta.is_dir, false);
        assert_eq!(meta.owner, "alice");
        assert_eq!(meta.group, "alice");
        assert_eq!(meta.uid, 1000);
        assert_eq!(meta.gid, 1000);
    }

    #[test]
    fn parse_stat_output_directory() {
        let out = "4096 41ed 0 0 drwxr-xr-x root root";
        let meta = parse_stat_output(out).unwrap();
        assert_eq!(meta.size, 4096);
        assert_eq!(meta.mode, 0o755);
        assert_eq!(meta.is_dir, true);
        assert_eq!(meta.owner, "root");
        assert_eq!(meta.group, "root");
        assert_eq!(meta.uid, 0);
        assert_eq!(meta.gid, 0);
    }

    #[test]
    fn parse_stat_output_root_owned() {
        let out = "8192 81a4 0 0 -rw------- root root";
        let meta = parse_stat_output(out).unwrap();
        assert_eq!(meta.mode, 0o600);
        assert_eq!(meta.is_dir, false);
        assert_eq!(meta.owner, "root");
    }

    #[test]
    fn parse_stat_output_malformed() {
        assert!(parse_stat_output("too few fields").is_err());
        assert!(parse_stat_output("").is_err());
    }

    #[test]
    fn parse_stat_output_non_numeric_uid() {
        // uid field is not a number
        assert!(parse_stat_output("123 81a4 abc 1000 -rw-r--r-- alice alice").is_err());
    }

    // ── file_metadata (non-sudo, works on daemon-readable files) ───────

    #[test]
    fn file_metadata_reads_owned_file() {
        let d = std::env::temp_dir().join(format!("agentd_sudo_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("test.txt");
        std::fs::write(&f, "hello world").unwrap();

        let meta = file_metadata(&f).unwrap();
        assert_eq!(meta.size, 11);
        assert_eq!(meta.is_dir, false);
        // mode should have at least read bit for owner
        assert!(meta.mode & 0o400 != 0);

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn file_metadata_reads_directory() {
        let d = std::env::temp_dir().join(format!("agentd_sudo_dir_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();

        let meta = file_metadata(&d).unwrap();
        assert_eq!(meta.is_dir, true);

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn file_metadata_nonexistent_fails() {
        let result = file_metadata(std::path::Path::new("/nonexistent/path/that/does/not/exist"));
        assert!(result.is_err());
    }

    // ── set_owner_mode (non-sudo path, mode only — chown usually needs root) ─

    #[cfg(unix)]
    #[test]
    fn set_owner_mode_chmod_works_without_sudo() {
        let d = std::env::temp_dir().join(format!("agentd_chmod_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("file.txt");
        std::fs::write(&f, "data").unwrap();

        // chmod to 0o600 (owner-only) — should work since we own the file.
        set_owner_mode(&f, 0o600, None, false).unwrap();

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&f).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        let _ = std::fs::remove_dir_all(&d);
    }

    // ── read_file / write_file (non-sudo path) ─────────────────────────

    #[test]
    fn read_file_non_sudo() {
        let d = std::env::temp_dir().join(format!("agentd_rw_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("read.txt");
        std::fs::write(&f, "line1\nline2\n").unwrap();

        let content = read_file(&f, false).unwrap();
        assert_eq!(content, "line1\nline2\n");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn write_file_non_sudo() {
        let d = std::env::temp_dir().join(format!("agentd_ww_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("write.txt");

        write_file(&f, "written content", false).unwrap();
        let read_back = std::fs::read_to_string(&f).unwrap();
        assert_eq!(read_back, "written content");

        let _ = std::fs::remove_dir_all(&d);
    }

    // ── mkdir_all / rename / touch (non-sudo path) ─────────────────────

    #[test]
    fn mkdir_all_creates_nested() {
        let d = std::env::temp_dir().join(format!("agentd_mkdir_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);

        let nested = d.join("a/b/c");
        mkdir_all(&nested, false).unwrap();
        assert!(nested.is_dir());

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rename_moves_file() {
        let d = std::env::temp_dir().join(format!("agentd_rename_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let src = d.join("src.txt");
        let dst = d.join("dst.txt");
        std::fs::write(&src, "content").unwrap();

        rename(&src, &dst, false).unwrap();
        assert!(!src.exists());
        assert!(dst.exists());
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "content");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn touch_creates_empty_file() {
        let d = std::env::temp_dir().join(format!("agentd_touch_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("touched.txt");

        touch(&f, false).unwrap();
        assert!(f.exists());
        assert_eq!(std::fs::metadata(&f).unwrap().len(), 0);

        let _ = std::fs::remove_dir_all(&d);
    }
}
