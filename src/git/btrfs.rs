//! Btrfs primitives for snapshot lanes: filesystem ids, subvolume checks,
//! snapshot create and delete, free-space headroom.
//!
//! Everything here works without root. Read-only facts come from ioctls that
//! need no privilege (`BTRFS_IOC_FS_INFO`, `BTRFS_IOC_GET_SUBVOL_INFO`, kernel
//! 4.18 and later). Changes go through the `btrfs` command.
//!
//! Subvolume delete is the one step that differs by mount option. With
//! `user_subvol_rm_allowed`, the owner can run `btrfs subvolume delete`, which
//! returns at once. Without it, an unprivileged user can only `rmdir` an empty
//! subvolume, so every file is unlinked first: the same cost as deleting a
//! plain directory. [`delete_subvolume`] takes the fast path only when the
//! option is set, and says so when it is not.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `statfs.f_type` of a btrfs filesystem.
const BTRFS_SUPER_MAGIC: i64 = 0x9123_683E;
/// Inode number of every btrfs subvolume root.
const SUBVOL_ROOT_INO: u64 = 256;

/// Which filesystem a path is on.
///
/// Every btrfs subvolume has its own `st_dev`, and `statfs.f_fsid` also
/// differs per subvolume, so neither tells whether two paths share a
/// filesystem. For btrfs this uses the filesystem UUID; for anything else,
/// `st_dev`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsId {
    Btrfs([u8; 16]),
    Dev(u64),
}

/// The filesystem id of `path`, or `None` when it cannot be read.
pub fn filesystem_id(path: &Path) -> Option<FsId> {
    use std::os::unix::fs::MetadataExt;
    if is_btrfs(path)
        && let Ok(fsid) = btrfs_fsid(path)
    {
        return Some(FsId::Btrfs(fsid));
    }
    std::fs::metadata(path).ok().map(|m| FsId::Dev(m.dev()))
}

/// True when `a` and `b` are on the same filesystem (both readable).
pub fn same_filesystem(a: &Path, b: &Path) -> bool {
    match (filesystem_id(a), filesystem_id(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// True when `path` is on btrfs.
pub fn is_btrfs(path: &Path) -> bool {
    statfs_type(path) == Some(BTRFS_SUPER_MAGIC)
}

/// True when `path` is the root of a btrfs subvolume (a subvolume or a
/// snapshot). The empty placeholder a snapshot leaves for a nested subvolume
/// is not one.
pub fn is_subvolume(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(path) {
        Ok(m) => m.is_dir() && m.ino() == SUBVOL_ROOT_INO && is_btrfs(path),
        Err(_) => false,
    }
}

/// True when `path` is a subvolume root owned by the calling user, which is
/// what an unprivileged snapshot needs.
pub fn is_own_subvolume(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    is_subvolume(path)
        && std::fs::metadata(path).is_ok_and(|m| m.uid() == unsafe { libc::geteuid() })
}

/// What `BTRFS_IOC_GET_SUBVOL_INFO` says about a subvolume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubvolInfo {
    /// Transaction id of the last change to the subvolume's tree.
    pub generation: u64,
    /// Transaction id of the last change to a file in the subvolume.
    pub ctransid: u64,
    pub uuid: [u8; 16],
    /// UUID of the subvolume this one is a snapshot of (zero when none).
    pub parent_uuid: [u8; 16],
    pub readonly: bool,
}

/// Read [`SubvolInfo`] for the subvolume whose root is `path`.
pub fn subvol_info(path: &Path) -> io::Result<SubvolInfo> {
    // struct btrfs_ioctl_get_subvol_info_args: 504 bytes. Offsets checked
    // against <linux/btrfs.h> with offsetof.
    const IOC: libc::c_ulong = 0x81f8_943c;
    let mut buf = [0u64; 63];
    ioctl_dir(path, IOC, buf.as_mut_ptr().cast())?;
    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), 504) };
    let u64_at = |off: usize| u64::from_ne_bytes(bytes[off..off + 8].try_into().unwrap());
    let uuid_at = |off: usize| -> [u8; 16] { bytes[off..off + 16].try_into().unwrap() };
    const ROOT_SUBVOL_RDONLY: u64 = 1;
    Ok(SubvolInfo {
        generation: u64_at(280),
        readonly: u64_at(288) & ROOT_SUBVOL_RDONLY != 0,
        uuid: uuid_at(296),
        parent_uuid: uuid_at(312),
        ctransid: u64_at(344),
    })
}

