//! 音乐域聚合：多个音乐源（provider）按优先级选出当前生效的一个，经 resolver 补全
//! 歌词/封面/元数据后产出 `MusicState`，由推送线程 emit 给渲染层。
//! 控制按源的 `Capabilities` 路由，不比对源名。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once, OnceLock};

use tauri::AppHandle;

use island_core::{AppSettings, LyricsData, MusicAction, MusicArtwork, MusicState};

use crate::error::AppResult;

mod b64;
mod hash;
mod kugou;
mod kugou_cdp;
mod kugou_deploy;
mod lyrics;
mod media_sources;
mod ncm_bridge;
mod ncm_deploy;
mod net;
mod provider;
mod push;
mod resolver;
mod smtc;

pub use kugou::KugouStatus;
pub use kugou_deploy::{EnhanceStatus, PatchState};
pub use ncm_deploy::BridgeStatus;

/// 提权 helper 的命令行开关（`main` 在 Tauri 起来之前拦它）
pub const KUGOU_HELPER_FLAG: &str = kugou_deploy::HELPER_FLAG;

use ncm_bridge::NcmBridgeProvider;
use kugou_cdp::KugouCdpProvider;
use provider::{Control, MusicProvider};
use push::PushSignal;
use resolver::Resolver;
use smtc::SmtcProvider;

/// 酷狗接入开关（设置里可关）：关掉后酷狗的媒体会话按不存在处理，不显示也不接管控制
static KUGOU_ENABLED: AtomicBool = AtomicBool::new(true);
/// 酷狗进程内增强开关（设置里可开）：状态判定与「要不要打补丁」都看它
static KUGOU_ENHANCE_ENABLED: AtomicBool = AtomicBool::new(false);

/// 采样与自己外推差超过这个值就以采样为准（用户拖动进度条、时钟漂移都靠它纠正）
const SAMPLE_TOLERANCE_MS: i64 = 800;
/// 采样时刻比「这首歌起播」还早这么多，就认定它是上一首的残留。
/// 取小值是有意的：误丢一份新采样只是继续用自己数的进度（切歌后误差本来就不到一秒），
/// 误用一份旧采样却会让进度条错到下一份采样为止（最多 30 秒）。酷狗 30 秒才落一次盘，
/// 于是只有「切歌前 0.3 秒内正好落盘」这一小概率才会漏判。
const SAMPLE_STALE_MS: i64 = 300;

/// 无时间轴源的进度跟踪。
///
/// 酷狗的媒体会话不给时间轴，进度得自己算，但有两条路：
/// 1. 精确采样——酷狗每 30 秒把真进度写进 `KuGou.ini`（见 [`kugou::ini_state`]），
///    配上文件 mtime 就能把「采样那刻的进度」换算到当前时刻；
/// 2. 兜底估算——没有采样（读不到 ini、换了别的播放器）时按墙钟外推。
///
/// 切歌归零、暂停冻结、续播接着走；采样与自己外推差超过 [`SAMPLE_TOLERANCE_MS`] 时以采样为准，
/// 所以拖动进度条、或岛启动时歌已经放了一半，都能在一两次采样内对齐。
///
/// 另外酷狗的进度计数比墙钟快约 1.2%（实测 30 秒走 30.36 秒），照墙钟外推会每个采样周期
/// 落后 0.36 秒、攒到 0.8 秒再跳一下。所以这里从相邻两次采样**学出这个倍率**，
/// 采样之间按它外推，位置就贴着酷狗自己的走法（也就贴着酷狗自己显示的歌词）。
#[derive(Debug, Default)]
struct ProgressTracker {
    /// 当前曲目标识（标题|艺术家|来源）
    track: String,
    playing: bool,
    /// 锚点：anchor_pos_ms 是 anchor_epoch_ms 时刻的进度
    anchor_pos_ms: i64,
    anchor_epoch_ms: i64,
    /// 这首歌的起播时刻，用来识别上一首残留的采样
    track_start_ms: i64,
    /// 已经对过账的采样时刻，同一份采样不重复纠正
    sample_epoch_ms: i64,
    /// 上一份用过的采样，用来量出播放器时钟相对墙钟的倍率
    last_sample: Option<kugou::PositionSample>,
    /// 播放器时钟倍率（1.0 = 与墙钟一致）
    rate: f64,
}

/// 学倍率时用的合理区间：超出说明中间暂停/拖动过，这一轮不学
const LEARNED_RATE_RANGE: std::ops::RangeInclusive<f64> = 0.9..=1.1;

