//! tools 插件 —— 核心工具命令层(参照 DSH 的 tool 层:dsh-tool-fs)。
//!
//! 消费宿主核心服务 [`FsService`](crate::fs::FsService) 与
//! [`SubprocessService`](crate::exec::SubprocessService),把文件系统与子进程
//! 能力暴露为斜杠命令(全走 ctx.on_command,卸载即消失):
//! - `/fs ls [path]` 列目录(带类型/大小)
//! - `/fs cat <path>` 读文本(登记 observed)
//! - `/fs write <path> <content>` 原子写
//! - `/fs edit <path> <from> <to>` 字面量编辑(需先 /fs cat 读过,L3 read-before-edit)
//! - `/fs stat <path>` 元信息(含版本指纹)
//! - `/fs log [n]` 审计流水(读/写/拒)
//! - `/run <cmdline>` 子进程执行(工作区根内,默认超时 30s)
//! - `/mode [read-only|workspace-write|full]` 查看/切换 L0 沙箱模式
//! - `/pwd` 显示工作区根

use crate::cordis::context::Context;
use crate::cordis::plugin::Plugin;
use crate::exec::SubprocessService;
use crate::fs::{FsKind, FsService, SandboxMode, WriteGuard};
use std::sync::Arc;

pub fn factory() -> Box<dyn Plugin> {
    Box::new(ToolsPlugin)
}

pub struct ToolsPlugin;

#[derive(serde::Deserialize, Default)]
pub struct ToolsOptions {
    #[serde(default)]
    pub run_timeout_secs: Option<u64>,
}

impl Plugin for ToolsPlugin {
    fn name(&self) -> &'static str {
        "tools"
    }

    fn inject(&self) -> &'static [&'static str] {
        &["fs", "subprocess"]
    }

    fn apply(&self, ctx: &Context) -> anyhow::Result<()> {
        let fs_svc = ctx
            .inject::<FsService>()
            .ok_or_else(|| anyhow::anyhow!("缺少 fs 服务"))?;
        let exec_svc = ctx
            .inject::<SubprocessService>()
            .ok_or_else(|| anyhow::anyhow!("缺少 subprocess 服务"))?;
        let opts: ToolsOptions = ctx.options()?;
        let run_timeout = std::time::Duration::from_secs(opts.run_timeout_secs.unwrap_or(30));

        // ---- /fs ----
        let fs1 = Arc::clone(&fs_svc);
        ctx.on_command("fs", move |args: &str| fs_cmd(&fs1, args));

        // ---- /pwd ----
        let root = fs_svc.policy.workspace_root.clone();
        ctx.on_command("pwd", move |_: &str| root.display().to_string());

        // ---- /mode (L0 沙箱模式,运行时可切) ----
        let fs_mode = Arc::clone(&fs_svc);
        ctx.on_command("mode", move |args: &str| {
            let args = args.trim();
            if args.is_empty() {
                format!("当前模式: {}(默认 workspace-write)", fs_mode.mode())
            } else {
                match args.parse::<SandboxMode>() {
                    Ok(m) => {
                        fs_mode.set_mode(m);
                        format!("模式已切换: {m}")
                    }
                    Err(e) => format!("错误: {e}"),
                }
            }
        });

        // ---- /run ----
        let exec = Arc::clone(&exec_svc);
        ctx.on_command("run", move |args: &str| {
            let args = args.trim();
            if args.is_empty() {
                return "用法: /run <命令行>".into();
            }
            let opts = crate::exec::RunOptions {
                cwd: None,
                timeout: run_timeout,
                env: Vec::new(),
            };
            match exec.run(args, &opts) {
                Ok(out) => {
                    let s = format!(
                        "exit={} ({:?})\n--- stdout ---\n{}\n--- stderr ---\n{}",
                        out.status,
                        out.duration_ms,
                        out.stdout,
                        out.stderr
                    );
                    if s.trim().is_empty() {
                        format!("exit={}(无输出)", out.status)
                    } else {
                        s
                    }
                }
                Err(e) => format!("错误: {e:#}"),
            }
        });

        Ok(())
    }
}

