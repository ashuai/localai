# 架构:cordis 模式在 Rust 中的映射

> 目标:用 Rust 复刻 DSH 的 cordis 插件化架构 —— **一切皆插件,内核只负责插拔**。
> 本文回答:什么是 cordis 模式、Rust 里怎么表达、与原生 cordis 的差异与取舍。

---

## 1. cordis 模式是什么

cordis(Koishi 系插件框架,DSH 使用的 `@deepseek-ai/cordis` 即其派生)的核心理念:

1. **Context 是唯一入口** —— 插件拿到的 `ctx` 是"作用域化的" DI 容器 + 事件总线;
2. **所有注册都是 effect** —— `ctx.on` / `ctx.provide` / 任何注册内部都会登记一个
   清理函数;插件卸载时按**逆序**执行全部清理,注册的工具、事件、监听器自动撤销;
3. **Service 懒加载单例** —— `ctx.provide(name, svc)` 注册,`ctx.inject(name)` 注入,
   依赖缺失时插件暂停/启动失败;
4. **事件驱动** —— `ctx.on / ctx.emit`(串行)、`ctx.bail`(hook,首个非空短路);
5. **Loader 只做插拔** —— 从配置文件读取插件清单,负责加载/卸载/热重载,
   不修改任何插件逻辑。

## 2. 概念对照表

| cordis (TypeScript)              | localai (Rust)                                    | 说明 |
|----------------------------------|---------------------------------------------------|------|
| `Context`                        | `cordis::Context`                                 | 事件总线 + 服务注册表 + effect 栈 |
| `ctx.extend()` / 插件作用域      | `Context::fork()`                                 | 插件独立 effects/options,注册表共享 |
| `ctx.on / ctx.emit`              | `Context::on` / `Context::emit`                   | 类型化事件(以 `TypeId` 为键) |
| `ctx.bail`(hook)                 | `Context::on_bail` / `Context::bail`              | 首个 `Some` 短路 |
| `ctx.effect(fn)`                 | `Context::effect` / `Context::dispose`            | 卸载时逆序回滚 |
| `ctx.provide / ctx.inject`       | `Context::provide` / `Context::inject`            | `Arc<dyn Service>` + 类型化 downcast |
| `Service`                        | `cordis::Service` trait                           | `service_name()` + `as_any()` |
| `Plugin { name, inject, apply }` | `cordis::Plugin` trait                            | 极简契约,与 PLUG-BOOK.md 一致 |
| Loader + cordis.yml              | `cordis::Loader` + `localai.yml`                  | 配置驱动插拔,运行时 `/load` `/unload` |
| 事件名(字符串)                   | 事件类型 + `Event::name()`                       | Rust 类型系统替代字符串键 |

## 3. 模块结构

```
localai/
├── localai.yml              # loader 配置:server + 插件清单(cordis.yml 风格)
├── src/
│   ├── main.rs              # 装配:读配置 → 建 Loader → 加载插件 → TUI / --once
│   ├── lib.rs
│   ├── cordis/              # ★ 插件化核心(模式本体)
│   │   ├── context.rs       # Context:事件 + 服务 + effect 生命周期
│   │   ├── event.rs         # Event trait
│   │   ├── service.rs       # Service trait
│   │   ├── plugin.rs        # Plugin trait
│   │   └── loader.rs        # Loader:配置驱动的加载/卸载
│   ├── llm/                 # 模型层(论证见 docs/model-layer.md)
│   │   ├── client.rs        # OpenAI 兼容客户端(限流/重试/关思考)
│   │   └── micro.rs         # 微调用协议(MicroTask → MicroOutcome)
│   ├── plugins/
│   │   ├── chat.rs          # 聊天插件:提供 chat 服务 + 订阅 session/input
│   │   └── microtask.rs     # 微调用插件:环境微调用 + /micro 流水线演示
│   ├── events.rs            # 应用级事件:session/input · session/reply · session/status
│   └── tui/                 # ratatui 交互
└── docs/
    ├── architecture.md      # 本文
    └── model-layer.md       # 模型层论证
```

## 4. 一个插件的完整形态(chat)

```rust
impl Plugin for ChatPlugin {
    fn name(&self) -> &'static str { "chat" }
    fn inject(&self) -> &'static [&'static str] { &["llm"] }

    fn apply(&self, ctx: &Context) -> anyhow::Result<()> {
        // 1. 注入核心服务
        let llm = ctx.inject::<LlmService>().ok_or_else(|| anyhow!("缺少 llm 服务"))?;
        // 2. 提供服务(全局可见;卸载时自动回收)
        let svc = Arc::new(ChatService { client: llm.client.clone(), .. });
        ctx.provide(svc.clone());
        // 3. 订阅事件(卸载时自动移除)
        ctx.on(|ev: &SessionInput| { /* 后台线程调 LLM,完成后 emit SessionReply */ });
        // 4. 注册命令(卸载时自动移除)
        ctx.on_command("chat", |rest| { /* 同步问答 */ });
        Ok(())
    }
}
```

与 PLUG-BOOK.md 的最小范式逐条对应:注册走 `ctx`、卸载自动撤销、依赖注入声明。

## 5. 与原生 cordis 的差异(刻意取舍)

| 差异 | cordis | localai v1 | 原因 |
|------|--------|------------|------|
| 事件键 | 字符串名(声明合并) | `TypeId` 类型键 | Rust 无声明合并;类型键编译期安全,零字符串拼写错误 |
| 服务可见性 | 作用域链(父→子) | 全局共享注册表 | v1 简化;插件服务全局可见,卸载按 `provided` 回收 |
| 依赖级联 | 服务被卸载 → 依赖方级联卸载 | 不级联,注入方下次访问报缺依赖 | v1 不引入依赖图;文档化 |
| 事件传播 | 父子上下文传播 | 全应用广播 | v1 简化;作用域化事件列为后续工作 |
| HMR | 文件变更 → 热重载 | `/load` `/unload` 运行时插拔 | 热重载(文件监听)列为后续工作 |

这些差异都是**刻意收敛**:v1 优先保证"卸载 = 全部回滚"这一核心性质(有单测覆盖),
作用域精细化留给插件生态膨胀后再演进。

## 6. 核心性质与测试

- **卸载即回滚**:`dispose()` 逆序执行 effects、回收本作用域提供的服务、移除注册的
  监听与命令 → `src/cordis/context.rs` 单测 `dispose_removes_registered_handlers_and_commands`
- **可重入事件**:监听器内再 `emit` 不死锁 → `emit_is_reentrant`
- **hook 短路**:`bail` 首个非空即返回 → `bail_short_circuits_on_first_some`
- **loader 插拔**:load/unload 循环后注册表干净 → `loader.rs` 单测 `load_unload_cycle_reverts_registrations`

## 7. 演进路线

1. 作用域化事件传播(父子链)与依赖级联卸载 —— 更贴近真 cordis;
2. HMR:监听插件源码/配置变更自动 reload(对齐 DSH 的 cordis HMR 体系);
3. 插件包化:把 PluginFactory 从内置注册表推广为可加载的独立包(dylib/子进程);
4. 可观测服务:会话日志、微调用耗时/Token 统计(对接 model-layer 的可观测设计)。
