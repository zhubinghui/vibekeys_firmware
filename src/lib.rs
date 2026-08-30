//! 宿主可测的纯逻辑层。
//!
//! 这里只放**不依赖 ESP-IDF** 的模块,让 CI 能在没有 Xtensa 工具链的普通 runner 上跑
//! `cargo test --lib --target <host>`。ESP 专有依赖在 Cargo.toml 里由
//! `cfg(target_os = "espidf")` 门控,所以宿主构建不会把 esp-idf-sys 拉进依赖图。
//!
//! 固件 bin 不经过这个 lib —— 它用自己的 `mod` 声明编译同样的源文件。多编一份纯逻辑
//! 模块的代价可以忽略,换来的是不必把全项目的 `crate::` 路径改写成 `vibekeys::`。

pub mod broker_uri;
pub mod png_frame;
pub mod protocol;
