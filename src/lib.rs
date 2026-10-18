//! MinIO/S3 前置的 OSS 同参数图片处理网关。
//!
//! 网关就是对象存储的公开入口：请求原样转给可信上游；GET 带 `x-oss-process=image/...` 时取回原图，
//! 按 OSS 的参数语义加工后返回。不签名、不持凭据、不接收 EasOS 票据。
mod admission;
mod cache;
pub mod config;
pub mod error;
pub mod process;
pub mod proxy;
pub mod transform;
pub mod web;
