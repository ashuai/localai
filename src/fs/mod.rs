//! FsService —— 文件系统工具服务(参照 DSH 真实源码 `dsh-fs` / `dsh-fs-sandbox`)。
//!
//! 分层对齐:provider 契约(dsh-fs 的 12 原语裁剪)+ 策略 fence(dsh-fs-sandbox)。
//! 设计论证见本地文档 `localai-docs/fs-tools.md`。
//!
//! 核心语义:
//! - **resolve 边界**:任何路径先规范化(存在的部分 canonicalize,不存在的部分
//!   规范化父目录再拼接),必须在工作区根或额外可写区内,否则 `PermissionDenied`;
//! - **只读模式**:`read_only` 拒绝一切写(对齐 sandbox read-only);
//! - **敏感文件**:`.env`、`*.key`、`id_rsa*` 等模式读写都拒绝(防 key 泄漏);
//! - **版本守卫**:`CreateIfAbsent` / `ReplaceIfVersion(v)`,v = mtime_nanos ^ size;
//! - **原子写**:同目录临时文件 + rename。

use crate::cordis::service::Service;
use std::fmt;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

// ---------- 结构化错误码(对齐 dsh-fs) ----------

#[derive(Debug)]
pub enum FsError {
    NotFound(String),
    NotDirectory(String),
    PermissionDenied(String),
    NotText(String),
    TooLarge(u64),
    StaleVersion(u64, u64),
    Io(String),
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FsError::NotFound(p) => write!(f, "FS_NOT_FOUND: {p}"),
            FsError::NotDirectory(p) => write!(f, "FS_NOT_DIRECTORY: {p}"),
            FsError::PermissionDenied(p) => write!(f, "FS_PERMISSION_DENIED: {p}"),
            FsError::NotText(p) => write!(f, "FS_NOT_TEXT: {p}"),
            FsError::TooLarge(n) => write!(f, "FS_TOO_LARGE: 超过 {n} 字节上限"),
            FsError::StaleVersion(got, want) => {
                write!(f, "FS_STALE_VERSION: 期望 v{want},当前 v{got}")
            }
            FsError::Io(e) => write!(f, "FS_IO_ERROR: {e}"),
        }
    }
}

impl std::error::Error for FsError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone)]
pub struct FsInfo {
    pub kind: FsKind,
    pub size: Option<u64>,
    /// 版本指纹:mtime_nanos ^ size,供写守卫使用
    pub version: u64,
}

#[derive(Debug, Clone)]
pub struct FsEntry {
    pub name: String,
    pub kind: FsKind,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteGuard {
    /// 无前置条件(仍原子,只是不校验版本)
    Unconditional,
    /// 仅当目标不存在时创建(不覆盖)
    CreateIfAbsent,
    /// 仅当当前版本等于 expected 时替换
    ReplaceIfVersion(u64),
}

// ---------- 策略 fence(对齐 dsh-fs-sandbox) ----------

#[derive(Debug, Clone)]
pub struct FsPolicy {
    /// 工作区根:默认当前目录;所有读写必须规范化后落在此内(或 extra_writable)
    pub workspace_root: PathBuf,
    /// 额外可写区(如系统临时目录)
    pub extra_writable: Vec<PathBuf>,
    /// 只读模式:拒绝一切写(对齐 sandbox read-only)
    pub read_only: bool,
    /// 敏感文件名/后缀模式:读写都拒绝
    pub sensitive: Vec<String>,
}

impl FsPolicy {
    pub fn new(workspace_root: PathBuf) -> Self {
        // 规范化根,避免 /var vs /private/var 这类符号链接导致 starts_with 失败
        let workspace_root = fs::canonicalize(&workspace_root).unwrap_or(workspace_root);
        Self {
            workspace_root,
            extra_writable: Vec::new(),
            read_only: false,
            sensitive: vec![
                ".env".into(),
                "*.key".into(),
                "id_rsa".into(),
                "id_rsa.pub".into(),
                "id_ed25519".into(),
                "id_ed25519.pub".into(),
                "*.pem".into(),
            ],
        }
    }

    /// 路径是否落在可写根内(workspace_root 或 extra_writable 之一)
    pub fn within_writable_roots(&self, normalized: &Path) -> bool {
        normalized.starts_with(&self.workspace_root)
            || self
                .extra_writable
                .iter()
                .any(|r| normalized.starts_with(r))
    }

    fn is_sensitive(&self, normalized: &Path) -> bool {
        let name = normalized
            .file_name()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        self.sensitive.iter().any(|pat| {
            if let Some(suffix) = pat.strip_prefix('*') {
                name.ends_with(suffix)
            } else {
                name == *pat
            }
        })
    }

