//! system —— 启动时收集本机基本信息,注入 base prompt(chat 系统提示词)。
//!
//! 轻量实现,零第三方依赖:OS/架构/主机名/用户/工作目录/默认 shell/CPU 数/
//! 语言环境/终端。附带 Windows shell 策略:优先 cmd,仅当 cmd 无法胜任时用 PowerShell。

use std::fmt::Write as _;

pub struct SystemInfo {
    pub os: &'static str,
    pub os_family: &'static str,
    pub arch: &'static str,
    pub host: String,
    pub user: String,
    pub cwd: String,
    pub shell: String,
    pub cpus: usize,
    pub lang: String,
    pub term: String,
    pub windows: bool,
}

impl SystemInfo {
    /// 收集当前环境基本信息(纯环境变量 + std 常量,不执行外部命令)。
    pub fn collect() -> Self {
        let host = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "unknown".into());
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "unknown".into());
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unknown".into());
        // unix 用 SHELL;Windows 用 COMSPEC(通常是 cmd.exe 路径)
        let shell = std::env::var("SHELL")
            .or_else(|_| std::env::var("COMSPEC"))
            .unwrap_or_else(|_| "unknown".into());
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let lang = ["LANG", "LC_ALL", "LANGUAGE"]
            .iter()
            .find_map(|k| std::env::var(k).ok())
            .unwrap_or_default();
        let term = std::env::var("TERM_PROGRAM")
            .or_else(|_| std::env::var("TERM"))
            .unwrap_or_default();
        Self {
            os: std::env::consts::OS,
            os_family: std::env::consts::FAMILY,
            arch: std::env::consts::ARCH,
            host,
            user,
            cwd,
            shell,
            cpus,
            lang,
            term,
            windows: cfg!(windows),
        }
    }

    /// 格式化为注入 base prompt 的文本块。
    pub fn to_prompt(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "- 操作系统: {} (family {}, {})", self.os, self.os_family, self.arch);
        let _ = writeln!(s, "- 主机名: {}", self.host);
        let _ = writeln!(s, "- 用户: {}", self.user);
        let _ = writeln!(s, "- 当前工作目录: {}", self.cwd);
        let _ = writeln!(s, "- 默认 shell: {}", self.shell);
        let _ = writeln!(s, "- CPU 逻辑核数: {}", self.cpus);
        if !self.lang.is_empty() {
            let _ = writeln!(s, "- 语言环境: {}", self.lang);
        }
        if !self.term.is_empty() {
            let _ = writeln!(s, "- 终端: {}", self.term);
        }
        if self.windows {
            s.push_str(
                "- Shell 策略: 优先使用 cmd(cmd /C 执行,支持内置命令/管道/重定向);\n  \
                 仅当 cmd 无法胜任(需要 PowerShell 专属语法)时才用 powershell -Command ...",
            );
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_fills_basic_fields() {
        let s = SystemInfo::collect();
        assert!(!s.os.is_empty());
        assert!(!s.arch.is_empty());
        assert!(!s.cwd.is_empty());
        assert!(s.cpus >= 1);
        let p = s.to_prompt();
        assert!(p.contains("操作系统"));
        assert!(p.contains(&s.cwd), "prompt 应包含工作目录");
        assert!(p.contains("默认 shell"));
    }

    #[test]
    fn windows_prompt_carries_shell_policy() {
        let s = SystemInfo::collect();
        if s.windows {
            assert!(s.to_prompt().contains("cmd"), "Windows 提示应声明 cmd 优先策略");
        }
    }
}
