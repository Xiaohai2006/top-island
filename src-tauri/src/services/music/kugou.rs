//! 酷狗音乐接入：官方开放接口（搜索 / 歌词）与宿主检测。
//!
//! 岛不注入酷狗进程——酷狗 PC 版自带系统播放控件（SMTC）实现，在客户端
//! 「设置 → 常规设置 → 播放 → 支持系统播放控件，如锁屏界面」开启后，播放状态与控制
//! 走 [`SmtcProvider`](super::smtc) 的通用通道。本模块只补 SMTC 拿不到的东西：
//! 歌词、专辑、封面直链、时长，以及设置页要展示的连接状态。
//!
//! 三条接口都不依赖 cookie 与签名（实测可用）：
//! - 搜索 `songsearch.kugou.com/song_search_v2`：`data.lists[]` 的 `FileHash` / `AlbumName` /
//!   `Duration`（秒）/ `trans_param.union_cover`（封面模板，`{size}` 换成尺寸）
//! - 歌词 `krcs.kugou.com/search`（按 `FileHash` 取候选）→ `lyrics.kugou.com/download`
//!   （`fmt=lrc` 让服务端直接把 KRC 转成 LRC，回来是 base64）
//! - 封面只认 `union_cover`：`imge.kugou.com/stdmusic/{size}/{album_id}.jpg` 那种拼 album_id
//!   的老写法对任何 album_id（含不存在的）都返回同一张占位图，用了等于挂错封面

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use island_core::{LyricLine, LyricsData};

use super::b64;
use super::lyrics::{parse_lrc, query_matches_song};
use super::net::{fetch_json, url_encode};
use super::provider::TrackMeta;

const REFERER: &str = "https://www.kugou.com/";
/// 封面模板里的尺寸占位；480 比接口默认的 240 清晰且体积仍很小
const COVER_SIZE: &str = "480";

/// SMTC 的 SourceAppUserModelId 是否来自酷狗（"KuGou.exe" / "KuGou.KuGouMusic" 等）
pub fn is_source(source_app_id: &str) -> bool {
    source_app_id.to_lowercase().contains("kugou")
}

/// 一次搜索命中的曲目
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// 音频文件 hash，查询歌词的键
    pub hash: String,
    pub title: String,
    pub artist: String,
    /// 秒（接口给的就是秒；歌词接口的 duration 是毫秒，别混）
    pub duration_s: i64,
    pub album: String,
    /// 封面直链，接口没给时为空
    pub cover_url: String,
}

/// 从 song_search_v2 响应里挑最匹配的一条：先按歌手全等，再退回首条做词面相关性校验。
/// 与 163/QQ 同一套思路——宁可没有信息，也别把别的歌的信息安上去。
fn pick_hit(json: &serde_json::Value, title: &str, artist: &str) -> Option<Hit> {
    let lists = json.get("data")?.get("lists")?.as_array()?;
    let query = if artist.is_empty() { title.to_string() } else { format!("{title} {artist}") };

    if !artist.is_empty() {
        for item in lists {
            let hit_artist = item.get("SingerName").and_then(|s| s.as_str()).unwrap_or("");
            if artist_matches(hit_artist, artist) {
                if let Some(hit) = hit_of(item) {
                    return Some(hit);
                }
            }
        }
    }

    let first = hit_of(lists.first()?)?;
    query_matches_song(&query, &first.title).then_some(first)
}

fn hit_of(item: &serde_json::Value) -> Option<Hit> {
    let hash = item.get("FileHash").and_then(|h| h.as_str()).unwrap_or("");
    if hash.is_empty() {
        return None;
    }
    Some(Hit {
        hash: hash.to_string(),
        title: item.get("SongName").and_then(|s| s.as_str()).unwrap_or("").to_string(),
        artist: item.get("SingerName").and_then(|s| s.as_str()).unwrap_or("").to_string(),
        duration_s: item.get("Duration").and_then(int_of).unwrap_or(0),
        album: item.get("AlbumName").and_then(|s| s.as_str()).unwrap_or("").to_string(),
        cover_url: cover_url(item),
    })
}

/// 合唱曲目的 SingerName 是「甲、乙」，逐个比对
fn artist_matches(singer_field: &str, artist: &str) -> bool {
    let want = artist.trim();
    !want.is_empty()
        && singer_field
            .split(['、', '/', '&', ';', ',', '，'])
            .any(|s| s.trim().eq_ignore_ascii_case(want))
}

