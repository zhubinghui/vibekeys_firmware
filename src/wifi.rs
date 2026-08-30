use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    wifi::{AuthMethod, BlockingWifi, EspWifi},
};

pub fn connect(
    esp_wifi: &mut EspWifi<'static>,
    ssid: &str,
    pass: &str,
    sysloop: EspSystemEventLoop,
) -> anyhow::Result<()> {
    let mut auth_method = AuthMethod::WPA2Personal;
    if ssid.is_empty() {
        anyhow::bail!("Missing WiFi name")
    }
    if pass.is_empty() {
        auth_method = AuthMethod::None;
        log::info!("Wifi password is empty");
    }

    let mut wifi = BlockingWifi::wrap(esp_wifi, sysloop)?;

    wifi.set_configuration(&esp_idf_svc::wifi::Configuration::Client(
        esp_idf_svc::wifi::ClientConfiguration {
            // SSID/密码是 heapless 定长字段(32/64 字节),而输入来自用户配置(BLE 写入,
            // 中文密码 22 个汉字即超 64)。这里 panic 会在 panic_abort 下整机重启,且坏配置
            // 留在 NVS 里,每次进模式都重演 —— 转成 Err,调用方本就把连接失败当 WiFi off 处理。
            ssid: ssid.try_into().map_err(|_| {
                anyhow::anyhow!("SSID too long for 802.11 ({} bytes, max 32)", ssid.len())
            })?,
            password: pass.try_into().map_err(|_| {
                anyhow::anyhow!("WiFi password too long ({} bytes, max 64)", pass.len())
            })?,
            auth_method,
            ..Default::default()
        },
    ))?;

    wifi.start()?;

    log::info!("Connecting wifi...");

    wifi.connect()?;

    log::info!("Waiting for DHCP lease...");

    wifi.wait_netif_up()?;

    let ip_info = wifi.wifi().sta_netif().get_ip_info()?;

    log::info!("Wifi DHCP info: {:?}", ip_info);

    Ok(())
}

/// 扫描周围 WiFi,返回去重(保序)后的 ssid 列表。
pub fn scan(
    esp_wifi: &mut EspWifi<'static>,
    sysloop: esp_idf_svc::eventloop::EspSystemEventLoop,
) -> anyhow::Result<Vec<String>> {
    let mut wifi = BlockingWifi::wrap(esp_wifi, sysloop)?;
    // scan 只能在 STA 模式下进行。EspWifi 默认/上次可能是 softAP,
    // 直接 start 会以 AP 模式起来 -> scan 返回 ESP_FAIL。先强制切到 Client。
    wifi.set_configuration(&esp_idf_svc::wifi::Configuration::Client(
        esp_idf_svc::wifi::ClientConfiguration::default(),
    ))?;
    // scan 需要驱动已 start;若已 start(同次会话二次扫描)则忽略 already-started。
    let _ = wifi.start();
    let results = wifi.scan()?;
    let mut seen = std::collections::HashSet::new();
    let mut ssids = Vec::new();
    for ap in results {
        let s = ap.ssid.as_str().to_string();
        if !s.is_empty() && seen.insert(s.clone()) {
            ssids.push(s);
        }
    }
    Ok(ssids)
}