fn btrfs_fsid(path: &Path) -> io::Result<[u8; 16]> {
    // struct btrfs_ioctl_fs_info_args: 1024 bytes, fsid at offset 16.
    const IOC: libc::c_ulong = 0x8400_941f;
    let mut buf = [0u64; 128];
    ioctl_dir(path, IOC, buf.as_mut_ptr().cast())?;
    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), 1024) };
    Ok(bytes[16..32].try_into().unwrap())
}

/// Run a read-only ioctl on `path` (or its parent, when it is not a
/// directory).
fn ioctl_dir(path: &Path, request: libc::c_ulong, arg: *mut libc::c_void) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let file = std::fs::File::open(path)?;
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), request, arg) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn statfs_type(path: &Path) -> Option<i64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(c.as_ptr(), &mut st) };
    (rc == 0).then_some(st.f_type as i64)
}

/// Why a btrfs change failed.
#[derive(Debug)]
pub enum BtrfsError {
    /// The filesystem is out of space (data or metadata). Never answer this
    /// by falling back to a full copy.
    NoSpace(String),
    /// Anything else, with the command and its error text.
    Failed(String),
}

impl std::fmt::Display for BtrfsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BtrfsError::NoSpace(m) => write!(f, "no space left on the filesystem: {m}"),
            BtrfsError::Failed(m) => f.write_str(m),
        }
    }
}

/// True when the `btrfs` command is on PATH.
pub fn btrfs_available() -> bool {
    which::which("btrfs").is_ok()
}

fn btrfs(args: &[&std::ffi::OsStr]) -> Result<(), BtrfsError> {
    let cmd = format!(
        "btrfs {}",
        args.iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    );
    let out = Command::new("btrfs")
        .args(args)
        .output()
        .map_err(|e| BtrfsError::Failed(format!("{cmd}: {e}")))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if stderr.contains("No space left") {
        Err(BtrfsError::NoSpace(format!("{cmd}: {stderr}")))
    } else {
        Err(BtrfsError::Failed(format!("{cmd}: {stderr}")))
    }
}

/// `btrfs subvolume create <path>`. Needs write access to the parent only.
pub fn create_subvolume(path: &Path) -> Result<(), BtrfsError> {
    btrfs(&["subvolume".as_ref(), "create".as_ref(), path.as_os_str()])
}

/// `btrfs subvolume snapshot [-r] <source> <dest>`. `dest` must not exist.
pub fn snapshot(source: &Path, dest: &Path, readonly: bool) -> Result<(), BtrfsError> {
    let mut args: Vec<&std::ffi::OsStr> = vec!["subvolume".as_ref(), "snapshot".as_ref()];
    if readonly {
        args.push("-r".as_ref());
    }
    args.push(source.as_os_str());
    args.push(dest.as_os_str());
    btrfs(&args)
}

/// Turn the read-only flag of a subvolume off, which its owner may do.
pub fn set_writable(path: &Path) -> Result<(), BtrfsError> {
    btrfs(&[
        "property".as_ref(),
        "set".as_ref(),
        path.as_os_str(),
        "ro".as_ref(),
        "false".as_ref(),
    ])
}

/// How [`delete_subvolume`] removed a subvolume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deleted {
    /// `btrfs subvolume delete`: returned at once, the cleaner frees the tree
    /// in the background.
    Fast,
    /// Every file unlinked, then `rmdir`: `user_subvol_rm_allowed` is not set.
    FileByFile,
}

/// Environment variable that forces the file-by-file delete, for tests.
pub const RM_ALLOWED_ENV: &str = "GROVE_SUBVOL_RM_ALLOWED";

/// True when `user_subvol_rm_allowed` is set on the filesystem holding
/// `path`. `GROVE_SUBVOL_RM_ALLOWED=0` makes it false.
pub fn rm_allowed(path: &Path) -> bool {
    if std::env::var(RM_ALLOWED_ENV).is_ok_and(|v| v == "0") {
        return false;
    }
    let Ok(canon) = path.canonicalize() else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };
    mountinfo_rm_allowed(&text, &canon)
}

/// Parse `/proc/self/mountinfo`: true when the mount holding `path` (the
/// longest matching mount point) has `user_subvol_rm_allowed` in its super
/// options.
pub fn mountinfo_rm_allowed(mountinfo: &str, path: &Path) -> bool {
    let mut best: Option<(usize, bool)> = None;
    for line in mountinfo.lines() {
        let mut fields = line.split(' ');
        let Some(mount_point) = fields.nth(4) else {
            continue;
        };
        let mount_point = unescape_mount(mount_point);
        if !path.starts_with(&mount_point) {
            continue;
        }
        let Some((_, rest)) = line.split_once(" - ") else {
            continue;
        };
        let super_opts = rest.split(' ').nth(2).unwrap_or("");
        let allowed = super_opts.split(',').any(|o| o == "user_subvol_rm_allowed");
        let len = mount_point.as_os_str().len();
        if best.is_none_or(|(l, _)| len >= l) {
            best = Some((len, allowed));
        }
    }
    best.is_some_and(|(_, allowed)| allowed)
}

