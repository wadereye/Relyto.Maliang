// release 按 GUI 子系统链接:双击启动不再弹黑色控制台框。
// debug 保留控制台子系统,方便看日志与断点输出。
// 命令行子命令的输出由 win::attach_parent_console 挂回调用方终端。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod anim;
mod app;
mod assets;
mod assets_format;
mod curves;
mod db;
mod device;
mod effects;
mod exif;
mod export;
mod fswatch;
mod hash;
mod i18n;
mod i18n_en;
mod ipc;
mod mft;
mod nameidx;
mod net;
mod oneclick;
mod picasa;
mod pool;
mod psd_preview;
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
mod update;
mod video;
mod watch;
mod win;
mod wpd;
mod xmp;

#[cfg(feature = "ai")]
mod ai;

#[cfg(feature = "ai")]
mod faces;

/// 人脸模型就位检查与后台下载(依赖 `ai` 的模型目录与加载校验)。
#[cfg(feature = "ai")]
mod models;

#[cfg(feature = "ai")]
mod clip;

#[cfg(feature = "ai")]
mod ai_queue;

#[cfg(feature = "ai")]
mod ai_sched;

#[cfg(feature = "ai")]
mod ai_models;

#[cfg(feature = "ai")]
mod ai_text;

#[cfg(feature = "ai")]
mod ai_tags;

#[cfg(feature = "ai")]
mod ai_ocr;

#[cfg(feature = "ai")]
mod ai_search;

#[cfg(feature = "ai")]
mod ai_parse;

#[cfg(feature = "ai")]
mod ai_review;

#[cfg(feature = "ai")]
mod ai_caption;

