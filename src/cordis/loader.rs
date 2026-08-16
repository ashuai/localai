//! Loader —— 对应 cordis 的 loader:内核只负责插拔。
//!
//! 从配置(localai.yml)读取启用的插件清单,逐个 fork 作用域并 apply;
//! 支持运行时 `/load` `/unload`,演示"内核不修改、能力即插即拔"。

use crate::cordis::context::Context;
use crate::cordis::plugin::Plugin;
use crate::llm::{LlmClient, LlmService};
use std::collections::HashMap;
use std::sync::Arc;

pub type PluginFactory = fn() -> Box<dyn Plugin>;

pub struct LoadedPlugin {
    pub name: String,
    pub ctx: Context,
}

pub struct PluginStatus {
    pub name: String,
    pub loaded: bool,
}

pub struct Loader {
    root: Context,
    registry: HashMap<&'static str, PluginFactory>,
    active: HashMap<String, LoadedPlugin>,
}

impl Loader {
    /// 创建 loader:构造根作用域,注入核心 `llm` 服务,登记内置插件工厂。
    pub fn new(client: LlmClient, factories: Vec<PluginFactory>) -> Self {
        let root = Context::new();
        root.provide(Arc::new(LlmService { client }));
        let mut registry = HashMap::new();
        for f in factories {
            let name = f().name();
            registry.insert(name, f);
        }
        Self {
            root,
            registry,
            active: HashMap::new(),
        }
    }

    pub fn root(&self) -> &Context {
        &self.root
    }

    /// 加载插件:fork 作用域 → 写选项 → apply;失败立即回滚。
    pub fn load(&mut self, name: &str, options: serde_yaml::Value) -> anyhow::Result<()> {
        if self.active.contains_key(name) {
            anyhow::bail!("插件 `{name}` 已加载");
        }
        let factory = self.registry.get(name).ok_or_else(|| {
            anyhow::anyhow!(
                "未知插件 `{name}`;可用: {}",
                self.registry.keys().map(|s| *s).collect::<Vec<_>>().join(", ")
            )
        })?;
        let plugin = factory();
        let ctx = self.root.fork();
        ctx.set_options(options);
        if let Err(e) = plugin.apply(&ctx) {
            ctx.dispose();
            return Err(e.context(format!("插件 `{name}` 启动失败")));
        }
        self.active.insert(
            name.to_string(),
            LoadedPlugin {
                name: name.to_string(),
                ctx,
            },
        );
        Ok(())
    }

    /// 卸载插件:dispose 其作用域(所有 effect 逆序回滚)。
    pub fn unload(&mut self, name: &str) -> anyhow::Result<()> {
        let lp = self
            .active
            .remove(name)
            .ok_or_else(|| anyhow::anyhow!("插件 `{name}` 未加载"))?;
        lp.ctx.dispose();
        Ok(())
    }

    pub fn is_loaded(&self, name: &str) -> bool {
        self.active.contains_key(name)
    }

    pub fn list(&self) -> Vec<PluginStatus> {
        let mut v: Vec<PluginStatus> = self
            .registry
            .keys()
            .map(|n| PluginStatus {
                name: (*n).to_string(),
                loaded: self.active.contains_key(*n),
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cordis::event::Event;
    use crate::cordis::service::Service;
    use crate::llm::LlmConfig;

    struct DemoEv;
    impl Event for DemoEv {
        fn name(&self) -> &'static str {
            "demo/ev"
        }
    }

    struct DemoService;
    impl Service for DemoService {
        fn service_name_static() -> &'static str {
            "demo"
        }
    }

    struct DemoPlugin;
    impl Plugin for DemoPlugin {
        fn name(&self) -> &'static str {
            "demo"
        }
        fn apply(&self, ctx: &Context) -> anyhow::Result<()> {
            ctx.provide(Arc::new(DemoService));
            ctx.on_command("demo", |_| "hello from demo".into());
            ctx.on(|_: &DemoEv| {});
            Ok(())
        }
    }
    fn demo_factory() -> Box<dyn Plugin> {
        Box::new(DemoPlugin)
    }

    fn test_loader() -> Loader {
        Loader::new(
            LlmClient::new(LlmConfig {
                base_url: "http://127.0.0.1:9".into(),
                api_key: "test".into(),
                model: "test-model".into(),
                timeout_secs: 1,
                max_concurrent: 2,
            }),
            vec![demo_factory],
        )
    }

    #[test]
    fn load_unload_cycle_reverts_registrations() {
        let mut l = test_loader();
        l.load("demo", serde_yaml::Value::Null).unwrap();
        assert!(l.is_loaded("demo"));
        assert!(l.root().has_service::<DemoService>());
        assert!(l.root().run_command("demo").is_some());
        l.unload("demo").unwrap();
        assert!(!l.is_loaded("demo"));
        assert!(!l.root().has_service::<DemoService>(), "服务应随卸载回收");
        assert!(l.root().run_command("demo").is_none(), "命令应随卸载移除");
    }

    #[test]
    fn double_load_fails() {
        let mut l = test_loader();
        l.load("demo", serde_yaml::Value::Null).unwrap();
        assert!(l.load("demo", serde_yaml::Value::Null).is_err());
    }

    #[test]
    fn unknown_plugin_fails() {
        let mut l = test_loader();
        assert!(l.load("nope", serde_yaml::Value::Null).is_err());
    }

    #[test]
    fn failing_apply_rolls_back_scope() {
        struct BadPlugin;
        impl Plugin for BadPlugin {
            fn name(&self) -> &'static str {
                "bad"
            }
            fn apply(&self, _ctx: &Context) -> anyhow::Result<()> {
                anyhow::bail!("boom")
            }
        }
        fn bad_factory() -> Box<dyn Plugin> {
            Box::new(BadPlugin)
        }
        let mut l = Loader::new(
            LlmClient::new(LlmConfig {
                base_url: "http://127.0.0.1:9".into(),
                api_key: "test".into(),
                model: "m".into(),
                timeout_secs: 1,
                max_concurrent: 2,
            }),
            vec![demo_factory, bad_factory],
        );
        assert!(l.load("bad", serde_yaml::Value::Null).is_err());
        assert!(!l.is_loaded("bad"));
    }
}
