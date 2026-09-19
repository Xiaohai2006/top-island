//! 酷狗进程内增强的部署：给 `libcef.dll` 打补丁，把 DevTools 端口（CDP 12233）打开。
//!
//! 为什么必须打补丁：酷狗在代码里覆盖了 `--remote-debugging-port`，只传启动参数没用
//! （实测过：12233/9222/9229 全不响应）。补丁针对 **CEF 89.20.0**（酷狗 20.1.x 自带），
//! 九处字节：P1/P7 把「端口未设」哨兵 `0xAAAAAAAA` 写成 12233；P2/P3/P6/P9 NOP 掉范围/
//! 成功性条件跳转；P4/P8 NOP 掉解析 CALL。表来自社区实现 Metabox-Nexus-PlayerCap（MIT）
//! 的逆向结果，我们对每份 DLL 先做**九处联合指纹校验**，不认识就一个字节都不写。
//!
//! 安全措施（对齐本仓 ncm_deploy 的谨慎风格）：
//! - 落笔前复检指纹、写前备份到 `<libcef>.topisland.bak`、写后自校验、失败立即回滚；
//! - 改的是第三方签名文件，需要管理员：用 `ShellExecuteW("runas")` 拉起**自身**的
//!   `--kugou-patch-helper`，一次 UAC 干一件事（打补丁或还原，见 [`HelperMode`]）；
//!   提权进程只负责写盘，停/起酷狗留给未提权的主进程（否则酷狗会被以后台管理员身份拉起来）；
//! - 备份就落在 `libcef.dll` 旁边（多半在 Program Files），所以**还原也要提权**——不然
//!   往那儿拷备份只会拿到 `os error 5`；
//! - 只在用户显式打开/关闭「酷狗音乐」开关时才动，且不自动重试（弹 UAC 的循环会夺焦）。

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

use crate::error::{AppError, AppResult};

use super::kugou;

/// 补丁打开的 DevTools 端口（与 `kugou_cdp::CDP_PORT` 一致）
const CDP_PORT: u16 = 12233;
const BACKUP_SUFFIX: &str = ".topisland.bak";
/// 提权 helper 的命令行开关（main 在 Tauri 起来之前拦它）
pub const HELPER_FLAG: &str = "--kugou-patch-helper";
const READY_FILE: &str = "ready";
const GO_FILE: &str = "go";
const RESULT_FILE: &str = "result";

/// 提权 helper 要干的那一件事。开关打开时打补丁，关掉时还原；两件事都要管理员
/// （备份就落在 `<libcef>.topisland.bak`，跟 `libcef.dll` 在同一个目录）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperMode {
    Patch,
    Restore,
}

impl HelperMode {
    /// 传给提权进程的第一个参数
    fn as_arg(self) -> &'static str {
        match self {
            HelperMode::Patch => "patch",
            HelperMode::Restore => "restore",
        }
    }

    /// 认不出来的参数按打补丁处理（老版本命令行没这个参数）
    fn from_arg(arg: &str) -> Self {
        match arg {
            "restore" => HelperMode::Restore,
            _ => HelperMode::Patch,
        }
    }

    /// 出错信息里给用户看的动作名
    fn label(self) -> &'static str {
        match self {
            HelperMode::Patch => "打补丁",
            HelperMode::Restore => "还原",
        }
    }
}

/// 一处补丁：文件偏移 + 原始字节 + 打完的字节。两者一起构成版本指纹。
struct Site {
    offset: u64,
    orig: &'static [u8],
    data: &'static [u8],
}

