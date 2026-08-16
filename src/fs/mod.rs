//! FsService —— 文件系统工具服务(参照 DSH 真实源码 `dsh-fs` / `dsh-fs-sandbox` /
//! `dsh-fs-observation-policy`)。
//!
//! 四层权限模型:
//! - **L0 模式 SandboxMode**:read-only / workspace-write(默认)/ full,运行时可切(`/mode`);
//! - **L1 边界**:路径规范化后必须落在工作区根/额外可写区内(full 放开);
//! - **L2 文件分类**:敏感文件(.env/*.key/id_rsa*/*.pem)任何模式读写都拒;
//! - **L3 操作守卫**:edit 前必须已读(read-before-edit)、版本守卫、原子写。
//! 审计:每次操作记入环形 ledger,`/fs log` 可查。

use crate::cordis::service::Service;
use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

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

// ---------- L0 模式(SandboxMode,对齐 dsh-fs-sandbox 的 mode) ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// 只读:拒绝一切写
    ReadOnly,
    /// 工作区可写(默认)
    WorkspaceWrite,
    /// 放开边界(可读写工作区外),敏感文件与守卫仍保留
    Full,
}

impl fmt::Display for SandboxMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SandboxMode::ReadOnly => write!(f, "read-only"),
            SandboxMode::WorkspaceWrite => write!(f, "workspace-write"),
            SandboxMode::Full => write!(f, "full"),
        }
    }
}

impl std::str::FromStr for SandboxMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "read-only" | "readonly" | "ro" => Ok(SandboxMode::ReadOnly),
            "workspace-write" | "workspace" | "ww" => Ok(SandboxMode::WorkspaceWrite),
            "full" | "all" => Ok(SandboxMode::Full),
            _ => Err(format!("未知模式 `{s}`(可选:read-only / workspace-write / full)")),
        }
    }
}

// ---------- L1/L2 静态规则(路径边界 + 文件分类) ----------

#[derive(Debug, Clone)]
pub struct FsPolicy {
    /// 工作区根:默认当前目录
    pub workspace_root: PathBuf,
    /// 额外可写区(如系统临时目录)
    pub extra_writable: Vec<PathBuf>,
    /// 敏感文件名/后缀模式(L2):读写都拒绝,任何模式下不放松
    pub sensitive: Vec<String>,
}

