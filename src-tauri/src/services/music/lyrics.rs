//! 歌词获取：网易云 / QQ 音乐 / 酷狗 API（阻塞 ureq，调用方已 off_thread）。
//! 移植自 electron/main/services/lyrics/*，含 LRC 解析与搜索相关性校验。

use island_core::{LyricLine, LyricsData};

use super::b64;
use super::net::{fetch_json, url_encode};
use super::provider::TrackMeta;

fn norm_eq(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// 标题相关性校验：搜索词与歌名至少要有词面上的交集（防止无关命中）
pub fn query_matches_song(query: &str, song_name: &str) -> bool {
    fn norm(s: &str) -> String {
        s.to_lowercase()
            .chars()
            .filter(|c| {
                !c.is_whitespace()
                    && !matches!(
                        c,
                        '-' | '_' | '(' | ')' | '[' | ']' | '（' | '）' | '【' | '】' | '\'' | '"'
                            | ',' | '.' | '，' | '。' | '!' | '！' | '?' | '？'
                    )
            })
            .collect()
    }
    let q = norm(query);
    let n = norm(song_name);
    if q.is_empty() || n.is_empty() {
        return false;
    }
    q.contains(&n) || n.contains(&q)
}

/// 解析 [mm:ss(.xx)] 时间标签；小数部分按位数解释：2 位是厘秒，3 位是毫秒
fn parse_time_tag(s: &str) -> Option<(i64, usize)> {
    debug_assert!(s.starts_with('['));
    let end = s.as_bytes().iter().position(|&b| b == b']')?;
    let inner = &s[1..end];
    let (mm, rest) = inner.split_once(':')?;
    if mm.is_empty() || mm.len() > 3 || !mm.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (ss, frac_raw) = match rest.split_once(['.', ':']) {
        Some((ss, f)) => {
            // 有小数分隔符就必须有小数位，否则整个不是时间标签（对齐 JS 正则）
            if f.is_empty() {
                return None;
            }
            (ss, f)
        }
        None => (rest, ""),
    };
    if ss.is_empty() || ss.len() > 2 || !ss.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if frac_raw.len() > 3 || !frac_raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let min: i64 = mm.parse().ok()?;
    let sec: i64 = ss.parse().ok()?;
    let frac: i64 = match frac_raw.len() {
        3 => frac_raw.parse().ok()?,
        2 => frac_raw.parse::<i64>().ok()? * 10,
        1 => frac_raw.parse::<i64>().ok()? * 100,
        _ => 0,
    };
    Some(((min * 60 + sec) * 1000 + frac, end + 1))
}

pub fn parse_lrc(lrc: &str) -> Vec<LyricLine> {
    let mut out = Vec::new();
    for raw in lrc.lines() {
        let mut times = Vec::new();
        let mut text = String::new();
        let mut rest = raw;
        while !rest.is_empty() {
            if rest.starts_with('[') {
                if let Some((t, len)) = parse_time_tag(rest) {
                    times.push(t);
                    rest = &rest[len..];
                    continue;
                }
            }
            let ch = rest.chars().next().expect("rest 非空");
            text.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        for &t in &times {
            out.push(LyricLine { time_ms: t, text: text.to_string() });
        }
    }
    out.sort_by_key(|l| l.time_ms);
    out
}

// ---- 网易云 ----

fn search_163(title: &str, artist: &str) -> Option<(i64, i64)> {
    let query = if artist.is_empty() { title.to_string() } else { format!("{title} {artist}") };
    let url = format!(
        "https://music.163.com/api/search/get/web?s={}&type=1&offset=0&total=true&limit=10",
        url_encode(&query)
    );
    let json = fetch_json(&url, None)?;
    let songs = json.get("result")?.get("songs")?.as_array()?;
    if songs.is_empty() {
        return None;
    }

    let id_duration = |s: &serde_json::Value| -> Option<(i64, i64)> {
        Some((
            s.get("id")?.as_i64()?,
            s.get("duration").and_then(|d| d.as_i64()).unwrap_or(0),
        ))
    };

    if !artist.is_empty() {
        for s in songs {
            let artist_hit = s
                .get("artists")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|a| a.get("name").and_then(|n| n.as_str()))
                        .any(|n| norm_eq(n, artist))
                })
                .unwrap_or(false);
            if artist_hit {
                return id_duration(s);
            }
        }
    }
    let first = &songs[0];
    if let Some(name) = first.get("name").and_then(|n| n.as_str()) {
        if !query_matches_song(&query, name) {
            return None;
        }
    }
    id_duration(first)
}