impl ProgressTracker {
    /// 返回 (position_ms, anchor_epoch_ms, rate)。`duration_ms` 为空表示时长未知
    /// （歌词接口没查到），此时照常跟踪位置，只是不做上限钳制。
    fn observe(
        &mut self,
        track: &str,
        playing: bool,
        now_ms: i64,
        sample: Option<kugou::PositionSample>,
        duration_ms: Option<i64>,
    ) -> (i64, i64, f64) {
        if track != self.track {
            let first_sight = self.track.is_empty();
            self.track = track.to_string();
            self.playing = playing;
            self.anchor_pos_ms = 0;
            self.anchor_epoch_ms = now_ms;
            self.track_start_ms = now_ms;
            self.sample_epoch_ms = 0;
            self.last_sample = None;
            self.rate = 1.0;
            // 第一次看到这首歌时它可能已经播到一半了：直接采用采样，歌词立刻对得上，
            // 也把起播时刻反推出来（采样时刻 - 当时进度）
            if first_sight {
                if let Some(s) = sample {
                    self.anchor_pos_ms = s.position_ms;
                    self.anchor_epoch_ms = s.at_epoch_ms;
                    self.track_start_ms = s.at_epoch_ms - s.position_ms;
                    self.sample_epoch_ms = s.at_epoch_ms;
                    // 记下来，下一份采样就能量出播放器时钟倍率
                    self.last_sample = Some(s);
                }
            }
        } else if playing != self.playing {
            self.anchor_pos_ms = self.position_at(now_ms);
            self.anchor_epoch_ms = now_ms;
            self.playing = playing;
        }

        if let Some(s) = sample {
            // 采样可能还是上一首的（切歌后最多 30 秒才会写进新值）：那时它落在本曲起播之前
            let stale = s.at_epoch_ms + SAMPLE_STALE_MS < self.track_start_ms;
            let plausible = duration_ms.is_none_or(|d| s.position_ms <= d);
            if s.at_epoch_ms != self.sample_epoch_ms && !stale && plausible {
                self.learn_rate(s);
                let sample_now =
                    s.position_ms + self.elapsed_since(s.at_epoch_ms, now_ms);
                if (sample_now - self.position_at(now_ms)).abs() > SAMPLE_TOLERANCE_MS {
                    self.anchor_pos_ms = sample_now;
                    self.anchor_epoch_ms = now_ms;
                }
                self.sample_epoch_ms = s.at_epoch_ms;
                self.last_sample = Some(s);
            }
        }

        let position = match duration_ms {
            Some(d) if d > 0 => self.position_at(now_ms).clamp(0, d),
            _ => self.position_at(now_ms).max(0),
        };
        (position, now_ms, if self.playing { self.rate } else { 0.0 })
    }

    /// 用相邻两次采样量播放器时钟倍率（同一首歌、都在播、中间没暂停才可信）
    fn learn_rate(&mut self, sample: kugou::PositionSample) {
        let Some(prev) = self.last_sample else { return };
        let d_pos = sample.position_ms - prev.position_ms;
        let d_at = sample.at_epoch_ms - prev.at_epoch_ms;
        if d_at <= 0 || !self.playing {
            return;
        }
        let rate = d_pos as f64 / d_at as f64;
        if LEARNED_RATE_RANGE.contains(&rate) {
            self.rate = rate;
        }
    }

    /// 从 `from_ms` 到 `to_ms` 之间播放器时钟走了多少（暂停时不走）
    fn elapsed_since(&self, from_ms: i64, to_ms: i64) -> i64 {
        if !self.playing {
            return 0;
        }
        (((to_ms - from_ms).max(0) as f64) * self.rate).round() as i64
    }

    /// 锚点外推到 `at_ms`；暂停时停着不动
    fn position_at(&self, at_ms: i64) -> i64 {
        self.anchor_pos_ms + self.elapsed_since(self.anchor_epoch_ms, at_ms)
    }
}

struct Service {
    /// 优先级从高到低，第一个有快照的即生效源
    providers: Vec<&'static dyn MusicProvider>,
    bridge: &'static NcmBridgeProvider,
    /// 酷狗 CDP 源（需要 libcef 打过补丁；开关关掉时空转）
    kugou_cdp: &'static KugouCdpProvider,
    /// 酷狗接入状态要直接问系统媒体会话
    smtc: &'static SmtcProvider,
    resolver: &'static Resolver,
    /// 上次 poll 选中的源，控制路由到它
    active: Mutex<&'static dyn MusicProvider>,
    /// 无时间轴源的进度跟踪
    progress: Mutex<ProgressTracker>,
}

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service").finish_non_exhaustive()
    }
}