fn unescape_mount(s: &str) -> PathBuf {
    // mountinfo escapes space, tab, newline and backslash as \ooo.
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(out))
}

/// Delete the subvolume at `path` and everything in it.
///
/// With `user_subvol_rm_allowed`, nested subvolumes directly named in
/// `nested` are deleted first (a subvolume that holds one cannot be deleted),
/// then the subvolume itself, at once. Without it, every file is unlinked and
/// the empty subvolumes are removed with `rmdir`, read-only ones made
/// writable first. The caller has already done every safety check.
pub fn delete_subvolume(path: &Path, nested: &[PathBuf]) -> Result<Deleted, BtrfsError> {
    if !is_subvolume(path) {
        return Err(BtrfsError::Failed(format!(
            "{} is not a btrfs subvolume",
            path.display()
        )));
    }
    if rm_allowed(path) {
        for inner in nested.iter().filter(|p| is_subvolume(p)) {
            btrfs(&["subvolume".as_ref(), "delete".as_ref(), inner.as_os_str()])?;
        }
        match btrfs(&["subvolume".as_ref(), "delete".as_ref(), path.as_os_str()]) {
            Ok(()) => return Ok(Deleted::Fast),
            // A nested subvolume grove did not make: fall through to the
            // slow path, which removes those with rmdir as it meets them.
            Err(BtrfsError::Failed(m)) if m.contains("not empty") => {}
            Err(e) => return Err(e),
        }
    }
    if subvol_info(path).is_ok_and(|i| i.readonly) {
        set_writable(path)?;
    }
    std::fs::remove_dir_all(path)
        .map_err(|e| BtrfsError::Failed(format!("remove {}: {e}", path.display())))?;
    Ok(Deleted::FileByFile)
}

/// Free space that bounds a snapshot: unallocated device space, and space
/// left inside the metadata chunks already allocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Headroom {
    pub unallocated: u64,
    pub metadata_free: u64,
}

/// Environment variable with the headroom floor in GiB (default 16).
pub const MIN_FREE_ENV: &str = "GROVE_SNAPSHOT_MIN_FREE_GIB";
const DEFAULT_MIN_FREE_GIB: u64 = 16;

/// Read [`Headroom`] for the filesystem holding `path` with
/// `btrfs filesystem usage -b`, which works without root.
pub fn headroom(path: &Path) -> Option<Headroom> {
    let out = Command::new("btrfs")
        .args(["filesystem", "usage", "-b"])
        .arg(path)
        .output()
        .ok()?;
    parse_usage(&String::from_utf8_lossy(&out.stdout))
}

/// Parse the output of `btrfs filesystem usage -b`.
pub fn parse_usage(text: &str) -> Option<Headroom> {
    let mut unallocated = None;
    let mut metadata_free = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("Device unallocated:") {
            unallocated = v.split_whitespace().next()?.parse().ok();
        } else if line.starts_with("Metadata,") {
            // "Metadata,DUP: Size:244276264960, Used:202778607616 (83.01%)"
            let num = |key: &str| -> Option<u64> {
                let rest = line.split(key).nth(1)?;
                rest.split(|c: char| !c.is_ascii_digit())
                    .next()?
                    .parse()
                    .ok()
            };
            if let (Some(size), Some(used)) = (num("Size:"), num("Used:")) {
                metadata_free = Some(size.saturating_sub(used));
            }
        }
    }
    Some(Headroom {
        unallocated: unallocated?,
        metadata_free: metadata_free?,
    })
}

/// Refuse when unallocated space plus free metadata is below the floor
/// (`GROVE_SNAPSHOT_MIN_FREE_GIB`, default 16), or cannot be read.
pub fn check_headroom(path: &Path) -> Result<(), String> {
    let floor_gib = std::env::var(MIN_FREE_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MIN_FREE_GIB);
    let Some(room) = headroom(path) else {
        return Err(format!(
            "could not read free space with `btrfs filesystem usage -b {}`",
            path.display()
        ));
    };
    headroom_ok(room, floor_gib)
}

