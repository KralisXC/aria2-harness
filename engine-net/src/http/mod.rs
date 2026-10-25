//! HTTP 直链：Range GET + 重定向 + 断点 + 代理 + HTTP/2（Phase 3）。

pub mod client;
pub mod h2;
pub mod proxy;
pub mod redirect;
pub mod session;
pub mod tls;
