//! 酷狗 CDP 源：通过 DevTools 协议读酷狗进程内的真实播放状态。
//!
//! 酷狗的媒体会话（SMTC）不给时间轴，`KuGou.ini` 只每 30 秒落一次盘；而它自己的 CEF 页面里
//! 有个 JS 桥 `external.SuperCall`，`SuperCall(864)` 直接返回**精确到 100 纳秒**的播放位置。
//! 要用上它，得让 CEF 打开 DevTools 端口（12233）——那需要给 `libcef.dll` 打补丁，
//! 见 [`super::kugou_deploy`]，本模块只负责连上去读数据。
//!
//! 与 [`super::smtc`] 一样，[`KugouCdpProvider::snapshot`] 必须快速返回：真正的轮询在后台
//! 线程上跑，快照只读它写下的缓存。

use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tungstenite::stream::MaybeTlsStream;
use tungstenite::Message;

use super::provider::{now_epoch_ms, Capabilities, Control, MusicProvider, ProviderState};
use crate::error::{AppError, AppResult};

/// 补丁打开的 DevTools 端口（与 [`super::kugou_deploy`] 的补丁写入的立即数一致）
pub const CDP_PORT: u16 = 12233;
/// 轮询间隔：歌词换行靠它，200ms 足够（渲染层在两次锚点之间按速率自己外推）
const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// 断线后的重连间隔
const RECONNECT_INTERVAL: Duration = Duration::from_secs(3);
/// 关掉增强时线程空转的间隔
const IDLE_INTERVAL: Duration = Duration::from_millis(1000);
/// 快照最大允许陈旧时间（超过就当没有源）
const SNAPSHOT_TTL: Duration = Duration::from_millis(2000);

/// `SuperCall(864)` 返回的播放信息（字段都是字符串）
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlayInfo {
    /// 「艺术家 - 标题」
    pub filename: String,
    /// playing | paused | stopped
    pub play_status: String,
    pub position_ms: i64,
    pub duration_ms: i64,
    pub cover_url: String,
    /// 正在播放那份音频的 hash（拿它取歌词就没有版本歧义）
    pub hash: String,
}

impl PlayInfo {
    pub fn is_playing(&self) -> bool {
        self.play_status == "playing"
    }

    /// 「艺术家 - 标题」拆成 (标题, 艺术家)；没有分隔符时整串当标题
    pub fn title_artist(&self) -> (String, String) {
        match self.filename.split_once(" - ") {
            Some((artist, title)) => (title.trim().to_string(), artist.trim().to_string()),
            None => (self.filename.trim().to_string(), String::new()),
        }
    }
}

/// CDP `progress` 是 100 纳秒刻度（实测：~9.91e6 单位/秒，扣掉酷狗自身快 1.2% 的时钟正好 1e7）。
/// 数值大小本就无法与毫秒单点区分，故按实测单位解，并用时长做一次上限保护。
pub fn decode_progress_ms(raw: &str, duration_ms: i64) -> i64 {
    let value = raw.parse::<i64>().unwrap_or(0).max(0);
    let ticks = value / 10_000;
    if duration_ms > 0 && ticks > duration_ms {
        duration_ms
    } else {
        ticks
    }
}

/// 字段可能给字符串也可能给数字
fn text_of(v: &serde_json::Value, key: &str) -> String {
    match v.get(key) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// 把 `SuperCall(864)` 的 JSON 解析成 [`PlayInfo`]
pub fn parse_play_info(raw: &str) -> Option<PlayInfo> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    // 没有歌名且没有 hash 说明不是一首歌（广告、空队列）
    let filename = text_of(&v, "filename");
    let hash = text_of(&v, "hash");
    if filename.is_empty() && hash.is_empty() {
        return None;
    }
    let duration_ms = text_of(&v, "duration").parse::<i64>().unwrap_or(0);
    Some(PlayInfo {
        filename,
        play_status: text_of(&v, "play_status"),
        position_ms: decode_progress_ms(&text_of(&v, "progress"), duration_ms),
        duration_ms,
        cover_url: text_of(&v, "cover"),
        hash,
    })
}