static SERVICE: OnceLock<Service> = OnceLock::new();
static PUSH: PushSignal = PushSignal::new();

fn request_push() {
    PUSH.request();
}

pub fn sync(app: &AppHandle, settings: &AppSettings) {
    let enabled = settings.diagnostics.music_poll;
    KUGOU_ENABLED.store(settings.music.kugou_support, Ordering::Relaxed);
    KUGOU_ENHANCE_ENABLED.store(settings.music.kugou_enhance, Ordering::Relaxed);
    PUSH.set_enabled(enabled);
    if enabled {
        static START: Once = Once::new();
        START.call_once(|| init(app.clone()));
    }
    // CDP 源要酷狗接入与增强都开着；关掉后线程空转、不再连酷狗
    if let Some(svc) = SERVICE.get() {
        svc.kugou_cdp
            .set_enabled(enabled && settings.music.kugou_support && settings.music.kugou_enhance);
    }
    // 子系统关着时 WS 服务端没在跑，部署了代理也连不上
    ncm_deploy::set_wanted(enabled && settings.music.netease_bridge);
}

fn init(app: AppHandle) {
    let smtc: &'static SmtcProvider = Box::leak(Box::new(SmtcProvider::new()));
    let bridge: &'static NcmBridgeProvider = Box::leak(Box::new(NcmBridgeProvider::start(request_push)));
    let kugou_cdp: &'static KugouCdpProvider =
        Box::leak(Box::new(KugouCdpProvider::start(request_push)));
    let resolver: &'static Resolver = Box::leak(Box::new(Resolver::new(request_push)));
    let _ = SERVICE.set(Service {
        // 酷狗 CDP 排在 SMTC 前面：它有毫秒级位置与精确时长/hash，SMTC 只是兜底
        providers: vec![bridge, kugou_cdp, smtc],
        bridge,
        kugou_cdp,
        smtc,
        resolver,
        active: Mutex::new(smtc),
        progress: Mutex::new(ProgressTracker::default()),
    });
    push::start(app, &PUSH, "music:state", poll_state);
    island_windows::smtc::start_watch(request_push);
}

pub fn bridge_status() -> BridgeStatus {
    let connected = SERVICE.get().map(|s| s.bridge.is_connected()).unwrap_or(false);
    ncm_deploy::status(connected)
}

/// 酷狗接入状态：CDP 已连上就报已连接；增强模式下按补丁/进程状态给出下一步
pub fn kugou_status() -> KugouStatus {
    let Some(svc) = SERVICE.get() else {
        return kugou::status(None);
    };
    if svc.kugou_cdp.is_connected() {
        return KugouStatus::Connected;
    }
    if KUGOU_ENHANCE_ENABLED.load(Ordering::Relaxed) {
        let st = kugou_deploy::status();
        return match st.patch {
            // 没找到安装：按进程在不在区分「未检测到」与「未运行」
            PatchState::Missing => {
                if st.kugou_running {
                    KugouStatus::NotRunning
                } else {
                    KugouStatus::NotDetected
                }
            }
            PatchState::Unsupported | PatchState::Pending => KugouStatus::NeedsPatch,
            PatchState::Patched => {
                if !st.kugou_running {
                    KugouStatus::NotRunning
                } else if st.needs_restart {
                    KugouStatus::NeedsRestart
                } else {
                    KugouStatus::Connecting
                }
            }
        };
    }
    let session_app = svc.smtc.snapshot().map(|s| s.source_app_id);
    kugou::status(session_app.as_deref())
}

/// 增强的详细状态（设置页据此决定显示「修复」还是「还原」按钮）
pub fn kugou_enhance_status() -> EnhanceStatus {
    kugou_deploy::status()
}

/// 打开「酷狗音乐」开关时自动补增强：提权打补丁（一次 UAC；酷狗开着才重启它）
pub fn kugou_repair() -> AppResult<String> {
    kugou_deploy::ensure_applied()
}

/// 关闭「酷狗音乐」开关时自动还原：提权把 libcef.dll 恢复成原版（一次 UAC；酷狗开着才重启它）
pub fn kugou_revert() -> AppResult<String> {
    kugou_deploy::revert().map(|()| "已还原酷狗的 libcef.dll。".to_string())
}