/// `orig` 是 20.1.51.27967 / CEF 89.20.0 原版实测字节，`data` 是打完的字节
const SITES: &[Site] = &[
    Site { offset: 0x58C63EB, orig: &[0xC7, 0x06, 0xAA, 0xAA, 0xAA, 0xAA], data: &[0xC7, 0x06, 0xC9, 0x2F, 0x00, 0x00] },
    Site { offset: 0x58C6415, orig: &[0xE8, 0x96, 0xA9, 0xE9, 0xFC], data: &[0x90, 0x90, 0x90, 0x90, 0x90] },
    Site { offset: 0x58C6420, orig: &[0x0F, 0x87, 0x96, 0x01, 0x00, 0x00], data: &[0x90, 0x90, 0x90, 0x90, 0x90, 0x90] },
    Site { offset: 0x58C6428, orig: &[0x0F, 0x84, 0x8E, 0x01, 0x00, 0x00], data: &[0x90, 0x90, 0x90, 0x90, 0x90, 0x90] },
    Site { offset: 0x4BC180E, orig: &[0x8B, 0x95, 0x7C, 0x01, 0x00, 0x00], data: &[0xBA, 0xC9, 0x2F, 0x00, 0x00, 0x90] },
    Site { offset: 0x4BEDE41, orig: &[0x0F, 0x84, 0x70, 0x01, 0x00, 0x00], data: &[0x90, 0x90, 0x90, 0x90, 0x90, 0x90] },
    Site { offset: 0x4BEDE4C, orig: &[0x41, 0xC7, 0x07, 0xAA, 0xAA, 0xAA, 0xAA], data: &[0x41, 0xC7, 0x07, 0xC9, 0x2F, 0x00, 0x00] },
    Site { offset: 0x4BEDEB5, orig: &[0xE8, 0xF6, 0x2E, 0xB7, 0xFD], data: &[0x90, 0x90, 0x90, 0x90, 0x90] },
    Site { offset: 0x4BEDED0, orig: &[0x0F, 0x44, 0xDA], data: &[0x90, 0x90, 0x90] },
];

/// libcef.dll 的补丁状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PatchState {
    /// 没找到 libcef.dll
    Missing,
    /// 找到了，但九处指纹里有陌生的字节：换了 CEF 基线，拒绝盲打
    Unsupported,
    /// 认识，但还没打全（含「打了一部分」）
    Pending,
    /// 九处都已是补丁字节
    Patched,
}

/// 增强的整体状态（设置页展示 + 决定要不要给「修复」按钮）
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnhanceStatus {
    pub patch: PatchState,
    /// CDP 端口是否已经在应答（= 补丁生效且酷狗在跑）
    pub cdp: bool,
    pub kugou_running: bool,
    /// 补丁已就位但酷狗还跑着旧 DLL：需要重启酷狗
    pub needs_restart: bool,
}

pub fn status() -> EnhanceStatus {
    let patch = patch_state();
    let kugou_running = kugou::is_running();
    let cdp = cdp_available();
    let needs_restart = kugou_running && !cdp && patch == PatchState::Patched;
    EnhanceStatus { patch, cdp, kugou_running, needs_restart }
}

/// CDP 端口是否在应答
pub fn cdp_available() -> bool {
    ureq::get(&format!("http://127.0.0.1:{CDP_PORT}/json"))
        .config()
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .call()
        .is_ok()
}

// ---- 定位 ----

/// KuGou.exe 实际加载的那份 `libcef.dll`。
///
/// 酷狗把 CEF 按版本号放子目录，升级后旧目录会残留，选错就会拿不匹配的偏移写坏 DLL。
/// 所以优先「正在运行的进程所在目录」——那是唯一没有歧义的答案；其次注册表
/// `HKCU\Software\KuGou\KuGou8`（酷狗自己维护的当前版本目录指针）；最后扫安装根下
/// 带 libcef.dll 的版本目录，取改动时间最新的那个。
pub fn libcef_path() -> Option<PathBuf> {
    if let Some(dir) = running_exe_dir() {
        let c = dir.join("libcef.dll");
        if c.is_file() {
            return Some(c);
        }
    }
    let base = libcef_base_dirs();
    if let Some(dir) = sys_info_current_dir() {
        let c = dir.join("libcef.dll");
        if c.is_file() {
            return Some(c);
        }
    }
    newest_version_libcef(&base)
}

/// 安装根目录（含版本子目录的那一层）
fn libcef_base_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Ok(base) = std::env::var(var) {
            out.push(PathBuf::from(base).join("KuGou").join("KGMusic"));
        }
    }
    out
}

/// `HKCU\Software\KuGou\KuGou8`：酷狗自己维护的当前版本目录（可能被改写成 exe 路径，需守卫）
fn sys_info_current_dir() -> Option<PathBuf> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey(r"Software\KuGou").ok()?;
    let dir: String = key.get_value("KuGou8").ok()?;
    let path = PathBuf::from(dir.trim().trim_matches('"'));
    path.is_dir().then_some(path)
}

