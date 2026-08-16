use std::any::Any;

/// Service:cordis 的服务抽象(懒加载单例,全局注册、按需注入)。
///
/// Rust 版以具体类型的 `TypeId` 为注册键做类型化注入,
/// `service_name()` 对应 cordis 的 service key(用于日志与依赖声明)。
/// 存储与注入走 `Arc<dyn Any + Send + Sync>` + `Arc::downcast`,全程安全。
pub trait Service: Any + Send + Sync {
    /// 服务名(cordis 的 service key)
    fn service_name(&self) -> &'static str {
        Self::service_name_static()
    }

    /// 静态服务名(注册时无需实例即可记录)
    fn service_name_static() -> &'static str;
}