    /// 读 fence:必须在根内,且非敏感文件
    pub fn check_read(&self, normalized: &Path) -> Result<(), FsError> {
        if !self.within_writable_roots(normalized) {
            return Err(FsError::PermissionDenied(format!(
                "读取越界(工作区外): {}",
                normalized.display()
            )));
        }
        if self.is_sensitive(normalized) {
            return Err(FsError::PermissionDenied(format!(
                "敏感文件拒绝读取: {}",
                normalized.display()
            )));
        }
        Ok(())
    }

    /// 写 fence:只读模式拒绝一切写;必须在根内;敏感文件拒绝写
    pub fn check_write(&self, normalized: &Path) -> Result<(), FsError> {
        if self.read_only {
            return Err(FsError::PermissionDenied("只读模式:拒绝一切写操作".into()));
        }
        if !self.within_writable_roots(normalized) {
            return Err(FsError::PermissionDenied(format!(
                "写入越界(工作区外): {}",
                normalized.display()
            )));
        }
        if self.is_sensitive(normalized) {
            return Err(FsError::PermissionDenied(format!(
                "敏感文件拒绝写入: {}",
                normalized.display()
            )));
        }
        Ok(())
    }
}

// ---------- FsService ----------

pub struct FsService {
    pub policy: FsPolicy,
}

impl Service for FsService {
    fn service_name_static() -> &'static str {
        "fs"
    }
}

impl FsService {
    /// 规范化:存在的部分 canonicalize,不存在的部分 canonicalize 父目录后拼接
    pub fn normalize(&self, path: &Path) -> Result<PathBuf, FsError> {
        if path.exists() {
            return fs::canonicalize(path).map_err(|e| FsError::Io(e.to_string()));
        }
        let parent = path.parent().unwrap_or(Path::new("."));
        let name = path.file_name().ok_or_else(|| FsError::Io("非法路径".into()))?;
        let parent_canon = fs::canonicalize(parent).map_err(|e| FsError::Io(e.to_string()))?;
        Ok(parent_canon.join(name))
    }

