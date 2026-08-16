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
```

## 多平台构建(GitHub Actions)

无需本地工具链,推到 GitHub 即可自动编译,产物直接下载:

```bash
git remote add origin <你的仓库地址>
git push -u origin main
```

- **Actions 页面**:每次 push 后自动构建 4 个 artifact,在仓库 `Actions` → 对应 run 的
  **Artifacts** 里下载(登录 GitHub 后直接点):
  - `localai-win-x64.zip` —— Windows 10+ x64(MSVC)
  - `localai-macos-aarch64.tar.gz` —— macOS Apple Silicon
  - `localai-macos-x86_64.tar.gz` —— macOS Intel
  - `localai-linux-x64.tar.gz` —— Linux x64,CentOS 7+ 兼容(glibc 2.17,cross 容器构建)
- **Release 页面**:打 tag 自动发布(推荐正式版本用):
  ```bash
  git tag v0.1.0 && git push origin v0.1.0
  ```
  构建完成后四份产物会挂到仓库 `Releases` 页面,点开即下。

每个压缩包内:`localai[.exe]` + `localai.yml` + `.env.example` + `README.md`。
首次使用:复制 `.env.example` 为 `.env` 并填入 LLM_API_KEY(服务器
`~/.omlx/settings.json` 的 `auth.api_key`),然后运行 `localai`。

## 测试

```bash
cargo test    # cordis 核心性质(卸载回滚/事件/服务)+ loader + JSON 提取
```