fn fetch_163_inner(title: &str, artist: &str) -> Option<LyricsData> {
    let (id, duration_ms) = search_163(title, artist)?;
    let json = fetch_json(
        &format!("https://music.163.com/api/song/lyric?id={id}&lv=-1&kv=-1&tv=-1"),
        None,
    )?;
    let lrc = json
        .get("lrc")
        .and_then(|l| l.get("lyric"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    Some(LyricsData { lines: parse_lrc(lrc), duration_ms })
}

/// 按 songId 直取，不经搜索；搜索会匹配到 live、翻唱等错误版本
pub fn fetch_163_by_id(song_id: &str) -> Option<LyricsData> {
    let lyric_url = format!("https://music.163.com/api/song/lyric?id={song_id}&lv=-1&kv=-1&tv=-1");
    let detail_url = format!("https://music.163.com/api/song/detail?id={song_id}&ids=%5B{song_id}%5D");
    let detail_thread = std::thread::spawn(move || fetch_json(&detail_url, None));
    let lyric = fetch_json(&lyric_url, None);
    let detail = detail_thread.join().ok().flatten();
    let lrc = lyric
        .as_ref()
        .and_then(|j| j.get("lrc"))
        .and_then(|l| l.get("lyric"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let duration_ms = detail
        .as_ref()
        .and_then(|j| j.get("songs"))
        .and_then(|s| s.as_array())
        .and_then(|a| a.first())
        .and_then(|s| s.get("duration"))
        .and_then(|d| d.as_i64())
        .unwrap_or(0);
    if lrc.is_empty() && duration_ms == 0 {
        return None;
    }
    Some(LyricsData { lines: parse_lrc(lrc), duration_ms })
}

pub fn fetch_163_detail(song_id: &str) -> Option<TrackMeta> {
    let detail = fetch_json(
        &format!("https://music.163.com/api/song/detail?id={song_id}&ids=%5B{song_id}%5D"),
        None,
    )?;
    let song = detail.get("songs")?.as_array()?.first()?;
    let title = song.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let artist = song
        .get("artists")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| a.get("name").and_then(|n| n.as_str()))
                .collect::<Vec<_>>()
                .join(" / ")
        })
        .unwrap_or_default();
    let album = song
        .get("album")
        .and_then(|al| al.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let pic = song
        .get("album")
        .and_then(|al| al.get("picUrl"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let cover_url = if pic.is_empty() { String::new() } else { format!("{pic}?param=512y512") };
    let duration_ms = song.get("duration").and_then(|d| d.as_i64()).unwrap_or(0);
    if title.is_empty() && cover_url.is_empty() && duration_ms == 0 {
        return None;
    }
    Some(TrackMeta { title, artist, album, cover_url, duration_ms })
}

fn provider_163_fetch(title: &str, artist: &str) -> Option<LyricsData> {
    let r = fetch_163_inner(title, artist);
    if r.as_ref().is_some_and(|r| !r.lines.is_empty() || r.duration_ms > 0) {
        return r;
    }
    // 带歌手检索失败则退化为仅曲名检索
    if artist.is_empty() {
        None
    } else {
        fetch_163_inner(title, "")
    }
}

// ---- QQ 音乐 ----

const QQ_REFERER: &str = "https://y.qq.com/";

fn search_qq(title: &str, artist: &str) -> Option<(String, i64)> {
    let query = if artist.is_empty() { title.to_string() } else { format!("{title} {artist}") };
    let url = format!(
        "https://c.y.qq.com/soso/fcgi-bin/client_search_cp?w={}&format=json&n=10&p=1&cr=1&t=0",
        url_encode(&query)
    );
    let json = fetch_json(&url, Some(QQ_REFERER))?;
    let songs = json.get("data")?.get("song")?.get("list")?.as_array()?;
    if songs.is_empty() {
        return None;
    }

    let mid_duration = |s: &serde_json::Value| -> Option<(String, i64)> {
        Some((
            s.get("songmid")?.as_str()?.to_string(),
            s.get("interval").and_then(|i| i.as_i64()).unwrap_or(0) * 1000,
        ))
    };

    if !artist.is_empty() {
        for s in songs {
            let artist_hit = s
                .get("singer")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|a| a.get("name").and_then(|n| n.as_str()))
                        .any(|n| norm_eq(n, artist))
                })
                .unwrap_or(false);
            if artist_hit {
                return mid_duration(s);
            }
        }
    }
    let first = &songs[0];
    if let Some(name) = first.get("songname").and_then(|n| n.as_str()) {
        if !query_matches_song(&query, name) {
            return None;
        }
    }
    mid_duration(first)
}

fn fetch_qq_inner(title: &str, artist: &str) -> Option<LyricsData> {
    let (songmid, duration_ms) = search_qq(title, artist)?;
    let json = fetch_json(
        &format!(
            "https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric_new.fcg?songmid={songmid}&format=json&nobase64=0&g_tk=5381"
        ),
        Some(QQ_REFERER),
    )?;
    let lrc_b64 = json.get("lyric").and_then(|v| v.as_str()).unwrap_or("");
    if lrc_b64.is_empty() {
        return Some(LyricsData { lines: Vec::new(), duration_ms });
    }
    let lrc_bytes = b64::decode(lrc_b64);
    let lrc = String::from_utf8_lossy(&lrc_bytes);
    Some(LyricsData { lines: parse_lrc(&lrc), duration_ms })
}

fn provider_qq_fetch(title: &str, artist: &str) -> Option<LyricsData> {
    let r = fetch_qq_inner(title, artist);
    if r.as_ref().is_some_and(|r| !r.lines.is_empty() || r.duration_ms > 0) {
        return r;
    }
    if artist.is_empty() {
        None
    } else {
        fetch_qq_inner(title, "")
    }
}

/// 歌词源。酷狗曲目先问酷狗（同一个曲库，版本匹配率最高），其余源先网易云后 QQ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Kugou,
    Netease,
    Qq,
}

