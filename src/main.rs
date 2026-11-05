// release 按 GUI 子系统链接:双击启动不再弹黑色控制台框。
// debug 保留控制台子系统,方便看日志与断点输出。
// 命令行子命令的输出由 win::attach_parent_console 挂回调用方终端。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

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
mod ipc;
mod mft;
mod nameidx;
mod net;
mod oneclick;
mod picasa;
mod pool;
mod rec;
mod recipes;
mod save;
mod scan;
mod scroll;
mod service;
mod shaders;
mod share;
mod theme;
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

/// 命令行子命令。返回 `true` 表示本次启动只是跑子命令,不进 GUI。
///
/// - `--export-assets <路径>` / `--import-assets <路径>`:资源包文本导出 / 导入。
/// - `--service`:索引服务主体(SCM 拉起,见 [`service`])。
/// - `--console`:同一套引擎跑前台控制台(调试)。
/// - `--install-service` / `--uninstall-service`:提权子命令,注册 / 撤销服务。
/// - `--stop-service` / `--start-service`:提权子命令,只停 / 只起(不动注册项)。
/// - `--mft-scan [卷字母表]` / `--mft-check` / `--mft-diag`:索引人工入口与诊断。
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
        // 索引服务主体:由 SCM 以 `<exe> --service` 拉起(自动启动、LocalSystem 账户)。
        // 常驻维护 L1 名字索引 + USN 保鲜 + L2 结构元数据落库,并通过命名管道答复 UI 查询。
        "--service" => {
            if let Err(e) = service::run_service() {
                // 服务会话里没有控制台,eprintln 未必有人看;退出码才是给 SCM 的信号。
                eprintln!("索引服务失败:{e:#}");
                std::process::exit(1);
            }
            true
        }
        // 同一套引擎跑在前台控制台(调试用):日志同时打 stdout,Ctrl+C 退出。
        "--console" => {
            if let Err(e) = service::run_console() {
                eprintln!("索引服务(控制台模式)失败:{e:#}");
                std::process::exit(1);
            }
            true
        }
        // 提权子命令:注册服务并立刻启动。由非提权 UI 用 runas 拉起(整个功能唯一一次 UAC)。
        // 退出码是唯一回报通道(GUI 子系统下 stdout 无处可去),所以失败必须非 0。
        "--install-service" => {
            // 这个分支跑在提权进程里,是唯一能把数据目录告诉 LocalSystem 服务的机会:
            // 服务的「用户目录」是 systemprofile,读不到登录用户的 data.location。
            match db::sync_machine_marker() {
                Ok(p) => println!("服务数据目录已写入:{p:?}"),
                Err(e) => eprintln!(
                    "警告:机器级数据目录写不进去({e}),服务会改建在 ProgramData 而不是 {:?}",
                    db::data_dir()
                ),
            }
            match std::env::current_exe() {
                Ok(exe) => match win::service_install(&exe) {
                    Ok(()) => println!("索引服务已安装并启动:{}", service::SERVICE_NAME),
                    Err(e) => {
                        eprintln!("安装索引服务失败:{e}");
                        std::process::exit(1);
                    }
                },
                Err(e) => {
                    eprintln!("取自身路径失败:{e}");
                    std::process::exit(1);
                }
            }
            true
        }
        // 提权子命令:停服务 + 删注册(撤销授权)。幂等:没装过也算成功。
        "--uninstall-service" => {
            match win::service_uninstall() {
                Ok(()) => println!("索引服务已卸载"),
                Err(e) => {
                    eprintln!("卸载索引服务失败:{e}");
                    std::process::exit(1);
                }
            }
            true
        }
        // 提权子命令:**只停**服务,注册项留着,之后可以原地起来。
        // 存在的理由:服务在跑就锁着自己的 exe,升级/重编/搬目录全都动不了;
        // 这条路必须能在程序里点出来,而不是让用户去敲 `net stop` 或开 services.msc。
        "--stop-service" => {
            match win::service_stop() {
                Ok(()) => println!("索引服务已停止(注册项保留)"),
                Err(e) => {
                    eprintln!("停止索引服务失败:{e}");
                    std::process::exit(1);
                }
            }
            true
        }
        // 提权子命令:**只启动**已注册的服务(不改配置)。和 --stop-service 成对,
        // 停完只有这一条回头路,不用重新安装、也少一次 UAC 之外的等待。
        "--start-service" => {
            match win::service_start() {
                Ok(()) => println!("索引服务已启动"),
                Err(e) => {
                    eprintln!("启动索引服务失败:{e}");
                    std::process::exit(1);
                }
            }
            true
        }
        // 手动/诊断入口:全量枚举 NTFS 卷 USN/MFT → SoA 名字索引落盘。
        // 常驻路径是 `--service`;这里只给人工跑一轮,需要提权才有 $MFT 读权限。
        // 用法:`--mft-scan [卷字母表]`(旧的 `--mft-report` 报告协议已废除)。
        "--mft-scan" => {
            let spec = args.iter().skip(1).find(|a| !a.starts_with("--")).cloned();
            match nameidx::cli_mft_scan(spec.as_deref()) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("MFT 扫描失败:{e:#}");
                    std::process::exit(1);
                }
            }
            true
        }
        // 诊断:载入名字索引,打印统计与样例查询结果。
        "--mft-check" => {
            match nameidx::cli_mft_check() {
                Ok(()) => {}
                Err(e) => eprintln!("索引检查失败:{e:#}"),
            }
            true
        }
        // S2 硬门:只读验证 $MFT 记录偏移口径(附 A1「实施第 0 步」)。
        // 用法:`--mft-diag [卷字母表] [报告文件]`,如 `--mft-diag C report.txt`;
        // 缺省诊断全部 NTFS 卷。有 FAIL 项时退出码 2 —— S3 之前必须过关。
        "--mft-diag" => {
            let positional: Vec<String> = args
                .iter()
                .skip(1)
                .filter(|a| !a.starts_with("--"))
                .cloned()
                .collect();
            match rec::cli_mft_diag(
                positional.first().map(String::as_str),
                positional.get(1).map(String::as_str),
            ) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("MFT 偏移诊断失败:{e:#}");
                    std::process::exit(1);
                }
            }
            true
        }
        _ => false,
    }
}