/// 正在运行的酷狗主进程所在目录
fn running_exe_dir() -> Option<PathBuf> {
    use windows::Win32::Foundation::{CloseHandle, MAX_PATH};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    for pid in kugou::enum_kugou_pids() {
        unsafe {
            let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                continue;
            };
            let mut buf = [0u16; MAX_PATH as usize + 16];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
            .is_ok();
            let _ = CloseHandle(handle);
            if !ok {
                continue;
            }
            let exe = String::from_utf16_lossy(&buf[..len as usize]);
            let path = PathBuf::from(exe);
            // 只认主程序（子进程同名，但都从同一个目录起）
            if path.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("KuGou.exe"))
                || path.file_name().is_some_and(|n| n.to_string_lossy().starts_with("KuGou"))
            {
                if let Some(dir) = path.parent() {
                    return Some(dir.to_path_buf());
                }
            }
        }
    }
    None
}

/// 扫 `<base>\*\libcef.dll`，取改动时间最新的（当前版本目录通常最后被写入）
fn newest_version_libcef(bases: &[PathBuf]) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for base in bases {
        let Ok(entries) = std::fs::read_dir(base) else { continue };
        for entry in entries.flatten() {
            let candidate = entry.path().join("libcef.dll");
            let Ok(meta) = std::fs::metadata(&candidate) else { continue };
            let Ok(modified) = meta.modified() else { continue };
            if best.as_ref().is_none_or(|(t, _)| modified > *t) {
                best = Some((modified, candidate));
            }
        }
    }
    best.map(|(_, path)| path)
}

// ---- 指纹与写盘 ----

/// 读九处字节给 DLL 分类。任一偏移读不到 → 返回错误（宁可报错也不盲打）。
fn classify_reader(r: &mut (impl Read + Seek)) -> AppResult<PatchState> {
    let mut all_data = true;
    for site in SITES {
        let mut buf = vec![0u8; site.data.len()];
        r.seek(SeekFrom::Start(site.offset))
            .map_err(|e| AppError::new(format!("error.io: 定位 0x{:X} 失败: {e}", site.offset)))?;
        r.read_exact(&mut buf)
            .map_err(|e| AppError::new(format!("error.io: 读 0x{:X} 失败: {e}", site.offset)))?;
        if buf != site.data {
            all_data = false;
            if buf != site.orig {
                return Ok(PatchState::Unsupported);
            }
        }
    }
    Ok(if all_data { PatchState::Patched } else { PatchState::Pending })
}

pub fn patch_state() -> PatchState {
    let Some(path) = libcef_path() else {
        return PatchState::Missing;
    };
    match std::fs::File::open(&path).map_err(|e| e.to_string()).and_then(|mut f| {
        classify_reader(&mut f).map_err(|e| e.to_string())
    }) {
        Ok(state) => state,
        Err(e) => {
            eprintln!("[kugou-deploy] 读 {} 失败: {e}", path.display());
            PatchState::Missing
        }
    }
}

fn backup_path(libcef: &Path) -> PathBuf {
    let mut name = libcef.as_os_str().to_os_string();
    name.push(BACKUP_SUFFIX);
    PathBuf::from(name)
}

/// 备份 → 只写变更的字节 → 写后自校验，失败立即回滚。
/// 进入前酷狗必须已退出（libcef.dll 被加载时写不进去）。
fn apply(libcef: &Path) -> AppResult<()> {
    match classify_file(libcef)? {
        PatchState::Unsupported => {
            return Err(AppError::new(
                "error.denied: libcef.dll 版本指纹不符（酷狗换过 CEF 基线），拒绝打补丁",
            ))
        }
        PatchState::Patched => return Ok(()),
        _ => {}
    }

    let backup = backup_path(libcef);
    if !backup.is_file() {
        std::fs::copy(libcef, &backup)
            .map_err(|e| AppError::new(format!("error.io: 备份 libcef.dll 失败: {e}")))?;
    }
    // 只备份，不整文件重写：按偏移只写打补丁的那几个字节，flock 窗口最小
    let write_result = (|| -> AppResult<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(libcef)
            .map_err(|e| AppError::new(format!("error.io: 打开 libcef.dll 写失败: {e}")))?;
        for site in SITES {
            f.seek(SeekFrom::Start(site.offset))
                .and_then(|_| f.write_all(site.data))
                .map_err(|e| AppError::new(format!("error.io: 写 0x{:X} 失败: {e}", site.offset)))?;
        }
        f.sync_all().map_err(|e| AppError::new(format!("error.io: fsync 失败: {e}")))
    })();

    let verified = write_result.is_ok()
        && matches!(classify_file(libcef), Ok(PatchState::Patched));
    if verified {
        Ok(())
    } else {
        // 回滚：从备份恢复。恢复成功才删备份；恢复失败保留备份并在日志留痕，供人工恢复
        let rolled = std::fs::copy(&backup, libcef).is_ok();
        if rolled {
            let _ = std::fs::remove_file(&backup);
            Err(AppError::new("error.io: 补丁未生效（可能被杀软回滚），已从备份恢复"))
        } else {
            Err(AppError::new(format!(
                "error.io: 补丁失败且回滚失败，原文件备份保留在 {}",
                backup.display()
            )))
        }
    }
}

