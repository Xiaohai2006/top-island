use serde::{Deserialize, Serialize};

// 序列化全部 camelCase：要兼容 Electron 版留下的 store.json，前端 TS 类型也不用动。

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeId {
    Dark,
    Light,
    Graphite,
    Cream,
    Pink,
    Cyberpink,
    Aurora,
    Sunset,
    Ocean,
    Mint,
    Custom,
}

impl Default for ThemeId {
    fn default() -> Self {
        Self::Dark
    }
}

/// 自定义主题：渐变双色 + 强调色
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomTheme {
    pub a: String,
    pub b: String,
    pub accent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IslandLayout {
    /// 缩放百分比（45~300）
    pub scale: f64,
    /// 上滑隐藏时露出的高度（px，2~20）
    pub hidden_peek: f64,
    /// 所在显示器：'primary' 或显示器名
    pub display_id: String,
}

impl Default for IslandLayout {
    fn default() -> Self {
        Self { scale: 100.0, hidden_peek: 6.0, display_id: "primary".into() }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LangPref {
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en-US")]
    EnUs,
}

/// 可单独停用的后台子系统（故障排查用）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DiagnosticsToggles {
    pub clipboard_poll: bool,
    pub music_poll: bool,
    /// 开发者模式开关，仅前端渲染 FPS 浮层
    pub dev_overlay: bool,
}

impl Default for DiagnosticsToggles {
    fn default() -> Self {
        Self { clipboard_poll: true, music_poll: true, dev_overlay: false }
    }
}

/// 隐私模式细项：弹窗卡按需遮挡各字段
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationsPrivacy {
    pub enabled: bool,
    pub blur_avatar: bool,
    pub blur_name: bool,
    pub replace_body: bool,
    /// 替换文案（空则用内置默认）
    pub body_text: String,
}

impl Default for NotificationsPrivacy {
    fn default() -> Self {
        Self {
            enabled: false,
            blur_avatar: true,
            blur_name: true,
            replace_body: false,
            body_text: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationsConfig {
    /// 总开关：读通知库属隐私敏感，默认关闭，用户显式开启
    pub enabled: bool,
    pub popup: bool,
    pub privacy: NotificationsPrivacy,
    /// 接管系统横幅（改来源应用的 ShowBanner 注册表）。属系统设置修改，默认关闭
    pub suppress_banner: bool,
    /// 微信消息接入（需先获取密钥），默认关闭
    pub wechat: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MusicConfig {
    /// 网易云进程内增强，默认开启
    pub netease_bridge: bool,
    /// 酷狗音乐接入（系统媒体会话 + 酷狗接口补全歌词/封面/时长），默认开启
    pub kugou_support: bool,
    /// 酷狗进程内增强（给 libcef.dll 打补丁开 DevTools 端口，读毫秒级进度）。
    /// 需要改酷狗自己的文件 + 一次管理员授权，所以默认关闭、由用户在设置里显式打开
    pub kugou_enhance: bool,
}

impl Default for MusicConfig {
    fn default() -> Self {
        Self { netease_bridge: true, kugou_support: true, kugou_enhance: false }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    pub theme: ThemeId,
    pub custom_theme: CustomTheme,
    pub island: IslandLayout,
    pub lang: LangPref,
    pub notifications: NotificationsConfig,
    pub diagnostics: DiagnosticsToggles,
    pub music: MusicConfig,
    /// 开机自启动（默认开启；仅安装版实际生效）
    pub auto_launch: bool,
}

impl AppSettings {
    /// store.json 里 settings 可能缺字段（老版本），逐字段 serde default 补齐
    pub fn from_value(value: serde_json::Value) -> Self {
        serde_json::from_value(value).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn music_config_fills_missing_fields_from_default() {
        // 容器级 serde default 取的是 MusicConfig::default()，不是字段类型默认值：
        // 老 store.json 里没有 kugouSupport，必须补成「默认开启」而不是被关掉
        let s = AppSettings::from_value(serde_json::json!({
            "theme": "dark",
            "music": { "neteaseBridge": false }
        }));
        assert!(!s.music.netease_bridge, "已保存的开关要保留");
        assert!(s.music.kugou_support, "缺字段时应默认开启酷狗接入");
        assert!(!s.music.kugou_enhance, "进程内增强要动播放器的文件，缺字段时必须默认关闭");
    }

    #[test]
    fn empty_settings_fall_back_to_defaults() {
        let s = AppSettings::from_value(serde_json::json!({}));
        assert!(s.music.netease_bridge && s.music.kugou_support);
        assert!(!s.music.kugou_enhance);
    }
}
