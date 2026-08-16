//! Context —— cordis 的核心对象:事件总线 + 服务注册表 + effect 生命周期。
//!
//! 设计要点:
//! - **所有注册都是 effect**:`on` / `on_bail` / `on_command` / `provide`
//!   内部都会追加一个 disposer 到本作用域的 effects;`dispose()` 逆序执行,
//!   实现 cordis 的"卸载即全部回滚"。
//! - **fork 出插件作用域**:插件通过 [`Context::fork`] 拿到独立作用域
//!   (独立 effects / provided / options),但服务、事件、命令注册表全局共享
//!   —— 插件 A 提供的服务,插件 B 立即可见;A 卸载时回收。
//! - **事件类型化**:以 `TypeId` 为键,`emit` 串行同步调用;`bail` 是
//!   hook 语义,第一个返回 `Some` 即短路。
//! - **派发不消费**:`emit`/`bail`/`run_command` 对监听器做 Arc 快照后
//!   在锁外调用,监听器常驻直到卸载;同类型事件的**可重入发射**用
//!   派发守卫跳过(避免无限递归)。

use crate::cordis::event::Event;
use crate::cordis::service::Service;
use std::any::{Any, TypeId};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

type Listener = Arc<Mutex<dyn FnMut(&dyn Any) + Send>>;
type Bailer = Arc<Mutex<dyn FnMut(&dyn Any) -> Option<Box<dyn Any>> + Send>>;
type Effect = Box<dyn FnOnce() + Send>;
type CommandHandler = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// 服务存储类型:具体类型 + 自动 trait,支持 `Arc::downcast` 安全取回。
type SharedService = Arc<dyn Any + Send + Sync>;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::SeqCst)
}

/// 派发守卫:emit/bail 期间占位,防同类型事件可重入;Drop 时释放。
struct DispatchGuard<'a> {
    set: &'a Mutex<HashSet<TypeId>>,
    key: TypeId,
}

impl Drop for DispatchGuard<'_> {
    fn drop(&mut self) {
        self.set.lock().unwrap().remove(&self.key);
    }
}

#[derive(Clone)]
pub struct Context {
    inner: Arc<ContextInner>,
}

struct ContextInner {
    /// 作用域 id(调试用)
    id: u64,
    /// 服务注册表:全局共享,按具体类型的 TypeId 注册
    services: Arc<Mutex<HashMap<TypeId, SharedService>>>,
    /// 服务名索引(供 service_names() 展示)
    service_names: Arc<Mutex<HashMap<TypeId, &'static str>>>,
    /// 事件监听:全局共享(emit 全应用可见),注册 id 用于卸载回滚
    listeners: Arc<Mutex<HashMap<TypeId, Vec<(u64, Listener)>>>>,
    /// hook 监听(cordis bail 语义)
    bailers: Arc<Mutex<HashMap<TypeId, Vec<(u64, Bailer)>>>>,
    /// 插件向 TUI 注册的命令:全局共享
    commands: Arc<Mutex<HashMap<String, Vec<(u64, CommandHandler)>>>>,
    /// 正在派发的事件类型(防可重入无限递归)
    dispatching: Mutex<HashSet<TypeId>>,
    /// 本作用域的 effect(卸载时逆序执行)
    effects: Mutex<Vec<Effect>>,
    /// 本作用域提供的服务(卸载时回收)
    provided: Mutex<HashMap<TypeId, SharedService>>,
    /// 插件选项(loader 在 apply 前写入)
    options: Mutex<serde_yaml::Value>,
    disposed: AtomicBool,
}

