#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // 提权补丁 helper（酷狗 libcef 增强）：带这个开关时只写盘，不起界面
    if let Some(code) = island_app_lib::run_kugou_patch_helper_if_requested() {
        std::process::exit(code);
    }
    island_app_lib::run()
}