/// 提权 helper 的入口（`main` 在 Tauri 起来之前调用；返回进程退出码）
pub fn run_kugou_patch_helper(mode: &str, libcef: &str, control: &str) -> i32 {
    kugou_deploy::run_patch_helper(mode, libcef, control)
}

pub fn poll_state() -> MusicState {
    let Some(svc) = SERVICE.get() else {
        return MusicState::default();
    };

    let kugou_enabled = KUGOU_ENABLED.load(Ordering::Relaxed);
    let found = svc.providers.iter().find_map(|p| {
        let src = p.snapshot()?;
        // 关掉酷狗接入时它的会话当不存在：岛显示「未在播放」，而不是显示一个不该管的播放器
        if !kugou_enabled && kugou::is_source(&src.source_app_id) {
            return None;
        }
        Some((*p, src))
    });
    let Some((provider, src)) = found else {
        svc.resolver.reset();
        return MusicState::default();
    };
    *svc.active.lock().unwrap_or_else(|e| e.into_inner()) = provider;

    let r = svc.resolver.observe(&src, provider);
    let track = if r.title.is_empty() { fallback_track(&src.source_app_id) } else { r.title };
    // 酷狗不上报时间轴，进度得自己算（读它落的 ini），否则歌词永远停在第一行；
    // 其它源没有时间轴时保持原样（位置为 0），不臆造进度
    let (position_ms, anchor_epoch_ms, rate) = if src.duration_ms.is_none()
        && kugou::is_source(&src.source_app_id)
    {
        let key = format!("{track}|{}|{}", r.artist, src.source_app_id);
        let mut progress = svc.progress.lock().unwrap_or_else(|e| e.into_inner());
        progress.observe(
            &key,
            src.is_playing,
            provider::now_epoch_ms(),
            kugou::position_sample(),
            r.duration_ms,
        )
    } else {
        (src.position_ms, src.anchor_epoch_ms, src.rate)
    };
    MusicState {
        provider: provider.name().to_string(),
        is_playing: src.is_playing,
        track: Some(track),
        artist: (!r.artist.is_empty()).then_some(r.artist),
        album: r.album,
        source_app_id: Some(src.source_app_id.clone()),
        song_id: src.song_id.clone(),
        position_ms,
        anchor_epoch_ms,
        rate,
        duration_ms: r.duration_ms,
        seek_supported: src.seek_supported,
        artwork_url: r.artwork_url,
        artwork_hash: r.artwork_hash,
        lyrics_id: r.lyrics_id,
    }
}

/// 无标题时用来源标识兜底：AUMID 最后一段，或末尾 30 字符
fn fallback_track(source_app_id: &str) -> String {
    let tail = if source_app_id.contains('.') {
        source_app_id.rsplit('.').next().unwrap_or(source_app_id).to_string()
    } else {
        source_app_id.chars().rev().take(30).collect::<Vec<_>>().into_iter().rev().collect()
    };
    format!("SMTC: {tail}")
}

/// hash 不匹配说明已切歌，返回 None
pub fn artwork(hash: &str) -> Option<MusicArtwork> {
    let (hash, data_url) = SERVICE.get()?.resolver.artwork(hash)?;
    Some(MusicArtwork { hash, data_url })
}

/// id 不匹配说明已切歌，返回 None
pub fn lyrics(id: &str) -> Option<LyricsData> {
    SERVICE.get()?.resolver.lyrics(id)
}

pub fn seek(position_ms: i64) -> bool {
    route(Control::Seek(position_ms.max(0)))
}

pub fn control(action: MusicAction, level: Option<i64>) -> AppResult<String> {
    let msg = match action {
        MusicAction::Play => {
            route(Control::Play);
            "已发送播放指令。"
        }
        MusicAction::Pause => {
            route(Control::Pause);
            "已发送暂停指令。"
        }
        MusicAction::Next => {
            route(Control::Next);
            "已切换到下一首。"
        }
        MusicAction::Prev => {
            route(Control::Prev);
            "已切换到上一首。"
        }
        MusicAction::Volume => {
            let lvl = level.unwrap_or(50).clamp(0, 100) as f32 / 100.0;
            return match island_windows::coreaudio::set_master_volume_scalar(lvl) {
                Ok(()) => Ok(format!("音量已设置为 {}%", (lvl * 100.0).round() as i64)),
                Err(e) => {
                    eprintln!("[music] 调整音量失败: {e}");
                    Ok("无法调整音量。".to_string())
                }
            };
        }
    };
    Ok(msg.to_string())
}

