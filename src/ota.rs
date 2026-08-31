//! OTA 模式:主固件设置菜单进入,同进程跑 HTTP server,把新固件写到对面 OTA 分区后重启。
//! 两种更新来源:浏览器上传(`/ota` PUT)、从 GitHub release 拉最新(`DownloadLatest`)。
//! 复用主固件的 `crate::lcd` / `crate::wifi` / `crate::bt_wifi_mode`,不再像旧版那样
//! 独立成一个小二进制并复制一份 lcd/wifi 驱动。

use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::reset::restart,
    http::server::{Configuration as HttpServerConf, EspHttpServer, Method},
    io::Write,
    ota::EspOta,
};

/// 从 GitHub release 拉最新固件的目标 URL。
///
/// 默认指向本 fork(zhubinghui/vibekeys_firmware)的 `releases/latest`(稳定版)。CI 构建
/// 预发布(rc/beta)时,通过环境变量 `VIBEKEYS_OTA_URL` 覆盖成该 tag 的具体资产 URL——因为
/// GitHub 的 `releases/latest` 排除 prerelease,prerelease 必须钉死 tag 才能下到。
/// 资产按硬件 feature 选:max2 → `vibekeys_max2_ota.bin`,否则 → `vibekeys_ota.bin`。
#[cfg(feature = "max2")]
const DEFAULT_OTA_URL: &str = "https://github.com/zhubinghui/vibekeys_firmware/releases/latest/download/vibekeys_max2_ota.bin";
#[cfg(not(feature = "max2"))]
const DEFAULT_OTA_URL: &str =
    "https://github.com/zhubinghui/vibekeys_firmware/releases/latest/download/vibekeys_ota.bin";

pub const OTA_DOWNLOAD_URL: &str = match option_env!("VIBEKEYS_OTA_URL") {
    Some(url) => url,
    None => DEFAULT_OTA_URL,
};

static OTA_INDEX_HTML: &str = include_str!("../assets/ota_index.html");

enum OtaEvent {
    DataChunk(Vec<u8>),
    Complete,
    DownloadLatest,
}

/// 进度阶段(枚举而非裸字符串,发送端拼错编译期就报)。
#[derive(Clone, Copy)]
enum OtaPhase {
    Downloading,
    Uploading,
    Finalizing,
}

impl OtaPhase {
    fn as_str(self) -> &'static str {
        match self {
            OtaPhase::Downloading => "downloading",
            OtaPhase::Uploading => "uploading",
            OtaPhase::Finalizing => "finalizing",
        }
    }
}

/// worker → 主线程的进度事件(屏幕由主线程持有,worker 只发数据)。
struct OtaProgress {
    written: usize,
    /// 目标总字节数;浏览器上传 / 无 content-length 时为 None。
    total: Option<usize>,
    phase: OtaPhase,
}

/// 进度上报节流:每写入这么多字节报一次(4KB 一报会刷屏过频)。
const PROGRESS_STEP: usize = 64 * 1024;

/// OTA 只读输入(scan_list + setting)打包成一个 struct 按引用传,避免 ota::run 参数过多
/// 触发 Xtensa codegen bug(最后一个栈参数 setting 被传成 null)。打包后 ota::run 共 6 个
/// 参数,全进寄存器(a2-a7),不压栈。
pub struct OtaData<'a> {
    pub scan_list: &'a Vec<String>,
    pub setting: &'a crate::bt_wifi_mode::Setting,
}

