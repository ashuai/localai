# localai

DSH 式 **cordis 插件模式**的 Rust 本地 LLM 壳 —— 内核只负责插拔,能力全部来自插件。

- **模型层**:接入本地 oMLX 服务器(192.168.0.5:9870,主模型 `Qwen3.6-35b`),
  按"小上下文、零成本、频繁微调用"原则设计(论证见 `docs/model-layer.md`);
- **架构**:cordis 模式(事件总线 + 服务注入 + effect 生命周期 + Loader)的
  Rust 映射(见 `docs/architecture.md`);
- **交互**:TUI(ratatui);另有 `--once` 非交互模式便于脚本验证。

## 快速开始

```bash
cd localai
cp .env.example .env        # 填入 LLM_API_KEY(服务器 ~/.omlx/settings.json 的 auth.api_key)
cargo run --release         # TUI
cargo run -- --once "你好"  # 非交互跑一轮对话
cargo run -- --micro "..."  # 微调用流水线演示(意图→并行抽取,带每阶段耗时)
cargo run -- --list-plugins # 查看插件
```

## 使用

- 普通输入 → 回车发送(chat 插件 → 本地 LLM);
- `/micro <文本>` → 微调用流水线演示(意图分类 → 并行抽取标题/关键词/摘要);
- `/load chat` `/unload microtask` → 运行时热插拔(cordis 核心性质:卸载即回滚);
- `/model Qwen3.6-35b` → 切换模型(默认 35b,27b 非必要不用);
- `/plugins` `/help` `/clear` `/quit`。

每次回复后,microtask 插件会自动做 2 个并行微调用(意图分类 + 关键词),
状态区会显示 `[micro] 意图=… | 关键词=… (耗时)` —— 零成本频繁微调用的活体演示。

## 目录

```
src/cordis/    插件化核心(Context / 事件 / Service / Plugin / Loader)
src/llm/       模型层(OpenAI 兼容客户端 + 微调用协议)
src/plugins/   内置插件(chat / microtask)
src/tui/       TUI 交互
docs/          architecture.md(cordis 映射)· model-layer.md(模型层论证)
changelog/     版本日志 —— CI 发版的唯一数据源(见 changelog/README.md)
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

每个压缩包内:`localai[.exe]` + `localai.yml` + `.env.example` + `README.md`。
首次使用:复制 `.env.example` 为 `.env` 并填入 LLM_API_KEY(服务器
`~/.omlx/settings.json` 的 `auth.api_key`),然后运行 `localai`。

## 测试

```bash
cargo test    # cordis 核心性质(卸载回滚/事件/服务)+ loader + JSON 提取
```
