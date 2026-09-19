use island_core::{LyricsData, MusicAction, MusicArtwork, MusicState};

use super::off_thread;
use crate::error::AppResult;
use crate::services;
use crate::services::music::{BridgeStatus, EnhanceStatus, KugouStatus};
#[tauri::command]
pub async fn music_poll() -> AppResult<MusicState> {
    off_thread(|| Ok(services::music::poll_state())).await
}

#[tauri::command]
pub async fn music_control(action: MusicAction, level: Option<i64>) -> AppResult<String> {
    off_thread(move || services::music::control(action, level)).await
}

#[tauri::command]
pub async fn music_seek(position_ms: i64) -> AppResult<bool> {
    off_thread(move || Ok(services::music::seek(position_ms))).await
}

/// 按 hash 取当前曲目封面；hash 不匹配（已切歌）时返回 None
#[tauri::command]
pub async fn music_artwork(hash: String) -> AppResult<Option<MusicArtwork>> {
    off_thread(move || Ok(services::music::artwork(&hash))).await
}

/// 按 lyricsId 取当前曲目歌词；id 不匹配（已切歌）时返回 None
#[tauri::command]
pub async fn music_lyrics(id: String) -> AppResult<Option<LyricsData>> {
    off_thread(move || Ok(services::music::lyrics(&id))).await
}

#[tauri::command]
pub async fn music_bridge_status() -> AppResult<BridgeStatus> {
    off_thread(|| Ok(services::music::bridge_status())).await
}

/// 酷狗接入状态（设置页展示；增强模式下按补丁/进程状态给出下一步）
#[tauri::command]
pub async fn music_kugou_status() -> AppResult<KugouStatus> {
    off_thread(|| Ok(services::music::kugou_status())).await
}

/// 酷狗增强的详细状态（设置页据此显示开关旁的状态与「重试增强」）
#[tauri::command]
pub async fn music_kugou_enhance_status() -> AppResult<EnhanceStatus> {
    off_thread(|| Ok(services::music::kugou_enhance_status())).await
}

/// 打开「酷狗音乐」开关时自动调用：打补丁打开酷狗的 DevTools 端口（弹一次 UAC）
#[tauri::command]
pub async fn music_kugou_repair() -> AppResult<String> {
    off_thread(services::music::kugou_repair).await
}

/// 关闭「酷狗音乐」开关时自动调用：提权还原酷狗的 libcef.dll（弹一次 UAC）
#[tauri::command]
pub async fn music_kugou_revert() -> AppResult<String> {
    off_thread(services::music::kugou_revert).await
}
