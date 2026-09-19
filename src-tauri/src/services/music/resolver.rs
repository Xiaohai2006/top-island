//! 把 provider 的原始状态补全成可展示的曲目信息（元数据、歌词、封面），资源就绪时通知推送线程。
//! 每种资源一个 `Slot`：键、值、正在抓的键。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use island_core::LyricsData;

use super::b64;
use super::hash::content_hash;
use super::kugou;
use super::lyrics as api;
use super::media_sources;
use super::provider::{MusicProvider, ProviderState, TrackMeta};

/// CDP 源给的 songId 形如 `kugou:<音频hash>`：见到它就按 hash 精确取词（不经搜索）
const KUGOU_SONG_PREFIX: &str = "kugou:";

/// meta 槽要抓什么：有平台 songId 的按 ID 取（网易云 bridge）；酷狗没有 songId，按标题/艺术家
/// 查酷狗接口；其余源不抓——SMTC 的标题常常只是网页标题，照它搜出来的东西比没有更糟。
#[derive(Debug, PartialEq)]
enum MetaTask {
    SongId(String),
    Kugou { title: String, artist: String },
}

impl MetaTask {
    fn fetch(self) -> Option<TrackMeta> {
        match self {
            MetaTask::SongId(id) => api::fetch_163_detail(&id),
            MetaTask::Kugou { title, artist } => kugou::fetch_detail_by_query(&title, &artist),
        }
    }
}

/// 返回 (缓存键, 任务)。键只作切歌判定的身份，不参与分派，标题里有什么字符都不影响
fn meta_task_for(
    song_id: Option<&str>,
    source_app_id: &str,
    title: &str,
    artist: &str,
) -> Option<(String, MetaTask)> {
    let Some(id) = song_id else {
        return (kugou::is_source(source_app_id) && !title.is_empty()).then(|| {
            let key = format!("kugou\u{1f}{title}\u{1f}{artist}");
            (key, MetaTask::Kugou { title: title.to_string(), artist: artist.to_string() })
        });
    };
    // CDP 源已经带来精确时长与封面，不需要再按标题搜一遍（搜了反而可能配错版本）
    if id.starts_with(KUGOU_SONG_PREFIX) {
        return None;
    }
    Some((id.to_string(), MetaTask::SongId(id.to_string())))
}

#[derive(Debug)]
struct Slot<T> {
    /// 最近决定要抓的键，也是切歌检测水位
    key: String,
    value: Option<(String, T)>,
    fetching: Option<String>,
}

// 派生 Default 会给 T 加 Default 约束
impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self { key: String::new(), value: None, fetching: None }
    }
}

impl<T> Slot<T> {
    fn get(&self, key: &str) -> Option<&T> {
        self.value.as_ref().filter(|(k, _)| k == key).map(|(_, v)| v)
    }
}

#[derive(Debug)]
pub struct Artwork {
    pub hash: String,
    pub data_url: String,
}

#[derive(Debug, Default)]
struct Caches {
    meta: Slot<TrackMeta>,
    lyrics: Slot<LyricsData>,
    artwork: Slot<Artwork>,
}

#[derive(Debug, Default)]
pub struct Resolved {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub artwork_url: Option<String>,
    pub artwork_hash: Option<String>,
    pub duration_ms: Option<i64>,
    pub lyrics_id: Option<String>,
}

pub struct Resolver {
    caches: Mutex<Caches>,
    on_ready: Arc<dyn Fn() + Send + Sync>,
}

impl std::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Resolver").finish_non_exhaustive()
    }
}