impl Context {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ContextInner {
                id: next_id(),
                services: Arc::new(Mutex::new(HashMap::new())),
                service_names: Arc::new(Mutex::new(HashMap::new())),
                listeners: Arc::new(Mutex::new(HashMap::new())),
                bailers: Arc::new(Mutex::new(HashMap::new())),
                commands: Arc::new(Mutex::new(HashMap::new())),
                dispatching: Mutex::new(HashSet::new()),
                effects: Mutex::new(Vec::new()),
                provided: Mutex::new(HashMap::new()),
                options: Mutex::new(serde_yaml::Value::Null),
                disposed: AtomicBool::new(false),
            }),
        }
    }

    pub fn id(&self) -> u64 {
        self.inner.id
    }

    /// 派生插件作用域:共享服务/事件/命令注册表,独立 effect/options/提供的服务。
    pub fn fork(&self) -> Context {
        let inner = ContextInner {
            id: next_id(),
            services: Arc::clone(&self.inner.services),
            service_names: Arc::clone(&self.inner.service_names),
            listeners: Arc::clone(&self.inner.listeners),
            bailers: Arc::clone(&self.inner.bailers),
            commands: Arc::clone(&self.inner.commands),
            dispatching: Mutex::new(HashSet::new()),
            effects: Mutex::new(Vec::new()),
            provided: Mutex::new(HashMap::new()),
            options: Mutex::new(serde_yaml::Value::Null),
            disposed: AtomicBool::new(false),
        };
        Context {
            inner: Arc::new(inner),
        }
    }

    // ---------- 插件选项 ----------

    pub fn set_options(&self, v: serde_yaml::Value) {
        *self.inner.options.lock().unwrap() = v;
    }

    pub fn options<T: serde::de::DeserializeOwned + Default>(&self) -> anyhow::Result<T> {
        let v = self.inner.options.lock().unwrap().clone();
        if v.is_null() {
            return Ok(T::default());
        }
        serde_yaml::from_value(v).map_err(Into::into)
    }

    // ---------- effect 生命周期(cordis 核心:卸载时逆序回滚) ----------

    /// 注册一个 effect;`dispose()` 时逆序执行。
    pub fn effect(&self, f: impl FnOnce() + Send + 'static) {
        self.inner.effects.lock().unwrap().push(Box::new(f));
    }

    /// 销毁本作用域:逆序执行所有 effect,并回收本作用域提供的服务。
    pub fn dispose(&self) {
        if self.inner.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut effects = self.inner.effects.lock().unwrap();
            while let Some(f) = effects.pop() {
                f();
            }
        }
        let provided = std::mem::take(&mut *self.inner.provided.lock().unwrap());
        for (key, svc) in provided {
            let mut services = self.inner.services.lock().unwrap();
            if let Some(cur) = services.get(&key) {
                if Arc::ptr_eq(cur, &svc) {
                    services.remove(&key);
                    self.inner.service_names.lock().unwrap().remove(&key);
                }
            }
        }
    }

    pub fn disposed(&self) -> bool {
        self.inner.disposed.load(Ordering::SeqCst)
    }

    // ---------- 事件 ----------

    /// 注册事件监听(cordis `ctx.on`)。卸载时自动移除。
    pub fn on<E: Event>(&self, handler: impl FnMut(&E) + Send + 'static) {
        let id = next_id();
        let key = TypeId::of::<E>();
        let mut handler = handler;
        let wrapped: Listener = Arc::new(Mutex::new(move |any: &dyn Any| {
            let ev = any.downcast_ref::<E>().expect("event type mismatch");
            handler(ev);
        }));
        self.inner
            .listeners
            .lock()
            .unwrap()
            .entry(key)
            .or_default()
            .push((id, wrapped));
        let inner = Arc::clone(&self.inner);
        self.effect(move || {
            if let Some(list) = inner.listeners.lock().unwrap().get_mut(&key) {
                list.retain(|(i, _)| *i != id);
            }
        });
    }

    /// 同步串行发射(cordis `ctx.emit`)。监听器常驻;同类型可重入发射被跳过。
    pub fn emit<E: Event>(&self, ev: E) {
        let key = TypeId::of::<E>();
        {
            let mut set = self.inner.dispatching.lock().unwrap();
            if !set.insert(key) {
                return; // 同类型已在派发(可重入),跳过避免无限递归
            }
        }
        let _guard = DispatchGuard {
            set: &self.inner.dispatching,
            key,
        };
        let snapshot: Vec<Listener> = {
            let map = self.inner.listeners.lock().unwrap();
            map.get(&key)
                .map(|l| l.iter().map(|(_, h)| Arc::clone(h)).collect())
                .unwrap_or_default()
        };
        for h in &snapshot {
            (h.lock().unwrap())(&ev);
        }
    }

    /// 注册 hook 监听(cordis `ctx.bail`):返回 `Option<R>` 的监听器,
    /// 第一个返回 `Some` 即短路并把结果回传。
    pub fn on_bail<E: Event, R: Any + Send>(
        &self,
        handler: impl FnMut(&E) -> Option<R> + Send + 'static,
    ) {
        let id = next_id();
        let key = TypeId::of::<E>();
        let mut handler = handler;
        let wrapped: Bailer = Arc::new(Mutex::new(move |any: &dyn Any| {
            let ev = any.downcast_ref::<E>().expect("event type mismatch");
            handler(ev).map(|r| Box::new(r) as Box<dyn Any>)
        }));
        self.inner
            .bailers
            .lock()
            .unwrap()
            .entry(key)
            .or_default()
            .push((id, wrapped));
        let inner = Arc::clone(&self.inner);
        self.effect(move || {
            if let Some(list) = inner.bailers.lock().unwrap().get_mut(&key) {
                list.retain(|(i, _)| *i != id);
            }
        });
    }

    /// 执行 hook:依次调用,第一个返回 `Some` 即短路(cordis `ctx.bail`)。
    pub fn bail<E: Event, R: Any + Send>(&self, ev: E) -> Option<R> {
        let key = TypeId::of::<E>();
        {
            let mut set = self.inner.dispatching.lock().unwrap();
            if !set.insert(key) {
                return None;
            }
        }
        let _guard = DispatchGuard {
            set: &self.inner.dispatching,
            key,
        };
        let snapshot: Vec<Bailer> = {
            let map = self.inner.bailers.lock().unwrap();
            map.get(&key)
                .map(|l| l.iter().map(|(_, h)| Arc::clone(h)).collect())
                .unwrap_or_default()
        };
        for h in &snapshot {
            if let Some(ret) = (h.lock().unwrap())(&ev) {
                return ret.downcast::<R>().ok().map(|b| *b);
            }
        }
        None
    }

    // ---------- 服务(cordis DI) ----------

    /// 注册服务到全局注册表;本作用域卸载时回收。
    pub fn provide<T: Service>(&self, svc: Arc<T>) {
        let key = TypeId::of::<T>();
        let shared: SharedService = svc;
        self.inner
            .services
            .lock()
            .unwrap()
            .insert(key, Arc::clone(&shared));
        self.inner
            .service_names
            .lock()
            .unwrap()
            .insert(key, T::service_name_static());
        self.inner.provided.lock().unwrap().insert(key, shared);
    }

    /// 按具体类型注入服务(返回 `Arc<T>` 克隆)。拿不到返回 `None`。
    pub fn inject<T: Service>(&self) -> Option<Arc<T>> {
        let key = TypeId::of::<T>();
        let svc = self.inner.services.lock().unwrap().get(&key)?.clone();
        svc.downcast::<T>().ok()
    }

    pub fn has_service<T: Service>(&self) -> bool {
        self.inner.services.lock().unwrap().contains_key(&TypeId::of::<T>())
    }

    pub fn service_names(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self
            .inner
            .service_names
            .lock()
            .unwrap()
            .values()
            .copied()
            .collect();
        names.sort_unstable();
        names
    }

    // ---------- 命令(插件向 TUI 注册命令) ----------

    /// 注册一条斜杠命令(`/name [args]`)。卸载时自动移除。
    pub fn on_command(&self, name: &str, handler: impl Fn(&str) -> String + Send + Sync + 'static) {
        let id = next_id();
        let key = name.to_string();
        let wrapped: CommandHandler = Arc::new(handler);
        self.inner
            .commands
            .lock()
            .unwrap()
            .entry(key.clone())
            .or_default()
            .push((id, wrapped));
        let inner = Arc::clone(&self.inner);
        self.effect(move || {
            if let Some(list) = inner.commands.lock().unwrap().get_mut(&key) {
                list.retain(|(i, _)| *i != id);
            }
        });
    }

    /// 执行命令(传 `/name args` 或 `name args`);无插件注册时返回 `None`。
    pub fn run_command(&self, line: &str) -> Option<String> {
        let trimmed = line.trim_start_matches('/');
        let (name, rest) = trimmed
            .split_once(' ')
            .map(|(n, r)| (n, r.trim()))
            .unwrap_or((trimmed, ""));
        let key = name.to_string();
        let snapshot: Vec<CommandHandler> = {
            let map = self.inner.commands.lock().unwrap();
            map.get(&key)
                .map(|l| l.iter().map(|(_, h)| Arc::clone(h)).collect())
                .unwrap_or_default()
        };
        if snapshot.is_empty() {
            // 未注册,或已被卸载回滚
            return None;
        }
        let results: Vec<String> = snapshot.iter().map(|h| h(rest)).collect();
        Some(results.join("\n"))
    }

    pub fn command_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.inner.commands.lock().unwrap().keys().cloned().collect();
        names.sort();
        names
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct EvA;
    impl Event for EvA {
        fn name(&self) -> &'static str {
            "test/a"
        }
    }
    struct EvB;
    impl Event for EvB {
        fn name(&self) -> &'static str {
            "test/b"
        }
    }

    struct TestSvc;
    impl Service for TestSvc {
        fn service_name_static() -> &'static str {
            "test-svc"
        }
    }

    #[test]
    fn effects_run_in_reverse_order_on_dispose() {
        let ctx = Context::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let o1 = Arc::clone(&order);
        ctx.effect(move || o1.lock().unwrap().push(1));
        let o2 = Arc::clone(&order);
        ctx.effect(move || o2.lock().unwrap().push(2));
        let o3 = Arc::clone(&order);
        ctx.effect(move || o3.lock().unwrap().push(3));
        ctx.dispose();
        assert_eq!(*order.lock().unwrap(), vec![3, 2, 1]);
    }

    #[test]
    fn emit_calls_all_listeners_in_order() {
        let ctx = Context::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let c1 = Arc::clone(&calls);
        ctx.on(move |_: &EvA| c1.lock().unwrap().push("a1"));
        let c2 = Arc::clone(&calls);
        ctx.on(move |_: &EvA| c2.lock().unwrap().push("a2"));
        let c3 = Arc::clone(&calls);
        ctx.on(move |_: &EvB| c3.lock().unwrap().push("b1"));
        ctx.emit(EvA);
        assert_eq!(*calls.lock().unwrap(), vec!["a1", "a2"]);
    }

    #[test]
    fn emit_does_not_consume_listeners() {
        let ctx = Context::new();
        let count = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&count);
        ctx.on(move |_: &EvA| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        ctx.emit(EvA);
        ctx.emit(EvA);
        ctx.emit(EvA);
        assert_eq!(count.load(Ordering::SeqCst), 3, "多次 emit 监听器应常驻");
    }

    #[test]
    fn emit_is_reentrant() {
        // 监听器内部再 emit 同类型:被派发守卫跳过,不死锁不无限递归
        let ctx = Context::new();
        let count = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&count);
        let ctx2 = ctx.clone();
        ctx.on(move |_: &EvA| {
            c.fetch_add(1, Ordering::SeqCst);
            ctx2.emit(EvA);
        });
        ctx.emit(EvA);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn command_can_run_multiple_times() {
        let ctx = Context::new();
        ctx.on_command("demo", |_| "ok".into());
        assert_eq!(ctx.run_command("demo").as_deref(), Some("ok"));
        assert_eq!(ctx.run_command("demo").as_deref(), Some("ok"), "命令应常驻");
    }

    #[test]
    fn bail_short_circuits_on_first_some() {
        let ctx = Context::new();
        ctx.on_bail(|_: &EvA| -> Option<i32> { None });
        ctx.on_bail(|_: &EvA| -> Option<i32> { Some(42) });
        ctx.on_bail(|_: &EvA| -> Option<i32> { Some(99) });
        let got: Option<i32> = ctx.bail(EvA);
        assert_eq!(got, Some(42));
    }

    #[test]
    fn service_provide_inject_and_remove_on_dispose() {
        let ctx = Context::new();
        assert!(!ctx.has_service::<TestSvc>());
        ctx.provide(Arc::new(TestSvc));
        assert!(ctx.has_service::<TestSvc>());
        assert!(ctx.inject::<TestSvc>().is_some());
        ctx.dispose();
        assert!(!ctx.has_service::<TestSvc>());
    }

    #[test]
    fn dispose_removes_registered_handlers_and_commands() {
        let root = Context::new();
        let plugin_ctx = root.fork();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        plugin_ctx.on(move |_: &EvA| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        plugin_ctx.on_command("demo", |_| "ok".into());
        root.emit(EvA);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(root.run_command("demo").is_some());
        plugin_ctx.dispose();
        root.emit(EvA);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "卸载后监听器应已移除");
        assert!(root.run_command("demo").is_none(), "卸载后命令应已移除");
    }

    #[test]
    fn fork_plugin_provides_service_visible_to_root() {
        let root = Context::new();
        let plugin_ctx = root.fork();
        plugin_ctx.provide(Arc::new(TestSvc));
        assert!(root.has_service::<TestSvc>());
        plugin_ctx.dispose();
        assert!(!root.has_service::<TestSvc>());
    }
}
