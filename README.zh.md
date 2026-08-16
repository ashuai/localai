# localai

用 Rust 实现 **DSH 式 cordis 插件模式**的本地 LLM 壳 —— 内核只负责插拔,能力全部来自插件。

- **模型层**:接入本地 oMLX 服务器(`192.168.0.5:9870`,主模型 `Qwen3.6-35b`),
  按**小上下文、零成本、频繁微调用**原则设计(论证见 [`docs/model-layer.md`](docs/model-layer.md));
- **架构**:cordis 模式 —— 事件总线 + 服务注入 + effect 生命周期(卸载即全部回滚)+ Loader
  (见 [`docs/architecture.md`](docs/architecture.md));
- **交互**:基于 [ratatui](https://github.com/ratatui/ratatui) 的 TUI;另有
  `--once` / `--micro` 非交互模式便于脚本验证。

## 快速开始(用编译好的二进制)

从 **[GitHub Releases](https://github.com/ashuai/localai/releases)** 下载对应平台的压缩包
(或 Actions 最新一次运行的 **Artifacts**),解压后:

```bash
# 1. 首次使用配置 API key(服务器 ~/.omlx/settings.json 的 auth.api_key)
cp .env.example .env        # 编辑 .env 填入 LLM_API_KEY

# 2. 直接运行编译好的 localai
./localai                   # TUI
./localai --once "你好"     # 非交互跑一轮对话(打印回复后退出)
./localai --micro "..."     # 微调用流水线演示(意图 → 并行抽取)
./localai --list-plugins    # 查看插件与加载状态
```

> Windows 下是 `localai.exe`;`.env` 与可执行文件放在同一目录。
> 压缩包内含:`localai[.exe]` + `localai.yml` + `.env.example` + `README.md`。

## 使用

- 普通输入 → 回车发送(`chat` 插件 → 本地 LLM);
- `/micro <文本>` → 微调用流水线演示(意图分类 → 并行标题/关键词/摘要);
- `/load <插件>` `/unload <插件>` → 运行时热插拔(核心性质:卸载即回滚);
- `/model Qwen3.6-35b` → 切换模型(默认主模型 35b;27b 是 Claude 蒸馏变体,非必要不用);
- `/plugins` `/help` `/clear` `/quit`;**PageUp/PageDown** 翻看历史;
- `/fs ls|cat|write|stat [路径]` —— 文件系统工具,带策略 fence(工作区边界 + 敏感文件保护,
  对齐 DSH 的 dsh-fs-sandbox);
- `/run <命令行>` —— 工作区根内执行子进程(默认超时 30s);
- `/pwd` —— 显示工作区根。

每次回复后,microtask 插件会自动做 2 个并行微调用(意图分类 + 关键词),状态区显示
`[micro] 意图=question | 关键词=… (1.2s)` —— 零成本环境微调用的活体演示。

## 目录

```
src/cordis/    插件化核心(Context / 事件 / Service / Plugin / Loader)
src/llm/       模型层(OpenAI 兼容客户端 + 微调用协议)
src/fs/        FsService + 策略 fence(工作区边界、敏感文件保护)
src/exec/      SubprocessService(超时、工作目录限定)
src/plugins/   内置插件(chat / microtask / tools / tui)
src/tui/       TUI 渲染状态(归 tui 插件所有)
docs/          architecture.md(cordis 映射)· model-layer.md(模型层论证)
changelog/     版本日志 —— CI 发版的唯一数据源
```

## 多平台构建与发布(GitHub Actions)

无需本地工具链。**何时触发**(规则参照 DSH 项目):

- **自动构建**:向 `main` push 且改动命中 `changelog/**` 或 `.github/workflows/**`
  (即出现新版本日志、或 workflow 变更)才自动构建 + 发布;
  平时改 `src/`、`README.md` **不触发**;
- **手动构建**:GitHub → **Actions** → **Run workflow**(`workflow_dispatch`)。

**发版**(推荐方式,产物进 Release 页一键下载):

```bash
cp changelog/v0.1.0.md changelog/v0.2.0.md   # 写新版本日志(唯一要做的事)
git add changelog && git commit -m "release: v0.2.0" && git push
```

流程自动完成:三平台构建通过 → 读 changelog 最高版本 → 对应 Release 不存在则发布
(`vX.Y.Z`,notes 用日志文件内容),产物重命名为 `localai-v<版本>-<平台>.*`:
`localai-v0.1.0-win-x64.zip`、`localai-v0.1.0-macos-{aarch64,x86_64}.tar.gz`、
`localai-v0.1.0-linux-x64.tar.gz`(CentOS 7+ 兼容,glibc 2.17)。已发布的版本
自动跳过,不会重复发版。

## 从源码构建(开发者)

```bash
cargo run --release          # TUI
cargo run -- --once "你好"   # 非交互一轮对话
cargo test                   # cordis 核心性质 + loader + JSON 提取
```

## 测试

```bash
cargo test    # 卸载回滚 / 事件总线 / 服务注入 / loader / JSON 提取
```