impl Resolver {
    pub fn new(on_ready: impl Fn() + Send + Sync + 'static) -> Self {
        Self { caches: Mutex::new(Caches::default()), on_ready: Arc::new(on_ready) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Caches> {
        self.caches.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn reset(&self) {
        let mut c = self.lock();
        c.meta.key.clear();
        c.lyrics.key.clear();
        c.artwork.key.clear();
    }

    /// 用缓存补全展示字段，缺的资源在后台发起抓取
    pub fn observe(&'static self, src: &ProviderState, provider: &'static dyn MusicProvider) -> Resolved {
        let mut out = Resolved {
            title: src.title.clone(),
            artist: src.artist.clone(),
            album: src.album.clone(),
            artwork_url: src.artwork_url.clone(),
            duration_ms: src.duration_ms,
            ..Default::default()
        };

        // spawn 内部要再拿锁，出锁后再 spawn
        let mut fetch_meta: Option<(String, MetaTask)> = None;
        let mut fetch_lyrics: Option<(String, String, String, Option<String>, String, Option<i64>)> =
            None;
        let mut fetch_artwork: Option<String> = None;

        {
            let mut c = self.lock();

            let meta = meta_task_for(src.song_id.as_deref(), &src.source_app_id, &out.title, &out.artist);
            match meta {
                Some((key, task)) => {
                    if key != c.meta.key {
                        c.meta.key = key.clone();
                        fetch_meta = Some((key.clone(), task));
                    }
                    if let Some(m) = c.meta.get(&key) {
                        if out.title.is_empty() {
                            out.title = m.title.clone();
                        }
                        if out.artist.is_empty() {
                            out.artist = m.artist.clone();
                        }
                        if out.album.is_none() && !m.album.is_empty() {
                            out.album = Some(m.album.clone());
                        }
                        if out.artwork_url.is_none() && !m.cover_url.is_empty() {
                            out.artwork_url = Some(m.cover_url.clone());
                        }
                        if out.duration_ms.is_none() && m.duration_ms > 0 {
                            out.duration_ms = Some(m.duration_ms);
                        }
                    }
                }
                None => c.meta.key.clear(),
            }

            // 键按补全后的标题算：标题晚到时若用空标题记键，之后就不会再抓
            let track_key = format!("{}|{}|{}", out.title, out.artist, src.source_app_id);
            let lyrics_key = match &src.song_id {
                // CDP 源给的 kugou:<hash> 本身就是键（按 hash 取词）
                Some(id) if id.starts_with(KUGOU_SONG_PREFIX) => id.clone(),
                Some(id) => format!("163:{id}"),
                None => track_key.clone(),
            };

            // 封面：位图（SMTC 缩略图）是播放器自己给的那张，优先；直链（网易云/酷狗接口
            // 按 ID 或标题搜出来的）只在位图还没抓到、或这个源压根没有位图能力时用
            if provider.capabilities().artwork_bitmap {
                if track_key != c.artwork.key {
                    c.artwork.key = track_key.clone();
                    fetch_artwork = Some(track_key.clone());
                }
                if let Some(a) = c.artwork.get(&track_key) {
                    out.artwork_hash = Some(a.hash.clone());
                    out.artwork_url = None;
                }
            } else {
                c.artwork.key = track_key.clone();
            }

            let can_fetch = src.song_id.is_some() || !out.title.is_empty();
            if lyrics_key != c.lyrics.key && can_fetch {
                c.lyrics.key = lyrics_key.clone();
                if src.song_id.is_some() || media_sources::lyrics_supported(&src.source_app_id) {
                    fetch_lyrics = Some((
                        lyrics_key.clone(),
                        out.title.clone(),
                        out.artist.clone(),
                        src.song_id.clone(),
                        src.source_app_id.clone(),
                        out.duration_ms,
                    ));
                }
            }
            if let Some(l) = c.lyrics.get(&lyrics_key) {
                if !l.lines.is_empty() {
                    out.lyrics_id = Some(lyrics_key.clone());
                }
            }
        }

        if let Some((key, task)) = fetch_meta {
            self.spawn_meta(key, task);
        }
        if let Some((k, t, a, sid, app, duration)) = fetch_lyrics {
            self.spawn_lyrics(k, t, a, sid, app, duration);
        }
        if let Some(k) = fetch_artwork {
            self.spawn_artwork(k, provider);
        }
        out
    }

    pub fn artwork(&self, hash: &str) -> Option<(String, String)> {
        let c = self.lock();
        let (_, a) = c.artwork.value.as_ref()?;
        (a.hash == hash).then(|| (a.hash.clone(), a.data_url.clone()))
    }

    pub fn lyrics(&self, id: &str) -> Option<LyricsData> {
        self.lock().lyrics.get(id).cloned()
    }

    /// 后台跑 `work`，结果仍匹配当前键则写入并通知；失败清键，下次 observe 重试
    fn fetch_once<T: Send + 'static>(
        &'static self,
        thread_name: &'static str,
        key: String,
        select: fn(&mut Caches) -> &mut Slot<T>,
        work: impl FnOnce() -> Option<T> + Send + 'static,
    ) {
        {
            let mut c = self.lock();
            let slot = select(&mut c);
            if slot.fetching.as_deref() == Some(key.as_str()) {
                return;
            }
            slot.fetching = Some(key.clone());
        }
        let on_ready = Arc::clone(&self.on_ready);
        let key_for_cleanup = key.clone();
        let spawned = std::thread::Builder::new().name(thread_name.into()).spawn(move || {
            let result = work();
            let notify = {
                let mut c = self.lock();
                let slot = select(&mut c);
                let still_current = slot.key == key;
                if slot.fetching.as_deref() == Some(key.as_str()) {
                    slot.fetching = None;
                }
                match result {
                    Some(v) if still_current => {
                        slot.value = Some((key.clone(), v));
                        true
                    }
                    None if still_current => {
                        slot.key.clear();
                        false
                    }
                    _ => false,
                }
            };
            if notify {
                on_ready();
            }
        });
        if let Err(e) = spawned {
            eprintln!("[resolver] {thread_name} 线程启动失败: {e}");
            let mut c = self.lock();
            let slot = select(&mut c);
            if slot.fetching.as_deref() == Some(key_for_cleanup.as_str()) {
                slot.fetching = None;
            }
        }
    }

    /// meta 槽：按任务走对应接口（网易云 songId / 酷狗标题搜索）
    fn spawn_meta(&'static self, key: String, task: MetaTask) {
        self.fetch_once("music-meta", key, |c| &mut c.meta, move || task.fetch());
    }

    fn spawn_lyrics(
        &'static self,
        key: String,
        title: String,
        artist: String,
        song_id: Option<String>,
        source_app_id: String,
        duration_ms: Option<i64>,
    ) {
        self.fetch_once("music-lyrics", key, |c| &mut c.lyrics, move || {
            let by_query = || api::fetch_lyrics_for(&source_app_id, &title, &artist);
            match &song_id {
                // CDP 源：按音频 hash 精确取词（本地 .krc 优先，其次接口按 hash），
                // 取不到再退回标题搜索链
                Some(sid) if sid.starts_with(KUGOU_SONG_PREFIX) => {
                    let hash = sid.trim_start_matches(KUGOU_SONG_PREFIX).to_string();
                    kugou::provider_fetch_by_hash(&hash, duration_ms.unwrap_or(0), &title)
                        .or_else(by_query)
                }
                Some(sid) => api::fetch_163_by_id(sid).or_else(by_query),
                None => by_query(),
            }
        });
    }

    fn spawn_artwork(&'static self, key: String, provider: &'static dyn MusicProvider) {
        let key_for_check = key.clone();
        let this: &'static Resolver = self;
        self.fetch_once("music-artwork", key, |c| &mut c.artwork, move || {
            // 切歌后播放器填充封面有延迟
            std::thread::sleep(Duration::from_millis(800));
            for attempt in 0..6 {
                if this.lock().artwork.key != key_for_check {
                    return None;
                }
                if let Some(bytes) = provider.artwork_bytes() {
                    return Some(Artwork {
                        hash: content_hash(&bytes),
                        data_url: format!("data:{};base64,{}", sniff_mime(&bytes), b64::encode(&bytes)),
                    });
                }
                std::thread::sleep(Duration::from_millis(if attempt < 3 { 500 } else { 1000 }));
            }
            None
        });
    }
}

fn sniff_mime(bytes: &[u8]) -> &'static str {
    match bytes {
        [0x89, 0x50, ..] => "image/png",
        [0xFF, 0xD8, ..] => "image/jpeg",
        [0x47, 0x49, ..] => "image/gif",
        [0x42, 0x4D, ..] => "image/bmp",
        _ => "image/jpeg",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_mime_detects_common_image_formats() {
        assert_eq!(sniff_mime(&[0x89, 0x50, 0x4E, 0x47]), "image/png");
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF]), "image/jpeg");
        assert_eq!(sniff_mime(&[0x47, 0x49, 0x46]), "image/gif");
        assert_eq!(sniff_mime(&[0x42, 0x4D, 0x00]), "image/bmp");
        assert_eq!(sniff_mime(&[0x00, 0x01]), "image/jpeg", "未知格式回退 jpeg");
    }

    #[test]
    fn slot_get_only_hits_matching_key() {
        let mut s: Slot<i32> = Slot::default();
        s.value = Some(("a".into(), 1));
        assert_eq!(s.get("a"), Some(&1));
        assert_eq!(s.get("b"), None, "键不匹配（已切歌）不得返回旧值");
    }

    #[test]
    fn meta_task_uses_song_id_for_bridge_and_query_for_kugou() {
        let (key, task) = meta_task_for(Some("12345"), "cloudmusic.exe", "晴天", "周杰伦")
            .expect("有平台 songId 就该抓");
        assert_eq!(key, "12345", "songId 直接当缓存键");
        assert_eq!(task, MetaTask::SongId("12345".into()));

        let (key, task) = meta_task_for(None, "KuGou.exe", "晴天", "周杰伦").expect("酷狗曲目该抓");
        assert!(key.starts_with("kugou"), "酷狗曲目的缓存键要独立命名空间: {key}");
        assert_eq!(task, MetaTask::Kugou { title: "晴天".into(), artist: "周杰伦".into() });
    }

    #[test]
    fn meta_task_survives_separators_in_titles() {
        // 键只作身份，分派靠 MetaTask；标题里带分隔符也必须原样传给搜索
        let (_, task) = meta_task_for(None, "KuGou.exe", "A|B", "C、D").expect("该抓");
        assert_eq!(task, MetaTask::Kugou { title: "A|B".into(), artist: "C、D".into() });
    }

    #[test]
    fn other_smtc_sources_get_no_meta_fetch() {
        assert_eq!(
            meta_task_for(None, "chrome.exe", "某网页标题", ""),
            None,
            "浏览器等源的标题只是网页标题，不该拿去搜曲目信息"
        );
        assert_eq!(
            meta_task_for(None, "kugou.exe", "", ""),
            None,
            "标题还没到时不能拿空标题占住键"
        );
    }
}
