use std::path::Path;

/// Link `dest` at `target`. Unix uses a symlink. Windows uses a file or directory symlink.
pub fn link_path(target: &Path, dest: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, dest)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{symlink_dir, symlink_file};
        if target.is_dir() {
            symlink_dir(target, dest)
        } else {
            symlink_file(target, dest)
        }
    }
}

/// Mark `path` executable on Unix. On Windows the file is already runnable.
pub fn make_executable(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(perms.mode() | 0o755);
        std::fs::set_permissions(path, perms)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}