fn classify_file(libcef: &Path) -> AppResult<PatchState> {
    let mut f = std::fs::File::open(libcef)
        .map_err(|e| AppError::new(format!("error.io: 打开 libcef.dll 失败: {e}")))?;
    classify_reader(&mut f)
}

/// 关闭「酷狗音乐」开关时把 libcef.dll 还原成原版（从备份恢复）。
///
/// 备份在 `libcef.dll` 旁边（多半是 Program Files），未提权写不进去，所以和打补丁一样
/// 走一次 UAC。酷狗开着才需要停它、也才顺手拉回来；本来没开就只动文件。
pub fn revert() -> AppResult<()> {
    let Some(libcef) = libcef_path() else {
        return Ok(());
    };
    if !backup_path(&libcef).is_file() {
        return Ok(()); // 没备份就没什么可还原的
    }
    let was_running = kugou::is_running();

    let control = control_dir()?;
    let outcome = (|| -> AppResult<String> {
        launch_and_wait_ready(HelperMode::Restore, &libcef, &control)?;
        if was_running && !stop_kugou_and_wait() {
            return Err(AppError::new("error.io: 酷狗没退出，无法还原 libcef.dll"));
        }
        match wait_unlocked(&libcef) {
            Some(true) => {}
            // 未提权探测不了：给点处理器释放的余量，剩下的交给提权 helper 试
            None => std::thread::sleep(Duration::from_millis(500)),
            Some(false) => {
                if was_running {
                    relaunch_kugou(&libcef);
                }
                return Err(AppError::new("error.io: libcef.dll 仍被占用，无法还原"));
            }
        }
        go_and_collect(&control)
    })();
    let _ = std::fs::remove_dir_all(&control);

    let result = outcome?;
    let ok = result.trim() == "ok";
    if was_running {
        relaunch_kugou(&libcef); // 原来是开着的，无论成败都拉回来
    }
    if ok {
        Ok(())
    } else {
        Err(AppError::new(format!("error.io: 还原失败（{result}）")))
    }
}

/// 从备份恢复并删掉备份（供提权 helper 与测试使用）
fn restore_from_backup(libcef: &Path) -> AppResult<()> {
    let backup = backup_path(libcef);
    std::fs::copy(&backup, libcef)
        .map_err(|e| AppError::new(format!("error.io: 还原 libcef.dll 失败: {e}")))?;
    let _ = std::fs::remove_file(&backup);
    Ok(())
}

// ---- 停 / 起酷狗 ----

