use crate::cordis::context::Context;

/// 插件契约(对应 cordis 的 `name + inject + apply(ctx)` 极简范式)。
///
/// 核心原则(与 PLUG-BOOK.md 一致):
/// - 所有能力通过 `ctx` 注册(事件/服务/命令),不碰全局;
/// - 插件卸载时,注册的工具、事件、监听器自动撤销(effect 逆序回滚);
/// - 依赖通过 `inject` 声明,拿不到就在 `apply` 里显式报错。
pub trait Plugin: Send + Sync {
    /// 插件名
    fn name(&self) -> &'static str;

    /// 声明依赖的服务名(展示用;实际注入在 `apply` 里通过类型化 `ctx.inject` 完成)
    fn inject(&self) -> &'static [&'static str] {
        &[]
    }

    /// 应用插件:在 ctx 上注册服务/事件/命令/effect。
    /// 返回 Err 时 loader 会立即 dispose 该作用域并向上报错。
    fn apply(&self, ctx: &Context) -> anyhow::Result<()>;
}