    /// 解析用户给的路径(相对 → 工作区根),返回规范化绝对路径
    pub fn resolve(&self, path: &str) -> Result<PathBuf, FsError> {
        let p = Path::new(path);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.policy.workspace_root.join(p)
        };
        self.normalize(&abs)
    }

    pub fn stat(&self, path: &Path) -> Result<Option<FsInfo>, FsError> {
        let normalized = self.normalize(path)?;
        self.policy.check_read(&normalized)?;
        match fs::symlink_metadata(&normalized) {
            Ok(meta) => Ok(Some(fs_info(&meta))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(FsError::Io(e.to_string())),
        }
    }

    /// 读整个文本文件;二进制/NUL 拒绝(对齐 FS_NOT_TEXT)
    pub fn read_text(&self, path: &Path) -> Result<String, FsError> {
        let normalized = self.normalize(path)?;
        self.policy.check_read(&normalized)?;
        let meta = fs::metadata(&normalized).map_err(|e| map_meta_err(e, &normalized))?;
        if !meta.is_file() {
            return Err(FsError::NotDirectory(normalized.display().to_string()));
        }
        let bytes = fs::read(&normalized).map_err(|e| FsError::Io(e.to_string()))?;
        if bytes.contains(&0) {
            return Err(FsError::NotText(normalized.display().to_string()));
        }
        String::from_utf8(bytes).map_err(|_| FsError::NotText(normalized.display().to_string()))
    }

    /// 读原始字节,带 max_bytes 上限(对齐 readBytes 的 FS_TOO_LARGE)
    pub fn read_bytes(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, FsError> {
        let normalized = self.normalize(path)?;
        self.policy.check_read(&normalized)?;
        let meta = fs::metadata(&normalized).map_err(|e| map_meta_err(e, &normalized))?;
        if !meta.is_file() {
            return Err(FsError::NotDirectory(normalized.display().to_string()));
        }
        if meta.len() > max_bytes {
            return Err(FsError::TooLarge(max_bytes));
        }
        fs::read(&normalized).map_err(|e| FsError::Io(e.to_string()))
    }

    /// 列出目录直接子项(稳定名字序;不读内容)
    pub fn list_dir(&self, path: &Path) -> Result<Vec<FsEntry>, FsError> {
        let normalized = self.normalize(path)?;
        self.policy.check_read(&normalized)?;
        let meta = fs::metadata(&normalized).map_err(|e| map_meta_err(e, &normalized))?;
        if !meta.is_dir() {
            return Err(FsError::NotDirectory(normalized.display().to_string()));
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(&normalized).map_err(|e| FsError::Io(e.to_string()))? {
            let entry = entry.map_err(|e| FsError::Io(e.to_string()))?;
            let meta = entry.metadata().map_err(|e| FsError::Io(e.to_string()))?;
            entries.push(FsEntry {
                name: entry.file_name().to_string_lossy().to_string(),
                kind: fs_kind(&meta),
                size: if meta.is_file() { Some(meta.len()) } else { None },
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// 原子写(临时文件 + rename);支持版本守卫
    pub fn write_text(&self, path: &Path, content: &str, guard: WriteGuard) -> Result<(), FsError> {
        let normalized = self.normalize(path)?;
        self.policy.check_write(&normalized)?;
        // 版本守卫
        match guard {
            WriteGuard::CreateIfAbsent => {
                if normalized.exists() {
                    return Err(FsError::StaleVersion(version_of(&normalized)?, u64::MAX));
                }
            }
            WriteGuard::ReplaceIfVersion(want) => {
                if !normalized.exists() {
                    return Err(FsError::StaleVersion(0, want));
                }
                let got = version_of(&normalized)?;
                if got != want {
                    return Err(FsError::StaleVersion(got, want));
                }
            }
            WriteGuard::Unconditional => {}
        }
        atomic_write(&normalized, content.as_bytes())
    }

    /// 字面量编辑(对齐 editText):from 必须恰好出现一次,替换后原子写
    pub fn edit_text(
        &self,
        path: &Path,
        from: &str,
        to: &str,
        guard: WriteGuard,
    ) -> Result<(), FsError> {
        let normalized = self.normalize(path)?;
        self.policy.check_write(&normalized)?;
        let content = self.read_text(&normalized)?;
        let mut count = 0;
        let mut idx = 0;
        while let Some(rel) = content[idx..].find(from) {
            count += 1;
            idx += rel + from.len();
            if count > 1 {
                break;
            }
        }
        if count != 1 {
            return Err(FsError::Io(format!(
                "editText: 模式出现 {count} 次(需要恰好 1 次): {from:?}"
            )));
        }
        let replaced = content.replace(from, to);
        match guard {
            WriteGuard::ReplaceIfVersion(want) => {
                let got = version_of(&normalized)?;
                if got != want {
                    return Err(FsError::StaleVersion(got, want));
                }
            }
            WriteGuard::CreateIfAbsent => {
                return Err(FsError::Io("editText 不支持 CreateIfAbsent".into()));
            }
            WriteGuard::Unconditional => {}
        }
        atomic_write(&normalized, replaced.as_bytes())
    }

    /// 创建目录(含父目录)
    pub fn mkdir(&self, path: &Path) -> Result<(), FsError> {
        let normalized = self.normalize(path)?;
        self.policy.check_write(&normalized)?;
        fs::create_dir_all(&normalized).map_err(|e| FsError::Io(e.to_string()))
    }
}

// ---------- helpers ----------

fn fs_info(meta: &fs::Metadata) -> FsInfo {
    let kind = fs_kind(meta);
    FsInfo {
        kind,
        size: if meta.is_file() { Some(meta.len()) } else { None },
        version: version_from_meta(meta),
    }
}

fn fs_kind(meta: &fs::Metadata) -> FsKind {
    if meta.is_file() {
        FsKind::File
    } else if meta.is_dir() {
        FsKind::Dir
    } else if meta.file_type().is_symlink() {
        FsKind::Symlink
    } else {
        FsKind::Other
    }
}

fn version_from_meta(meta: &fs::Metadata) -> u64 {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    mtime ^ meta.len()
}

fn version_of(normalized: &Path) -> Result<u64, FsError> {
    let meta = fs::metadata(normalized).map_err(|e| FsError::Io(e.to_string()))?;
    Ok(version_from_meta(&meta))
}

fn map_meta_err(e: std::io::Error, path: &Path) -> FsError {
    if e.kind() == std::io::ErrorKind::NotFound {
        FsError::NotFound(path.display().to_string())
    } else {
        FsError::Io(e.to_string())
    }
}

/// 原子写:同目录临时文件 + rename(写中断不留半截)
fn atomic_write(normalized: &Path, bytes: &[u8]) -> Result<(), FsError> {
    let dir = normalized.parent().unwrap_or(Path::new("."));
    let name = normalized
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "tmp".into());
    let tmp = dir.join(format!(".{name}.localai-tmp-{}", std::process::id()));
    let mut f = fs::File::create(&tmp).map_err(|e| FsError::Io(e.to_string()))?;
    f.write_all(bytes).map_err(|e| FsError::Io(e.to_string()))?;
    f.sync_all().map_err(|e| FsError::Io(e.to_string()))?;
    drop(f);
    fs::rename(&tmp, normalized).map_err(|e| FsError::Io(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn test_fs() -> (tempfile::TempDir, Arc<FsService>) {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = Arc::new(FsService {
            policy: FsPolicy::new(dir.path().to_path_buf()),
        });
        (dir, svc)
    }

    #[test]
    fn write_read_roundtrip() {
        let (dir, fs) = test_fs();
        let p = dir.path().join("a.txt");
        fs.write_text(&p, "hello", WriteGuard::Unconditional).unwrap();
        assert_eq!(fs.read_text(&p).unwrap(), "hello");
        let info = fs.stat(&p).unwrap().unwrap();
        assert_eq!(info.kind, FsKind::File);
        assert_eq!(info.size, Some(5));
    }

    #[test]
    fn create_if_absent_guard() {
        let (dir, fs) = test_fs();
        let p = dir.path().join("g.txt");
        fs.write_text(&p, "v1", WriteGuard::CreateIfAbsent).unwrap();
        // 第二次 CreateIfAbsent 应失败(已存在)
        assert!(fs.write_text(&p, "v2", WriteGuard::CreateIfAbsent).is_err());
        assert_eq!(fs.read_text(&p).unwrap(), "v1");
    }

    #[test]
    fn version_guard_rejects_stale() {
        let (dir, fs) = test_fs();
        let p = dir.path().join("v.txt");
        fs.write_text(&p, "v1", WriteGuard::Unconditional).unwrap();
        let v1 = fs.stat(&p).unwrap().unwrap().version;
        fs.write_text(&p, "v2", WriteGuard::ReplaceIfVersion(v1)).unwrap();
        // 用旧版本再写 → 应 FS_STALE_VERSION
        assert!(matches!(
            fs.write_text(&p, "v3", WriteGuard::ReplaceIfVersion(v1)),
            Err(FsError::StaleVersion(_, _))
        ));
    }

    #[test]
    fn edit_text_replaces_once() {
        let (dir, fs) = test_fs();
        let p = dir.path().join("e.txt");
        fs.write_text(&p, "foo bar foo", WriteGuard::Unconditional).unwrap();
        // 出现两次 → 拒绝
        assert!(fs.edit_text(&p, "foo", "X", WriteGuard::Unconditional).is_err());
        fs.write_text(&p, "hello world", WriteGuard::Unconditional).unwrap();
        fs.edit_text(&p, "world", "rust", WriteGuard::Unconditional).unwrap();
        assert_eq!(fs.read_text(&p).unwrap(), "hello rust");
    }

    #[test]
    fn sensitive_file_denied() {
        let (dir, fs) = test_fs();
        let p = dir.path().join(".env");
        // 敏感文件写与读都应被拒绝
        assert!(matches!(
            fs.write_text(&p, "KEY=secret", WriteGuard::Unconditional),
            Err(FsError::PermissionDenied(_))
        ));
        fs::write(&p, "KEY=secret").unwrap(); // 绕过服务直接写
        assert!(matches!(
            fs.read_text(&p),
            Err(FsError::PermissionDenied(_))
        ));
    }

    #[test]
    fn outside_workspace_denied() {
        let (dir, fs) = test_fs();
        let outside = dir.path().parent().unwrap().join("escape.txt");
        assert!(matches!(
            fs.write_text(&outside, "x", WriteGuard::Unconditional),
            Err(FsError::PermissionDenied(_))
        ));
        assert!(matches!(
            fs.read_text(&outside),
            Err(FsError::PermissionDenied(_))
        ));
    }

    #[test]
    fn read_only_mode_denies_writes() {
        let (dir, mut fs) = test_fs();
        Arc::get_mut(&mut fs).unwrap().policy.read_only = true;
        let p = dir.path().join("r.txt");
        assert!(matches!(
            fs.write_text(&p, "x", WriteGuard::Unconditional),
            Err(FsError::PermissionDenied(_))
        ));
    }

    #[test]
    fn list_dir_sorted() {
        let (dir, fs) = test_fs();
        fs::write(dir.path().join("b.txt"), "1").unwrap();
        fs::write(dir.path().join("a.txt"), "2").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let entries = fs.list_dir(dir.path()).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "b.txt", "sub"]);
        assert_eq!(entries[2].kind, FsKind::Dir);
    }
}