use crate::i18n::{t, t_with};
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
/// - `--check-update`:按统一配置 maliang.ini 检查一次版本清单并打印(验证配置用)。
/// - `--download-update`:抓清单、后台下载并校验,不安装(部署时验证代理/Range 用)。
/// - `--version`:打印版本号后退出;不带它时未知参数会落到 GUI 分支,脚本无从取版本。
/// - `--apply-update <目录> <令牌> <是否起服务>`:提权安装子进程,内部使用;
///   等服务状态落定 → 停服务 → 换程序文件 → 起服务。
/// - `--restore-service`:更新被中断后,把索引服务起回来并清掉中断标记。
fn handle_cli(args: &[String]) -> bool {
    let Some(flag) = args.first().map(|s| s.as_str()) else {
        return false;
    };
    match flag {
        "--export-assets" => {
            let out = args.get(1).cloned().unwrap_or_else(|| "presets.txt".into());
            match std::fs::write(&out, assets::export_text()) {
                Ok(()) => println!(
                    "{}",
                    t_with("已导出资源包文本 -> {out}", &[("out", out.to_string())])
                ),
                Err(e) => eprintln!("{}", t_with("导出失败:{e}", &[("e", e.to_string())])),
            }
            true
        }
        "--import-assets" => {
            let Some(src) = args.get(1) else {
                eprintln!("{}", t("用法:--import-assets <文本路径>"));
                return true;
            };
            let text = match std::fs::read_to_string(src) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("{}", t_with("读取失败:{e}", &[("e", e.to_string())]));
                    return true;
                }
            };
            let bytes = match assets::import_text(&text) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("{}", t_with("解析失败:{e}", &[("e", e.to_string())]));
                    return true;
                }
            };
            match assets::write_user_asset(&bytes) {
                Ok(p) => println!(
                    "{}",
                    t_with(
                        "已写入用户资源包 -> {p1}",
                        &[("p1", (p.display()).to_string())]
                    )
                ),
                Err(e) => eprintln!("{}", t_with("写入失败:{e}", &[("e", e.to_string())])),
            }
            true
        }
        // 索引服务主体:由 SCM 以 `<exe> --service` 拉起(自动启动、LocalSystem 账户)。
        // 常驻维护 L1 名字索引 + USN 保鲜 + L2 结构元数据落库,并通过命名管道答复 UI 查询。
        "--service" => {
            if let Err(e) = service::run_service() {
                // 服务会话里没有控制台,eprintln 未必有人看;退出码才是给 SCM 的信号。
                eprintln!("{}", t_with("索引服务失败:{e}", &[("e", format!("{e:#}"))]));
                std::process::exit(1);
            }
            true
        }
        // 同一套引擎跑在前台控制台(调试用):日志同时打 stdout,Ctrl+C 退出。
        "--console" => {
            if let Err(e) = service::run_console() {
                eprintln!(
                    "{}",
                    t_with("索引服务(控制台模式)失败:{e}", &[("e", format!("{e:#}"))])
                );
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
                Ok(p) => println!(
                    "{}",
                    t_with("服务数据目录已写入:{p}", &[("p", format!("{p:?}"))])
                ),
                Err(e) => eprintln!(
                    "{}",
                    t_with(
                        "警告:机器级数据目录写不进去({e}),服务会改建在 ProgramData 而不是 {p2}",
                        &[
                            ("e", e.to_string()),
                            ("p2", format!("{:?}", db::data_dir()))
                        ]
                    )
                ),
            }
            match std::env::current_exe() {
                Ok(exe) => match win::service_install(&exe) {
                    Ok(()) => println!(
                        "{}",
                        t_with(
                            "索引服务已安装并启动:{p1}",
                            &[("p1", service::SERVICE_NAME.to_string())]
                        )
                    ),
                    Err(e) => {
                        eprintln!(
                            "{}",
                            t_with("安装索引服务失败:{e}", &[("e", e.to_string())])
                        );
                        std::process::exit(1);
                    }
                },
                Err(e) => {
                    eprintln!("{}", t_with("取自身路径失败:{e}", &[("e", e.to_string())]));
                    std::process::exit(1);
                }
            }
            true
        }
        // 提权子命令:停服务 + 删注册(撤销授权)。幂等:没装过也算成功。
        "--uninstall-service" => {
            match win::service_uninstall() {
                Ok(()) => println!("{}", t("索引服务已卸载")),
                Err(e) => {
                    eprintln!(
                        "{}",
                        t_with("卸载索引服务失败:{e}", &[("e", e.to_string())])
                    );
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
                Ok(()) => println!("{}", t("索引服务已停止(注册项保留)")),
                Err(e) => {
                    eprintln!(
                        "{}",
                        t_with("停止索引服务失败:{e}", &[("e", e.to_string())])
                    );
                    std::process::exit(1);
                }
            }
            true
        }
        // 提权子命令:**只启动**已注册的服务(不改配置)。和 --stop-service 成对,
        // 停完只有这一条回头路,不用重新安装、也少一次 UAC 之外的等待。
        "--start-service" => {
            match win::service_start() {
                Ok(()) => println!("{}", t("索引服务已启动")),
                Err(e) => {
                    eprintln!(
                        "{}",
                        t_with("启动索引服务失败:{e}", &[("e", e.to_string())])
                    );
                    std::process::exit(1);
                }
            }
            true
        }
        // 手动/诊断入口:全量枚举 NTFS 卷 USN/MFT → SoA 名字索引落盘。
        // 常驻路径是 `--service`;这里只给人工跑一轮,需要提权才有 $MFT 读权限。
        // 用法:`--mft-scan [卷字母表]`(旧的 `--mft-report` 报告协议已废除)。
        "--mft-scan" => {
            // 旧版定时任务带 `--mft-task` 参数。常驻服务在位时让它空转退出:
            // 两套索引器会并发写同一份 nameindex.bin,谁后写谁覆盖另一方。
            if args.iter().any(|a| a == "--mft-task")
                && crate::win::service_state() == crate::win::ServiceState::Running
            {
                println!("{}", t("常驻索引服务在运行,本次定时扫描跳过"));
                return true;
            }
            let spec = args.iter().skip(1).find(|a| !a.starts_with("--")).cloned();
            match nameidx::cli_mft_scan(spec.as_deref()) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("{}", t_with("MFT 扫描失败:{e}", &[("e", format!("{e:#}"))]));
                    std::process::exit(1);
                }
            }
            true
        }
        // 诊断:载入名字索引,打印统计与样例查询结果。
        "--mft-check" => {
            match nameidx::cli_mft_check() {
                Ok(()) => {}
                Err(e) => eprintln!("{}", t_with("索引检查失败:{e}", &[("e", format!("{e:#}"))])),
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
                    eprintln!(
                        "{}",
                        t_with("MFT 偏移诊断失败:{e}", &[("e", format!("{e:#}"))])
                    );
                    std::process::exit(1);
                }
            }
            true
        }
        // USN 原始记录诊断:提权运行,把某卷日志尾部的原始记录写进指定文件。
        // 用法:`--usn-dump D <输出文件> [回看字节数]`,缺省回看 256 KB。
        "--usn-dump" => {
            let letter = args
                .get(1)
                .and_then(|a| a.chars().next())
                .map(|c| c.to_ascii_uppercase())
                .unwrap_or('C');
            let Some(out) = args.get(2).cloned() else {
                eprintln!("{}", t("用法:--usn-dump <卷字母> <输出文件> [回看字节数]"));
                std::process::exit(1);
            };
            let tail: u64 = args
                .get(3)
                .and_then(|a| a.parse().ok())
                .unwrap_or(256 * 1024);
            match mft::cli_usn_dump(letter, &out, tail) {
                Ok(()) => println!(
                    "{}",
                    t_with("USN 记录已写入:{out}", &[("out", out.to_string())])
                ),
                Err(e) => {
                    eprintln!("{}", t_with("USN 诊断失败:{e}", &[("e", format!("{e:#}"))]));
                    std::process::exit(1);
                }
            }
            true
        }
        // 人工验证更新配置:只查清单、不下载、不改程序文件。
        "--check-update" => {
            if let Err(e) = update::cli_check() {
                eprintln!("{}", t_with("检查更新失败:{e}", &[("e", e.to_string())]));
                std::process::exit(1);
            }
            true
        }
        // 部署时验证整条下载链:清单 → Range 分段 → 校验 → 落地,不安装。
        "--download-update" => {
            if let Err(e) = update::cli_download() {
                eprintln!("{}", t_with("下载更新失败:{e}", &[("e", e.to_string())]));
                std::process::exit(1);
            }
            true
        }
        // 安装更新的提权子进程。退出码供父进程判断成败,详情写在结果文件里
        // (GUI 子系统下 stdout 未必有人看)。令牌用于区分本次与上次遗留的结果。
        "--apply-update" => {
            if let Err(e) = update::cli_apply_update(
                args.get(1).map(String::as_str),
                args.get(2).map(String::as_str),
                args.get(3).map(String::as_str),
            ) {
                eprintln!("{}", t_with("应用更新失败:{e}", &[("e", e.to_string())]));
                std::process::exit(1);
            }
            true
        }
        // 更新途中 helper 被杀时的兜底:只把服务起回来,不动任何文件。
        "--restore-service" => {
            if let Err(e) = update::cli_restore_service(args.get(1).map(String::as_str)) {
                eprintln!(
                    "{}",
                    t_with("恢复索引服务失败:{e}", &[("e", e.to_string())])
                );
                std::process::exit(1);
            }
            true
        }
        // 命令行帮助:只读、不进 GUI、不碰数据库。
        "--help" | "-h" => {
            println!("{}", t("用法: maliang.exe [子命令]"));
            println!("{}", t("可用子命令:"));
            for (cmd, desc) in [
                ("--version", t("打印版本号")),
                ("--help / -h", t("显示本帮助")),
                ("--check-update", t("检查更新")),
                ("--download-update", t("下载更新包")),
                ("--install-service", t("安装索引服务")),
                ("--uninstall-service", t("卸载索引服务")),
                ("--start-service", t("启动索引服务")),
                ("--stop-service", t("停止索引服务")),
                ("--service", t("以索引服务运行")),
                ("--console", t("控制台模式运行")),
                ("--mft-scan", t("扫描名字索引")),
                ("--mft-check", t("校验名字索引")),
                ("--export-assets [路径]", t("导出资源包")),
                ("--import-assets <路径>", t("导入资源包")),
            ] {
                println!("  {cmd:<28} {desc}");
            }
            true
        }
        // 打印版本号后退出。
        "--version" => {
            println!(
                "{}",
                t_with(
                    "马良 (Relyto.Maliang) v{p1}",
                    &[("p1", update::CURRENT_VERSION.to_string())]
                )
            );
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
    // 更新相关的子命令都不碰数据库,没必要先做分钟级的数据目录迁移。
    let is_update_helper = matches!(
        args.first().map(String::as_str),
        Some("--apply-update")
            | Some("--restore-service")
            | Some("--check-update")
            | Some("--download-update")
            | Some("--version")
            | Some("--help")
            | Some("-h")
    );
    // 统一配置迁移(旧 data.location / update.ini → maliang.ini)必须最先行:
    // 后续一切配置与数据目录解析都只认新文件。update-helper 子命令也要跑,
    // 否则 --check-update 会在迁移前读到空配置。
    crate::db::migrate_legacy_configs();
    // 数据目录 pending 迁移必须先于一切打开库/索引的动作(包括命令行子命令)。
    if !is_update_helper {
        crate::db::migrate_pending_data();
    }
    // 服务模式由 SCM 在 session 0 拉起:那里没有父终端可挂,日志走数据目录下的
    // index-service.log,所以不能调 attach_parent_console(会白等一个不存在的控制台)。
    let is_service = args.iter().any(|a| a == "--service");
    // GUI 子系统下带参数启动,多半是命令行子命令;先把输出挂回父终端再执行,
    // 否则 println! 的结果无处可去。普通双击(无参数)不受影响。
    if !is_service && !args.is_empty() {
        win::attach_parent_console();
    }
    // 命令行分支不走 App::new,这里单独读一次界面语言设置。
    crate::i18n::init_from_settings();
    if handle_cli(&args) {
        return Ok(());
    }
    // 上次替换留下的旧程序备份,这时已没人占用,顺手清掉。
    update::cleanup_old_exe();
    // 上次安装把服务停了却没起回来(helper 被杀 / 断电)时,在这里补一次。
    update::restore_service_if_interrupted();

    // 单实例:同一登录会话只允许一个马良 GUI 进程。已有实例在跑时,
    // 激活它的主窗口并弹提示,本进程随即退出(命令行子命令/服务模式不在此列)。
    // 守卫必须活到 main 结束 —— 提前 drop 等于交出单实例权。
    let _instance_guard = match win::acquire_single_instance() {
        Some(guard) => guard,
        None => {
            win::activate_running_instance();
            win::alert_already_running();
            return Ok(());
        }
    };

    // UI 线程提优先级(UI 线程 ABOVE_NORMAL,
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
