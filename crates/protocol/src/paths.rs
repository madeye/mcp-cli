//! Shared path helpers for the bridge, daemon, and installer.
//!
//! The guiding invariant: both ends of the bridge <-> daemon pair must
//! derive the *same* socket path from the *same* project root without any
//! runtime coordination. That means no env lookups that the other side
//! might miss, no home-dir probing, no user-supplied strings beyond the
//! canonicalized project root itself.

use std::path::{Path, PathBuf};

/// Derive the per-project UDS path. Stable across runs for a given
/// canonicalized project root. Parent directory is *not* created here;
/// callers that bind do so via [`ensure_socket_parent`].
pub fn socket_path_for(canonical_root: &Path) -> PathBuf {
    socket_path_for_in(canonical_root, xdg_runtime_dir().as_deref())
}

/// Like [`socket_path_for`] but takes the runtime directory explicitly
/// instead of reading `$XDG_RUNTIME_DIR`. Exposed for tests and callers
/// that need to derive the path without touching process env.
pub fn socket_path_for_in(canonical_root: &Path, runtime_dir: Option<&Path>) -> PathBuf {
    let hash = hash_path(canonical_root);
    let file = format!("{hash}.sock");

    if let Some(dir) = runtime_dir {
        return dir.join("mcp-cli").join(file);
    }

    // Fallback: /tmp/mcp-cli-<user>-<hash>.sock — keeps paths short
    // enough to clear macOS's 104-byte sun_path limit even for
    // long usernames.
    let user = current_user();
    let file = format!("mcp-cli-{user}-{hash}.sock");
    PathBuf::from("/tmp").join(file)
}

/// Create the parent directory of `socket` if it does not exist, with
/// mode 0700 on Unix. Returns Ok(()) if the path is a plain `/tmp/...`
/// style file with no intermediate dirs to create.
pub fn ensure_socket_parent(socket: &Path) -> std::io::Result<()> {
    let Some(parent) = socket.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() || parent.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        // Best-effort — don't fail on filesystems that reject chmod.
        let _ = std::fs::set_permissions(parent, perms);
    }
    Ok(())
}

/// Hex-encoded FNV-1a 64-bit hash of the path's string form. We avoid
/// the stdlib `DefaultHasher` because its output is not stable across
/// Rust versions, and both sides of the bridge must agree on the hash.
fn hash_path(path: &Path) -> String {
    let s = path.as_os_str().to_string_lossy();
    let h = fnv1a_64(s.as_bytes());
    format!("{h:016x}")
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    // FNV-1a constants.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

fn xdg_runtime_dir() -> Option<PathBuf> {
    let v = std::env::var_os("XDG_RUNTIME_DIR")?;
    if v.is_empty() {
        return None;
    }
    let p = PathBuf::from(v);
    if p.is_absolute() {
        Some(p)
    } else {
        None
    }
}

fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_path_same_socket() {
        // socket_path_for reads XDG_RUNTIME_DIR; the env-mutating
        // tests below hold ENV_LOCK while they flip it, so we have
        // to hold it here too — otherwise a parallel test run sees
        // a stale env mid-call and the two `socket_path_for(p)`
        // results disagree (was a long-standing CI flake).
        let _g = ENV_LOCK.lock().unwrap();
        let p = Path::new("/home/alice/projects/foo");
        assert_eq!(socket_path_for(p), socket_path_for(p));
    }

    #[test]
    fn different_paths_different_sockets() {
        let _g = ENV_LOCK.lock().unwrap();
        let a = socket_path_for(Path::new("/home/alice/projects/foo"));
        let b = socket_path_for(Path::new("/home/alice/projects/bar"));
        assert_ne!(a, b);
    }

    #[test]
    fn socket_is_under_runtime_dir_when_set() {
        // SAFETY: test-only mutation of env; the whole module's tests
        // run serially on this var via the lock pattern below.
        let _g = ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("XDG_RUNTIME_DIR");
        std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000");
        let s = socket_path_for(Path::new("/home/alice/foo"));
        assert!(
            s.starts_with("/run/user/1000/mcp-cli/"),
            "unexpected: {}",
            s.display()
        );
        match prev {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    #[test]
    fn socket_falls_back_to_tmp_when_runtime_dir_missing() {
        let _g = ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("XDG_RUNTIME_DIR");
        std::env::remove_var("XDG_RUNTIME_DIR");
        let s = socket_path_for(Path::new("/home/alice/foo"));
        assert!(s.starts_with("/tmp/"), "unexpected: {}", s.display());
        assert!(s.to_string_lossy().contains("mcp-cli-"));
        if let Some(v) = prev {
            std::env::set_var("XDG_RUNTIME_DIR", v);
        }
    }

    #[test]
    fn fnv1a_offset_basis() {
        // Empty input must yield the FNV-1a 64-bit offset basis.
        // Canary: catches any accidental edit to the constant.
        assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
    }

    #[test]
    fn fnv1a_distinguishes_short_strings() {
        assert_ne!(fnv1a_64(b"foo"), fnv1a_64(b"bar"));
        assert_ne!(fnv1a_64(b"foo"), fnv1a_64(b"fo"));
    }

    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());
}