fn window_icon() -> egui::IconData {
    let img = image::load_from_memory(include_bytes!("../assets/icon.png"))
        .expect("内置图标 assets/icon.png 解码失败")
        .into_rgba8();
    let (w, h) = img.dimensions();
    egui::IconData {
        width: w,
        height: h,
        rgba: img.into_raw(),
    }
}

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // 数据目录 pending 迁移必须先于一切打开库/索引的动作(包括命令行子命令)。
    crate::db::migrate_pending_data();
    // 服务模式由 SCM 在 session 0 拉起:那里没有父终端可挂,日志走数据目录下的
    // index-service.log,所以不能调 attach_parent_console(会白等一个不存在的控制台)。
    let is_service = args.iter().any(|a| a == "--service");
    // GUI 子系统下带参数启动,多半是命令行子命令;先把输出挂回父终端再执行,
    // 否则 println! 的结果无处可去。普通双击(无参数)不受影响。
    if !is_service && !args.is_empty() {
        win::attach_parent_console();
    }
    if handle_cli(&args) {
        return Ok(());
    }

    // UI 线程提优先级(与原版一致:UI 线程 ABOVE_NORMAL,
    // 扫描/缩略图后台线程 BELOW_NORMAL —— 交互永远优先于后台计算)。
    win::set_current_thread_priority(win::Priority::AboveNormal);

    let store = db::Store::open().expect("打开索引数据库失败");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Relyto.Maliang")
            .with_inner_size([1280.0, 860.0])
            .with_icon(Arc::new(window_icon())),
        ..Default::default()
    };
    eframe::run_native(
        "Relyto.Maliang",
        options,
        Box::new(|cc| {
            let mut app = app::App::new(cc, store.clone());
            app.start_initial_tasks(cc.egui_ctx.clone());
            Ok(Box::new(app))
        }),
    )
}
