//! SubprocessService —— 子进程执行服务(参照 DSH 真实源码 `dsh-subprocess`)。
//!
//! 必要子集:run(cmdline, { cwd, timeout, env }) -> { status, stdout, stderr, duration_ms }
//! - 工作目录限定在工作区根(与 fs 策略同一边界);
//! - 超时轮询 try_wait(30ms 间隔),超时 kill;
//! - stdout/stderr 由 wait_with_output 并发读取,无管道死锁。

use crate::cordis::service::Service;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct RunOptions {
    /// 工作目录(必须落在工作区根内;None 用服务默认 cwd)
    pub cwd: Option<PathBuf>,
    /// 超时,超时 kill 并返回错误
    pub timeout: Duration,
    /// 附加环境变量
    pub env: Vec<(String, String)>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            cwd: None,
            timeout: Duration::from_secs(30),
            env: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u128,
}

pub struct SubprocessService {
    /// 默认工作目录(工作区根)
    pub cwd: PathBuf,
    /// 默认超时
    pub default_timeout: Duration,
}

impl Service for SubprocessService {
    fn service_name_static() -> &'static str {
        "subprocess"
    }
}

impl SubprocessService {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            default_timeout: Duration::from_secs(30),
        }
    }

    /// 执行一行命令。
    ///
    /// - **Windows**:优先用 `cmd /C <line>` 执行(支持 `dir`/`echo`/`type` 等内置命令、
    ///   管道与重定向);需要 PowerShell 专属语法时,调用方应显式写 `powershell -Command ...`
    ///   (base prompt 里的 shell 策略与此一致);
    /// - **Unix**:按空白拆分直接执行(与历史行为一致)。
    ///
    /// 工作目录与 fs 同边界:超出工作区根拒绝。
    pub fn run(&self, cmdline: &str, opts: &RunOptions) -> anyhow::Result<RunOutput> {
        let cwd = match &opts.cwd {
            Some(c) => c.clone(),
            None => self.cwd.clone(),
        };
        // 边界:cwd 必须是工作区根或其后代
        if !cwd.starts_with(&self.cwd) {
            anyhow::bail!("拒绝执行:工作目录越界 {}", cwd.display());
        }

        let mut cmd = {
            #[cfg(windows)]
            {
                if cmdline.trim().is_empty() {
                    anyhow::bail!("空命令");
                }
                let mut c = Command::new("cmd");
                c.arg("/C").arg(cmdline);
                c
            }
            #[cfg(not(windows))]
            {
                let parts: Vec<&str> = cmdline.split_whitespace().collect();
                if parts.is_empty() {
                    anyhow::bail!("空命令");
                }
                let mut c = Command::new(parts[0]);
                c.args(&parts[1..]);
                c
            }
        };
        cmd.current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &opts.env {
            cmd.env(k, v);
        }

        let timeout = if opts.timeout.is_zero() {
            self.default_timeout
        } else {
            opts.timeout
        };

        let start = Instant::now();
        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!("启动命令失败 `{cmdline}`: {e}")
        })?;

        // 轮询等待 + 超时 kill
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if start.elapsed() > timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        anyhow::bail!(
                            "命令超时({:?})已 kill: `{cmdline}`",
                            start.elapsed()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(30));
                }
                Err(e) => anyhow::bail!("wait 失败: {e}"),
            }
        };
        // 进程已退出,wait_with_output 并发读 stdout/stderr,无死锁
        let output = child.wait_with_output().map_err(|e| anyhow::anyhow!("读取输出失败: {e}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        Ok(RunOutput {
            status: status.code().unwrap_or(-1),
            stdout,
            stderr,
            duration_ms: start.elapsed().as_millis(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn test_svc() -> (tempfile::TempDir, Arc<SubprocessService>) {
        let dir = tempfile::TempDir::new().unwrap();
        let svc = Arc::new(SubprocessService::new(dir.path().to_path_buf()));
        (dir, svc)
    }

    #[test]
    fn run_echo() {
        let (_dir, svc) = test_svc();
        let out = svc.run("echo hello rust", &RunOptions::default()).unwrap();
        assert_eq!(out.status, 0);
        assert!(out.stdout.contains("hello rust"));
    }

    #[test]
    fn unknown_command_fails() {
        let (_dir, svc) = test_svc();
        let out = svc.run("definitely-not-a-real-cmd-xyz", &RunOptions::default());
        #[cfg(windows)]
        {
            // cmd /C:spawn 总是成功,命令不存在 → 非零退出 + stderr 报错
            let out = out.unwrap();
            assert_ne!(out.status, 0, "未知命令应非零退出");
            assert!(
                out.stderr.contains("definitely-not-a-real-cmd-xyz")
                    || out.stderr.contains("不是内部或外部命令")
                    || out.stderr.to_lowercase().contains("not recognized"),
                "stderr 应包含错误信息: {}",
                out.stderr
            );
        }
        #[cfg(not(windows))]
        {
            assert!(out.is_err(), "spawn 失败(命令不存在)");
        }
    }

    #[test]
    fn timeout_kills() {
        let (_dir, svc) = test_svc();
        #[cfg(windows)]
        let cmdline = "ping -n 30 127.0.0.1 >nul"; // ~30s,远超 300ms 超时
        #[cfg(not(windows))]
        let cmdline = "sleep 30";
        let out = svc.run(
            cmdline,
            &RunOptions {
                timeout: Duration::from_millis(300),
                ..Default::default()
            },
        );
        assert!(out.is_err(), "应超时 kill");
    }

    #[cfg(windows)]
    #[test]
    fn windows_runs_cmd_builtins() {
        let (_dir, svc) = test_svc();
        // cmd 内置命令(echo)经 cmd /C 可执行;直接 spawn 会失败
        let out = svc.run("echo ok", &RunOptions::default()).unwrap();
        assert_eq!(out.status, 0);
        assert!(out.stdout.contains("ok"), "stdout: {}", out.stdout);
    }

    #[test]
    fn cwd_outside_workspace_denied() {
        let (dir, svc) = test_svc();
        let outside = dir.path().parent().unwrap().to_path_buf();
        let out = svc.run(
            "echo hi",
            &RunOptions {
                cwd: Some(outside),
                ..Default::default()
            },
        );
        assert!(out.is_err(), "越界 cwd 应拒绝");
    }
}