/// 进入 OTA 模式。复用调用方(main)已建好的 WiFi/显示/按钮。
///
/// - 先用 boot 阶段的 `scan_list` 与 `setting.wifi_list` 匹配连 WiFi;
/// - 起 HTTP server(上传 `/ota`、下载触发 `/ota/download`、页面 `/`);
/// - `ota_task` 在 worker 线程里写分区;
/// - 主线程轮询按钮:`accept` 触发 download-latest,`esc` 退出回 boot menu;
/// - 任一更新路径完成都在 worker 里 `restart()`;ESC 时干净关闭 server 让 worker 退出后返回。
pub fn run(
    target: &mut crate::lcd::FrameBuffer,
    accept_btn: &mut crate::AnyBtn,
    esc_btn: &mut crate::AnyBtn,
    wifi: &mut esp_idf_svc::wifi::EspWifi<'static>,
    sysloop: EspSystemEventLoop,
    data: &OtaData,
) -> anyhow::Result<()> {
    let scan_list = data.scan_list;
    let setting = data.setting;

    crate::lcd::display_text(target, "OTA Mode\n Connecting wifi", 0)?;

    // 合并后 OTA 复用主固件的 wifi 实例:若刚从 remote 过来,wifi 可能已经连上了,
    // 这时再调 wifi::connect 的 connect() 会因「已连接」报错。所以已连接就直接复用,
    // 没连才走 pick_cred + connect(与主固件 remote 一致)。
    // 直接复用 boot 阶段的 scan_list(remote 也用它):在已扫描过的 wifi 上再做一次
    // wifi::scan 会触发驱动空指针崩溃(第二次 scan 状态不稳),故不再重扫。
    if !wifi.is_connected().unwrap_or(false) {
        log::info!(
            "OTA: scan_list={} ssids, wifi_list={} creds",
            scan_list.len(),
            setting.wifi_list.len()
        );
        let r = match crate::bt_wifi_mode::pick_cred(scan_list.as_slice(), &setting.wifi_list) {
            Some(c) => {
                log::info!("OTA: picked ssid={:?} pass_len={}", c.ssid, c.pass.len());
                crate::wifi::connect(wifi, &c.ssid, &c.pass, sysloop)
            }
            None => anyhow::Result::<()>::Err(anyhow::anyhow!(
                "no known network in range (scan {})",
                scan_list.len()
            )),
        };
        if let Err(e) = r {
            log::error!("OTA wifi connect failed: {:?}", e);
        }
    }
    if !wifi.is_connected().unwrap_or(false) {
        crate::lcd::display_text(target, "OTA Mode\n Connect wifi Failed\n ESC to back", 0)?;
        // wait_button_release 只等「松开」,而此刻 ESC 本就没按下 —— 直接返回,
        // 失败画面只存在 ~20ms,看起来像闪退。先等按下,再等松开。
        while esc_btn.is_high() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        wait_button_release(esc_btn);
        return Ok(());
    }

    let ip = wifi.sta_netif().get_ip_info()?.ip;
    log::info!("OTA: WiFi connected, IP {}", ip);

    // 同步时间:OTA download-latest 走 HTTPS(TLS),证书校验依赖正确时间。
    // WiFi 已连上,可以 NTP。失败不阻塞——HTTP 上传不需要 TLS,download-latest 才需要。
    crate::lcd::display_text(target, "OTA Mode\n Syncing time...", 0)?;
    if let Err(e) = crate::sync_time(target) {
        log::warn!(
            "OTA: time sync failed (download-latest may fail TLS): {:?}",
            e
        );
    }

    crate::lcd::display_text(
        target,
        &format!(
            "OTA: http://{ip}\n Accept: download latest\n ESC: exit\n (or upload via browser)"
        ),
        0,
    )?;

    let (tx, rx) = std::sync::mpsc::channel::<OtaEvent>();
    let (ptx, prx) = std::sync::mpsc::channel::<OtaProgress>();
    let screen_tx = tx.clone();
    let http_server = ota_http_server(tx)?;
    let ota_worker = std::thread::Builder::new()
        .name("ota-worker".to_string())
        .stack_size(1024 * 24)
        .spawn(move || {
            if let Err(e) = ota_task(rx, ptx) {
                log::error!("OTA worker failed: {e:?}");
            }
        })?;

    // 轮询按钮 + 排水进度事件。accept 触发下载最新;esc 退出回 boot menu。
    // HTTP 上传通路始终在线:首个进度事件到达即切进度画面并忽略按键(更新不可中断)。
    // 成功路径由 worker restart;进度开始后通道断开 = worker 失败退出,提示后回菜单。
    let mut updating = false;
    loop {
        let mut latest: Option<OtaProgress> = None;
        let mut worker_gone = false;
        loop {
            match prx.try_recv() {
                Ok(p) => latest = Some(p),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    worker_gone = true;
                    break;
                }
            }
        }
        if let Some(p) = latest {
            updating = true;
            let _ = crate::ui::render_ota_progress(target, p.written, p.total, p.phase.as_str());
        }
        // 通道断开 = worker 已退出。成功路径在 worker 内 restart,走到这一律是失败——
        // 包括 HTTP 连接/TLS/404 等在**首个进度帧之前**就出错的情况(此前只在
        // `updating` 后才处理,早期失败会让屏幕停在 "Requesting latest..." 且
        // 再按 ACCEPT 只是朝已关闭的通道发消息,静默无效)。
        if worker_gone {
            crate::lcd::display_text(target, "OTA failed\n ESC to back", 0)?;
            while esc_btn.is_high() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            wait_button_release(esc_btn);
            break;
        }
        if !updating {
            if accept_btn.is_low() {
                wait_button_release(accept_btn);
                log::info!("OTA: accept pressed, downloading latest from release");
                crate::lcd::display_text(target, "OTA Mode\n Requesting latest...", 0)?;
                let _ = screen_tx.send(OtaEvent::DownloadLatest);
            }
            if esc_btn.is_low() {
                wait_button_release(esc_btn);
                log::info!("OTA: esc pressed, exiting to boot menu");
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // 关闭所有 sender(http_server 持有 upload/download 的 clone,screen_tx 是我们的),
    // 让 worker 的 rx 收到关闭信号后干净退出(下载/上传路径则早已 restart,join 不会返回)。
    drop(screen_tx);
    drop(http_server);
    let _ = ota_worker.join();
    Ok(())
}

/// 等按钮松开 + 简单消抖(按下期间一直 is_low)。
fn wait_button_release(btn: &crate::AnyBtn) {
    while btn.is_low() {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    std::thread::sleep(std::time::Duration::from_millis(20));
}

fn ota_http_server(
    tx: std::sync::mpsc::Sender<OtaEvent>,
) -> anyhow::Result<EspHttpServer<'static>> {
    let mut server = EspHttpServer::new(&HttpServerConf {
        stack_size: 10240,
        ..Default::default()
    })?;

    let upload_tx = tx.clone();
    server.fn_handler("/ota", Method::Put, move |mut request| {
        let mut buf = vec![0u8; 4096];
        let mut total = 0usize;

        loop {
            let n = request.read(&mut buf).map_err(|e| {
                log::error!("Failed to read OTA body: {:?}", e);
                anyhow::anyhow!("Failed to read OTA body: {:?}", e)
            })?;
            total += n;
            if n == 0 {
                break;
            }
            upload_tx
                .send(OtaEvent::DataChunk(buf[..n].to_vec()))
                .map_err(|e| {
                    log::error!("OTA channel closed: {:?}", e);
                    anyhow::anyhow!("OTA channel closed: {:?}", e)
                })?;
        }

        upload_tx.send(OtaEvent::Complete).map_err(|e| {
            log::error!("OTA channel closed: {:?}", e);
            anyhow::anyhow!("OTA channel closed: {:?}", e)
        })?;

        let mut resp = request.into_ok_response()?;
        resp.write_all(format!("OTA received: {} bytes", total).as_bytes())?;
        Result::<(), anyhow::Error>::Ok(())
    })?;

    server.fn_handler("/ota/download", Method::Post, move |request| {
        tx.send(OtaEvent::DownloadLatest).map_err(|e| {
            log::error!("OTA channel closed: {:?}", e);
            anyhow::anyhow!("OTA channel closed: {:?}", e)
        })?;

        let mut resp = request.into_ok_response()?;
        resp.write_all(b"Download started. Device will reboot after OTA completes.")?;
        Result::<(), anyhow::Error>::Ok(())
    })?;

    server.fn_handler("/", Method::Get, |req| {
        let html = OTA_INDEX_HTML.replace("{{OTA_DOWNLOAD_URL}}", OTA_DOWNLOAD_URL);
        req.into_ok_response()?.write_all(html.as_bytes())?;
        Result::<(), anyhow::Error>::Ok(())
    })?;

    server.fn_handler("/favicon.ico", Method::Get, |req| {
        req.into_ok_response()?.write_all(&[])?;
        Result::<(), anyhow::Error>::Ok(())
    })?;

    Ok(server)
}

/// worker:按到达的事件分发。DataChunk/DownloadLatest 各自接管 rx 走完整流程并 restart;
/// rx 关闭(主线程退出 OTA 模式)时返回 Ok。
fn ota_task(
    rx: std::sync::mpsc::Receiver<OtaEvent>,
    ptx: std::sync::mpsc::Sender<OtaProgress>,
) -> anyhow::Result<()> {
    while let Ok(ev) = rx.recv() {
        match ev {
            OtaEvent::DataChunk(data) => return ota_write_upload(rx, data, ptx),
            OtaEvent::DownloadLatest => return ota_download_latest(ptx),
            OtaEvent::Complete => {}
        }
    }
    Ok(())
}

/// 处理浏览器上传:把后续 chunk 顺序写进对面 OTA 分区,Complete 后切换启动槽并 restart。
fn ota_write_upload(
    rx: std::sync::mpsc::Receiver<OtaEvent>,
    first_chunk: Vec<u8>,
    ptx: std::sync::mpsc::Sender<OtaProgress>,
) -> anyhow::Result<()> {
    let mut ota = EspOta::new()?;
    ota.mark_running_slot_valid()?;

    let mut update = ota.initiate_update()?;
    log::info!("OTA upload first chunk: {} bytes", first_chunk.len());
    update.write(&first_chunk)?;
    let mut written = first_chunk.len();
    let mut last_sent = 0usize;
    let _ = ptx.send(OtaProgress {
        written,
        total: None,
        phase: OtaPhase::Uploading,
    });

    while let Ok(ev) = rx.recv() {
        match ev {
            OtaEvent::DataChunk(data) => {
                log::info!("OTA chunk: {} bytes", data.len());
                update.write(&data)?;
                written += data.len();
                if written - last_sent >= PROGRESS_STEP {
                    last_sent = written;
                    let _ = ptx.send(OtaProgress {
                        written,
                        total: None,
                        phase: OtaPhase::Uploading,
                    });
                }
            }
            OtaEvent::Complete => break,
            OtaEvent::DownloadLatest => {
                log::warn!("Ignoring download request while upload OTA is active");
            }
        }
    }
    let _ = ptx.send(OtaProgress {
        written,
        total: Some(written),
        phase: OtaPhase::Finalizing,
    });
    update.complete()?;
    log::info!("OTA upload complete, restarting into new firmware");
    restart();
}

/// 从 GitHub release 下载最新固件写进对面分区。带 content-length 时按已知尺寸擦写,
/// 否则擦整分区。
fn ota_download_latest(ptx: std::sync::mpsc::Sender<OtaProgress>) -> anyhow::Result<()> {
    log::info!("OTA download latest from {}", OTA_DOWNLOAD_URL);

    let config = esp_idf_svc::http::client::Configuration {
        buffer_size: Some(16 * 1024),
        buffer_size_tx: Some(1024),
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        timeout: Some(std::time::Duration::from_secs(60)),
        ..Default::default()
    };
    let conn = esp_idf_svc::http::client::EspHttpConnection::new(&config)?;
    let mut client = embedded_svc::http::client::Client::wrap(conn);
    let request = client.get(OTA_DOWNLOAD_URL)?;
    let mut response = request.submit()?;
    let status = response.status();
    log::info!("OTA download HTTP status: {}", status);
    if status != 200 {
        anyhow::bail!("OTA download failed: HTTP {}", status);
    }

    let content_len = response
        .header("content-length")
        .and_then(|value| value.parse::<usize>().ok());

    let mut ota = EspOta::new()?;
    ota.mark_running_slot_valid()?;
    let mut update = match content_len {
        Some(len) => {
            log::info!("OTA download content-length: {} bytes", len);
            ota.initiate_update_with_known_size(len)?
        }
        None => {
            log::warn!("OTA download missing content-length; erasing full OTA partition");
            ota.initiate_update()?
        }
    };

    // 起始进度帧:主线程据此立即从「Requesting latest...」切到进度画面。
    let _ = ptx.send(OtaProgress {
        written: 0,
        total: content_len,
        phase: OtaPhase::Downloading,
    });

    let mut buf = vec![0u8; 8192];
    let mut total = 0usize;
    let mut last_sent = 0usize;
    loop {
        let n = response.read(&mut buf)?;
        if n == 0 {
            break;
        }
        update.write(&buf[..n])?;
        total += n;
        if total - last_sent >= PROGRESS_STEP {
            last_sent = total;
            let _ = ptx.send(OtaProgress {
                written: total,
                total: content_len,
                phase: OtaPhase::Downloading,
            });
        }
        log::info!("OTA download chunk: {} bytes, total {}", n, total);
    }

    let _ = ptx.send(OtaProgress {
        written: total,
        total: content_len.or(Some(total)),
        phase: OtaPhase::Finalizing,
    });
    update.complete()?;
    log::info!("OTA download complete: {} bytes, restarting", total);
    restart();
}