impl Source {
    fn fetch(self, title: &str, artist: &str) -> Option<LyricsData> {
        match self {
            Source::Kugou => super::kugou::provider_fetch(title, artist),
            Source::Netease => provider_163_fetch(title, artist),
            Source::Qq => provider_qq_fetch(title, artist),
        }
    }
}

/// 网易云 / QQ 的默认顺序
const DEFAULT_SOURCES: &[Source] = &[Source::Netease, Source::Qq];
/// 酷狗曲目：酷狗自己的歌词优先，取不到再退回默认顺序
const KUGOU_SOURCES: &[Source] = &[Source::Kugou, Source::Netease, Source::Qq];

pub fn sources_for(source_app_id: &str) -> &'static [Source] {
    if super::kugou::is_source(source_app_id) {
        KUGOU_SOURCES
    } else {
        DEFAULT_SOURCES
    }
}

/// 按给定顺序搜索歌词：有歌词行即最优，只有时长则记为兜底继续尝试下一个源
pub fn fetch_lyrics_with(title: &str, artist: &str, sources: &[Source]) -> Option<LyricsData> {
    if title.is_empty() {
        return None;
    }
    let mut best: Option<LyricsData> = None;
    for source in sources {
        let Some(r) = source.fetch(title, artist) else { continue };
        if !r.lines.is_empty() {
            return Some(r);
        }
        if best.is_none() && r.duration_ms > 0 {
            best = Some(r);
        }
    }
    best
}

/// 按来源选歌词源顺序（酷狗曲目走酷狗接口）
pub fn fetch_lyrics_for(source_app_id: &str, title: &str, artist: &str) -> Option<LyricsData> {
    fetch_lyrics_with(title, artist, sources_for(source_app_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lrc_extracts_time_and_text() {
        let lines = parse_lrc("[00:12.34]你好世界");
        assert_eq!(lines.len(), 1, "单行单标签应得一行歌词");
        assert_eq!(lines[0].time_ms, 12340, "12.34 秒应为 12340ms，厘秒位数解释有误");
        assert_eq!(lines[0].text, "你好世界");
    }

    #[test]
    fn parse_lrc_handles_multiple_tags_on_one_line() {
        let lines = parse_lrc("[00:01.00][00:30.50]重复行");
        assert_eq!(lines.len(), 2, "一行多时间标签应展开为多行（对唱/重复句）");
        assert_eq!(lines[1].time_ms, 30500, "第二个标签 30.50 秒应为 30500ms");
        assert!(lines.iter().all(|l| l.text == "重复行"), "展开行应共享同一段文本");
    }

    #[test]
    fn parse_lrc_interprets_fraction_by_digit_count() {
        let lines = parse_lrc("[01:02:345]x");
        assert_eq!(lines[0].time_ms, 62345, "3 位小数按毫秒解释：1:02.345 应为 62345ms");
        let lines = parse_lrc("[00:00.5]x");
        assert_eq!(lines[0].time_ms, 500, "1 位小数按十分之一秒解释：0.5s 应为 500ms");
    }

    #[test]
    fn parse_lrc_skips_metadata_and_empty_text() {
        let lines = parse_lrc("[ti:歌名]\n[ar:歌手]\n[00:01.00]\n[00:02.00]有词");
        assert_eq!(lines.len(), 1, "元数据标签与空文本行不应产生歌词行");
        assert_eq!(lines[0].text, "有词");
    }

    #[test]
    fn parse_lrc_sorts_lines_by_time() {
        let lines = parse_lrc("[00:30.00]后\n[00:01.00]先");
        assert_eq!(lines[0].text, "先", "歌词行应按时间升序，错了当前行匹配会乱");
    }

    #[test]
    fn query_matches_song_requires_lexical_overlap() {
        assert!(query_matches_song("晴天 周杰伦", "晴天"), "搜索词包含歌名应判定相关");
        assert!(query_matches_song("晴天(Live)", "晴天 (Live)"), "括号与空白差异不应影响判定");
        assert!(!query_matches_song("晴天", "雨天"), "词面无交集应判定无关，防止错误命中");
    }

    #[test]
    fn kugou_tracks_ask_kugou_first_then_fall_back() {
        let kugou = sources_for("KuGou.exe");
        assert_eq!(kugou.first(), Some(&Source::Kugou), "酷狗曲目应先用酷狗自己的歌词");
        assert!(kugou.contains(&Source::Netease) && kugou.contains(&Source::Qq), "酷狗找不到仍要退回网易云/QQ");
        assert_eq!(
            sources_for("cloudmusic.exe"),
            &[Source::Netease, Source::Qq],
            "其它播放器不受酷狗影响，顺序保持网易云后 QQ"
        );
    }

    #[test]
    fn empty_title_never_hits_any_source() {
        assert!(fetch_lyrics_with("", "周杰伦", &[Source::Kugou]).is_none(), "空标题不该发起搜索");
    }
}
