//! 密钥隔离与 Core HTTP 传输。

mod http;
mod secret;

pub(crate) use http::call;