impl FsPolicy {
    pub fn new(workspace_root: PathBuf) -> Self {
        // 规范化根,避免 /var vs /private/var 这类符号链接导致 starts_with 失败
        let workspace_root = fs::canonicalize(&workspace_root).unwrap_or(workspace_root);
        Self {
            workspace_root,
            extra_writable: Vec::new(),
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

    /// L1:路径是否落在可写根内(workspace_root 或 extra_writable 之一)
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
}

// ---------- 审计流水 ----------

#[derive(Debug, Clone)]
pub struct AccessLog {
    pub time: String,
    pub op: String,
    pub path: String,
    pub ok: bool,
}

// ---------- FsService ----------

pub struct FsService {
    pub policy: FsPolicy,
    /// L0 会话模式(运行时可切)
    pub mode: Arc<RwLock<SandboxMode>>,
    /// L3 观察状态:本会话已读过的文件(规范化路径);edit 前置条件
    observed: Mutex<HashSet<PathBuf>>,
    /// 审计 ledger(环形,最多 200 条)
    ledger: Mutex<VecDeque<AccessLog>>,
}

impl Service for FsService {
    fn service_name_static() -> &'static str {
        "fs"
    }
}

impl FsService {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            policy: FsPolicy::new(workspace_root),
            mode: Arc::new(RwLock::new(SandboxMode::WorkspaceWrite)),
            observed: Mutex::new(HashSet::new()),
            ledger: Mutex::new(VecDeque::new()),
        }
    }

    pub fn mode(&self) -> SandboxMode {
        *self.mode.read().unwrap()
    }

    pub fn set_mode(&self, mode: SandboxMode) {
        *self.mode.write().unwrap() = mode;
    }

    /// 最近 N 条访问流水(审计)
    pub fn access_log(&self, n: usize) -> Vec<AccessLog> {
        self.ledger
            .lock()
            .unwrap()
            .iter()
            .rev()
            .take(n)
            .cloned()
            .collect()
    }

    fn log(&self, op: &str, path: &Path, ok: bool) {
        let mut l = self.ledger.lock().unwrap();
        if l.len() >= 200 {
            l.pop_front();
        }
        l.push_back(AccessLog {
            time: chrono_now(),
            op: op.into(),
            path: path.display().to_string(),
            ok,
        });
    }

    // ---------- fence(L0 模式 + L1 边界 + L2 敏感) ----------

    fn fence_read(&self, normalized: &Path) -> Result<(), FsError> {
        let mode = self.mode();
        if mode != SandboxMode::Full && !self.policy.within_writable_roots(normalized) {
            return Err(FsError::PermissionDenied(format!(
                "读取越界(工作区外,模式={mode}): {}",
                normalized.display()
            )));
        }
        if self.policy.is_sensitive(normalized) {
            return Err(FsError::PermissionDenied(format!(
                "敏感文件拒绝读取(L2): {}",
                normalized.display()
            )));
        }
        Ok(())
    }

    fn fence_write(&self, normalized: &Path) -> Result<(), FsError> {
        let mode = self.mode();
        if mode == SandboxMode::ReadOnly {
            return Err(FsError::PermissionDenied(format!(
                "只读模式(L0)拒绝一切写: {}",
                normalized.display()
            )));
        }
        if mode != SandboxMode::Full && !self.policy.within_writable_roots(normalized) {
            return Err(FsError::PermissionDenied(format!(
                "写入越界(工作区外,模式={mode}): {}",
                normalized.display()
            )));
        }
        if self.policy.is_sensitive(normalized) {
            return Err(FsError::PermissionDenied(format!(
                "敏感文件拒绝写入(L2): {}",
                normalized.display()
            )));
        }
        Ok(())
    }

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
        if let Err(e) = self.fence_read(&normalized) {
            self.log("stat", &normalized, false);
            return Err(e);
        }
        let result = match fs::symlink_metadata(&normalized) {
            Ok(meta) => Ok(Some(fs_info(&meta))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(FsError::Io(e.to_string())),
        };
        self.log("stat", &normalized, result.is_ok());
        result
    }

    /// 读整个文本文件;二进制/NUL 拒绝(对齐 FS_NOT_TEXT);登记 observed
    pub fn read_text(&self, path: &Path) -> Result<String, FsError> {
        let normalized = self.normalize(path)?;
        if let Err(e) = self.fence_read(&normalized) {
            self.log("read", &normalized, false);
            return Err(e);
        }
        let meta = fs::metadata(&normalized).map_err(|e| map_meta_err(e, &normalized))?;
        if !meta.is_file() {
            return Err(FsError::NotDirectory(normalized.display().to_string()));
        }
        let bytes = fs::read(&normalized).map_err(|e| FsError::Io(e.to_string()))?;
        if bytes.contains(&0) {
            return Err(FsError::NotText(normalized.display().to_string()));
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| FsError::NotText(normalized.display().to_string()))?;
        self.observed.lock().unwrap().insert(normalized.clone());
        self.log("read", &normalized, true);
        Ok(text)
    }

    /// 读原始字节,带 max_bytes 上限(对齐 readBytes 的 FS_TOO_LARGE);登记 observed
    pub fn read_bytes(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, FsError> {
        let normalized = self.normalize(path)?;
        if let Err(e) = self.fence_read(&normalized) {
            self.log("read", &normalized, false);
            return Err(e);
        }
        let meta = fs::metadata(&normalized).map_err(|e| map_meta_err(e, &normalized))?;
        if !meta.is_file() {
            return Err(FsError::NotDirectory(normalized.display().to_string()));
        }
        if meta.len() > max_bytes {
            return Err(FsError::TooLarge(max_bytes));
        }
        let bytes = fs::read(&normalized).map_err(|e| FsError::Io(e.to_string()))?;
        self.observed.lock().unwrap().insert(normalized.clone());
        self.log("read", &normalized, true);
        Ok(bytes)
    }

    /// 列出目录直接子项(稳定名字序;不读内容,不登记 observed)
    pub fn list_dir(&self, path: &Path) -> Result<Vec<FsEntry>, FsError> {
        let normalized = self.normalize(path)?;
        if let Err(e) = self.fence_read(&normalized) {
            self.log("ls", &normalized, false);
            return Err(e);
        }
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
        self.log("ls", &normalized, true);
        Ok(entries)
    }

    /// 原子写(临时文件 + rename);支持版本守卫;写不要求已读(与 DSH 一致)
    pub fn write_text(&self, path: &Path, content: &str, guard: WriteGuard) -> Result<(), FsError> {
        let normalized = self.normalize(path)?;
        if let Err(e) = self.fence_write(&normalized) {
            self.log("write", &normalized, false);
            return Err(e);
        }
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
        let result = atomic_write(&normalized, content.as_bytes());
        self.log("write", &normalized, result.is_ok());
        result
    }

    /// 字面量编辑(对齐 editText):**必须先读过该文件**(L3 read-before-edit),
    /// from 必须恰好出现一次,替换后原子写。
    pub fn edit_text(
        &self,
        path: &Path,
        from: &str,
        to: &str,
        guard: WriteGuard,
    ) -> Result<(), FsError> {
        let normalized = self.normalize(path)?;
        if let Err(e) = self.fence_write(&normalized) {
            self.log("edit", &normalized, false);
            return Err(e);
        }
        if !self.observed.lock().unwrap().contains(&normalized) {
            self.log("edit", &normalized, false);
            return Err(FsError::PermissionDenied(format!(
                "编辑前需先读取(L3 read-before-edit): {}",
                normalized.display()
            )));
        }
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
        let result = atomic_write(&normalized, replaced.as_bytes());
        self.log("edit", &normalized, result.is_ok());
        result
    }

    /// 创建目录(含父目录)
    pub fn mkdir(&self, path: &Path) -> Result<(), FsError> {
        let normalized = self.normalize(path)?;
        if let Err(e) = self.fence_write(&normalized) {
            self.log("mkdir", &normalized, false);
            return Err(e);
        }
        let result = fs::create_dir_all(&normalized).map_err(|e| FsError::Io(e.to_string()));
        self.log("mkdir", &normalized, result.is_ok());
        result
    }
}