/// `/fs ls|cat|write|stat [path] [content]` 的实现
fn fs_cmd(fs: &Arc<FsService>, args: &str) -> String {
    let mut parts = args.splitn(3, ' ');
    let sub = parts.next().unwrap_or("").trim();
    let path = parts.next().unwrap_or("").trim();
    let rest = parts.next().map(|s| s.trim()).unwrap_or("");

    match sub {
        "ls" => {
            let p = if path.is_empty() { "." } else { path };
            match fs.resolve(p) {
                Ok(abs) => match fs.list_dir(&abs) {
                    Ok(entries) => {
                        if entries.is_empty() {
                            format!("(空目录) {}", abs.display())
                        } else {
                            let mut lines = format!("{}:", abs.display()).to_string();
                            for e in entries {
                                let icon = match e.kind {
                                    FsKind::Dir => "d",
                                    FsKind::Symlink => "l",
                                    _ => "-",
                                };
                                let size = e.size.map(|s| s.to_string()).unwrap_or_else(|| "-".into());
                                lines.push_str(&format!("\n{icon} {size:>10} {}", e.name));
                            }
                            lines
                        }
                    }
                    Err(e) => e.to_string(),
                },
                Err(e) => e.to_string(),
            }
        }
        "cat" => {
            if path.is_empty() {
                return "用法: /fs cat <path>".into();
            }
            match fs.resolve(path) {
                Ok(abs) => match fs.read_text(&abs) {
                    Ok(text) => text,
                    Err(e) => e.to_string(),
                },
                Err(e) => e.to_string(),
            }
        }
        "write" => {
            if path.is_empty() {
                return "用法: /fs write <path> <content>".into();
            }
            match fs.resolve(path) {
                Ok(abs) => match fs.write_text(&abs, rest, WriteGuard::Unconditional) {
                    Ok(()) => format!("已写入 {}", abs.display()),
                    Err(e) => e.to_string(),
                },
                Err(e) => e.to_string(),
            }
        }
        "stat" => {
            if path.is_empty() {
                return "用法: /fs stat <path>".into();
            }
            match fs.resolve(path) {
                Ok(abs) => match fs.stat(&abs) {
                    Ok(Some(info)) => format!(
                        "{} kind={:?} size={} version=v{}",
                        abs.display(),
                        info.kind,
                        info.size.unwrap_or(0),
                        info.version
                    ),
                    Ok(None) => format!("FS_NOT_FOUND: {}", abs.display()),
                    Err(e) => e.to_string(),
                },
                Err(e) => e.to_string(),
            }
        }
        "edit" => {
            // /fs edit <path> <from> <to>(参数无空格;需要先 /fs cat 读过)
            let mut p2 = rest.splitn(3, ' ');
            let from = p2.next().unwrap_or("");
            let to = p2.next().unwrap_or("");
            if path.is_empty() || from.is_empty() {
                return "用法: /fs edit <path> <from> <to>(先 /fs cat 读过该文件)".into();
            }
            match fs.resolve(path) {
                Ok(abs) => match fs.edit_text(&abs, from, to, WriteGuard::Unconditional) {
                    Ok(()) => format!("已编辑 {}", abs.display()),
                    Err(e) => e.to_string(),
                },
                Err(e) => e.to_string(),
            }
        }
        "log" => {
            let n = path.parse::<usize>().unwrap_or(20);
            let log = fs.access_log(n);
            if log.is_empty() {
                return "(暂无访问记录)".into();
            }
            let mut lines = Vec::new();
            for l in log {
                lines.push(format!(
                    "{} {:<6} {} {}",
                    l.time,
                    l.op,
                    if l.ok { "OK  " } else { "DENY" },
                    l.path
                ));
            }
            lines.join("\n")
        }
        _ => "用法: /fs ls [path] | /fs cat <path> | /fs write <path> <content> | /fs edit <path> <from> <to> | /fs stat <path> | /fs log [n]".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cordis::loader::{Loader, LoaderService};
    use crate::llm::{LlmClient, LlmConfig};

    /// 装配:fs + subprocess 核心服务 + tools 插件
    fn test_loader(dir: &std::path::Path) -> Arc<std::sync::Mutex<Loader>> {
        let client = LlmClient::new(LlmConfig {
            base_url: "http://127.0.0.1:9".into(),
            api_key: "t".into(),
            model: "m".into(),
            timeout_secs: 1,
            max_concurrent: 2,
        });
        let loader = Arc::new(std::sync::Mutex::new(Loader::new(client, vec![factory])));
        {
            let l = loader.lock().unwrap();
            l.root().provide(Arc::new(LoaderService { loader: Arc::clone(&loader) }));
            l.root().provide(Arc::new(FsService::new(dir.to_path_buf())));
            l.root().provide(Arc::new(SubprocessService::new(dir.to_path_buf())));
        }
        loader
            .lock()
            .unwrap()
            .load("tools", serde_yaml::Value::Null)
            .unwrap();
        loader
    }

    #[test]
    fn fs_commands_work() {
        let dir = tempfile::TempDir::new().unwrap();
        let loader = test_loader(dir.path());
        let root = loader.lock().unwrap().root().clone();

        // /fs write → /fs cat
        let w = root.run_command("fs write hello.txt 你好,localai").unwrap();
        assert!(w.contains("已写入"), "{w}");
        let c = root.run_command("fs cat hello.txt").unwrap();
        assert_eq!(c, "你好,localai");
        // /fs stat
        let s = root.run_command("fs stat hello.txt").unwrap();
        assert!(s.contains("version=v"), "{s}");
        // /fs ls
        let ls = root.run_command("fs ls .").unwrap();
        assert!(ls.contains("hello.txt"), "{ls}");
        // /fs cat 敏感文件 → 权限拒绝
        std::fs::write(dir.path().join(".env"), "KEY=x").unwrap();
        let deny = root.run_command("fs cat .env").unwrap();
        assert!(deny.contains("敏感文件"), "{deny}");
        // /fs cat 越界 → 权限拒绝(用绝对路径)
        let outside = dir.path().parent().unwrap().join("x.txt");
        std::fs::write(&outside, "x").unwrap();
        let deny2 = root.run_command(&format!("fs cat {}", outside.display())).unwrap();
        assert!(deny2.contains("越界"), "{deny2}");
        // /pwd(FsPolicy 规范化过 workspace_root,期望值同样规范化)
        let pwd = root.run_command("pwd").unwrap();
        let expected = std::fs::canonicalize(dir.path())
            .unwrap_or_else(|_| dir.path().to_path_buf())
            .to_string_lossy()
            .to_string();
        assert_eq!(pwd, expected);
    }

    #[test]
    fn run_command_works() {
        let dir = tempfile::TempDir::new().unwrap();
        let loader = test_loader(dir.path());
        let root = loader.lock().unwrap().root().clone();
        let out = root.run_command("run echo hello").unwrap();
        assert!(out.contains("exit=0"), "{out}");
        assert!(out.contains("hello"), "{out}");
    }

    #[test]
    fn mode_command_switches_and_fences() {
        let dir = tempfile::TempDir::new().unwrap();
        let loader = test_loader(dir.path());
        let root = loader.lock().unwrap().root().clone();

        // 默认 workspace-write
        let m = root.run_command("mode").unwrap();
        assert!(m.contains("workspace-write"), "{m}");
        // 切 read-only → 写被拒,读仍可
        let r = root.run_command("mode read-only").unwrap();
        assert!(r.contains("read-only"), "{r}");
        let fs = root.inject::<FsService>().unwrap();
        assert_eq!(fs.mode(), SandboxMode::ReadOnly);
        let w = root.run_command("fs write x.txt hi").unwrap();
        assert!(w.contains("只读"), "{w}");
        // 切回 workspace-write → 恢复
        root.run_command("mode workspace-write").unwrap();
        assert_eq!(fs.mode(), SandboxMode::WorkspaceWrite);
        let ok = root.run_command("fs write x.txt hi").unwrap();
        assert!(ok.contains("已写入"), "{ok}");
        // /fs log 有流水(含一次 DENY)
        let log = root.run_command("fs log 10").unwrap();
        assert!(log.contains("DENY"), "{log}");
    }

    #[test]
    fn unload_removes_commands() {
        let dir = tempfile::TempDir::new().unwrap();
        let loader = test_loader(dir.path());
        loader.lock().unwrap().unload("tools").unwrap();
        let root = loader.lock().unwrap().root().clone();
        assert!(root.run_command("fs").is_none());
        assert!(root.run_command("run").is_none());
        assert!(root.run_command("pwd").is_none());
    }
}