/// union_cover 是模板串（`http://imge.kugou.com/stdmusic/{size}/日期/名.jpg`）
fn cover_url(item: &serde_json::Value) -> String {
    let raw = item
        .get("trans_param")
        .and_then(|t| t.get("union_cover"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    if raw.is_empty() {
        return String::new();
    }
    // 图床 http/https 都通，统一 https 免得被 WebView 拦混合内容
    raw.replace("http://", "https://").replace("{size}", COVER_SIZE)
}

/// 按标题/歌手搜一次（阻塞）
fn search(title: &str, artist: &str) -> Option<Hit> {
    if title.is_empty() {
        return None;
    }
    let query = if artist.is_empty() { title.to_string() } else { format!("{title} {artist}") };
    let url = format!(
        "https://songsearch.kugou.com/song_search_v2?keyword={}&page=1&pagesize=10&platform=WebFilter&userid=-1&clientver=2000&iscorrection=1&privilege_filter=0",
        url_encode(&query)
    );
    let json = fetch_json(&url, Some(REFERER))?;
    pick_hit(&json, title, artist)
}

/// 按标题/歌手补全专辑、封面与时长（SMTC 只给标题艺术家时用）
pub fn fetch_detail_by_query(title: &str, artist: &str) -> Option<TrackMeta> {
    let hit = search(title, artist)?;
    let meta = TrackMeta {
        title: hit.title,
        artist: hit.artist,
        album: hit.album,
        cover_url: hit.cover_url,
        duration_ms: hit.duration_s.max(0) * 1000,
    };
    // 全空说明这次搜索没带来任何新信息，当作未命中，别占住缓存键
    (!meta.album.is_empty() || !meta.cover_url.is_empty() || meta.duration_ms > 0).then_some(meta)
}

/// 按曲目搜索歌词，供 [`super::lyrics`] 的歌词源链调用。
/// 先看酷狗本地歌词缓存（正在播的那份音频自己的词，毫秒级），没有再走接口搜索。
pub(super) fn provider_fetch(title: &str, artist: &str) -> Option<LyricsData> {
    if let Some(local) = local_lyrics(title, artist) {
        return Some(local);
    }
    let hit = search(title, artist)?;
    fetch_lrc(&hit.hash, hit.duration_s, &hit.title)
}

/// krcs 取候选 → 下载 LRC
fn fetch_lrc(hash: &str, duration_s: i64, title: &str) -> Option<LyricsData> {
    let url = format!(
        "https://krcs.kugou.com/search?ver=1&man=yes&client=mobi&keyword=&duration={}&hash={}",
        duration_s.max(0),
        url_encode(hash)
    );
    let json = fetch_json(&url, Some(REFERER))?;
    let candidates = json.get("candidates")?.as_array()?;
    let candidate = pick_candidate(candidates, title)?;
    // 歌词候选的 id 接口给的是字符串（同一份数据里数字与字符串混用，两种都得认）
    let id = candidate.get("id").and_then(id_of)?;
    let accesskey = candidate.get("accesskey").and_then(|k| k.as_str())?;
    // 候选自带毫秒级时长；缺了就用搜索给到的秒数
    let duration_ms = candidate
        .get("duration")
        .and_then(int_of)
        .filter(|d| *d > 0)
        .unwrap_or(duration_s.max(0) * 1000);

    let url = format!(
        "https://lyrics.kugou.com/download?ver=1&client=pc&id={id}&accesskey={}&fmt=lrc&charset=utf8",
        url_encode(accesskey)
    );
    let json = fetch_json(&url, Some(REFERER))?;
    parse_lyric_response(&json, duration_ms)
}

/// 一个 hash 常有多个用户上传的歌词，挑歌名对得上的；都不对就按接口给的相关度取第一条
fn pick_candidate<'a>(
    candidates: &'a [serde_json::Value],
    title: &str,
) -> Option<&'a serde_json::Value> {
    candidates
        .iter()
        .find(|c| {
            c.get("song")
                .and_then(|s| s.as_str())
                .is_some_and(|song| query_matches_song(title, song))
        })
        .or_else(|| candidates.first())
}

/// 下载响应 → 歌词：`content` 是 base64 的 LRC（`fmt=lrc` 时服务端已转码）
fn parse_lyric_response(json: &serde_json::Value, duration_ms: i64) -> Option<LyricsData> {
    if json.get("status").and_then(int_of) != Some(200) {
        return None;
    }
    let content = json.get("content").and_then(|c| c.as_str()).unwrap_or("");
    if content.is_empty() {
        return None;
    }
    let lrc = String::from_utf8_lossy(&b64::decode(content)).into_owned();
    Some(LyricsData { lines: parse_lrc(&lrc), duration_ms })
}