fn stop_kugou_and_wait() -> bool {
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::System::Threading::{
        OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };

    let pids = kugou::enum_kugou_pids();
    if pids.is_empty() {
        return true;
    }
    let mut handles = Vec::new();
    for pid in &pids {
        unsafe {
            if let Ok(h) = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, *pid) {
                let _ = TerminateProcess(h, 1);
                handles.push(h);
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut all_exited = true;
    for h in &handles {
        let left = deadline.saturating_duration_since(Instant::now());
        let waited = unsafe { WaitForSingleObject(*h, left.as_millis() as u32) };
        if waited != WAIT_OBJECT_0 {
            all_exited = false;
        }
    }
    for h in handles {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(h);
        }
    }
    if !all_exited {
        eprintln!("[kugou-deploy] 仍有酷狗进程未退出: {:?}", kugou::enum_kugou_pids());
    }
    all_exited && kugou::enum_kugou_pids().is_empty()
}

/// 等文件锁释放（进程退出到句柄关闭有个窗口）。
///
/// 注意这个探测是**未提权**做的：如果 `libcef.dll`（多半在 Program Files）的 ACL 本来
/// 就不让当前用户写，那「打不开」说明不了「还被占用」——这种情况返回 `None`，
/// 由提权的 helper 去真正裁决（它写不进去会带原因回来）。
fn wait_unlocked(path: &Path) -> Option<bool> {
    for _ in 0..50 {
        match std::fs::OpenOptions::new().write(true).open(path) {
            Ok(_) => return Some(true),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return None,
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    Some(false)
}

fn relaunch_kugou(libcef: &Path) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    let Some(dir) = libcef.parent() else { return };
    // 主程序在安装根（版本目录的上一层），两个位置都试
    let mut candidates = vec![dir.join("KuGou.exe")];
    if let Some(root) = dir.parent() {
        candidates.push(root.join("KuGou.exe"));
    }
    let Some(exe) = candidates.into_iter().find(|p| p.is_file()) else {
        eprintln!("[kugou-deploy] 找不到 KuGou.exe，请手动启动酷狗");
        return;
    };
    let spawned = std::process::Command::new(&exe)
        .current_dir(exe.parent().unwrap_or(dir))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn();
    if let Err(e) = spawned {
        eprintln!("[kugou-deploy] 重新拉起酷狗失败: {e}（请手动启动）");
    }
}

// ---- 提权流程（一次 UAC）----

/// 打开「酷狗音乐」开关时走这里：CDP 已就绪就直接返回；否则提权打补丁。
///
/// 酷狗开着就顺手重启它（补丁要新 DLL 才生效）；没开就不动它——文件本来就没被占用，
/// 下次启动酷狗时补丁自然生效，也免得用户只想开个开关却被拉起播放器。
///
/// 提权进程只写盘；停/起酷狗由**未提权**的这边做——否则酷狗会被以管理员身份拉起来。
pub fn ensure_applied() -> AppResult<String> {
    if cdp_available() {
        return Ok("酷狗增强已生效。".to_string());
    }
    let Some(libcef) = libcef_path() else {
        return Err(AppError::new("error.io: 没找到酷狗安装（或其中的 libcef.dll）"));
    };
    let was_running = kugou::is_running();
    // 酷狗没在跑时只剩「注册表指针」和「最新目录」两条线索：后者是猜的，
    // 猜错会白打一份用不上的补丁，所以这种情况请用户先开一次酷狗把目录定死
    if !was_running && sys_info_current_dir().is_none() {
        return Err(AppError::new(
            "error.io: 认不准酷狗的版本目录，请先启动一次酷狗音乐，再打开本项",
        ));
    }
    if classify_file(&libcef)? == PatchState::Unsupported {
        return Err(AppError::new(
            "error.denied: 这个酷狗版本的 libcef.dll 与已知补丁不符，不能自动修复（等适配）",
        ));
    }

    let control = control_dir()?;
    let outcome = (|| -> AppResult<String> {
        launch_and_wait_ready(HelperMode::Patch, &libcef, &control)?;
        if was_running && !stop_kugou_and_wait() {
            return Err(AppError::new("error.io: 酷狗没退出，补丁未应用"));
        }
        match wait_unlocked(&libcef) {
            Some(true) => {}
            // 未提权探测不了：给点处理器释放的余量，剩下的交给提权 helper 试
            None => std::thread::sleep(Duration::from_millis(500)),
            Some(false) => {
                if was_running {
                    relaunch_kugou(&libcef);
                }
                return Err(AppError::new("error.io: libcef.dll 仍被占用，补丁未应用"));
            }
        }
        go_and_collect(&control)
    })();
    let _ = std::fs::remove_dir_all(&control);

    let result = outcome?;
    let ok = result.trim() == "ok";
    if was_running {
        relaunch_kugou(&libcef); // 原来是开着的，无论成败都拉回来
    }
    if ok {
        Ok(if was_running {
            "酷狗增强已安装，酷狗已重启。".to_string()
        } else {
            "酷狗增强已安装，下次打开酷狗即生效。".to_string()
        })
    } else {
        Err(AppError::new(format!("error.io: 补丁失败（{result}）")))
    }
}

/// 建一个干净的控制目录：helper 与主进程靠它传 `ready` / `go` / `result` 三个信号
fn control_dir() -> AppResult<PathBuf> {
    let dir = std::env::temp_dir().join(format!("top-island-kugou-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)
        .map_err(|e| AppError::new(format!("error.io: 建临时目录失败: {e}")))?;
    Ok(dir)
}

/// 拉起提权 helper 并等它报到——这一步就是那次 UAC（用户点了「是」helper 才写得出 ready）
fn launch_and_wait_ready(mode: HelperMode, libcef: &Path, control: &Path) -> AppResult<()> {
    launch_elevated_helper(mode, libcef, control)?;
    wait_for_file(&control.join(READY_FILE), Duration::from_secs(30))
        .map_err(|_| AppError::new("error.denied: 未取得管理员权限（UAC 被拒绝或超时）"))
}

/// 放行 helper 去写盘，并收下它的结果（`"ok"` = 成功）
fn go_and_collect(control: &Path) -> AppResult<String> {
    std::fs::write(control.join(GO_FILE), b"go")
        .map_err(|e| AppError::new(format!("error.io: 触发写盘失败: {e}")))?;
    Ok(wait_for_file(&control.join(RESULT_FILE), Duration::from_secs(90))
        .ok()
        .and_then(|_| std::fs::read_to_string(control.join(RESULT_FILE)).ok())
        .unwrap_or_else(|| "timeout".to_string()))
}

fn wait_for_file(path: &Path, timeout: Duration) -> Result<(), ()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.is_file() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    Err(())
}

/// 用 `runas` 拉起自身跑 helper（一次 UAC）
fn launch_elevated_helper(mode: HelperMode, libcef: &Path, control: &Path) -> AppResult<()> {
    let exe = std::env::current_exe()
        .map_err(|e| AppError::new(format!("error.io: 取自身路径失败: {e}")))?;
    let params = helper_params(mode, libcef, control);
    shell_execute("runas", &exe, &params)
        .map_err(|_| AppError::new("error.denied: 拉起提权进程失败（用户拒绝了 UAC？）"))
}

fn helper_params(mode: HelperMode, libcef: &Path, control: &Path) -> String {
    format!(
        "{HELPER_FLAG} {} \"{}\" \"{}\"",
        mode.as_arg(),
        libcef.display(),
        control.display()
    )
}

/// `ShellExecuteW` 薄封装：返回值 > 32 才算成功。抽出来是为了能在测试里用 `open`
/// 走同一条参数编组路径（不弹 UAC）。
fn shell_execute(verb: &str, exe: &Path, params: &str) -> AppResult<()> {
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let verb_w = wide(verb);
    let file_w = wide(&exe.to_string_lossy());
    let args_w = wide(params);
    let ret = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb_w.as_ptr()),
            PCWSTR(file_w.as_ptr()),
            PCWSTR(args_w.as_ptr()),
            PCWSTR::null(),
            SW_HIDE,
        )
    };
    if ret.0 as usize <= 32 {
        return Err(AppError::new(format!("error.io: ShellExecuteW({verb}) 失败")));
    }
    Ok(())
}