/// The rule behind [`check_headroom`].
pub fn headroom_ok(room: Headroom, floor_gib: u64) -> Result<(), String> {
    let gib = |b: u64| b as f64 / (1u64 << 30) as f64;
    let total = room.unallocated.saturating_add(room.metadata_free);
    if total < floor_gib << 30 {
        return Err(format!(
            "only {:.1} GiB unallocated plus {:.1} GiB free metadata, below the {floor_gib} GiB \
             floor ({MIN_FREE_ENV}); free space before making lanes",
            gib(room.unallocated),
            gib(room.metadata_free)
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A scratch directory for tests that make subvolumes, under
    /// `GROVE_BTRFS_TEST_DIR`. `None` (skip the test) when the variable is
    /// unset or the directory is not on btrfs.
    pub(crate) fn btrfs_scratch() -> Option<tempfile::TempDir> {
        let root = PathBuf::from(std::env::var_os("GROVE_BTRFS_TEST_DIR")?);
        std::fs::create_dir_all(&root).ok()?;
        if !is_btrfs(&root) {
            eprintln!("skipping: {} is not on btrfs", root.display());
            return None;
        }
        tempfile::tempdir_in(&root).ok()
    }

    #[test]
    fn usage_output_is_parsed() {
        let text = "Overall:\n    Device size:\t\t     3996490993664\n    Device unallocated:\t\t       94425124864\n\nData,single: Size:3413446230016, Used:3099835822080 (90.81%)\n\nMetadata,DUP: Size:244276264960, Used:202778607616 (83.01%)\n";
        assert_eq!(
            parse_usage(text),
            Some(Headroom {
                unallocated: 94_425_124_864,
                metadata_free: 244_276_264_960 - 202_778_607_616,
            })
        );
        assert_eq!(parse_usage("garbage"), None);
    }

    #[test]
    fn headroom_floor() {
        let gib = 1u64 << 30;
        let room = |u, m| Headroom {
            unallocated: u * gib,
            metadata_free: m * gib,
        };
        assert!(headroom_ok(room(10, 7), 16).is_ok());
        assert!(headroom_ok(room(10, 5), 16).is_err());
        assert!(headroom_ok(room(0, 0), 0).is_ok());
    }

    #[test]
    fn mountinfo_picks_the_longest_mount() {
        let text = "\
26 1 0:24 /@ / rw,noatime shared:1 - btrfs /dev/nvme0n1p2 rw,ssd,space_cache=v2,subvolid=256,subvol=/@
27 26 0:24 /@home /home rw,noatime shared:2 - btrfs /dev/nvme0n1p2 rw,ssd,user_subvol_rm_allowed,subvol=/@home
28 26 0:24 /@tmp /var/tmp rw,noatime shared:3 - btrfs /dev/nvme0n1p2 rw,ssd,subvol=/@tmp
29 26 0:25 / /mnt/my\\040disk rw - btrfs /dev/sdb rw,user_subvol_rm_allowed
";
        assert!(mountinfo_rm_allowed(text, Path::new("/home/o/work")));
        assert!(!mountinfo_rm_allowed(text, Path::new("/var/tmp/x")));
        assert!(!mountinfo_rm_allowed(text, Path::new("/usr")));
        assert!(mountinfo_rm_allowed(text, Path::new("/mnt/my disk/a")));
        assert!(!mountinfo_rm_allowed(text, Path::new("/home2")));
    }

    #[test]
    fn subvolume_facts_and_delete() {
        let Some(tmp) = btrfs_scratch() else {
            return;
        };
        let sv = tmp.path().join("sv");
        create_subvolume(&sv).unwrap();
        std::fs::write(sv.join("a"), "1").unwrap();
        create_subvolume(&sv.join("nested")).unwrap();
        std::fs::write(sv.join("nested/b"), "2").unwrap();

        assert!(is_subvolume(&sv));
        assert!(is_own_subvolume(&sv));
        assert!(!is_subvolume(&sv.join("a")));
        // Different st_dev, same filesystem.
        assert!(same_filesystem(&sv, &sv.join("nested")));
        assert!(same_filesystem(&sv, tmp.path()));
        assert!(!same_filesystem(&sv, Path::new("/proc")));

        let snap = tmp.path().join("snap");
        snapshot(&sv, &snap, false).unwrap();
        assert!(is_subvolume(&snap));
        // The nested subvolume is an empty placeholder in the snapshot.
        assert!(!is_subvolume(&snap.join("nested")));
        assert!(!snap.join("nested/b").exists());
        let (a, b) = (subvol_info(&sv).unwrap(), subvol_info(&snap).unwrap());
        assert_eq!(b.parent_uuid, a.uuid);
        assert!(!b.readonly);

        let ro = tmp.path().join("ro");
        snapshot(&sv, &ro, true).unwrap();
        assert!(subvol_info(&ro).unwrap().readonly);

        for p in [&ro, &snap] {
            delete_subvolume(p, &[]).unwrap();
            assert!(!p.exists());
        }
        delete_subvolume(&sv, &[sv.join("nested")]).unwrap();
        assert!(!sv.exists());
        assert!(delete_subvolume(&sv, &[]).is_err());
    }
}