/// 数字字段：酷狗同一份数据里 int 与数字字符串混用，两种都要认
fn int_of(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// 标识字段：同样是数字/字符串混用（歌词候选的 id 就是字符串）
fn id_of(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

// ---- 酷狗自己的落盘状态（进度 / 当前曲目 / 歌词目录）----

/// 一次精确进度采样：`position_ms` 是 `at_epoch_ms` 时刻的真实进度
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionSample {
    pub position_ms: i64,
    pub at_epoch_ms: i64,
}

/// `%APPDATA%\KuGou8\KuGou.ini` 里我们要的那几项
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IniState {
    /// 播放进度（毫秒）
    pub position_ms: Option<i64>,
    /// 正在播放的曲目显示名，形如「周杰伦 - 手写的从前」
    pub playing_title: Option<String>,
    /// 下载目录（歌词缓存在其下 `Lyric\`）
    pub download_path: Option<String>,
    /// 本份 ini 的写入时刻（= 进度对应的墙钟时刻）
    pub at_epoch_ms: i64,
}

const INI_NAME: &str = "KuGou.ini";

/// 读酷狗自己落的播放状态。
///
/// 酷狗的媒体会话不给时间轴，但它每 30 秒把当前进度写进 `%APPDATA%\KuGou8\KuGou.ini`
/// （UTF-16LE）的 `LastPlayingSongPos`（毫秒），切歌时归零，而文件 mtime 就是这次采样的
/// 墙钟时刻。于是「采样值 + (now - mtime)」能把进度还原到一秒以内——靠读用户自己机器上的
/// 文件拿到真进度，不必注入酷狗进程。同一份 ini 里还有当前曲目名与下载目录。
///
/// 30 秒才落一次盘，所以按 mtime 缓存：同一份文件反复读也只解析一次，调用方可以随便频繁调用。
pub fn ini_state() -> Option<IniState> {
    static CACHE: Mutex<Option<(i64, IniState)>> = Mutex::new(None);

    let path = config_dir()?.join(INI_NAME);
    let at = std::fs::metadata(&path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((cached, state)) = cache.as_ref() {
            if *cached == at {
                return Some(state.clone());
            }
        }
    }
    // 酷狗可能正在写这个文件（读到半截），解析不到就当这次没有状态，下次再试
    let bytes = std::fs::read(&path).ok()?;
    let text = decode_ini(&bytes);
    if parse_playing_pos(&text).is_none() && parse_value(&text, "LastPlayingTitleName").is_none() {
        return None;
    }
    let state = IniState {
        position_ms: parse_playing_pos(&text),
        playing_title: parse_value(&text, "LastPlayingTitleName"),
        download_path: parse_value(&text, "DownloadPath"),
        at_epoch_ms: at,
    };
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((at, state.clone()));
    Some(state)
}

/// 只取进度（[`ini_state`] 的薄封装）
pub fn position_sample() -> Option<PositionSample> {
    let state = ini_state()?;
    Some(PositionSample { position_ms: state.position_ms?, at_epoch_ms: state.at_epoch_ms })
}

/// ini 是 UTF-16LE 带 BOM；没有 BOM 时按 UTF-8 读，读不动也不 panic
fn decode_ini(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// 取 `key=value` 的值（去掉两端空白与 BOM；空值当没有）
fn parse_value(ini: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    let value = ini
        .lines()
        .find_map(|line| line.trim_start_matches('\u{feff}').trim().strip_prefix(&prefix))?
        .trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// 取 `LastPlayingSongPos=<毫秒>`；非数字、负数都当没有
fn parse_playing_pos(ini: &str) -> Option<i64> {
    parse_value(ini, "LastPlayingSongPos")?.parse::<i64>().ok().filter(|value| *value >= 0)
}

// ---- 本地歌词缓存（.krc）----

/// KRC 的固定 16 字节异或密钥
const KRC_KEY: [u8; 16] = [
    0x40, 0x47, 0x61, 0x77, 0x5E, 0x32, 0x74, 0x47, 0x51, 0x36, 0x31, 0x2D, 0xCE, 0xD2, 0x6E, 0x69,
];

/// 正在播放曲目的候选显示名，形如「周杰伦 - 手写的从前」。
///
/// 主窗口标题是「显示名 - 酷狗音乐」，随切歌**立即**变化，比 ini（30 秒落一次盘）及时；
/// 但酷狗的桌面歌词等窗口也带同样的后缀，所以这里给全部候选，由调用方按当前曲目挑。
/// ini 里的 `LastPlayingTitleName` 作为兜底（窗口都不在时）。
pub fn display_name_candidates() -> Vec<String> {
    let mut out = window_titles();
    if let Some(from_ini) = ini_state().and_then(|s| s.playing_title) {
        if !out.contains(&from_ini) {
            out.push(from_ini);
        }
    }
    out
}

/// 本地歌词缓存目录：`{DownloadPath}\Lyric`（酷狗把歌词按
/// 「显示名-音频hash-…​.krc」存在这里，也就是正在播的那份音频自己的歌词）
fn lyric_dir() -> Option<PathBuf> {
    let download = ini_state()?.download_path?;
    Some(Path::new(&download).join("Lyric"))
}

/// 用酷狗本地的 .krc 歌词（存在且与当前曲目对得上时）。
///
/// 比搜索回来的 LRC 强的地方：文件名里带着**正在播放那份音频的 hash**，
/// 所以不会串到别的版本（同名的 Live/DJ/和声版时长能差十几秒），
/// 而且时间轴是毫秒级、读本地文件即时可用。
pub(super) fn local_lyrics(track_title: &str, artist: &str) -> Option<LyricsData> {
    // 只有标题对得上的那个窗口/条目才是当前这首（桌面歌词窗口、上一首的残留都要排除）
    let display = display_name_candidates()
        .into_iter()
        .find(|candidate| titles_match(candidate, track_title, artist))?;
    let dir = lyric_dir()?;
    let path = newest_matching(&dir, &format!("{display}-"))?;
    let text = decrypt_krc(&std::fs::read(&path).ok()?)?;
    let lyrics = parse_krc(&text);
    (!lyrics.lines.is_empty()).then_some(lyrics)
}

/// 目录里以 `prefix` 开头、`.krc` 结尾的最新一个（同一首歌可能存过多个版本）
fn newest_matching(dir: &Path, prefix: &str) -> Option<PathBuf> {
    newest_matching_where(dir, |name| name.starts_with(prefix) && name.ends_with(".krc"))
}

/// 目录里符合条件的最新一个
fn newest_matching_where(dir: &Path, keep: impl Fn(&str) -> bool) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !keep(&name) {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if best.as_ref().is_none_or(|(t, _)| modified > *t) {
            best = Some((modified, entry.path()));
        }
    }
    best.map(|(_, path)| path)
}

/// 按音频 hash 取本地 .krc（文件名形如「艺术家 - 标题-<hash>-…​.krc」）。
/// 这是最精确的一条路：hash 就是正在播放那份音频本身，不存在版本歧义。
fn local_lyrics_by_hash(hash: &str) -> Option<LyricsData> {
    if hash.is_empty() {
        return None;
    }
    let dir = lyric_dir()?;
    let needle = format!("-{hash}-");
    let path = newest_matching_where(&dir, |name| name.contains(&needle))?;
    let text = decrypt_krc(&std::fs::read(&path).ok()?)?;
    let lyrics = parse_krc(&text);
    (!lyrics.lines.is_empty()).then_some(lyrics)
}

/// CDP 路径的取词：先按 hash 找本地缓存，再去接口按 hash 取。
/// 两条都不需要标题搜索，所以不会串到同名别的版本。
pub(super) fn provider_fetch_by_hash(hash: &str, duration_ms: i64, title: &str) -> Option<LyricsData> {
    if let Some(local) = local_lyrics_by_hash(hash) {
        return Some(local);
    }
    if hash.is_empty() {
        return None;
    }
    // krcs 的 duration 参数用秒
    fetch_lrc(hash, duration_ms.max(0) / 1000, title)
}

/// 「周杰伦 - 手写的从前」是否就是这一首：标题词面必须对得上，歌手只作额外确认。
/// 只看歌手是不够的——同一歌手的任何一首歌都会蒙对，那就等于没判。
fn titles_match(display: &str, track_title: &str, artist: &str) -> bool {
    if track_title.is_empty() || !query_matches_song(track_title, display) {
        return false;
    }
    artist.is_empty() || display.to_lowercase().contains(&artist.to_lowercase())
}

/// 解 KRC：跳过 4 字节 magic，整段按固定密钥异或，再找 zlib 流解压
fn decrypt_krc(data: &[u8]) -> Option<String> {
    if data.len() < 8 {
        return None;
    }
    let xored: Vec<u8> = data[4..].iter().enumerate().map(|(i, b)| b ^ KRC_KEY[i % 16]).collect();
    // 正常是紧跟着 zlib 流，个别版本前面还有几个字节，往前找一下头
    for start in 0..xored.len().min(64) {
        if !matches!(xored[start], 0x78) {
            continue;
        }
        let mut out = Vec::new();
        if flate2::read::ZlibDecoder::new(&xored[start..]).read_to_end(&mut out).is_ok() && !out.is_empty() {
            return Some(String::from_utf8_lossy(&out).into_owned());
        }
    }
    None
}

/// 解析 KRC 正文：`[行起始,行长]<字偏移,字长,0>字…`，逐字标签拼回整行
fn parse_krc(text: &str) -> LyricsData {
    let mut lines = Vec::new();
    let mut coverage_ms = 0;
    for raw in text.lines() {
        let raw = raw.trim_start_matches('\u{feff}');
        let Some(rest) = raw.strip_prefix('[') else { continue };
        let Some((head, body)) = rest.split_once(']') else { continue };
        let Some((start, duration)) = head.split_once(',') else { continue }; // [ti:..] 之类的元数据
        let (Ok(start_ms), Ok(duration_ms)) = (start.trim().parse::<i64>(), duration.trim().parse::<i64>())
        else {
            continue;
        };
        let text = strip_word_tags(body);
        if text.is_empty() {
            continue;
        }
        coverage_ms = coverage_ms.max(start_ms + duration_ms.max(0));
        lines.push(LyricLine { time_ms: start_ms, text });
    }
    lines.sort_by_key(|l| l.time_ms);
    LyricsData { lines, duration_ms: coverage_ms }
}

/// 去掉 `<偏移,时长,0>` 逐字标签，把一行拼回整句
fn strip_word_tags(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c == '<' {
            for inner in chars.by_ref() {
                if inner == '>' {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out.trim().to_string()
}

// ---- 宿主检测（设置页的连接状态）----

/// 酷狗接入状态。每一步都给出用户能直接执行的下一步。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum KugouStatus {
    /// 没有安装痕迹
    NotDetected,
    /// 装了但进程没在跑
    NotRunning,
    /// 进程在跑，但系统媒体会话里没有它：多半是没开「支持系统播放控件」
    NeedsSystemControls,
    /// 当前系统媒体会话来自酷狗（或 CDP 已连上）
    Connected,
    /// 进程内增强：libcef.dll 还没打补丁（需要授权修一次）
    NeedsPatch,
    /// 进程内增强：补丁已就位，但酷狗还在用旧 DLL，需要重启它
    NeedsRestart,
    /// 进程内增强：补丁已就位、酷狗在跑，CDP 正在连接
    Connecting,
}

/// 有会话即已连接，否则看进程与安装痕迹
pub fn status(session_app_id: Option<&str>) -> KugouStatus {
    classify(session_app_id.is_some_and(is_source), is_running(), installed())
}

fn classify(connected: bool, running: bool, installed: bool) -> KugouStatus {
    if connected {
        KugouStatus::Connected
    } else if running {
        KugouStatus::NeedsSystemControls
    } else if installed {
        KugouStatus::NotRunning
    } else {
        KugouStatus::NotDetected
    }
}

/// 酷狗进程在不在（KuGou.exe；升级/多开时还有 KuGou_1.exe）
pub(super) fn is_running() -> bool {
    !enum_kugou_pids().is_empty()
}

/// 所有酷狗进程的 pid（CEF 子进程同名，取集合用于认窗口归属）
pub(super) fn enum_kugou_pids() -> Vec<u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let mut pids = Vec::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return pids;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                if is_kugou_exe(&wide_nul_to_string(&entry.szExeFile)) {
                    pids.push(entry.th32ProcessID);
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    pids
}

/// 酷狗各窗口标题里的曲目显示名（去掉「 - 酷狗音乐」后缀）。
/// 主窗口与桌面歌词窗口都带这个后缀，所以返回全部，按当前曲目挑。
fn window_titles() -> Vec<String> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
    };

    const SUFFIX: &str = " - 酷狗音乐";

    struct Ctx {
        pids: Vec<u32>,
        titles: Vec<String>,
    }

    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // 回调里让 panic 展开到宿主是未定义行为，逐项判空即可，不做会 panic 的事
        let ctx = unsafe { &mut *(lparam.0 as *mut Ctx) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if !ctx.pids.contains(&pid) {
            return BOOL(1);
        }
        let len = unsafe { GetWindowTextLengthW(hwnd) };
        if len <= 0 {
            return BOOL(1);
        }
        let mut buf = vec![0u16; len as usize + 1];
        let written = unsafe { GetWindowTextW(hwnd, &mut buf) };
        if written <= 0 {
            return BOOL(1);
        }
        let title = String::from_utf16_lossy(&buf[..written as usize]);
        if let Some(name) = title.strip_suffix(SUFFIX) {
            let name = name.trim();
            if !name.is_empty() {
                ctx.titles.push(name.to_string());
            }
        }
        BOOL(1)
    }

    let pids = enum_kugou_pids();
    if pids.is_empty() {
        return Vec::new();
    }
    let mut ctx = Ctx { pids, titles: Vec::new() };
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    ctx.titles
}

/// 酷狗播放器进程名：主程序 KuGou.exe（升级/多开时并存 KuGou_1.exe）。
/// 按前缀认而不是白名单，酷狗换过好几次进程名；屏保 kgscrsaver.exe 之类不以此开头，不会误判
fn is_kugou_exe(file_name: &str) -> bool {
    let name = file_name.to_lowercase();
    name.starts_with("kugou") && name.ends_with(".exe")
}

/// 装没装：常见安装目录 → 卸载项 → 配置目录（跑过一次就会留下）
fn installed() -> bool {
    default_dirs().iter().any(|d| valid_install_dir(d))
        || uninstall_dir().is_some()
        || config_dir().is_some()
}

/// 安装目录特征：根目录有 KuGou.exe，或版本子目录里有 libcef.dll
fn valid_install_dir(dir: &Path) -> bool {
    if dir.join("KuGou.exe").exists() {
        return true;
    }
    std::fs::read_dir(dir)
        .map(|entries| entries.flatten().any(|e| e.path().join("libcef.dll").exists()))
        .unwrap_or(false)
}

fn default_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for var in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
        if let Ok(base) = std::env::var(var) {
            out.push(PathBuf::from(base).join("KuGou").join("KGMusic"));
        }
    }
    out
}

/// 卸载项：DisplayName 认「酷狗 / kugou」；InstallLocation 常为空，退回卸载程序所在目录
fn uninstall_dir() -> Option<PathBuf> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    const UNINSTALL: &[(&str, &str)] = &[
        ("HKLM", r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall"),
        ("HKLM", r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall"),
        ("HKCU", r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall"),
    ];
    for (hive, path) in UNINSTALL {
        let root = match *hive {
            "HKLM" => RegKey::predef(HKEY_LOCAL_MACHINE),
            _ => RegKey::predef(HKEY_CURRENT_USER),
        };
        let Ok(uninstall) = root.open_subkey(path) else {
            continue;
        };
        for name in uninstall.enum_keys().flatten() {
            let Ok(entry) = uninstall.open_subkey(&name) else {
                continue;
            };
            let display: String = entry.get_value("DisplayName").unwrap_or_default();
            if !(display.contains("酷狗") || display.to_lowercase().contains("kugou")) {
                continue;
            }
            let loc: String = entry.get_value("InstallLocation").unwrap_or_default();
            let dir = PathBuf::from(loc.trim().trim_matches('"'));
            if !loc.trim().is_empty() && valid_install_dir(&dir) {
                return Some(dir);
            }
            let uninst: String = entry.get_value("UninstallString").unwrap_or_default();
            if let Some(parent) = Path::new(uninst.trim().trim_matches('"')).parent() {
                if valid_install_dir(parent) {
                    return Some(parent.to_path_buf());
                }
            }
        }
    }
    None
}

/// 配置目录 %APPDATA%\KuGou8：注册表与默认目录都改过时还能认出来
fn config_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("APPDATA")?).join("KuGou8");
    dir.exists().then_some(dir)
}

fn wide_nul_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实响应裁剪而成（只留用到的字段）
    const SEARCH_FIXTURE: &str = r#"{
        "status": 1,
        "data": {
            "total": 2,
            "lists": [
                {
                    "SongName": "晴天",
                    "SingerName": "周杰伦",
                    "FileHash": "B3A52A7A958BF0AED0EBFBA2E9A818B7",
                    "AlbumName": "叶惠美",
                    "Duration": 269,
                    "trans_param": {
                        "union_cover": "http://imge.kugou.com/stdmusic/{size}/20230920/20230920142503632013.jpg"
                    }
                },
                {
                    "SongName": "晴天 (Live)",
                    "SingerName": "周杰伦、五月天",
                    "FileHash": "EB2CD30DD7258994F008E6DBDCE79024",
                    "AlbumName": "演唱会",
                    "Duration": 299,
                    "trans_param": {}
                }
            ]
        }
    }"#;

    fn fixture() -> serde_json::Value {
        serde_json::from_str(SEARCH_FIXTURE).expect("fixture 是合法 JSON")
    }

    #[test]
    fn only_kugou_sources_are_treated_as_kugou() {
        assert!(is_source("KuGou.exe"));
        assert!(is_source("kugou.exe"));
        assert!(is_source("KuGou.KuGouMusic"), "AUMID 形式也要认出来");
        assert!(!is_source("cloudmusic.exe"));
        assert!(!is_source(""));
    }

    #[test]
    fn hit_prefers_exact_artist_then_falls_back_to_first() {
        let hit = pick_hit(&fixture(), "晴天", "周杰伦").expect("应命中");
        assert_eq!(hit.hash, "B3A52A7A958BF0AED0EBFBA2E9A818B7");
        assert_eq!(hit.album, "叶惠美");
        assert_eq!(hit.duration_s, 269);
        assert_eq!(
            hit.cover_url,
            "https://imge.kugou.com/stdmusic/480/20230920/20230920142503632013.jpg",
            "封面模板要换成大图并统一 https"
        );

        let hit = pick_hit(&fixture(), "晴天", "五月天").expect("合唱条目里含五月天，应命中");
        assert_eq!(hit.hash, "EB2CD30DD7258994F008E6DBDCE79024");
        assert!(hit.cover_url.is_empty(), "没有 union_cover 就别编一个封面出来");
    }

    #[test]
    fn irrelevant_results_are_rejected() {
        // 歌名对不上：宁可没有信息，也不能把别的歌的信息安上去
        assert!(pick_hit(&fixture(), "雨天", "").is_none());
        assert!(pick_hit(&fixture(), "稻香", "").is_none());
        // 歌名能对上时（无歌手）走首条兜底
        assert_eq!(pick_hit(&fixture(), "晴天", "").map(|h| h.hash).as_deref(), Some("B3A52A7A958BF0AED0EBFBA2E9A818B7"));
    }

    #[test]
    fn empty_response_is_not_a_hit() {
        let empty = serde_json::json!({ "status": 0, "data": { "lists": [] } });
        assert!(pick_hit(&empty, "晴天", "周杰伦").is_none());
        assert!(pick_hit(&serde_json::json!({}), "晴天", "周杰伦").is_none());
        // FileHash 缺失的条目不能用
        let no_hash = serde_json::json!({ "data": { "lists": [{ "SongName": "晴天" }] } });
        assert!(pick_hit(&no_hash, "晴天", "").is_none());
    }

    #[test]
    fn candidate_prefers_matching_song_over_junk_then_first() {
        let cands = vec![
            serde_json::json!({ "song": "完全无关的歌", "id": "1", "accesskey": "A" }),
            serde_json::json!({ "song": "晴天", "id": "274944371", "accesskey": "B" }),
        ];
        let picked = pick_candidate(&cands, "晴天").expect("应挑到候选");
        assert_eq!(picked.get("id").and_then(id_of).as_deref(), Some("274944371"), "应跳过歌名对不上的条目");

        let none_match = vec![serde_json::json!({ "song": "完全无关", "id": "3", "accesskey": "C" })];
        let picked = pick_candidate(&none_match, "晴天").expect("都不匹配时退回第一条");
        assert_eq!(picked.get("id").and_then(id_of).as_deref(), Some("3"));
        assert!(pick_candidate(&[], "晴天").is_none());
    }

    #[test]
    fn numeric_and_string_fields_are_both_read() {        // 真实响应里歌词候选的 id 是字符串、duration 是数字；两种写法都得认，
        // 否则 id 读成 None 会让整条歌词链无声中断
        assert_eq!(id_of(&serde_json::json!("274944371")).as_deref(), Some("274944371"));
        assert_eq!(id_of(&serde_json::json!(274944371)).as_deref(), Some("274944371"));
        assert_eq!(id_of(&serde_json::json!("")), None);
        assert_eq!(id_of(&serde_json::json!(null)), None);

        assert_eq!(int_of(&serde_json::json!(269792)), Some(269_792));
        assert_eq!(int_of(&serde_json::json!("269792")), Some(269_792));
        assert_eq!(int_of(&serde_json::json!("abc")), None);
        assert_eq!(int_of(&serde_json::json!(null)), None);
    }

    #[test]
    fn lyric_response_decodes_base64_lrc() {
        let lrc = "[ti:晴天]\n[00:01.00]第一句\n[00:02.50]第二句";
        let json = serde_json::json!({ "status": 200, "content": b64::encode(lrc.as_bytes()) });
        let data = parse_lyric_response(&json, 269_000).expect("应解出歌词");
        assert_eq!(data.lines.len(), 2, "元数据行不该算歌词行");
        assert_eq!(data.lines[0].time_ms, 1000);
        assert_eq!(data.lines[1].text, "第二句");
        assert_eq!(data.duration_ms, 269_000, "接口没给时长时用调用方传进来的");
    }

    #[test]
    fn lyric_response_rejects_failure_and_empty_payload() {
        assert!(parse_lyric_response(&serde_json::json!({ "status": 0 }), 1).is_none());
        assert!(parse_lyric_response(&serde_json::json!({ "status": 200, "content": "" }), 1).is_none());
    }

    #[test]
    fn playing_pos_is_parsed_from_utf16_ini() {
        let text = "[PlaybackState]\r\nLastPlayingSongList=0\r\nLastPlayingSongPos=30354\r\nLastPlayingSongTable=6\r\n";
        assert_eq!(parse_playing_pos(text), Some(30_354));

        // 真机上的 ini 是 UTF-16LE 带 BOM
        let mut bytes = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(parse_playing_pos(&decode_ini(&bytes)), Some(30_354));
        assert_eq!(parse_playing_pos(&decode_ini(text.as_bytes())), Some(30_354), "无 BOM 时按 UTF-8 也要能读");
    }

    #[test]
    fn missing_or_dirty_position_is_not_a_sample() {
        assert_eq!(parse_playing_pos("[PlaybackState]\r\n"), None, "没有该键时当没有采样");
        assert_eq!(parse_playing_pos("LastPlayingSongPos=abc"), None);
        assert_eq!(parse_playing_pos("LastPlayingSongPos="), None);
        assert_eq!(parse_playing_pos("LastPlayingSongPos=-5"), None, "负进度是脏数据");
        assert_eq!(parse_playing_pos("LastPlayingSongPos= 1234 "), Some(1234), "值两端空格要容忍");
        assert_eq!(
            parse_playing_pos("LastPlayingSongPosition=999"),
            None,
            "同为 LastPlayingSong 前缀的别的键不能误认"
        );
    }

    #[test]
    fn ini_values_read_titles_and_paths() {
        let ini = "\u{feff}[PlaybackState]\r\nLastPlayingTitleName=周杰伦 - 手写的从前\r\n\
                   [DownloadConfigSection]\r\nDownloadPath=C:\\KuGou\r\nEmpty=\r\n";
        assert_eq!(parse_value(ini, "LastPlayingTitleName").as_deref(), Some("周杰伦 - 手写的从前"));
        assert_eq!(parse_value(ini, "DownloadPath").as_deref(), Some("C:\\KuGou"));
        assert_eq!(parse_value(ini, "Empty"), None, "空值当没有");
        assert_eq!(parse_value(ini, "Missing"), None);
    }

    #[test]
    fn krc_decrypts_and_parses_word_level_timing() {
        use std::io::Write;
        // 造一个真的 KRC：4 字节 magic + 异或后的 zlib 流
        let raw = "[ti:手写的从前]\n[0,5000]<0,500,0>第<500,500,0>一<1000,900,0>句\n\
                   [5000,4200]<0,400,0>第<400,600,0>二<1000,800,0>句\n";
        let mut payload = Vec::new();
        payload.extend_from_slice(b"krc1");
        let zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        let mut enc = zlib;
        enc.write_all(raw.as_bytes()).expect("压缩测试数据");
        let compressed = enc.finish().expect("压缩测试数据");
        payload.extend(compressed.iter().enumerate().map(|(i, b)| b ^ KRC_KEY[i % 16]));

        let text = decrypt_krc(&payload).expect("应能解出 KRC");
        let lyrics = parse_krc(&text);
        assert_eq!(lyrics.lines.len(), 2, "元数据行不该算歌词行");
        assert_eq!(lyrics.lines[0].text, "第一句", "逐字标签要去掉并拼回整句");
        assert_eq!(lyrics.lines[0].time_ms, 0);
        assert_eq!(lyrics.lines[1].time_ms, 5_000);
        assert_eq!(lyrics.duration_ms, 9_200, "覆盖时长取末行结束时刻");
    }

    #[test]
    fn krc_decrypt_rejects_junk() {
        assert!(decrypt_krc(b"").is_none());
        assert!(decrypt_krc(b"krc1\x00\x00\x00\x00").is_none(), "不是 zlib 流就当解不开");
    }

    #[test]
    fn only_the_same_song_may_use_the_local_cache() {
        // 窗口标题是「艺术家 - 标题」，SMTC 只给标题：要对得上才用本地词
        assert!(titles_match("周杰伦 - 手写的从前", "手写的从前", "周杰伦"));
        assert!(titles_match("风华音纪、指尖笑 - 夜良人", "夜良人", "风华音纪"));
        // 切歌瞬间 ini/窗口标题还是上一首：不能拿上一首的词
        assert!(!titles_match("周杰伦 - 手写的从前", "晴天", "周杰伦"));
        assert!(!titles_match("周杰伦 - 手写的从前", "", ""));
    }

    #[test]
    fn newest_matching_krc_wins() {
        let dir = std::env::temp_dir().join(format!("ti-krc-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let write = |name: &str, body: &str| std::fs::write(dir.join(name), body).expect("写测试文件");
        write("周杰伦 - 手写的从前-aaaa-1-00000000.krc", "old");
        std::thread::sleep(std::time::Duration::from_millis(20));
        write("周杰伦 - 手写的从前-bbbb-2-00000000.krc", "new");
        write("周杰伦 - 晴天-cccc-3-00000000.krc", "other");

        let picked = newest_matching(&dir, "周杰伦 - 手写的从前-").expect("应挑到文件");
        assert_eq!(picked.file_name().unwrap().to_string_lossy(), "周杰伦 - 手写的从前-bbbb-2-00000000.krc");
        assert!(newest_matching(&dir, "不存在的-").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_covers_all_four_labels() {        assert_eq!(classify(true, true, true), KugouStatus::Connected);
        assert_eq!(classify(true, false, true), KugouStatus::Connected, "会话在就算进程没扫到也已连接");
        assert_eq!(classify(false, true, true), KugouStatus::NeedsSystemControls);
        assert_eq!(classify(false, false, true), KugouStatus::NotRunning);
        assert_eq!(classify(false, false, false), KugouStatus::NotDetected);
    }

    #[test]
    fn exe_name_match_is_case_insensitive_and_versioned() {
        assert!(is_kugou_exe("KuGou.exe"));
        assert!(is_kugou_exe("kugou.exe"));
        assert!(is_kugou_exe("KuGou_1.exe"), "升级后的带下划线副本也算");
        assert!(!is_kugou_exe("kgscrsaver.exe"), "酷狗屏保不是播放器");
        assert!(!is_kugou_exe("cloudmusic.exe"));
        assert!(!is_kugou_exe("kugou"), "没有 .exe 后缀的不是进程名");
    }

    /// 真机检测冒烟（需要装了酷狗的 Windows）：
    /// `cargo test -p island-app kugou -- --ignored --nocapture`
    #[test]
    #[ignore = "需要装了酷狗的 Windows 机器"]
    fn live_host_detection_finds_installed_kugou() {        let (is_running, is_installed) = (is_running(), installed());
        println!("running = {is_running}, installed = {is_installed}, status = {:?}", status(None));
        assert!(is_installed, "本机装了酷狗（注册表卸载项 + Program Files），应能认出来");
        // 进程没跑时不该说「请在酷狗里开启系统播放控件」——那会把人带偏
        assert_eq!(
            status(None),
            if is_running { KugouStatus::NeedsSystemControls } else { KugouStatus::NotRunning }
        );
        assert_eq!(
            status(Some("cloudmusic.exe")),
            status(None),
            "别的播放器的会话不该被当成酷狗已连接"
        );
        assert_eq!(status(Some("KuGou.exe")), KugouStatus::Connected, "酷狗自己的会话即已连接");
    }

    /// 真机进度采样冒烟（酷狗在放歌时）：
    /// `cargo test -p island-app kugou -- --ignored --nocapture`
    #[test]
    #[ignore = "需要装了酷狗且在放歌的 Windows 机器"]
    fn live_position_sample_reads_playing_progress() {
        let first = position_sample().expect("酷狗在放歌时应能从 KuGou.ini 读到进度");
        println!("sample = {first:?}");
        assert!(first.position_ms >= 0);
        // 30 秒才落一次盘：等一轮再读，位置应前进 30 秒左右（切歌则归零）
        std::thread::sleep(std::time::Duration::from_secs(32));
        let second = position_sample().expect("第二次采样也该读得到");
        println!("sample = {second:?}");
        let advanced = second.position_ms - first.position_ms;
        let wall = second.at_epoch_ms - first.at_epoch_ms;
        if advanced > 0 {
            // 没切歌：进度前进的速度应与墙钟一致（30 秒落一次盘，容 3 秒取整/漂移）
            println!("advanced = {advanced}ms, wall = {wall}ms");
            assert!(advanced > 0 && (advanced - wall).abs() < 3_000, "进度应随墙钟前进: {advanced}ms vs {wall}ms");
        } else {
            println!("两次采样之间切歌或重播了：{advanced}ms");
        }
    }

    /// 真机本地歌词冒烟（酷狗在放歌时）：
    /// `cargo test -p island-app kugou -- --ignored --nocapture`
    #[test]
    #[ignore = "需要装了酷狗且在放歌的 Windows 机器"]
    fn live_local_krc_lyrics_match_the_playing_audio() {
        let state = ini_state().expect("应能读到 KuGou.ini 状态");
        // ini 里记着酷狗自己的曲目显示名（「艺术家 - 标题」），用它当基准
        let playing = state.playing_title.clone().expect("ini 应记着正在播放的曲目");
        let candidates = display_name_candidates();
        println!("playing = {playing:?}, download = {:?}, pos = {:?}", state.download_path, state.position_ms);
        println!("candidates = {candidates:?}");
        assert!(
            candidates.contains(&playing),
            "窗口标题候选里应该有正在播放的曲目（桌面歌词之类的不算）: {candidates:?}"
        );

        let (artist, title) = playing.split_once(" - ").expect("显示名含分隔符");
        let (artist, title) = (artist.trim(), title.trim());
        // 本地缓存不一定有这一版（酷狗只缓存它取过词的版本）；有就必须解得出来
        let cached = lyric_dir().and_then(|dir| newest_matching(&dir, &format!("{playing}-")));
        println!("cached_file = {cached:?}");
        match cached {
            Some(_) => {
                let lyrics = local_lyrics(title, artist).expect("本地有缓存就必须解出来");
                println!("lines = {}, coverage = {} ms", lyrics.lines.len(), lyrics.duration_ms);
                println!("first = {:?}, last = {:?}", lyrics.lines.first(), lyrics.lines.last());
                assert!(lyrics.lines.len() > 5, "行数不该这么少");
                assert!(
                    lyrics.lines.windows(2).all(|w| w[0].time_ms <= w[1].time_ms),
                    "时间轴应升序"
                );
                assert!(lyrics.duration_ms > 60_000, "覆盖时长应像一首歌");
            }
            None => {
                println!("这一版酷狗自己没缓存歌词，应老实返回 None 让上层去搜");
                assert!(local_lyrics(title, artist).is_none(), "没有缓存时不该硬凑别的版本");
            }
        }

        // 别的歌不能借到这首的本地词
        assert!(
            local_lyrics("完全不相干的歌名", "某歌手").is_none(),
            "对不上的曲目不该用本地缓存"
        );
    }

    /// 真机歌词缓存语料冒烟：从真实的 .krc 里抽一批，都要能解开且时间轴升序
    /// `cargo test -p island-app kugou -- --ignored --nocapture`
    #[test]
    #[ignore = "需要装了酷狗的 Windows 机器"]
    fn live_krc_corpus_decrypts_and_parses() {
        let Some(dir) = lyric_dir() else {
            panic!("应能从 KuGou.ini 的 DownloadPath 找到歌词目录");
        };
        let files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("读歌词目录")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "krc"))
            .collect();
        assert!(files.len() > 10, "歌词缓存里应有一批 .krc，实际 {}", files.len());

        let step = (files.len() / 20).max(1);
        let sampled: Vec<&PathBuf> = files.iter().step_by(step).collect();
        let mut ok = 0;
        for path in &sampled {
            let Some(text) = std::fs::read(path).ok().and_then(|bytes| decrypt_krc(&bytes)) else {
                continue;
            };
            let lyrics = parse_krc(&text);
            if lyrics.lines.is_empty() {
                continue;
            }
            assert!(
                lyrics.lines.windows(2).all(|w| w[0].time_ms <= w[1].time_ms),
                "时间轴应升序: {path:?}"
            );
            assert!(lyrics.lines.iter().all(|l| l.time_ms >= 0), "时间轴不该有负数: {path:?}");
            ok += 1;
        }
        println!("decrypted {ok}/{} sampled krc files", sampled.len());
        assert!(ok * 2 > sampled.len(), "至少一半样本应能解开（ok={ok}/{}）", sampled.len());
    }

    /// 真接口冒烟（需要网络）：
    /// `cargo test -p island-app kugou -- --ignored --nocapture`
    #[test]
    #[ignore = "需要网络"]
    fn live_api_returns_detail_and_lyrics() {
        let detail = fetch_detail_by_query("晴天", "周杰伦").expect("酷狗搜索应有结果");
        println!("detail = {detail:?}");
        assert_eq!(detail.album, "叶惠美");
        assert!((200_000..=300_000).contains(&detail.duration_ms), "时长应来自接口: {detail:?}");

        let lyrics = provider_fetch("晴天", "周杰伦").expect("应取到歌词");
        println!("lines = {}, duration = {}", lyrics.lines.len(), lyrics.duration_ms);
        assert!(lyrics.lines.len() > 5, "歌词行数应不止几行");
        assert!(lyrics.lines.iter().any(|l| l.text.contains("故事的小黄花")), "歌词内容应是晴天");
    }
}