/// 派给当前源；未送达或它不具备所需能力时回退到 fallback 源。返回是否送达。
fn route(action: Control) -> bool {
    let Some(svc) = SERVICE.get() else {
        return false;
    };
    let active: &'static dyn MusicProvider = *svc.active.lock().unwrap_or_else(|e| e.into_inner());
    let fallback = svc.providers.iter().copied().find(|p| p.capabilities().fallback);

    let capable = !action.needs_skip() || active.capabilities().skip;
    if capable {
        match active.control(action) {
            Ok(true) => return true,
            Ok(false) => {}
            Err(e) => eprintln!("[music] {} 控制失败，回退: {e}", active.name()),
        }
    }
    let is_active = |f: &&'static dyn MusicProvider| std::ptr::addr_eq(*f, active);
    match fallback.filter(|f| !is_active(f)) {
        Some(f) => f.control(action).unwrap_or_else(|e| {
            eprintln!("[music] {} 控制失败: {e}", f.name());
            false
        }),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_track_uses_aumid_tail_or_last_30_chars() {
        assert_eq!(fallback_track("cloudmusic.exe"), "SMTC: exe", "带点的 AUMID 取最后一段");
        let long = "a".repeat(40);
        assert_eq!(fallback_track(&long), format!("SMTC: {}", "a".repeat(30)), "无点时取末尾 30 字符");
    }

    #[test]
    fn estimate_counts_from_track_change_and_clamps_to_duration() {
        let mut t = ProgressTracker::default();
        assert_eq!(t.observe("a", true, 1_000, None, Some(10_000)).0, 0, "刚落到的曲目从 0 起算");
        assert_eq!(t.observe("a", true, 4_000, None, Some(10_000)).0, 3_000, "按墙钟推进");
        assert_eq!(t.observe("a", true, 99_000, None, Some(10_000)).0, 10_000, "不应超过曲目时长");
        assert_eq!(t.observe("b", true, 99_000, None, Some(10_000)).0, 0, "切歌归零");
    }

    #[test]
    fn estimate_freezes_while_paused_and_resumes_where_it_stopped() {
        let mut t = ProgressTracker::default();
        t.observe("a", true, 0, None, Some(100_000));
        let (pos, _, rate) = t.observe("a", false, 5_000, None, Some(100_000));
        assert_eq!((pos, rate), (5_000, 0.0), "暂停时冻结在当前位置");
        assert_eq!(t.observe("a", false, 60_000, None, Some(100_000)).0, 5_000, "暂停期间不推进");
        let (pos, _, rate) = t.observe("a", true, 60_000, None, Some(100_000));
        assert_eq!((pos, rate), (5_000, 1.0), "续播接着走而不是重新计时");
        assert_eq!(t.observe("a", true, 62_000, None, Some(100_000)).0, 7_000);
    }

    #[test]
    fn estimate_ignores_time_going_backwards() {
        let mut t = ProgressTracker::default();
        t.observe("a", true, 10_000, None, Some(100_000));
        let (pos, anchor, _) = t.observe("a", true, 9_000, None, Some(100_000));
        assert_eq!((pos, anchor), (0, 9_000), "时钟回拨时进度不退成负数");
    }

    fn sample(position_ms: i64, at_epoch_ms: i64) -> Option<kugou::PositionSample> {
        Some(kugou::PositionSample { position_ms, at_epoch_ms })
    }

    #[test]
    fn sample_puts_a_song_already_playing_back_in_sync() {
        // 岛刚起来时这首歌已经放到 90 秒，采样是 10 秒前落的盘 → 现在应该是 100 秒，
        // 而不是从 0 开始（这正是「歌词跟不上」的主因）
        let mut t = ProgressTracker::default();
        let (pos, _, rate) = t.observe("a", true, 1_000_000, sample(90_000, 990_000), Some(300_000));
        assert_eq!((pos, rate), (100_000, 1.0));
    }

    #[test]
    fn sample_from_previous_track_is_not_used_for_the_new_one() {
        let mut t = ProgressTracker::default();
        t.observe("a", true, 0, None, Some(300_000));
        t.observe("b", true, 1_000, None, Some(300_000)); // 切到 b，从 0 数起
        // ini 里还是上一首的 200 秒（这份采样是切歌前落的盘）：不能拿来当 b 的进度
        let (pos, _, _) = t.observe("b", true, 11_000, sample(200_000, 0), Some(300_000));
        assert_eq!(pos, 10_000, "残留采样应被丢弃，进度仍按自己数的 10 秒");
    }

    #[test]
    fn fresh_sample_corrects_a_jump_in_the_player() {
        let mut t = ProgressTracker::default();
        t.observe("a", true, 0, None, Some(300_000));
        // 用户在酷狗里拖到 120 秒：新落的采样与自己数的 10 秒差得远，以采样为准
        let (pos, _, _) = t.observe("a", true, 10_000, sample(120_000, 10_000), Some(300_000));
        assert_eq!(pos, 120_000);
    }

    #[test]
    fn sample_within_tolerance_does_not_jitter_the_position() {
        let mut t = ProgressTracker::default();
        t.observe("a", true, 0, None, Some(300_000));
        // 采样只比自己外推慢 0.3 秒（酷狗 30 秒落一次盘，本来就有微小偏差）：不纠正，免得跳一下
        let (pos, _, _) = t.observe("a", true, 10_000, sample(9_700, 10_000), Some(300_000));
        assert_eq!(pos, 10_000, "差在容差内应保持自己的外推");
    }

    #[test]
    fn same_sample_is_only_reconciled_once() {
        let mut t = ProgressTracker::default();
        t.observe("a", true, 0, sample(5_000, 0), Some(300_000));
        // 同一份采样（mtime 没变）反复读：按采样时刻继续外推，不能每次都重置成 5 秒
        assert_eq!(t.observe("a", true, 3_000, sample(5_000, 0), Some(300_000)).0, 8_000);
        assert_eq!(t.observe("a", true, 5_000, sample(5_000, 0), Some(300_000)).0, 10_000);
    }

    #[test]
    fn unknown_duration_still_tracks_position() {
        // 歌词接口没查到时长时也要跟得住位置：歌词照样滚，只是进度条没有上限
        let mut t = ProgressTracker::default();
        assert_eq!(t.observe("a", true, 0, sample(30_000, 0), None).0, 30_000);
        assert_eq!(t.observe("a", true, 5_000, sample(30_000, 0), None).0, 35_000);
    }

    #[test]
    fn learns_the_players_clock_rate_from_two_samples() {
        // 实测酷狗 30 秒走 30.36 秒：学会这个倍率后，两次采样之间就不会落后再跳回来
        let mut t = ProgressTracker::default();
        t.observe("a", true, 0, sample(0, 0), Some(600_000));
        let (pos, _, rate) = t.observe("a", true, 30_000, sample(30_360, 30_000), Some(600_000));
        assert!((rate - 1.012).abs() < 0.001, "应学出 1.012 的倍率，实测 {rate}");
        assert_eq!(pos, 30_360);
        // 采样的 15 秒后：按学到的倍率外推，正好跟上酷狗自己的走法
        assert_eq!(t.observe("a", true, 45_000, sample(30_360, 30_000), Some(600_000)).0, 45_540);
    }

    #[test]
    fn rate_learning_rejects_pauses_and_jumps() {
        let mut t = ProgressTracker::default();
        t.observe("a", true, 0, sample(0, 0), Some(600_000));
        // 中间暂停过：位置没怎么走，不能把倍率学成 0.1
        let (_, _, rate) = t.observe("a", true, 30_000, sample(6_000, 30_000), Some(600_000));
        assert!((rate - 1.0).abs() < f64::EPSILON, "不合理的倍率保持 1.0，实测 {rate}");
        // 拖动进度条往回跳：负倍率同样不学
        let (_, _, rate) = t.observe("a", true, 31_000, sample(-5_000, 31_000), Some(600_000));
        assert!((rate - 1.0).abs() < f64::EPSILON, "实测 {rate}");
    }

    #[test]
    fn rate_resets_when_the_track_changes() {
        let mut t = ProgressTracker::default();
        t.observe("a", true, 0, sample(0, 0), Some(600_000));
        t.observe("a", true, 30_000, sample(30_360, 30_000), Some(600_000));
        assert!(t.rate > 1.0);
        let (_, _, rate) = t.observe("b", true, 60_000, None, Some(600_000));
        assert!((rate - 1.0).abs() < f64::EPSILON, "新歌先按 1.0 走，等两次采样再学");
    }

    #[test]
    fn implausible_sample_is_ignored() {
        let mut t = ProgressTracker::default();
        t.observe("a", true, 0, None, Some(300_000));
        // 比曲目时长还大：脏数据，丢掉而不是把进度条顶到天上
        assert_eq!(t.observe("a", true, 10_000, sample(999_000, 10_000), Some(300_000)).0, 10_000);
    }
}
