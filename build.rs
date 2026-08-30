fn main() {
    // build.rs 始终在宿主上运行;只有编到 ESP target 时才需要注入 ESP-IDF 的 sysenv。
    // 宿主 target(CI 跑 `cargo test --lib`)下跳过,否则 embuild 会去找不存在的 IDF 环境。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("espidf") {
        embuild::espidf::sysenv::output();
    }
}