// ---------- helpers ----------

fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:02}:{:02}:{:02}", (d.as_secs() / 3600) % 24, (d.as_secs() / 60) % 60, d.as_secs() % 60)
}

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

    fn test_fs() -> (tempfile::TempDir, Arc<FsService>) {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = Arc::new(FsService::new(dir.path().to_path_buf()));
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
        assert!(matches!(
            fs.write_text(&p, "v3", WriteGuard::ReplaceIfVersion(v1)),
            Err(FsError::StaleVersion(_, _))
        ));
    }

    #[test]
    fn edit_requires_read_before_edit() {
        let (dir, fs) = test_fs();
        let p = dir.path().join("e.txt");
        fs.write_text(&p, "hello world", WriteGuard::Unconditional).unwrap();
        // 未读直接编辑 → L3 拒绝
        let err = fs.edit_text(&p, "world", "rust", WriteGuard::Unconditional).unwrap_err();
        assert!(
            matches!(&err, FsError::PermissionDenied(m) if m.contains("先读取")),
            "{err}"
        );
        // 先读再编辑 → 通过
        fs.read_text(&p).unwrap();
        fs.edit_text(&p, "world", "rust", WriteGuard::Unconditional).unwrap();
        assert_eq!(fs.read_text(&p).unwrap(), "hello rust");
    }

    #[test]
    fn edit_rejects_ambiguous_pattern() {
        let (dir, fs) = test_fs();
        let p = dir.path().join("e2.txt");
        fs.write_text(&p, "foo bar foo", WriteGuard::Unconditional).unwrap();
        fs.read_text(&p).unwrap();
        assert!(fs.edit_text(&p, "foo", "X", WriteGuard::Unconditional).is_err());
    }

    #[test]
    fn sensitive_file_denied() {
        let (dir, fs) = test_fs();
        let p = dir.path().join(".env");
        assert!(matches!(
            fs.write_text(&p, "KEY=secret", WriteGuard::Unconditional),
            Err(FsError::PermissionDenied(_))
        ));
        fs::write(&p, "KEY=secret").unwrap();
        assert!(matches!(fs.read_text(&p), Err(FsError::PermissionDenied(_))));
    }

    #[test]
    fn outside_workspace_denied_in_default_mode() {
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
    fn mode_read_only_then_back() {
        let (dir, fs) = test_fs();
        let p = dir.path().join("r.txt");
        fs.set_mode(SandboxMode::ReadOnly);
        assert!(matches!(
            fs.write_text(&p, "x", WriteGuard::Unconditional),
            Err(FsError::PermissionDenied(m)) if m.contains("只读")
        ));
        assert!(fs.stat(&p).is_ok(), "只读模式仍可读");
        fs.set_mode(SandboxMode::WorkspaceWrite);
        fs.write_text(&p, "x", WriteGuard::Unconditional).unwrap();
    }

    #[test]
    fn mode_full_allows_outside_but_keeps_sensitive() {
        let (dir, fs) = test_fs();
        fs.set_mode(SandboxMode::Full);
        // full:工作区外可写
        let outside_dir = tempfile::TempDir::new().unwrap();
        let outside = outside_dir.path().join("o.txt");
        fs.write_text(&outside, "x", WriteGuard::Unconditional).unwrap();
        assert_eq!(fs.read_text(&outside).unwrap(), "x");
        // full:敏感仍拒
        let p = dir.path().join(".env");
        assert!(matches!(
            fs.write_text(&p, "K=1", WriteGuard::Unconditional),
            Err(FsError::PermissionDenied(_))
        ));
        // full:读前编辑仍要求
        let e = dir.path().join("e.txt");
        fs.write_text(&e, "ab", WriteGuard::Unconditional).unwrap();
        assert!(fs.edit_text(&e, "a", "x", WriteGuard::Unconditional).is_err());
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

    #[test]
    fn ledger_records_access_including_denials() {
        let (dir, fs) = test_fs();
        let p = dir.path().join("l.txt");
        fs.write_text(&p, "x", WriteGuard::Unconditional).unwrap();
        fs.read_text(&p).unwrap();
        // 拒绝也要记录
        let _ = fs.read_text(&dir.path().join(".env"));
        let log = fs.access_log(10);
        assert!(log.iter().any(|l| l.op == "read" && l.ok), "应有成功读记录");
        assert!(log.iter().any(|l| !l.ok), "应有拒绝记录");
    }
}