/// DevTools 目标里的一项
fn pick_page(pages: &serde_json::Value) -> Option<String> {
    let list = pages.as_array()?;
    // 优先桌面弹窗页（就是带 KgSuperCall 的那个），否则退回任一 page
    let has = |p: &&serde_json::Value, needle: &str| {
        p.get("url").and_then(|u| u.as_str()).is_some_and(|u| u.contains(needle))
    };
    let pick = list
        .iter()
        .find(|p| has(p, "desktop-popup"))
        .or_else(|| list.iter().find(|p| p.get("type").and_then(|t| t.as_str()) == Some("page")))?;
    pick.get("webSocketDebuggerUrl").and_then(|u| u.as_str()).map(str::to_string)
}

/// 一段 CDP 会话：连上某个页面后可以反复 `Runtime.evaluate`
struct CdpSession {
    ws: tungstenite::WebSocket<MaybeTlsStream<TcpStream>>,
    next_id: i64,
}

impl CdpSession {
    fn connect() -> AppResult<Self> {
        let pages: serde_json::Value = ureq::get(&format!("http://127.0.0.1:{CDP_PORT}/json"))
            .call()
            .map_err(|e| AppError::new(format!("error.io: CDP 未响应: {e}")))?
            .body_mut()
            .read_json()
            .map_err(|e| AppError::new(format!("error.io: CDP /json 解析失败: {e}")))?;
        let url = pick_page(&pages).ok_or("error.io: CDP 没有可用页面（酷狗没在跑？）")?;
        let (mut ws, _) = tungstenite::connect(&url)
            .map_err(|e| AppError::new(format!("error.io: CDP 连接失败: {e}")))?;
        // 读超时让 evaluate 等待回复时有上限，不会把轮询线程挂死
        if let MaybeTlsStream::Plain(tcp) = ws.get_mut() {
            let _ = tcp.set_read_timeout(Some(Duration::from_millis(1500)));
        }
        // Runtime.enable：不等回复，后续按 id 匹配
        ws.send(Message::text(r#"{"id":0,"method":"Runtime.enable"}"#))
            .map_err(|e| AppError::new(format!("error.io: CDP 发送失败: {e}")))?;
        Ok(Self { ws, next_id: 1 })
    }

    /// `external.SuperCall(864)`：拿当前播放信息
    fn play_info(&mut self) -> AppResult<Option<PlayInfo>> {
        let raw = self.evaluate(SUPERCALL_PLAYINFO_JS)?;
        Ok(parse_play_info(&raw))
    }

    /// 执行一段返回 Promise 的 JS，等它 resolve 出字符串
    fn evaluate(&mut self, js: &str) -> AppResult<String> {
        let id = self.next_id;
        self.next_id += 1;
        let req = serde_json::json!({
            "id": id,
            "method": "Runtime.evaluate",
            "params": { "expression": js, "returnByValue": true, "awaitPromise": true }
        });
        self.ws
            .send(Message::text(req.to_string()))
            .map_err(|e| AppError::new(format!("error.io: CDP evaluate 发送失败: {e}")))?;

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let msg = match self.ws.read() {
                Ok(m) => m,
                // 读超时/半包：接着等
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                {
                    continue
                }
                Err(e) => return Err(AppError::new(format!("error.io: CDP 读失败: {e}"))),
            };
            let Message::Text(text) = msg else { continue };
            let v: serde_json::Value = match serde_json::from_str(text.as_str()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("id").and_then(|i| i.as_i64()) != Some(id) {
                continue;
            }
            if let Some(err) = v.get("result").and_then(|r| r.get("exceptionDetails")) {
                return Err(AppError::new(format!("error.io: CDP JS 异常: {err}")));
            }
            return Ok(v["result"]["result"]["value"].as_str().unwrap_or("").to_string());
        }
        Err(AppError::new("error.io: CDP evaluate 超时"))
    }
}

/// 参考实现（Metabox-Nexus-PlayerCap, MIT）同款调用：864 = 取当前播放信息
const SUPERCALL_PLAYINFO_JS: &str = r#"new Promise(function(resolve){var j="topisland_"+Date.now();window[j]=function(d){window[j]=null;resolve(typeof d==="string"?d:JSON.stringify(d));};try{external.SuperCall(864,JSON.stringify({callback:j}));}catch(e){resolve("");}setTimeout(function(){resolve("");},2500);})"#;

#[derive(Debug, Default)]
struct Cache {
    /// (快照时刻, 播放信息)
    latest: Option<(Instant, PlayInfo)>,
}

#[derive(Debug, Default)]
struct Inner {
    cache: Mutex<Cache>,
    connected: AtomicBool,
    /// 关闭增强后线程空转，不再打扰酷狗
    enabled: AtomicBool,
}

/// 酷狗 CDP 源
#[derive(Debug)]
pub struct KugouCdpProvider {
    inner: Arc<Inner>,
}

impl KugouCdpProvider {
    /// 起一条轮询线程：连 CDP → 每 200ms 读一次播放信息 → 有变化就 `on_change`
    pub fn start(on_change: impl Fn() + Send + 'static) -> Self {
        let inner = Arc::new(Inner::default());
        let worker = Arc::clone(&inner);
        let spawned = std::thread::Builder::new().name("kugou-cdp".into()).spawn(move || {
            while !worker.enabled.load(Ordering::Acquire) {
                std::thread::sleep(IDLE_INTERVAL);
            }
            loop {
                if !worker.enabled.load(Ordering::Acquire) {
                    worker.set_offline();
                    std::thread::sleep(IDLE_INTERVAL);
                    continue;
                }
                match CdpSession::connect() {
                    Ok(mut session) => {
                        worker.connected.store(true, Ordering::Release);
                        on_change();
                        loop {
                            if !worker.enabled.load(Ordering::Acquire) {
                                break;
                            }
                            match session.play_info() {
                                Ok(Some(info)) => {
                                    if worker.store(info) {
                                        on_change();
                                    }
                                }
                                // 酷狗没在放歌（广告、空队列）：清掉快照，让岛回落到 SMTC
                                Ok(None) => {
                                    if worker.clear() {
                                        on_change();
                                    }
                                }
                                // 连不上了（酷狗退出/重启/更新换掉了 DLL）：出去重连
                                Err(_) => break,
                            }
                            std::thread::sleep(POLL_INTERVAL);
                        }
                        worker.set_offline();
                        on_change();
                    }
                    Err(_) => std::thread::sleep(RECONNECT_INTERVAL),
                }
            }
        });
        if let Err(e) = spawned {
            eprintln!("[kugou-cdp] 轮询线程启动失败: {e}");
        }
        Self { inner }
    }

    /// 开关：关掉后线程空转并清掉缓存（岛回落到 SMTC）
    pub fn set_enabled(&self, on: bool) {
        self.inner.enabled.store(on, Ordering::Release);
    }

    pub fn is_connected(&self) -> bool {
        self.inner.connected.load(Ordering::Acquire)
    }
}

impl Inner {
    /// 写入并返回是否有实质变化（歌名/状态/位置都算）
    fn store(&self, info: PlayInfo) -> bool {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let changed = cache.latest.as_ref().is_none_or(|(_, prev)| *prev != info);
        cache.latest = Some((Instant::now(), info));
        changed
    }

    fn set_offline(&self) {
        self.connected.store(false, Ordering::Release);
        self.clear();
    }

    /// 清掉快照（连接还在时也可能是「没在放歌」），返回之前是否有快照
    fn clear(&self) -> bool {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.latest.take().is_some()
    }
}

impl MusicProvider for KugouCdpProvider {
    fn name(&self) -> &'static str {
        "kugou-cdp"
    }

    fn snapshot(&self) -> Option<ProviderState> {
        if !self.inner.enabled.load(Ordering::Acquire) {
            return None;
        }
        let (at, info) = {
            let cache = self.inner.cache.lock().unwrap_or_else(|e| e.into_inner());
            let (at, info) = cache.latest.as_ref()?;
            (*at, info.clone())
        };
        if at.elapsed() >= SNAPSHOT_TTL {
            return None;
        }
        let (title, artist) = info.title_artist();
        // 采样是在 200ms 内取的，用「采样时刻」当锚点即可（渲染层按速率外推）
        let anchor = now_epoch_ms() - at.elapsed().as_millis() as i64;
        let playing = info.is_playing();
        Some(ProviderState {
            is_playing: playing,
            title,
            artist,
            album: None,
            // 与 SMTC 会话同一个标识，好让酷狗的相关判定（is_source / 开关 / 歌词白名单）都认得
            source_app_id: "kugou.exe".to_string(),
            // 约定：kugou:<hash> —— resolver 见到它就走按 hash 取词的精确路径
            song_id: (!info.hash.is_empty()).then(|| format!("kugou:{}", info.hash)),
            position_ms: info.position_ms,
            anchor_epoch_ms: anchor,
            rate: if playing { 1.0 } else { 0.0 },
            duration_ms: (info.duration_ms > 0).then_some(info.duration_ms),
            // 播放位置能读，但跳转还没接（控制先由 SMTC 兜底）
            seek_supported: false,
            artwork_url: (!info.cover_url.is_empty())
                .then(|| info.cover_url.replace("http://", "https://")),
        })
    }

    fn capabilities(&self) -> Capabilities {
        // 控制不在这里做：route() 会回退到 SMTC（它有 skip + fallback）
        Capabilities { skip: false, artwork_bitmap: false, fallback: false }
    }

    fn control(&self, _action: Control) -> AppResult<bool> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_ticks_decode_to_milliseconds() {
        // 真机实测：progress=1545777188 时 ini（30 秒落盘）里是 136672ms，按 100ns 解是 154577ms
        assert_eq!(decode_progress_ms("1545777188", 251_293), 154_577);
        assert_eq!(decode_progress_ms("2265529271", 251_293), 226_552);
        assert_eq!(decode_progress_ms("0", 251_293), 0);
        assert_eq!(decode_progress_ms("abc", 251_293), 0, "脏数据当 0");
        // 除完超出时长说明单位变了：钳到时长，别让位置跑到歌外
        assert_eq!(decode_progress_ms("99999999999999", 251_293), 251_293);
    }

    #[test]
    fn play_info_parses_real_response() {
        // 真机 SuperCall(864) 返回（裁剪）
        let raw = r#"{"album_id":"983011","bitrate":"3487000",
            "cover":"http://imge.kugou.com/stdmusic/120/20221110/20221110154602109135.jpg",
            "duration":"251293","filename":"\u6797\u4FCA\u6770 - \u5F53\u4F60",
            "hash":"675fac779350d00c9b02e54bc2edb6a8","play_status":"playing",
            "progress":"2265529271","status":"1","type":"song"}"#;
        let info = parse_play_info(raw).expect("应能解析");
        assert_eq!(info.filename, "林俊杰 - 当你");
        assert_eq!(info.title_artist(), ("当你".into(), "林俊杰".into()));
        assert_eq!(info.play_status, "playing");
        assert!(info.is_playing());
        assert_eq!(info.position_ms, 226_552);
        assert_eq!(info.duration_ms, 251_293);
        assert_eq!(info.hash, "675fac779350d00c9b02e54bc2edb6a8");
        assert!(info.cover_url.starts_with("http://imge.kugou.com/"));
    }

