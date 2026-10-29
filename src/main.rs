mod app;
mod assets;
mod assets_format;
mod curves;
mod db;
mod device;
mod effects;
mod exif;
mod export;
mod hash;
mod net;
mod oneclick;
mod picasa;
mod pool;
mod recipes;
mod save;
mod scan;
mod scroll;
mod share;
mod thumbs;
mod timeline;
mod video;
mod watch;
mod win;
mod wpd;
mod xmp;

#[cfg(feature = "ai")]
mod ai;

#[cfg(feature = "ai")]
mod faces;

#[cfg(feature = "ai")]
mod clip;

use eframe::egui;
use std::sync::Arc;

/// 命令行子命令:资源包导出 / 导入。
///
/// - `--export-assets <路径>`:把当前生效的资源包导出为文本。
/// - `--import-assets <路径>`:从文本重建资源包,写入用户配置目录(下次启动生效)。
fn handle_cli(args: &[String]) -> bool {
    let Some(flag) = args.first().map(|s| s.as_str()) else {
        return false;
    };
    match flag {
        "--export-assets" => {
            let out = args.get(1).cloned().unwrap_or_else(|| "presets.txt".into());
            match std::fs::write(&out, assets::export_text()) {
                Ok(()) => println!("已导出资源包文本 -> {out}"),
                Err(e) => eprintln!("导出失败:{e}"),
            }
            true
        }
        "--import-assets" => {
            let Some(src) = args.get(1) else {
                eprintln!("用法:--import-assets <文本路径>");
                return true;
            };
            let text = match std::fs::read_to_string(src) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("读取失败:{e}");
                    return true;
                }
            };
            let bytes = match assets::import_text(&text) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("解析失败:{e}");
                    return true;
                }
            };
            match assets::write_user_asset(&bytes) {
                Ok(p) => println!("已写入用户资源包 -> {}", p.display()),
                Err(e) => eprintln!("写入失败:{e}"),
            }
            true
        }
        // 按当前排除规则清洗已入库的行(回收站/应用数据等历史遗留)。
        "--purge-excluded" => {
            match db::Store::open() {
                Ok(store) => {
                    let excl = scan::Exclusions::from_store(&store);
                    let n = store.purge_excluded(&excl);
                    println!("已清除 {n} 行;生效排除项:{}", excl.names().join(", "));
                }
                Err(e) => eprintln!("打开索引库失败:{e}"),
            }
            true
        }
        _ => false,
    }
}

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if handle_cli(&args) {
        return Ok(());
    }

    // UI 线程提优先级(与原版一致:UI 线程 ABOVE_NORMAL,
    // 扫描/缩略图后台线程 BELOW_NORMAL —— 交互永远优先于后台计算)。
    win::set_current_thread_priority(win::Priority::AboveNormal);

    let store = Arc::new(db::Store::open().expect("打开索引数据库失败"));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Relyto.Maliang")
            .with_inner_size([1280.0, 860.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Relyto.Maliang",
        options,
        Box::new(|cc| {
            let mut app = app::App::new(cc, store.clone());
            app.start_initial_tasks();
            Ok(Box::new(app))
        }),
    )
}