/// 提权 helper：等 `go` → 复查指纹 → 备份 → 写 → 自校验/回滚 → 写结果（`restore` 模式则从备份还原）。
/// 由 `main` 在 Tauri 初始化之前调用，跑完直接退出。
pub fn run_patch_helper(mode: &str, libcef: &str, control: &str) -> i32 {
    let mode = HelperMode::from_arg(mode);
    let libcef = PathBuf::from(libcef.trim().trim_matches('"'));
    let control = PathBuf::from(control.trim().trim_matches('"'));
    let result = |text: &str| {
        let _ = std::fs::write(control.join(RESULT_FILE), text);
    };
    if control.as_os_str().is_empty() {
        return 2;
    }
    let _ = std::fs::write(control.join(READY_FILE), b"ready");

    if wait_for_file(&control.join(GO_FILE), Duration::from_secs(300)).is_err() {
        result("timeout");
        return 3;
    }
    // 落笔前复检：从弹 UAC 到真正写盘之间，酷狗可能自更新换掉了这个文件
    let done = match mode {
        HelperMode::Patch => apply(&libcef),
        HelperMode::Restore => restore_from_backup(&libcef),
    };
    match done {
        Ok(()) => {
            result("ok");
            0
        }
        Err(e) => {
            eprintln!("[kugou-deploy] helper {}失败: {e}", mode.label());
            result(&format!("failed: {e}"));
            4
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// 造一份只含九处偏移的稀疏镜像
    fn image(pick: impl Fn(&Site) -> &'static [u8]) -> Cursor<Vec<u8>> {
        let size = SITES.iter().map(|s| s.offset).max().unwrap_or(0) as usize + 16;
        let mut bytes = vec![0u8; size];
        for site in SITES {
            let src = pick(site);
            let at = site.offset as usize;
            bytes[at..at + src.len()].copy_from_slice(src);
        }
        Cursor::new(bytes)
    }

    #[test]
    fn patch_table_is_sane() {
        // 表本身的不变量：长度相等、orig != data、偏移互不相同
        assert_eq!(SITES.len(), 9, "补丁表应有九处");
        for (i, site) in SITES.iter().enumerate() {
            assert_eq!(site.orig.len(), site.data.len(), "第{i}处 orig/data 长度不一致");
            assert_ne!(site.orig, site.data, "第{i}处 orig 与 data 相同，指纹没有区分力");
        }
        let mut offsets: Vec<u64> = SITES.iter().map(|s| s.offset).collect();
        offsets.sort_unstable();
        offsets.dedup();
        assert_eq!(offsets.len(), SITES.len(), "补丁偏移不该重复");
    }

    #[test]
    fn classify_covers_all_four_states() {
        assert_eq!(classify_reader(&mut image(|s| s.orig)).unwrap(), PatchState::Pending);
        assert_eq!(classify_reader(&mut image(|s| s.data)).unwrap(), PatchState::Patched);
        // 混合态（打了一部分）：仍算可打
        let mut mixed = image(|s| s.orig);
        {
            let bytes = mixed.get_mut();
            let at = SITES[0].offset as usize;
            bytes[at..at + SITES[0].data.len()].copy_from_slice(SITES[0].data);
        }
        assert_eq!(classify_reader(&mut mixed).unwrap(), PatchState::Pending);
        // 任一处是陌生字节 → 版本不认识，拒绝盲打
        let mut unknown = image(|s| s.orig);
        {
            let bytes = unknown.get_mut();
            let at = SITES[4].offset as usize;
            for b in &mut bytes[at..at + SITES[4].data.len()] {
                *b = 0xEE;
            }
        }
        assert_eq!(classify_reader(&mut unknown).unwrap(), PatchState::Unsupported);
    }

    #[test]
    fn classify_reports_error_on_short_file() {
        // 文件比偏移短：必须报错，而不是当成「可打」
        let mut tiny = Cursor::new(vec![0u8; 16]);
        assert!(classify_reader(&mut tiny).is_err());
    }

    /// 真机状态冒烟：定位到的应是「正在运行的那份」libcef.dll，且能认出补丁与 CDP。
    /// `cargo test -p island-app --lib kugou_deploy -- --ignored --nocapture`
    #[test]
    #[ignore = "需要装了酷狗且已打过补丁的环境"]
    fn live_status_reports_patched_and_cdp() {
        let path = libcef_path().expect("应能定位 libcef.dll");
        let st = status();
        println!("libcef = {}", path.display());
        println!("status = {st:?}");
        assert!(st.kugou_running, "前置：酷狗应在运行");
        assert_eq!(st.patch, PatchState::Patched, "前置：本机 libcef 已打过补丁");
        assert!(st.cdp, "CDP 应在应答（补丁生效）");
        assert!(!st.needs_restart, "补丁已生效，不该判需要重启");
    }

    /// 真机：用 `open`（而非 `runas`，免得弹 UAC）拉起真实的 island-app.exe 跑 helper，
    /// 验证的是同一条参数编组路径 —— 带空格的路径、引号、控制目录都能正确传进去；
    /// 打补丁与还原两种 mode 都跑一遍（关掉开关时走的就是 restore）。
    /// `cargo test -p island-app --lib kugou_deploy -- --ignored --nocapture`
    #[test]
    #[ignore = "会复制约 139MB 到临时目录，且需要先 cargo build 出 island-app.exe"]
    fn live_shell_execute_starts_helper_with_quoted_args() {
        // 测试进程是 deps 下的测试二进制，真正的 app 在 target/<profile>/island-app.exe
        let test_exe = std::env::current_exe().expect("取测试进程路径");
        let profile_dir = test_exe
            .parent()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
            .expect("deps 的上一层是 profile 目录");
        let app = profile_dir.join("island-app.exe");
        if !app.is_file() {
            panic!("先 cargo build -p island-app 生成 {}", app.display());
        }
        let Some(real) = libcef_path() else {
            panic!("没找到 libcef.dll");
        };

        // 带空格的临时目录，专门考验引号编组
        let dir = std::env::temp_dir().join(format!("ti helper space {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let control = dir.join("ctrl dir");
        std::fs::create_dir_all(&control).expect("建控制目录");
        let copy = dir.join("libcef.dll");
        std::fs::copy(&real, &copy).expect("复制 libcef.dll");
        {
            let mut f = std::fs::OpenOptions::new().write(true).open(&copy).unwrap();
            for site in SITES {
                f.seek(SeekFrom::Start(site.offset)).unwrap();
                f.write_all(site.orig).unwrap();
            }
        }
        assert_eq!(classify_file(&copy).unwrap(), PatchState::Pending);

        // 逐个 mode 跑一遍 helper，返回它写下的结果
        let run_helper = |mode: HelperMode, control: &Path| -> String {
            shell_execute("open", &app, &helper_params(mode, &copy, control)).expect("拉起 helper");
            wait_for_file(&control.join(READY_FILE), Duration::from_secs(30))
                .expect("helper 应写 ready");
            std::fs::write(control.join(GO_FILE), b"go").unwrap();
            wait_for_file(&control.join(RESULT_FILE), Duration::from_secs(120))
                .expect("helper 应写 result");
            std::fs::read_to_string(control.join(RESULT_FILE)).unwrap()
        };

        let result = run_helper(HelperMode::Patch, &control);
        println!("helper patch result = {result:?}");
        assert_eq!(result.trim(), "ok", "helper 应报告成功");
        assert_eq!(classify_file(&copy).unwrap(), PatchState::Patched, "九处应全部写入");

        // 还原：关掉开关时的那条路径，同样要能写回 Program Files 里的文件（提权由 runas 负责）
        let control_restore = dir.join("ctrl restore");
        std::fs::create_dir_all(&control_restore).expect("建控制目录");
        let result = run_helper(HelperMode::Restore, &control_restore);
        println!("helper restore result = {result:?}");
        assert_eq!(result.trim(), "ok", "还原也应报告成功");
        assert_eq!(classify_file(&copy).unwrap(), PatchState::Pending, "还原后应回到未打状态");
        assert!(!backup_path(&copy).exists(), "还原后备份应被删除");

        let _ = std::fs::remove_dir_all(&dir);
        println!("已清理临时目录");
    }

    /// 真机端到端（不动真实安装）：把真实 libcef.dll 复制到临时目录，跑
    /// 分类 → 打补丁 → 分类 → 还原。
    /// `cargo test -p island-app --lib kugou_deploy -- --ignored --nocapture`
    #[test]
    #[ignore = "需要装了酷狗（会复制约 139MB 的 libcef.dll 到临时目录）"]
    fn live_patch_apply_and_revert_on_a_real_copy() {
        let Some(real) = libcef_path() else {
            panic!("没找到 libcef.dll");
        };
        println!("real = {}", real.display());
        let before = patch_state();
        assert!(
            matches!(before, PatchState::Pending | PatchState::Patched),
            "前置：真实 DLL 应能被九处指纹识别（未打或已打），实得 {before:?}"
        );

        let dir = std::env::temp_dir().join(format!("ti-libcef-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let copy = dir.join("libcef.dll");
        std::fs::copy(&real, &copy).expect("复制 libcef.dll");
        println!("copy = {} ({} bytes)", copy.display(), std::fs::metadata(&copy).unwrap().len());

        // 先归一到 orig（真实文件可能已经打过了）
        {
            let mut f = std::fs::OpenOptions::new().write(true).open(&copy).unwrap();
            for site in SITES {
                f.seek(SeekFrom::Start(site.offset)).unwrap();
                f.write_all(site.orig).unwrap();
            }
        }
        assert_eq!(classify_file(&copy).unwrap(), PatchState::Pending);
        assert!(!backup_path(&copy).exists());

        apply(&copy).expect("打补丁");
        assert_eq!(classify_file(&copy).unwrap(), PatchState::Patched, "打完应为已打状态");
        assert!(backup_path(&copy).is_file(), "成功后备份要保留（关掉增强时靠它还原）");

        // 幂等：对已打的再打一次不报错
        apply(&copy).expect("重复打补丁应幂等");

        // 还原（与用户关掉增强时走的是同一条代码路径）
        restore_from_backup(&copy).expect("从备份还原");
        assert_eq!(classify_file(&copy).unwrap(), PatchState::Pending, "还原后应回到未打状态");
        assert!(!backup_path(&copy).exists(), "还原后备份应被删除");

        let _ = std::fs::remove_dir_all(&dir);
        println!("已清理临时副本");
    }
}