    #[test]
    fn play_info_rejects_ads_and_empty_payloads() {
        assert!(parse_play_info("").is_none());
        assert!(parse_play_info("not json").is_none());
        // 没歌名也没 hash：广告/空队列，不该当成在放歌
        assert!(parse_play_info(r#"{"status":"1","type":"ad"}"#).is_none());
        // 只有 hash 也算一首歌（有些版本不给 filename）
        assert!(parse_play_info(r#"{"hash":"abc","duration":1000,"progress":0}"#).is_some());
    }

    #[test]
    fn numeric_fields_are_accepted_too() {
        let info = parse_play_info(r#"{"filename":"A - B","hash":"h","duration":1000,"progress":5000000}"#)
            .expect("数字型字段也要能读");
        assert_eq!(info.duration_ms, 1000);
        assert_eq!(info.position_ms, 500);
    }

    #[test]
    fn page_pick_prefers_desktop_popup() {
        let pages = serde_json::json!([
            { "type": "page", "url": "https://pc.service.kugou.com/yueku/find/index.html",
              "webSocketDebuggerUrl": "ws://127.0.0.1:12233/devtools/page/FIND" },
            { "type": "page", "url": "https://pc.service.kugou.com/apps/pcwv-desktop-popup/dist/index.html",
              "webSocketDebuggerUrl": "ws://127.0.0.1:12233/devtools/page/POPUP" }
        ]);
        assert_eq!(pick_page(&pages).as_deref(), Some("ws://127.0.0.1:12233/devtools/page/POPUP"));

        // 没有 popup 时退回第一个 page；空列表返回 None
        let only_find = serde_json::json!([
            { "type": "page", "url": "find", "webSocketDebuggerUrl": "ws://x/FIND" }
        ]);
        assert_eq!(pick_page(&only_find).as_deref(), Some("ws://x/FIND"));
        assert!(pick_page(&serde_json::json!([])).is_none());
    }

    #[test]
    fn offline_snapshot_is_not_a_source() {
        let p = KugouCdpProvider { inner: Arc::new(Inner::default()) };
        assert!(p.snapshot().is_none(), "没连上时不能当源");
        p.set_enabled(true);
        assert!(p.snapshot().is_none(), "开关开着但没有快照时同样不能当源");
        assert!(!p.is_connected());
        // 塞一份快照：应当能作为源，并把 filename 拆成标题/艺术家
        p.inner.store(PlayInfo {
            filename: "周杰伦 - 晴天".into(),
            play_status: "playing".into(),
            position_ms: 12_345,
            duration_ms: 269_000,
            cover_url: "http://imge.kugou.com/x.jpg".into(),
            hash: "abc123".into(),
        });
        let s = p.snapshot().expect("有新鲜快照时应作为源");
        assert_eq!(s.title, "晴天");
        assert_eq!(s.artist, "周杰伦");
        assert_eq!(s.song_id.as_deref(), Some("kugou:abc123"));
        assert_eq!(s.position_ms, 12_345);
        assert_eq!(s.duration_ms, Some(269_000));
        assert_eq!(s.rate, 1.0);
        assert_eq!(s.source_app_id, "kugou.exe");
        assert_eq!(s.artwork_url.as_deref(), Some("https://imge.kugou.com/x.jpg"));
        // 关掉开关后必须立刻不再是源
        p.set_enabled(false);
        assert!(p.snapshot().is_none());
    }

    /// 真机冒烟：酷狗打过 libcef 补丁并在运行时，源应能给出正在播放的曲目。
    /// `cargo test -p island-app --lib kugou_cdp -- --ignored --nocapture`
    #[test]
    #[ignore = "需要打过 libcef 补丁并运行中的酷狗"]
    fn live_cdp_provider_snapshots_playing_track() {
        let provider = KugouCdpProvider::start(|| {});
        provider.set_enabled(true);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(s) = provider.snapshot() {
                println!("snapshot = {s:?}");
                assert!(!s.title.is_empty(), "应有标题");
                assert!(s.duration_ms.unwrap_or(0) > 60_000, "应有时长");
                assert!(
                    s.position_ms >= 0 && s.position_ms <= s.duration_ms.unwrap_or(i64::MAX),
                    "位置应落在 [0, 时长] 内: {} / {:?}",
                    s.position_ms,
                    s.duration_ms
                );
                assert!(s.song_id.is_some(), "应带 kugou:<hash> 以便按 hash 取词");
                // 用 CDP 拿到的 hash 走精确取词：本地 .krc 优先，其次接口按 hash
                let sid = s.song_id.clone().unwrap_or_default();
                let hash = sid.trim_start_matches("kugou:").to_string();
                let lyrics = crate::services::music::kugou::provider_fetch_by_hash(
                    &hash,
                    s.duration_ms.unwrap_or(0),
                    &s.title,
                )
                .expect("按 hash 应能取到歌词（本地缓存或接口）");
                println!("lyrics = {} 行, 覆盖 {} ms", lyrics.lines.len(), lyrics.duration_ms);
                assert!(lyrics.lines.len() > 3, "歌词行数不该这么少");
                assert!(
                    lyrics.lines.windows(2).all(|w| w[0].time_ms <= w[1].time_ms),
                    "时间轴应升序"
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!("10 秒内没拿到 CDP 快照（补丁生效了吗？酷狗在放歌吗？）");
    }
}
