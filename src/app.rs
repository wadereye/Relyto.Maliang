use crate::db::Store;
use crate::timeline::{self, PhotoRow};
use eframe::egui;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

/// 视图模式(视图菜单,参照 Picasa:时间线/人物/标签/文件夹)。
/// Faces/Semantic 仅 ai 构建的菜单会构造;无 ai 时变体存在但无人构造,允许死代码。
#[derive(PartialEq, Clone, Copy)]
enum ViewKind {
    Timeline,
    Folder,
    Duplicates,
    #[allow(dead_code)]
    Faces,
    Tags,
    #[allow(dead_code)]
    Semantic,
}

/// 排序方式(视图 > 排序方式,参照 Picasa)。
#[derive(PartialEq, Clone, Copy)]
enum SortMode {
    Date,
    Name,
}

struct ScanResult {
    scanned: usize,
    indexed: usize,
    /// 因 path+size+mtime 未变而跳过(快速路径命中)。
    unchanged: usize,
    /// 被排除规则跳过的目录数。
    excluded_dirs: usize,
    /// 被用户取消(而非正常完成)。
    cancelled: bool,
    err: Option<String>,
    scope: String,
}

type Dup = (i64, Vec<i64>);

pub struct App {
    store: Arc<Store>,
    rows: Vec<PhotoRow>,
    months: Vec<(String, Vec<usize>)>,
    folders: Vec<String>,
    selected_folder: usize,
    view: ViewKind,
    texs: TexCache,
    /// 常驻缩略图后台 worker(P0-1):启动时补存量,扫描后自动补新增。
    thumb_bg: Option<crate::thumbs::ThumbWorker>,
    scan_busy: Arc<AtomicBool>,
    scan_rx: Option<mpsc::Receiver<ScanResult>>,
    /// 扫描进度/取消控制(P1-8:进度回调 + 退出检查)。
    scan_ctrl: Option<Arc<crate::scan::ScanControl>>,
    /// 扫描进度显示的文案(如"正在扫描整个图库…")。
    scan_scope: String,
    status: String,
    dups: Vec<Dup>,
    /// 重复视图:包含近似重复(hamming ≤ NEAR_DUP_DIST)。
    dup_near: bool,
    id_row: HashMap<i64, usize>,    /// 预览中的照片 id(存 id 而非行索引:后台扫描/移除触发 rebuild 后不错位)。
    preview: Option<i64>,
    preview_tex: Option<(i64, egui::TextureHandle)>,
    /// P1-10:预览后台 worker(解码 + 落盘缓存,不阻塞 UI)。
    preview_bg: Option<crate::thumbs::PreviewWorker>,
    bake_rx: Option<mpsc::Receiver<Result<(), String>>>,
    input_tag: String,
    export_rx: Option<mpsc::Receiver<(usize, Vec<String>)>>,
    wifi: Option<crate::net::WifiHandle>,
    wifi_panel: bool,
    // —— 菜单相关状态 ——
    sel: BTreeSet<i64>,
    sort_mode: SortMode,
    sort_rev: bool,
    /// 当前缩略图档位(P1-9:小 96 / 普通 256 / 大 512 / 预览 1024)。
    thumb_tier: crate::thumbs::Tier,
    tags: Vec<timeline::TagInfo>,
    sel_tag: Option<String>,
    show_about: bool,
    show_shortcuts: bool,
    confirm_remove: bool,
    // —— 文件夹管理器 / 图库维护——
    show_folder_mgr: bool,
    /// P1-7:排除目录输入框(自定义排除规则的待添加目录名)。
    exclude_input: String,
    watched: Vec<(String, usize)>,
    confirm_folder_remove: Option<String>,
    maint_rx: Option<mpsc::Receiver<Result<String, String>>>,
    /// P1-12:变更历史窗口开关。
    show_changes: bool,
    /// P1-12:变更历史条目 (时间戳ms, 路径, 动作)。
    change_log: Vec<(i64, String, String)>,
    /// P1-11:uid64 查找窗口。
    show_uid_lookup: bool,
    /// uid64 查找输入框。
    uid_input: String,
    /// uid64 查找结果 (Ok=路径 / Err=原因)。
    uid_lookup_result: Option<Result<String, String>>,
    /// 监听文件夹:开关 + 变化消息。
    watch_rx: Option<mpsc::Receiver<String>>,
    /// 监听模式(事件驱动 / 降级轮询)—— 供 UI 展示。
    watch_mode: Option<Arc<std::sync::atomic::AtomicU8>>,
    watch_on: Arc<AtomicBool>,
    /// P1-9:滚动限速器(与 Picasa 3 一致的位移阻尼)。
    /// 滚动时按整形后的速度调节"本帧允许解码的缩略图数量",
    /// 滚动越快解码越少 → 大图库滚动不卡顿(限速节点的等价作用)。
    scroll_throttle: crate::scroll::ScrollSmoother,
    /// 本帧允许解码的缩略图数量(由滚动速度决定)。
    thumb_decode_budget: usize,
    /// 本帧已用的解码配额(每帧开头清零)。
    thumb_decode_count: usize,
    /// 上一帧主图库的滚动偏移(用于算增量)。
    last_scroll_y: f32,
    /// 上一帧主图库视口高度(作为阻尼的可视范围)。
    last_viewport_h: f32,
    /// 上一帧主图库内容总高(作为阻尼的内容包围盒)。
    last_content_h: f32,
    /// P1-9:键盘翻页/跳转的目标滚动偏移(滚动事件派发后生效)。
    gallery_scroll_target: Option<f32>,
    /// P1-13:设备热插拔监听(WM_DEVICECHANGE 链路)。
    device_watch: Option<crate::device::DeviceWatcher>,
    /// 上一次见到的可移动卷列表(用于识别新接入的设备)。
    last_volumes: Vec<PathBuf>,
    // —— P2-16 效果系统(效果栈 + 撤销栈)——
    /// 效果面板是否展开。
    fx_panel: bool,
    /// 当前草稿调整值(面板滑块;未应用到库里)。
    fx_draft: crate::effects::Adjust,
    /// 草稿所属照片 id(换照片时才重新载入已存效果,避免每帧覆盖用户拖动)。
    fx_draft_id: Option<i64>,
    /// 剪贴板里的效果栈("复制所有效果" → 内存暂存)。
    fx_clipboard: Option<crate::effects::FilterStack>,
    /// 每张照片的撤销/重做栈(粒度=命名编辑项)。
    fx_undo: HashMap<i64, crate::effects::EditStack>,
    /// 效果操作提示(状态栏)。
    fx_msg: String,
    /// 导出时是否套用已保存的效果(默认否,保持"导出=原图副本"的既有语义)。
    export_with_effects: bool,
    /// P2-18:邮件发送窗口开关 + 尺寸档。
    email_win: bool,
    email_size: crate::share::ShareSize,
    /// P2-18:礼品光盘窗口开关 + 尺寸档。
    cd_win: bool,
    cd_size: crate::share::ShareSize,
    /// P2-18:邮件/光盘后台任务结果通道。
    share_rx: Option<mpsc::Receiver<Result<String, String>>>,
    /// 预览已解出的原图(未加效果)—— 滑块变化时重新合成,避免重复解码。
    preview_base: Option<(i64, image::DynamicImage)>,
    /// 预览纹理已应用的效果栈签名(变化才重绘)。
    fx_applied_sig: Option<String>,
    /// 曲线草稿。
    fx_curve_draft: crate::curves::Curves,
    /// 曲线草稿所属照片 id(换照片时才重载)。
    fx_curve_draft_id: Option<i64>,
    /// 一键效果草稿(bw / sepia / grain / unsharp / warm / cool / vignette / faded)。
    fx_oneclick: Option<String>,
    /// 颗粒强度。
    fx_grain_amount: f32,
    /// 锐化强度(0~100)。
    fx_sharpen_amount: f32,
    /// 自主设计调色(暖调/冷调/暗角/褪色)的共通强度(0~100)。
    fx_tone_amount: f32,
    /// 风格配方草稿(见 `crate::recipes` 的名字表)。
    fx_recipe: Option<String>,
    /// 风格配方强度(0~100)。
    fx_recipe_amount: f32,
    /// 视频裁剪草稿:起点(秒)。
    fx_trim_start: f64,
    /// 视频裁剪草稿:终点(秒)。
    fx_trim_end: f64,
    /// 视频裁剪草稿:视频总时长(秒,0 = 不限制)。
    fx_trim_total: f64,
    /// 视频裁剪草稿所属照片 id(换照片时才重载)。
    fx_trim_id: Option<i64>,
    /// P1-14:字符串池诊断用的探针 id(配合 `pool_maintenance_ui` 的查询按钮)。
    pool_probe_id: i64,
    /// P1-13:从设备导入窗口开关 + 统一设备列表(懒枚举,打开窗口时刷新)。
    show_import_devices: bool,
    import_devices: Vec<crate::wpd::DeviceEntry>,
    import_devices_err: String,
    #[cfg(feature = "ai")]
    ocr_rx: Option<mpsc::Receiver<Result<(i64, Vec<crate::ai::ocr::Txt>), String>>>,
    #[cfg(feature = "ai")]
    ocr_panel: bool,
    #[cfg(feature = "ai")]
    ocr_results: Vec<crate::ai::ocr::Txt>,
    /// OCR 结果对应的照片 id(预览叠框用;仅会话内有效)。
    #[cfg(feature = "ai")]
    ocr_photo: Option<i64>,
    /// 预览叠框开关(OCR/人脸框图上标注;仅 ai 构建有意义)。
    #[allow(dead_code)]
    show_face_boxes: bool,
    #[allow(dead_code)]
    show_ocr_boxes: bool,
    /// 预览叠框缓存(照片 id, 归一化框 + 标签)。
    preview_boxes: Option<(i64, Vec<((f32, f32, f32, f32), String)>)>,
    #[cfg(feature = "ai")]
    face_rx: Option<mpsc::Receiver<Result<(usize, usize), String>>>,
    #[cfg(feature = "ai")]
    face_people: Vec<crate::faces::PeopleRow>,
    #[cfg(feature = "ai")]
    sel_cluster: Option<i64>,
    #[cfg(feature = "ai")]
    face_hits: Vec<crate::faces::FaceHit>,
    #[cfg(feature = "ai")]
    face_sugs: Vec<crate::faces::Suggestion>,
    #[cfg(feature = "ai")]
    face_hide_low: bool,
    #[cfg(feature = "ai")]
    face_show_ignored: bool,
    #[cfg(feature = "ai")]
    face_version_warn: bool,
    #[cfg(feature = "ai")]
    input_rename: String,
    #[cfg(feature = "ai")]
    clip_index_rx: Option<mpsc::Receiver<Result<(usize, usize), String>>>,
    #[cfg(feature = "ai")]
    clip_search_rx: Option<mpsc::Receiver<Result<Vec<(i64, f32)>, String>>>,
    #[cfg(feature = "ai")]
    clip_results: Vec<(i64, f32)>,
    /// 语义索引语言(英文/中文 CLIP 双模型;搜索按查询语言自动匹配)。
    #[cfg(feature = "ai")]
    clip_lang: crate::clip::Lang,
    #[cfg(feature = "ai")]
    input_query: String,
}

/// 近似重复的 dHash Hamming 阈值(64 bit 中 ≤10 不同 ≈ 肉眼难辨)。
const NEAR_DUP_DIST: u32 = 10;

/// 缩略图纹理缓存上限(P0-1)。每张 256×256 RGBA 纹理约 256KB,
/// 768 张 ≈ 192MB 显存,取一个既能覆盖常见滚动范围又不会撑爆的量。
const TEX_CACHE_CAP: usize = 768;

/// 10 色彩色标签(color:red..gray)。
const TAG_COLORS: [(&str, egui::Color32); 10] = [
    ("red", egui::Color32::from_rgb(214, 69, 65)),
    ("orange", egui::Color32::from_rgb(230, 145, 56)),
    ("yellow", egui::Color32::from_rgb(222, 197, 92)),
    ("green", egui::Color32::from_rgb(96, 173, 94)),
    ("blue", egui::Color32::from_rgb(77, 131, 196)),
    ("purple", egui::Color32::from_rgb(150, 100, 190)),
    ("pink", egui::Color32::from_rgb(224, 122, 158)),
    ("black", egui::Color32::from_rgb(45, 45, 45)),
    ("white", egui::Color32::from_rgb(228, 228, 228)),
    ("gray", egui::Color32::from_rgb(140, 140, 140)),
];

/// 颜色名 -> Color32(未知/None -> None)。
fn tag_color(name: Option<&str>) -> Option<egui::Color32> {
    let key = name?.trim().to_ascii_lowercase();
    TAG_COLORS.iter().find(|(n, _)| *n == key).map(|(_, c)| *c)
}

fn folder_of(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 从**已落盘的缩略图**加载纹理(不碰原图)。
///
/// P0-1 关键:原实现 `image::open(path)` 直接解原图,网格每帧每个可见格子
/// 都重解码一次 JPEG,是帧率杀手。现在只读缓存里的小图。
///
/// P1-9:按档位读(`tier`)。不同档位落在不同目录
/// (三张缩略图缓存表)。
fn thumb_tex(ctx: &egui::Context, id: i64, tier: crate::thumbs::Tier) -> Option<egui::TextureHandle> {
    let img = crate::thumbs::load_cached_tier(tier, id)?;
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    // 纹理名带档位 —— egui 每次 alloc 都产生新 TextureId,同名不复用,
    // 名字带档位便于排查"是不是加载了错档的图"。
    Some(ctx.load_texture(
        format!("thumb-{tier:?}-{id}"),
        color,
        egui::TextureOptions::default(),
    ))
}

fn load_cjk_font(ctx: &egui::Context) {
    let candidates = [
        r"C:\Windows\Fonts\msyh.ttc",   // 微软雅黑(ttc,ab_glyph 兼容时优先)
        r"C:\Windows\Fonts\simhei.ttf", // 黑体(纯 ttf,最稳妥)
        r"C:\Windows\Fonts\simsun.ttc", // 宋体
    ];
    let mut fonts = egui::FontDefinitions::default();
    for path in candidates {
        let Ok(data) = std::fs::read(path) else {
            continue;
        };
        fonts
            .font_data
            .insert("cjk".into(), std::sync::Arc::new(egui::FontData::from_owned(data)));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push("cjk".into());
        }
        ctx.set_fonts(fonts);
        return;
    }
    eprintln!("未找到系统中文字体,界面中文可能显示为方块");
}

/// 毫秒时间戳 → 本地时间字符串(变更历史用)。
fn format_ts(ms: i64) -> String {
    // 不引 chrono:用 SystemTime 的 UNIX_EPOCH 差值 + 手算日期。
    // 只到分钟精度即可,秒级用简易进位。
    let secs = ms / 1000;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // 1970-01-01 起的天数 → 年/月/日(简化:按 365.2425 天估年,再修正)。
    let mut y = 1970i64;
    let mut d = days;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let yd = if leap { 366 } else { 365 };
        if d < yd {
            break;
        }
        d -= yd;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let ml = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m_idx = 0usize;
    while m_idx < 12 && d >= ml[m_idx] {
        d -= ml[m_idx];
        m_idx += 1;
    }
    format!(
        "{y:04}-{:02}-{:02} {h:02}:{m:02}:{s:02}",
        m_idx + 1,
        d + 1
    )
}

/// 视图 > 在资源管理器中显示:在资源管理器中定位并选中文件(不打开文件)。
fn show_in_explorer(path: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let p = path.replace('/', "\\");
        let _ = std::process::Command::new("explorer")
            .raw_arg(format!("/select,\"{p}\""))
            .spawn();
    }
    #[cfg(not(windows))]
    {
        if let Some(parent) = Path::new(path).parent() {
            let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
        }
    }
}

/// 缩略图纹理缓存(带 LRU 上限 + 占位追踪)。
///
/// P0-1:用块文件 + 纹理缓存,不会随浏览量无限增长。
/// 我方原先用裸 `HashMap<i64, TextureHandle>`,每张看过的照片常驻一张 256×256
/// 纹理(约 256KB),几千张就把显存撑爆;这里加 LRU 上限,超出时淘汰最久未用的。
struct TexCache {
    map: HashMap<i64, egui::TextureHandle>,
    /// 访问顺序(队首最久未用)。
    order: Vec<i64>,
    /// 已请求过但**还没有**落盘缩略图的照片 id(显示灰色占位块)。
    /// 后台 worker 生成完后从 `update` 里清掉,让格子重新加载真图。
    pending: std::collections::HashSet<i64>,
    cap: usize,
}

impl TexCache {
    fn new(cap: usize) -> Self {
        TexCache {
            map: HashMap::new(),
            order: Vec::new(),
            pending: std::collections::HashSet::new(),
            cap,
        }
    }

    /// 取纹理并标记为最近使用。
    fn get(&mut self, id: i64) -> Option<egui::TextureHandle> {
        let t = self.map.get(&id)?.clone();
        if let Some(p) = self.order.iter().position(|x| *x == id) {
            self.order.remove(p);
        }
        self.order.push(id);
        Some(t)
    }

    /// 插入纹理;超出上限时淘汰最久未用的(egui 的 `TextureHandle` drop 会释放显存)。
    fn put(&mut self, id: i64, t: egui::TextureHandle) {
        self.pending.remove(&id);
        if self.map.insert(id, t).is_none() {
            self.order.push(id);
        } else if let Some(p) = self.order.iter().position(|x| *x == id) {
            self.order.remove(p);
            self.order.push(id);
        }
        while self.map.len() > self.cap {
            let Some(old) = self.order.first().copied() else {
                break;
            };
            self.order.remove(0);
            self.map.remove(&old);
        }
    }

    /// 该 id 是否正在等后台生成缩略图(是则不重复去磁盘找)。
    fn is_pending(&self, id: i64) -> bool {
        self.pending.contains(&id)
    }

    /// 标记为"等后台生成"。
    fn mark_pending(&mut self, id: i64) {
        self.pending.insert(id);
    }

    /// 后台生成完成:清掉占位标记,让格子下次重绘时加载真图。
    fn clear_pending(&mut self, ids: &[i64]) {
        for id in ids {
            self.pending.remove(id);
        }
    }

    /// 只保留仍然存在的照片 id(重建视图时调用)。
    fn retain(&mut self, alive: &BTreeSet<i64>) {
        self.map.retain(|k, _| alive.contains(k));
        self.order.retain(|k| alive.contains(k));
        self.pending.retain(|k| alive.contains(k));
    }

    /// 清空全部纹理(P1-9 切换档位时调用)。
    ///
    /// 必须全清 —— 旧档的纹理内容尺寸不同,复用会导致显示错档;
    /// `drop` 掉 `TextureHandle` 会让 egui 真正释放显存。
    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.pending.clear();
    }

    /// 调整 LRU 上限(P1-9 切大档时按显存预算下调)。
    fn set_cap(&mut self, cap: usize) {
        self.cap = cap;
        // 立刻收缩:多出来的纹理现在就该释放显存。
        while self.map.len() > self.cap {
            let Some(old) = self.order.first().copied() else {
                break;
            };
            self.order.remove(0);
            self.map.remove(&old);
        }
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, store: Arc<Store>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        load_cjk_font(&cc.egui_ctx);
        let app = Self {
            store,
            rows: Vec::new(),
            months: Vec::new(),
            folders: Vec::new(),
            selected_folder: 0,
            view: ViewKind::Timeline,
            texs: TexCache::new(TEX_CACHE_CAP),
            thumb_bg: None,
            scan_busy: Arc::new(AtomicBool::new(false)),
            scan_rx: None,
            scan_ctrl: None,
            scan_scope: String::new(),
            scroll_throttle: crate::scroll::ScrollSmoother::new(600.0, 0.0),
            thumb_decode_budget: crate::scroll::MAX_THUMB_DECODE_PER_FRAME,
            thumb_decode_count: 0,
            last_scroll_y: 0.0,
            last_viewport_h: 600.0,
            last_content_h: 0.0,
            gallery_scroll_target: None,
            device_watch: None,
            last_volumes: Vec::new(),
            fx_panel: false,
            fx_draft: crate::effects::Adjust::default(),
            fx_draft_id: None,
            fx_clipboard: None,
            fx_undo: HashMap::new(),
            fx_msg: String::new(),
            export_with_effects: false,
            email_win: false,
            email_size: crate::share::ShareSize::Large,
            cd_win: false,
            cd_size: crate::share::ShareSize::Small,
            share_rx: None,
            preview_base: None,
            fx_applied_sig: None,
            fx_curve_draft: crate::curves::Curves::new(),
            fx_curve_draft_id: None,
            fx_oneclick: None,
            fx_grain_amount: 0.15,
            fx_sharpen_amount: 60.0,
            fx_tone_amount: 50.0,
            fx_recipe: None,
            fx_recipe_amount: 100.0,
            fx_trim_start: 0.0,
            fx_trim_end: 0.0,
            fx_trim_total: 0.0,
            fx_trim_id: None,
            pool_probe_id: 0,
            show_import_devices: false,
            import_devices: Vec::new(),
            import_devices_err: String::new(),
            status: "就绪".into(),
            dups: Vec::new(),
            dup_near: false,
            id_row: HashMap::new(),
            preview: None,
            preview_tex: None,
            preview_bg: None,
            bake_rx: None,
            input_tag: String::new(),
            export_rx: None,
            wifi: None,
            wifi_panel: false,
            sel: BTreeSet::new(),
            sort_mode: SortMode::Date,
            sort_rev: false,
            thumb_tier: crate::thumbs::Tier::Normal,
            tags: Vec::new(),
            sel_tag: None,
            show_about: false,
            show_shortcuts: false,
            confirm_remove: false,
            show_folder_mgr: false,
            exclude_input: String::new(),
            watched: Vec::new(),
            confirm_folder_remove: None,
            maint_rx: None,
            show_changes: false,
            change_log: Vec::new(),
            show_uid_lookup: false,
            uid_input: String::new(),
            uid_lookup_result: None,
            watch_rx: None,
            watch_mode: None,
            watch_on: Arc::new(AtomicBool::new(true)),
            #[cfg(feature = "ai")]
            ocr_rx: None,
            #[cfg(feature = "ai")]
            ocr_panel: false,
            #[cfg(feature = "ai")]
            ocr_results: Vec::new(),
            #[cfg(feature = "ai")]
            ocr_photo: None,
            show_face_boxes: true,
            show_ocr_boxes: true,
            preview_boxes: None,
            #[cfg(feature = "ai")]
            face_rx: None,
            #[cfg(feature = "ai")]
            face_people: Vec::new(),
            #[cfg(feature = "ai")]
            sel_cluster: None,
            #[cfg(feature = "ai")]
            face_hits: Vec::new(),
            #[cfg(feature = "ai")]
            face_sugs: Vec::new(),
            #[cfg(feature = "ai")]
            face_hide_low: false,
            #[cfg(feature = "ai")]
            face_show_ignored: false,
            #[cfg(feature = "ai")]
            face_version_warn: false,
            #[cfg(feature = "ai")]
            input_rename: String::new(),
            #[cfg(feature = "ai")]
            clip_index_rx: None,
            #[cfg(feature = "ai")]
            clip_search_rx: None,
            #[cfg(feature = "ai")]
            clip_results: Vec::new(),
            #[cfg(feature = "ai")]
            clip_lang: crate::clip::Lang::En,
            #[cfg(feature = "ai")]
            input_query: String::new(),
        };
        app
    }

    pub fn start_initial_tasks(&mut self) {
        // 后台缩略图补齐(持续 worker:启动时补存量,扫描后自动补新增)
        self.thumb_bg = Some(crate::thumbs::spawn_worker(self.store.clone()));
        // P1-10:预览后台 worker(解码 + 落盘 previews/ 缓存,不阻塞 UI)
        self.preview_bg = Some(crate::thumbs::spawn_preview_worker(self.store.clone()));
        // 启动时静默导入新照片的 XMP sidecar(外部工具写的星标/标签,DB 权威不覆盖)
        self.start_xmp_import(false);
        // 监听文件夹(事件驱动 ReadDirectoryChangesW,不可用时自动降级轮询)
        let w = crate::watch::spawn(self.store.clone(), self.watch_on.clone());
        self.watch_rx = Some(w.events);
        self.watch_mode = Some(w.mode);
        // P1-13:设备热插拔监听(WM_DEVICECHANGE 链路)。
        // 新接入的相机/手机/可移动盘 → 自动扫描该卷。
        self.device_watch = Some(crate::device::spawn_device_watcher());
        self.last_volumes = crate::device::removable_volumes();
        self.rebuild();
        // P2-15:上次中断的扫描(持久化队列残留)自动续扫。
        // 正常完成/取消都会出队;本项目改用持久化队列,
        // 进程中途退出后仍可在下次启动时继续。
        let pending = self.store.pending_scans();
        // 参考件首启口径:首次运行只登记壳目录(我的文档/图片/桌面)
        // 并扫描一次,之后监听列表由用户在文件夹管理器自管。
        if self.store.ensure_first_run_scope() && pending.is_empty() {
            let roots = crate::db::default_roots();
            if !roots.is_empty() {
                self.start_scan_roots(roots, "常用目录");
            }
        }
        if !pending.is_empty() {
            let roots: Vec<PathBuf> = pending.iter().map(|(r, _)| PathBuf::from(r)).collect();
            let scope = if pending.len() == 1 {
                format!("续扫 {}", pending[0].0)
            } else {
                format!("续扫 {} 个上次中断的目录", pending.len())
            };
            self.start_scan_roots(roots, &scope);
        }
    }

    fn rebuild(&mut self) {
        let mut rows = timeline::load_all(&self.store).unwrap_or_default();
        match self.sort_mode {
            SortMode::Date => rows.sort_by(|a, b| b.taken.cmp(&a.taken).then(a.path.cmp(&b.path))),
            SortMode::Name => rows.sort_by(|a, b| a.path.cmp(&b.path)),
        }
        if self.sort_rev {
            rows.reverse();
        }
        self.months = timeline::by_month(&rows).into_iter().rev().collect(); // 新月份在前
        let mut seen = BTreeSet::new();
        let mut folders = Vec::new();
        for r in &rows {
            let f = folder_of(&r.path);
            if seen.insert(f.clone()) {
                folders.push(f);
            }
        }
        self.folders = folders;
        let alive: BTreeSet<i64> = rows.iter().map(|r| r.id).collect();
        self.texs.retain(&alive);
        self.sel.retain(|id| alive.contains(id));
        self.rows = rows;
        self.id_row = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| (r.id, i))
            .collect();
        self.dups = if self.dup_near {
            timeline::dup_groups_near(&self.store, NEAR_DUP_DIST).unwrap_or_default()
        } else {
            timeline::dup_groups(&self.store).unwrap_or_default()
        };
        self.tags = timeline::all_tags(&self.store).unwrap_or_default();
        self.watched = self.store.watched_stats();
        if self.selected_folder >= self.folders.len() {
            self.selected_folder = 0;
        }
    }

    /// 重扫当前全部监听目录。
    ///
    /// 参考件没有「扫全盘」概念:范围只经文件夹管理器扩大,
    /// 这里提供对已登记范围的一键重扫入口(不新增范围)。
    fn start_scan_full(&mut self) {
        let roots: Vec<PathBuf> = self
            .store
            .watched_stats()
            .into_iter()
            .map(|(p, _)| PathBuf::from(p))
            .collect();
        self.start_scan_roots(roots, "全部监听目录");
    }

    /// 启动扫描后台线程;scope 仅用于状态文案。
    ///
    /// P2-15:扫描根先登记进持久化队列,线程完成后移除 ——
    /// 应用中途退出时残留条目在下次启动自动续扫(`start_initial_tasks`)。
    fn start_scan_roots(&mut self, roots: Vec<PathBuf>, scope: &str) {
        if self.scan_busy.swap(true, Ordering::SeqCst) {
            // 忙时也登记:本次请求不丢,当前扫描结束后由续扫逻辑接管。
            let _ = self.store.enqueue_scan(&roots, scope);
            return;
        }
        let _ = self.store.enqueue_scan(&roots, scope);
        let store = self.store.clone();
        let busy = self.scan_busy.clone();
        let scope_owned = scope.to_string();
        let (tx, rx) = mpsc::channel();
        self.scan_rx = Some(rx);
        // P1-8:进度与取消控制(进度回调 + 退出检查)。
        let ctrl = Arc::new(crate::scan::ScanControl::new());
        self.scan_ctrl = Some(ctrl.clone());
        self.scan_scope = scope_owned.clone();
        self.status = format!("正在扫描{scope}…");
        std::thread::spawn(move || {
            let res = crate::scan::scan_dir_ctrl(&store, &roots, &ctrl);
            busy.store(false, Ordering::SeqCst);
            // P2-15:无论成功/失败/取消都出队(取消是用户意志,不续扫);
            // 只有线程根本没跑完(进程退出)才会残留 → 下次启动续扫。
            for r in &roots {
                let _ = store.dequeue_scan(&r.to_string_lossy());
            }
            let msg = match res {
                Ok(s) => ScanResult {
                    scanned: s.scanned,
                    indexed: s.indexed,
                    unchanged: s.unchanged,
                    excluded_dirs: s.excluded_dirs,
                    cancelled: s.cancelled,
                    err: None,
                    scope: scope_owned,
                },
                Err(e) => ScanResult {
                    scanned: 0,
                    indexed: 0,
                    unchanged: 0,
                    excluded_dirs: 0,
                    cancelled: false,
                    err: Some(e.to_string()),
                    scope: scope_owned,
                },
            };
            let _ = tx.send(msg);
        });
    }

    /// 文件 > 添加文件夹到图库:选目录 → 记入 watched → 只扫该目录。
    fn add_folder_to_library(&mut self) {
        let Some(dir) = rfd::FileDialog::new()
            .set_title("添加文件夹到图库")
            .pick_folder()
        else {
            return;
        };
        if let Err(e) = self.store.upsert_watched(&dir) {
            self.status = format!("记录文件夹失败(本次仍会扫描): {e}");
        }
        let scope = dir.display().to_string();
        self.start_scan_roots(vec![dir], &scope);
    }

    /// 星标/标签变更后写 XMP sidecar + picasa.ini(M3 互操作),不复制不移动原图。
    ///
    /// 数据库是权威源,XMP 只是 DB 的镜像输出。**首次导入**由
    /// `timeline::import_xmp_batch` 一次性完成(`xmp_imported` 标记,之后不再回灌),
    /// 否则删除标签后,旧标签会被 XMP 残留"复活"回数据库。
    fn sync_sidecars(&mut self, id: i64, star: i32) {
        let path = match self.rows.iter().find(|r| r.id == id) {
            Some(r) => r.path.clone(),
            None => return,
        };
        let photo = Path::new(&path);
        let tags = timeline::tags_of(&self.store, id).unwrap_or_default();
        let meta = crate::xmp::Meta { star, tags };
        let _ = crate::xmp::write(photo, &meta);
        let folder = folder_of(&path);
        let name = photo.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        let _ = crate::picasa::upsert(Path::new(&folder), &name, Some(star != 0), None);
        // 标签增删后刷新视图 > 标签 的左侧列表面板
        self.tags = timeline::all_tags(&self.store).unwrap_or_default();
    }

    /// 对当前选中项加/取消星标;无选中时作用于预览照片。
    fn star_selection(&mut self, star: i32) {
        let ids: Vec<i64> = if self.sel.is_empty() {
            match self.preview {
                Some(id) => vec![id],
                None => Vec::new(),
            }
        } else {
            self.sel.iter().copied().collect()
        };
        for id in ids {
            let _ = timeline::set_star(&self.store, id, star);
            if let Some(r) = self.rows.iter_mut().find(|r| r.id == id) {
                r.star = star;
            }
            self.sync_sidecars(id, star);
        }
    }

    /// 编辑 > 从图库中移除:只删索引,不删磁盘文件(有确认窗)。
    fn remove_selection(&mut self) {
        let ids: Vec<i64> = self.sel.iter().copied().collect();
        if ids.is_empty() {
            return;
        }
        let n = ids.len();
        match timeline::remove_photos(&self.store, &ids) {
            Ok(()) => self.status = format!("已从图库移除 {n} 张(未删除磁盘文件)"),
            Err(e) => self.status = format!("移除失败: {e}"),
        }
        self.sel.clear();
        self.preview = None;
        self.preview_tex = None;
        self.rebuild();
    }

    /// 启动保存烘培后台线程(备份+重编码+重缩略图)。
    fn start_bake(&mut self, id: i64) {
        if self.bake_rx.is_some() {
            self.status = "保存进行中,请稍候…".into();
            return;
        }
        let store = self.store.clone();
        let (tx, rx) = mpsc::channel();
        self.bake_rx = Some(rx);
        self.status = "正在保存(备份并重写原图)…".into();
        std::thread::spawn(move || {
            let res = crate::save::bake(&store, id)
                .and_then(|_| crate::thumbs::regen(&store, id))
                .map(|_| ())
                .map_err(|e| e.to_string());
            let _ = tx.send(res.map_err(|e| e.to_string()));
        });
    }

    /// 预览照片 OCR(M2b):后台线程加载模型并识别,结果弹窗显示。
    #[cfg(feature = "ai")]
    fn start_ocr(&mut self, id: i64, path: PathBuf) {
        let (tx, rx) = mpsc::channel::<Result<(i64, Vec<crate::ai::ocr::Txt>), String>>();
        self.ocr_rx = Some(rx);
        self.status = "正在 OCR 识别(首次需加载模型)…".into();
        std::thread::spawn(move || {
            let out = (|| -> Result<(i64, Vec<crate::ai::ocr::Txt>), String> {
                let img = image::open(&path).map_err(|e| e.to_string())?;
                let mut o = crate::ai::ocr::Ocr::load()?;
                Ok((id, o.extract(&img)?))
            })();
            let _ = tx.send(out);
        });
    }

    /// 后台人脸识别扫描(M2c):加载模型分析一批未分析照片并入库存簇。
    #[cfg(feature = "ai")]
    fn start_face_scan(&mut self) {
        if self.face_rx.is_some() {
            self.status = "人脸扫描进行中…".into();
            return;
        }
        let store = self.store.clone();
        let (tx, rx) = mpsc::channel();
        self.face_rx = Some(rx);
        self.status = "正在人脸识别(扫描 200 张照片)…".into();
        std::thread::spawn(move || {
            let res = crate::faces::scan_batch(&store, 200).map_err(|e| e.to_string());
            let _ = tx.send(res.map_err(|e| e.to_string()));
        });
    }

    /// 刷新人脸簇列表(扫描或重命名后调用)。
    #[cfg(feature = "ai")]
    fn refresh_people(&mut self) {
        self.face_people = crate::faces::people_overview(&self.store);
        self.check_face_algo_version();
    }

    /// 刷新簇列表 + 命名建议(建议阈值 0.55)。
    #[cfg(feature = "ai")]
    fn refresh_people_and_suggestions(&mut self) {
        self.refresh_people();
        self.face_sugs = crate::faces::face_suggestions(&self.store, 0.55);
    }

    /// 人脸算法版本检查(算法版本迁移):库中记录的版本与当前
    /// 算法版本不一致时弹窗,建议清除旧人脸数据重扫。
    #[cfg(feature = "ai")]
    fn check_face_algo_version(&mut self) {
        match self.store.get_setting("face_algo_version") {
            None => {
                let _ = self
                    .store
                    .set_setting("face_algo_version", crate::faces::FACE_ALGO_VERSION);
            }
            Some(v) if v != crate::faces::FACE_ALGO_VERSION => {
                self.face_version_warn = true;
            }
            _ => {}
        }
    }

    /// 后台语义索引(M4b):CLIP 图像编码器嵌入一批未处理照片(较慢)。
    /// lang = 索引语言(英文/中文模型各自独立索引)。
    #[cfg(feature = "ai")]
    fn start_clip_index(&mut self) {
        if self.clip_index_rx.is_some() {
            self.status = "语义索引进行中…".into();
            return;
        }
        let store = self.store.clone();
        let lang = self.clip_lang;
        let (tx, rx) = mpsc::channel();
        self.clip_index_rx = Some(rx);
        self.status = format!("正在建立语义索引({}模型,较慢)…", lang.label());
        std::thread::spawn(move || {
            let res = crate::clip::scan_batch_lang(&store, lang, 200).map_err(|e| e.to_string());
            let _ = tx.send(res.map_err(|e| e.to_string()));
        });
    }

    /// 后台语义搜索(M4b):编码查询文本,与入库嵌入算余弦排序。
    /// 查询语言自动探测(含 CJK → 中文模型);对应语言无索引时给出提示。
    #[cfg(feature = "ai")]
    fn start_clip_search(&mut self, query: String) {
        if self.clip_search_rx.is_some() {
            self.status = "语义搜索进行中…".into();
            return;
        }
        let lang = crate::clip::Lang::detect(&query);
        if crate::clip::clip_count_lang(&self.store, lang) == 0 {
            self.status = format!(
                "{}模型还没有语义索引 — 请先在下方选择「{}」模型并点「语义索引」",
                lang.label(),
                lang.label()
            );
            return;
        }
        let store = self.store.clone();
        let (tx, rx) = mpsc::channel();
        self.clip_search_rx = Some(rx);
        self.status = format!("正在语义搜索({}模型):{query}", lang.label());
        std::thread::spawn(move || {
            let res = crate::clip::search_lang(&store, lang, &query, 200);
            let _ = tx.send(res.map_err(|e| e.to_string()));
        });
    }

    /// 当前视图范围内的照片 id(选择/导出的作用范围与视图一致)。
    fn view_ids(&self) -> Vec<i64> {
        match self.view {
            ViewKind::Timeline => self.rows.iter().map(|r| r.id).collect(),
            ViewKind::Folder => {
                let folder = self
                    .folders
                    .get(self.selected_folder)
                    .cloned()
                    .unwrap_or_default();
                self.rows
                    .iter()
                    .filter(|r| folder_of(&r.path) == folder)
                    .map(|r| r.id)
                    .collect()
            }
            ViewKind::Duplicates => self
                .dups
                .iter()
                .flat_map(|(_, ids)| ids.iter().copied())
                .collect(),
            #[cfg(feature = "ai")]
            ViewKind::Faces => {
                // 一张照片可能有多个人脸框,去重后再作为选择/导出范围
                let mut seen = BTreeSet::new();
                self.face_hits
                    .iter()
                    .map(|h| h.photo_id)
                    .filter(|id| seen.insert(*id))
                    .collect()
            }
            #[cfg(not(feature = "ai"))]
            ViewKind::Faces => Vec::new(),
            ViewKind::Tags => self
                .sel_tag
                .as_deref()
                .and_then(|t| timeline::photos_by_tag(&self.store, t).ok())
                .unwrap_or_default(),
            #[cfg(feature = "ai")]
            ViewKind::Semantic => {
                let mut seen = BTreeSet::new();
                self.clip_results
                    .iter()
                    .map(|(id, _)| *id)
                    .filter(|id| seen.insert(*id))
                    .collect()
            }
            #[cfg(not(feature = "ai"))]
            ViewKind::Semantic => Vec::new(),
        }
    }

    /// 当前视图范围内的照片原图路径(导出范围与视图一致)。
    fn view_paths(&self) -> Vec<PathBuf> {
        self.view_ids()
            .into_iter()
            .filter_map(|id| self.id_row.get(&id).copied())
            .map(|i| PathBuf::from(&self.rows[i].path))
            .collect()
    }

    /// P2-18:分享条目 = 当前视图 + 可选效果(与导出同一作用域语义)。
    fn share_items(&self) -> Vec<(PathBuf, crate::effects::FilterStack)> {
        let paths = self.view_paths();
        if self.export_with_effects {
            self.rows
                .iter()
                .filter(|r| paths.iter().any(|p| *p == r.path))
                .map(|r| {
                    let st = self
                        .store
                        .get_filters(r.id)
                        .map(|s| crate::effects::FilterStack::parse(&s))
                        .unwrap_or_default();
                    (PathBuf::from(&r.path), st)
                })
                .collect()
        } else {
            paths
                .into_iter()
                .map(|p| (p, crate::effects::FilterStack::new()))
                .collect()
        }
    }

    /// P2-18:后台线程暂存邮件附件并调 Simple MAPI(附件暂存目录链路)。
    fn start_email(&mut self) {
        if self.share_rx.is_some() {
            self.status = "已有分享任务进行中…".into();
            return;
        }
        let items = self.share_items();
        if items.is_empty() {
            self.status = "当前视图没有可发送的照片".into();
            return;
        }
        let size = self.email_size;
        let dir = std::env::temp_dir().join("maliang_email");
        let (tx, rx) = mpsc::channel();
        self.share_rx = Some(rx);
        self.status = format!("正在准备 {} 张照片的邮件附件…", items.len());
        std::thread::spawn(move || {
            let res = (|| -> anyhow::Result<String> {
                let files = crate::share::stage_jpegs(&items, size.px(), &dir)?;
                crate::share::send_email("Maliang 照片分享", &files)?;
                Ok(format!("邮件已交给系统邮件客户端(附件 {} 张)", files.len()))
            })();
            let _ = tx.send(res.map_err(|e| e.to_string()));
        });
    }

    /// P2-18:后台线程暂存礼品光盘内容并走 IMAPI2 刻录。
    fn start_gift_cd(&mut self) {
        if self.share_rx.is_some() {
            self.status = "已有分享任务进行中…".into();
            return;
        }
        let items = self.share_items();
        if items.is_empty() {
            self.status = "当前视图没有可刻录的照片".into();
            return;
        }
        let size = self.cd_size;
        let dir = std::env::temp_dir().join("maliang_giftcd");
        let (tx, rx) = mpsc::channel();
        self.share_rx = Some(rx);
        self.status = format!("正在准备礼品光盘内容({} 张)…", items.len());
        std::thread::spawn(move || {
            let res = (|| -> anyhow::Result<String> {
                let files = crate::share::stage_gift_cd(&items, size.px(), &dir)?;
                crate::share::burn_folder(&dir, "Maliang CD", "Maliang")?;
                Ok(format!("刻录完成(共 {} 张)", files.len()))
            })();
            let _ = tx.send(res.map_err(|e| e.to_string()));
        });
    }

    /// 弹出目录选择,后台线程把当前视图的原图复制过去。
    fn start_export(&mut self) {
        if self.export_rx.is_some() {
            self.status = "导出进行中…".into();
            return;
        }
        let paths = self.view_paths();
        if paths.is_empty() {
            self.status = "当前视图没有可导出的照片".into();
            return;
        }
        let Some(dir) = rfd::FileDialog::new()
            .set_title("选择导出目录")
            .pick_folder()
        else {
            return;
        };
        // P2-16:导出可选择"套用已保存的效果"(不修改原图)。
        let with_fx = self.export_with_effects;
        let items: Vec<(PathBuf, crate::effects::FilterStack)> = if with_fx {
            self.rows
                .iter()
                .filter(|r| paths.iter().any(|p| *p == r.path))
                .map(|r| {
                    let st = self
                        .store
                        .get_filters(r.id)
                        .map(|s| crate::effects::FilterStack::parse(&s))
                        .unwrap_or_default();
                    (PathBuf::from(&r.path), st)
                })
                .collect()
        } else {
            paths
                .iter()
                .map(|p| (p.clone(), crate::effects::FilterStack::new()))
                .collect()
        };
        let (tx, rx) = mpsc::channel();
        self.export_rx = Some(rx);
        self.status = format!("正在导出 {} 张…", items.len());
        std::thread::spawn(move || {
            let (ok, errs) = if with_fx {
                crate::export::copy_all_with_effects(&items, &dir)
            } else {
                // 无效果 → 走纯复制(更快,且不改像素)。
                let plain: Vec<PathBuf> = items.into_iter().map(|(p, _)| p).collect();
                crate::export::copy_all(&plain, &dir)
            };
            let _ = tx.send((ok, errs));
        });
    }

    /// 文件 > 导入 > 从手机(WiFi)导入:未启动则启动并打开面板,已启动则停止。
    fn toggle_wifi(&mut self) {
        if let Some(h) = self.wifi.take() {
            h.stop();
            self.status = "WiFi 导入已停止".into();
        } else {
            match crate::net::start(self.store.clone()) {
                Ok(h) => {
                    self.status = format!("WiFi 导入已启动:{}/", h.addr);
                    let _ = self.store.upsert_watched(&crate::net::incoming_dir());
                    self.wifi = Some(h);
                    self.wifi_panel = true;
                    self.watched = self.store.watched_stats();
                }
                Err(e) => {
                    self.status = format!("WiFi 导入启动失败: {e}");
                }
            }
        }
    }

    /// 文件 > 导入 > 从设备(相机/手机):打开统一设备列表窗口。
    ///
    /// P1-13:WPD 设备 + 可移动卷合成一个统一列表。
    /// 枚举在**打开时**做一次(懒),之后靠热插拔事件失效缓存。
    fn open_import_devices(&mut self) {
        self.refresh_import_devices();
        self.show_import_devices = true;
    }

    /// 重新枚举设备(WPD 失败不中断 —— 只清标志,继续用盘符卷通道)。
    fn refresh_import_devices(&mut self) {
        let out = crate::wpd::devices_unified();
        self.import_devices_err = String::new();
        self.import_devices = out;
    }

    /// 工具 > 文件夹管理器:打开窗口并刷新列表。
    fn open_folder_mgr(&mut self) {
        self.watched = self.store.watched_stats();
        self.show_folder_mgr = true;
    }

    /// P1-12:重新读取变更历史(最近 500 条)。
    fn reload_changes(&mut self) {
        self.change_log = self.store.recent_changes(500);
    }

    /// P1-11:按输入的 uid64 查照片路径。
    fn lookup_uid(&mut self) -> Result<String, String> {
        let t = self.uid_input.trim();
        if t.is_empty() {
            return Err("请输入 uid64".into());
        }
        let raw = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X"));
        let uid = match raw {
            Some(hex) => u64::from_str_radix(hex, 16).map_err(|e| format!("十六进制解析失败: {e}"))?,
            None => t.parse::<u64>().map_err(|e| format!("数字解析失败: {e}"))?,
        };
        // uid64 存的是 i64(高位为 1 时是负数)→ 还原成 u64。
        let id = self
            .store
            .photo_by_uid64(uid)
            .ok_or_else(|| format!("未找到 uid64 = {uid} 的照片"))?;
        let path: String = self
            .store
            .0
            .lock()
            .unwrap()
            .query_row("SELECT path FROM photos WHERE id=?1", [id], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        Ok(path)
    }

    /// P1-9:切换缩略图档位。    ///
    /// 三件事:
    /// 1. 清空纹理缓存(旧档的纹理不能复用 —— egui 的 TextureId 按 alloc 分配);
    /// 2. 通知后台 worker 改补新档(切档时按需生成对应缓存表);
    /// 3. 清占位集(新档的缓存尚未生成,需要重新走"缺 → 标记待生成"流程)。
    fn set_thumb_tier(&mut self, tier: crate::thumbs::Tier) {
        if self.thumb_tier == tier {
            return;
        }
        self.thumb_tier = tier;
        self.texs.clear();
        // 大档像素多 → 按显存预算下调 LRU 上限(避免显存爆)。
        self.texs.set_cap(tier.suggested_cache_cap());
        if let Some(w) = &self.thumb_bg {
            w.tier.store(tier as u8, Ordering::Relaxed);
        }
        self.status = format!("已切换到{}", tier.label());
    }

    // ===== P2-16 效果系统 =====

    /// 读某张照片已保存的效果栈(`photos.filters` 列)。
    fn fx_stack_for(&self, id: i64) -> crate::effects::FilterStack {
        match self.store.get_filters(id) {
            Some(s) => crate::effects::FilterStack::parse(&s),
            None => crate::effects::FilterStack::new(),
        }
    }

    /// 预览实际渲染用的效果栈 = 已存效果 + 当前草稿(草稿覆盖同名项)。
    ///
    /// "参数 + op 树实时渲染":滑块一动 op 树立刻重建 → 我方等价实现:
    /// 预览纹理签名随草稿变化而重算。
    fn fx_preview_stack(&self, id: i64) -> crate::effects::FilterStack {
        let mut st = self.fx_stack_for(id);
        if Some(id) == self.preview {
            if !self.fx_draft.is_identity() {
                st.push(crate::effects::Effect::enhance(&self.fx_draft));
            }
            if !self.fx_curve_draft.is_identity() {
                st.push(crate::effects::Effect::curves(&self.fx_curve_draft));
            }
            if let Some(name) = &self.fx_oneclick {
                st.push(crate::effects::Effect::oneclick(
                    name,
                    Default::default(),
                ));
            }
        }
        st
    }

    /// 把草稿调整写入选中(或当前预览)的照片,并记入撤销栈。
    ///
    /// 提交一个命名编辑项 → 入撤销栈。
    fn fx_apply_draft(&mut self, targets: &[i64]) {
        if targets.is_empty() {
            self.fx_msg = "没有可应用的照片".into();
            return;
        }
        let adj = self.fx_draft.clamped();
        // 恒等调整不入栈(空效果不建编辑项)。
        if crate::effects::Effect::enhance(&adj).is_identity() {
            self.fx_msg = "当前调整全为默认值,未应用".into();
            return;
        }
        // 先记下提交前的栈(撤销用),再统一写入。
        let befores: Vec<(i64, crate::effects::FilterStack)> = targets
            .iter()
            .map(|id| (*id, crate::effects::read_stack(&self.store, *id)))
            .collect();
        let n = match crate::effects::commit_adjust(&self.store, targets, &adj) {
            Ok(n) => n,
            Err(e) => {
                self.fx_msg = format!("写入失败: {e}");
                return;
            }
        };
        for (id, before) in befores {
            let after = crate::effects::read_stack(&self.store, id);
            let stack = self
                .fx_undo
                .entry(id)
                .or_insert_with(crate::effects::EditStack::new);
            stack.commit("增强", before, after);
        }
        // 预览纹理需要重算(签名变化即触发)。
        self.fx_applied_sig = None;
        self.fx_msg = format!("已应用到 {n} 张照片");
    }

    /// 撤销一步。
    fn fx_undo_step(&mut self, id: i64) {
        let Some(stack) = self.fx_undo.get_mut(&id) else {
            self.fx_msg = "无可撤销的操作".into();
            return;
        };
        match crate::effects::undo_commit(&self.store, stack, id) {
            Some((label, _)) => {
                self.fx_applied_sig = None;
                self.fx_msg = format!("已撤销: {label}");
            }
            None => self.fx_msg = "无可撤销的操作".into(),
        }
    }

    /// 重做一步。
    fn fx_redo_step(&mut self, id: i64) {
        let Some(stack) = self.fx_undo.get_mut(&id) else {
            self.fx_msg = "无可重做的操作".into();
            return;
        };
        match crate::effects::redo_commit(&self.store, stack, id) {
            Some((label, _)) => {
                self.fx_applied_sig = None;
                self.fx_msg = format!("已重做: {label}");
            }
            None => self.fx_msg = "无可重做的操作".into(),
        }
    }

    /// 复制所有效果:从**单张**照片取效果栈到内存暂存。
    ///
    /// 选中数量校验要求"恰好选中一张"(提示语
    /// "Please select a single picture to copy the effects from.")。
    fn fx_copy_all(&mut self, source: i64) {
        let st = crate::effects::copy_from(&self.store, source);
        self.fx_clipboard = Some(st.clone());
        self.fx_msg = format!("已复制效果({} 项)", st.items.len());
    }

    /// 粘贴所有效果:把暂存的效果栈写给选中的多张照片。
    ///
    /// 效果批量应用对电影类型跳过(我方库内只有照片,天然满足)。
    fn fx_paste_all(&mut self, targets: &[i64]) {
        let Some(st) = self.fx_clipboard.clone() else {
            self.fx_msg = "剪贴板里没有效果".into();
            return;
        };
        if targets.is_empty() {
            self.fx_msg = "没有选中的照片".into();
            return;
        }
        match crate::effects::paste_to(&self.store, targets, &st) {
            Ok(n) => {
                self.fx_applied_sig = None;
                self.fx_msg = format!("已粘贴效果到 {n} 张照片");
            }
            Err(e) => self.fx_msg = format!("粘贴失败: {e}"),
        }
    }

    /// 效果面板 UI。
    fn fx_panel_ui(&mut self, ui: &mut egui::Ui) {
        let cur = self.preview;
        let Some(id) = cur else {
            ui.label("先打开一张照片");
            return;
        };
        // 打开新照片时,把已存的效果载入草稿(原版打开照片即显示其效果)。
        // 只在**换照片**时载入 —— 否则会把用户正在拖的滑块冲掉。
        if self.fx_draft_id != Some(id) {
            self.fx_draft_id = Some(id);
            let stored = self.fx_stack_for(id);
            self.fx_draft = stored.merged_adjust();
        }
        ui.heading("效果(增强)");
        ui.small("亮度 / 对比度 / 饱和度 / 曝光 / 暗部 / 锐化 / 模糊");
        let mut d = self.fx_draft;
        let mut changed = false;
        changed |= ui
            .add(egui::Slider::new(&mut d.brightness, -100.0..=100.0).text("亮度"))
            .changed();
        changed |= ui
            .add(egui::Slider::new(&mut d.contrast, -100.0..=100.0).text("对比度"))
            .changed();
        changed |= ui
            .add(egui::Slider::new(&mut d.saturation, -100.0..=100.0).text("饱和度"))
            .changed();
        changed |= ui
            .add(egui::Slider::new(&mut d.exposure, -3.0..=3.0).text("曝光(档)"))
            .changed();
        changed |= ui
            .add(egui::Slider::new(&mut d.blacks, -100.0..=100.0).text("暗部"))
            .changed();
        changed |= ui
            .add(egui::Slider::new(&mut d.sharpen, 0.0..=100.0).text("锐化"))
            .changed();
        changed |= ui
            .add(egui::Slider::new(&mut d.blur, 0.0..=20.0).text("模糊"))
            .changed();
        if changed {
            self.fx_draft = d;
            // 草稿变化 → 预览实时重算(不落库,属"未提交"状态)。
            self.fx_applied_sig = None;
        }
        ui.horizontal(|ui| {
            if ui.button("应用到选中").clicked() {
                let targets: Vec<i64> = if self.sel.is_empty() {
                    vec![id]
                } else {
                    self.sel.iter().copied().collect()
                };
                self.fx_apply_draft(&targets);
            }
            if ui.button("应用到这张").clicked() {
                self.fx_apply_draft(&[id]);
            }
            if ui.button("重置").clicked() {
                self.fx_draft = crate::effects::Adjust::default();
                self.fx_applied_sig = None;
            }
        });
        ui.separator();
        self.fx_curves_ui(ui, id);
        ui.separator();
        self.fx_oneclick_ui(ui, id);
        ui.separator();
        self.fx_trim_ui(ui, id);
        ui.separator();
        ui.horizontal(|ui| {
            let can_undo = self.fx_undo.get(&id).map(|s| s.can_undo()).unwrap_or(false);
            let can_redo = self.fx_undo.get(&id).map(|s| s.can_redo()).unwrap_or(false);
            if ui.add_enabled(can_undo, egui::Button::new("撤销")).clicked() {
                self.fx_undo_step(id);
            }
            if ui.add_enabled(can_redo, egui::Button::new("重做")).clicked() {
                self.fx_redo_step(id);
            }
            if ui.button("复制所有效果").clicked() {
                self.fx_copy_all(id);
            }
            let paste_targets: Vec<i64> = if self.sel.is_empty() {
                vec![id]
            } else {
                self.sel.iter().copied().collect()
            };
            if ui.button("粘贴所有效果").clicked() {
                self.fx_paste_all(&paste_targets);
            }
        });
        // 清除效果(移除栈中所有命名项 —— 逐项撤销到空栈)。
        if ui.button("清除这张的所有效果").clicked() {
            let before = self.fx_stack_for(id);
            if before.is_empty() {
                self.fx_msg = "这张照片没有效果".into();
            } else {
                let mut after = before.clone();
                while after.remove("enhance").is_some() {}
                let _ = self.store.set_filters(id, &after.serialize());
                let stack = self
                    .fx_undo
                    .entry(id)
                    .or_insert_with(crate::effects::EditStack::new);
                stack.commit("清除效果", before, after);
                self.fx_applied_sig = None;
                self.fx_draft = crate::effects::Adjust::default();
                self.fx_msg = "已清除效果".into();
            }
        }
        // 统计:整个视图里有多少张带效果(效果是记录级属性)。
        let view = self.view_ids();
        let with_fx = self.store.get_filters_many(&view).len();
        ui.small(format!("当前视图 {with_fx} 张带效果"));
        // 已应用的效果项清单(效果栈的每一项都是命名编辑项)。
        let st = self.fx_stack_for(id);
        if !st.is_empty() {
            ui.small("本张已应用:");
            for it in &st.items {
                // 参考里有但没做的项(如需笔刷交互)要显式标出,别让用户以为已生效
                let tag = match crate::effects::support_of(&it.name) {
                    crate::effects::Support::ReferenceOnly => "暂不支持",
                    crate::effects::Support::Unknown => "未知名",
                    crate::effects::Support::Pixel => {
                        if it.is_oneclick() {
                            "一键"
                        } else {
                            "调整"
                        }
                    }
                };
                ui.small(format!("  · {} [{}]", it.name, tag));
            }
        }
        if !self.fx_msg.is_empty() {
            ui.separator();
            ui.small(self.fx_msg.clone());
        }
        ui.separator();
        self.pool_maintenance_ui(ui);
    }

    /// P1-14 字符串池维护区。
    ///
    /// 字符串池:按列隔离、
    /// append-only、**无引用计数**,回收靠「写时重建」。
    /// 这个面板把内部状态暴露出来,并提供手动触发重建的入口。
    fn pool_maintenance_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("字符串池");
        ui.small("每列一个去重池,行记录只存 32 位 id;池按旧 id 升序重建(写时重建,无引用计数)");
        let stats = self.store.pool_stats().unwrap_or_default();
        if stats.is_empty() {
            ui.small("尚未池化任何列(扫描带 EXIF 相机型号的照片后会填充)");
            return;
        }
        let mut total_entries = 0i64;
        let mut total_refs = 0i64;
        for (pool, n, refs) in &stats {
            total_entries += *n;
            total_refs += *refs;
            ui.small(format!(
                "  · {pool}:池内 {n} 条/ 引用 {refs} 行  (去重率 {:.1}%)",
                if *refs > 0 { *n as f64 * 100.0 / *refs as f64 } else { 0.0 }
            ));
        }
        if total_refs > 0 {
            ui.small(format!(
                "合计:{} 条池条目承载 {total_refs} 行引用(平均压缩 {:.1}×)",
                total_entries,
                total_refs as f64 / total_entries.max(1) as f64
            ));
        }
        // 重建前的预估(纯函数,不改状态):能省下多少条孤儿
        if let (Ok(set), Ok(live)) = (self.store.load_pool_set(), self.store.live_pool_ids()) {
            for (pool, cur, alive) in crate::pool::vacuum_plan(&set, &live) {
                if cur > alive {
                    ui.small(format!("  · {pool}:可回收 {} 条孤儿({cur} → {alive})", cur - alive));
                }
            }
        }
        ui.horizontal(|ui| {
            if ui.button("重建池(回收孤儿)").clicked() {
                match self.store.vacuum_all_pools() {
                    Ok(report) => {
                        let mut removed = 0usize;
                        let mut remapped = 0usize;
                        for (_pool, (o, m)) in &report {
                            removed += o;
                            remapped += m;
                        }
                        self.fx_msg = format!(
                            "池重建完成:回收孤儿 {removed} 条,重映射 {remapped} 行"
                        );
                    }
                    Err(e) => self.fx_msg = format!("池重建失败:{e}"),
                }
            }
            if ui.button("查一个池 id").clicked() {
                // 诊断入口:按 id 查询池内字符串
                let id = self.pool_probe_id;
                match self.store.pooled_value(crate::timeline::CAM_POOL, id) {
                    Some(s) => self.fx_msg = format!("photos_cam[{id}] = {s}"),
                    None => self.fx_msg = format!("photos_cam[{id}] 不存在"),
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("池 id:");
            ui.add(egui::DragValue::new(&mut self.pool_probe_id).range(0..=9999));
        });
        ui.small(format!(
            "查重哈希(seed 0x12345678):hash(\"Nikon D850\") = 0x{:08x}",
            crate::pool::pool_hash("Nikon D850")
        ));
        // 内存池快照:展示池的当前内容
        if let Ok(set) = self.store.load_pool_set() {
            let total = set.total_entries();
            ui.small(format!(
                "内存池快照:{} 个池,共 {total} 条(去重后)",
                set.names().len()
            ));
            // 池 id = 值在池内的下标(查重哈希),按序展示
            let cam_vals: Vec<String> = set
                .column_values(crate::timeline::CAM_POOL)
                .map(<[String]>::to_vec)
                .unwrap_or_default();
            let preview: Vec<String> = cam_vals
                .iter()
                .take(8)
                .enumerate()
                .map(|(i, v)| format!("[{}]{v}", i as i64))
                .collect();
            if !preview.is_empty() {
                ui.small(format!("  · photos_cam: {}", preview.join(", ")));
            }
        }
            // 相机型号清单(走池解析):相册/搜索面板按相机分组就是靠池列。
        let cams = self.store.cams().unwrap_or_default();
        if !cams.is_empty() {
            ui.separator();
            ui.small(format!("相机型号(池列解析,共 {} 种):", cams.len()));
            egui::ScrollArea::vertical()
                .max_height(120.0)
                .show(ui, |ui| {
                    for (name, n) in &cams {
                        ui.small(format!("  · {name}  ×{n}"));
                    }
                });
        }
    }

    /// 曲线区 UI。
    ///
    /// 预设曲线数据为内置常量(与 Picasa 3 的滤镜观感对齐)。
    fn fx_curves_ui(&mut self, ui: &mut egui::Ui, id: i64) {
        if self.fx_curve_draft_id != Some(id) {
            self.fx_curve_draft_id = Some(id);
            // 从已存效果里读回曲线项(打开照片即显示其曲线)。
            let stored = self.fx_stack_for(id);
            self.fx_curve_draft = stored
                .items
                .iter()
                .find(|it| it.name.eq_ignore_ascii_case("curves"))
                .map(|it| it.curve_set())
                .unwrap_or_default();
        }
        ui.heading("色调曲线");
        ui.small("自然三次样条:Master 先算出公共值,RGB 三条曲线各自以同一值求值");
        // 内置滤镜预设
        let mut picked = 0usize;
        ui.horizontal_wrapped(|ui| {
            if ui.button("电影感").clicked() {
                picked = 1;
            }
            if ui.button("交叉冲洗").clicked() {
                picked = 2;
            }
            if ui.button("六十年代").clicked() {
                picked = 3;
            }
            if ui.button("反相").clicked() {
                picked = 4;
            }
            if ui.button("奥顿").clicked() {
                picked = 5;
            }
            if ui.button("素描").clicked() {
                picked = 6;
            }
            if ui.button("清空曲线").clicked() {
                picked = 7;
            }
        });
        if picked > 0 {
            self.fx_curve_draft = match picked {
                1 => crate::curves::presets::widescreen(),
                2 => crate::curves::presets::cross_develop(),
                3 => crate::curves::presets::retro(),
                4 => crate::curves::presets::invert(),
                5 => crate::curves::presets::soft_glow(50.0),
                6 => crate::curves::presets::pencil_sketch(100.0),
                _ => crate::curves::Curves::new(),
            };
            self.fx_applied_sig = None;
        }
        // 曝光档位(走 Master 槽的曝光整形点)
        let mut exp = self.fx_curve_draft.exposure;
        if ui
            .add(egui::Slider::new(&mut exp, -3.0..=3.0).text("曝光(档)"))
            .changed()
        {
            self.fx_curve_draft.exposure = exp;
            self.fx_applied_sig = None;
        }
        ui.horizontal(|ui| {
            if ui.button("应用曲线").clicked() {
                self.fx_commit_effect(id, "曲线", crate::effects::Effect::curves(&self.fx_curve_draft));
            }
            if ui.button("移除曲线").clicked() {
                self.fx_remove_effect(id, "曲线", "curves");
            }
        });
    }

    /// 一键效果区 UI。
    ///
    /// - `bw`/`sepia`/`grain`/`unsharp` 的算法已实现 → 可用。
    /// - `warm`/`cool`/`vignette`/`faded` 是本项目自主设计的调色 → 可用,
    ///   强度共用一个滑块(量程 0~100)。
    fn fx_oneclick_ui(&mut self, ui: &mut egui::Ui, id: i64) {
        ui.heading("一键效果");
        ui.horizontal_wrapped(|ui| {
            for (label, key) in [
                ("黑白", "bw"),
                ("棕褐色", "sepia"),
                ("颗粒", "grain"),
                ("锐化", "unsharp"),
                ("暖调", "warm"),
                ("冷调", "cool"),
                ("暗角", "vignette"),
                ("褪色", "faded"),
                ("扶正", "tilt"),
                ("暗部/高光", "shadow"),
            ] {
                let on = self.fx_oneclick.as_deref() == Some(key);
                if ui.selectable_label(on, label).clicked() {
                    if on {
                        self.fx_oneclick = None;
                    } else {
                        self.fx_oneclick = Some(key.to_string());
                    }
                    self.fx_applied_sig = None;
                }
            }
        });
        if self.fx_oneclick.as_deref() == Some("grain") {
            // 直接绑定状态字段,不用每帧重建的局部变量 ——
            // 否则滑块松手后会弹回初值(值其实存进去了,只是视觉上回跳)。
            if ui
                .add(
                    egui::Slider::new(&mut self.fx_grain_amount, 0.0..=1.0)
                        .text("颗粒强度"),
                )
                .changed()
            {
                self.fx_applied_sig = None;
            }
        }
        if self.fx_oneclick.as_deref() == Some("unsharp") {
            if ui
                .add(
                    egui::Slider::new(&mut self.fx_sharpen_amount, 0.0..=100.0)
                        .text("锐化强度"),
                )
                .changed()
            {
                self.fx_applied_sig = None;
            }
        }
        // 自主设计调色四件套共用一个强度滑块(语义都是"0 = 不动,100 = 最重")。
        if let Some(text) = match self.fx_oneclick.as_deref() {
            Some("warm") => Some("暖调强度"),
            Some("cool") => Some("冷调强度"),
            Some("vignette") => Some("暗角强度"),
            Some("faded") => Some("褪色强度"),
            // 扶正:0~100 折算到 ±15°,50 = 不旋转。
            Some("tilt") => Some("扶正角度"),
            // 暗部/高光:一根滑杆同时驱动两端。
            Some("shadow") => Some("修复强度"),
            _ => None,
        } {
            if ui
                .add(egui::Slider::new(&mut self.fx_tone_amount, 0.0..=100.0).text(text))
                .changed()
            {
                self.fx_applied_sig = None;
            }
        }
        ui.horizontal(|ui| {
            if ui.button("应用一键效果").clicked() {
                let Some(name) = self.fx_oneclick.clone() else {
                    self.fx_msg = "先选一个一键效果".into();
                    return;
                };
                let mut params = std::collections::BTreeMap::new();
                if name == "grain" {
                    // 用内置的颗粒参数预设。
                    let preset = crate::oneclick::grain_preset(0);
                    params.insert("amount".into(), format!("{:.4}", self.fx_grain_amount));
                    params.insert("seed".into(), format!("{}", preset.seed));
                    params.insert(
                        "channeloptions".into(),
                        format!("{}", preset.channel_options),
                    );
                    params.insert("grayscale".into(), preset.grayscale.to_string());
                }
                if name == "sepia" {
                    let (mix, shadow, highlight) = crate::oneclick::sepia_default();
                    params.insert("mix".into(), format!("{mix:.4}"));
                    params.insert(
                        "shadow".into(),
                        format!("{:02x}{:02x}{:02x}", shadow[0], shadow[1], shadow[2]),
                    );
                    params.insert(
                        "highlight".into(),
                        format!("{:02x}{:02x}{:02x}", highlight[0], highlight[1], highlight[2]),
                    );
                }
                if name == "unsharp" {
                    params.insert(
                        "amount".into(),
                        format!("{:.4}", self.fx_sharpen_amount),
                    );
                }
                // 自主设计调色:只有一个 amount 参数。
                if matches!(
                    name.as_str(),
                    "warm" | "cool" | "vignette" | "faded" | "tilt" | "shadow"
                ) {
                    params.insert("amount".into(), format!("{:.4}", self.fx_tone_amount));
                }
                self.fx_commit_effect(
                    id,
                    "一键效果",
                    crate::effects::Effect::oneclick(&name, params),
                );
            }
            if ui.button("移除一键效果").clicked() {
                for n in [
                    "bw", "sepia", "grain", "unsharp", "warm", "cool", "vignette", "faded",
                    "tilt", "shadow",
                ] {
                    self.fx_remove_effect(id, "一键效果", n);
                }
                self.fx_oneclick = None;
            }
        });

        // ===== 风格配方:一个名字对应一串固定顺序的操作 =====
        ui.separator();
        ui.label("风格配方(整组效果,强度控制与原图的混合比例)");
        ui.horizontal_wrapped(|ui| {
            for key in crate::recipes::RECIPES {
                let on = self.fx_recipe.as_deref() == Some(*key);
                if ui.selectable_label(on, crate::recipes::label(key)).clicked() {
                    if on {
                        self.fx_recipe = None;
                    } else {
                        self.fx_recipe = Some((*key).to_string());
                        self.fx_recipe_amount = crate::recipes::default_amount(key);
                    }
                    self.fx_applied_sig = None;
                }
            }
        });
        if self.fx_recipe.is_some() {
            let text = format!(
                "{} 强度",
                crate::recipes::label(self.fx_recipe.as_deref().unwrap_or(""))
            );
            if ui
                .add(egui::Slider::new(&mut self.fx_recipe_amount, 0.0..=100.0).text(text))
                .changed()
            {
                self.fx_applied_sig = None;
            }
        }
        ui.horizontal(|ui| {
            if ui.button("应用风格配方").clicked() {
                let Some(name) = self.fx_recipe.clone() else {
                    self.fx_msg = "先选一个风格配方".into();
                    return;
                };
                let mut params = std::collections::BTreeMap::new();
                params.insert("amount".into(), format!("{:.4}", self.fx_recipe_amount));
                self.fx_commit_effect(
                    id,
                    "风格配方",
                    crate::effects::Effect::oneclick(&name, params),
                );
            }
            if ui.button("移除风格配方").clicked() {
                for n in crate::recipes::RECIPES {
                    self.fx_remove_effect(id, "风格配方", n);
                }
                self.fx_recipe = None;
            }
        });
    }

    /// 视频裁剪面板(trim UI)。
    ///
    /// 单位说明:原版 `moviestart` / `movieend` 是 **100 纳秒计数**
    /// (10 000 000 = 1 秒);面板上用秒显示,落库时换算回 100 ns。
    /// 裁剪是**时间过滤**语义 —— 播放/导出只取这一段,不改原文件像素。
    fn fx_trim_ui(&mut self, ui: &mut egui::Ui, id: i64) {
        // 只有视频才显示(换照片时重载草稿)。
        let is_movie = self
            .rows
            .iter()
            .find(|r| r.id == id)
            .map(|r| crate::video::is_video(std::path::Path::new(&r.path)))
            .unwrap_or(false);
        if !is_movie {
            return;
        }
        if self.fx_trim_id != Some(id) {
            self.fx_trim_id = Some(id);
            let stored = self.fx_stack_for(id).serialize();
            match crate::video::MovieTrim::parse(&stored) {
                Some(t) => {
                    self.fx_trim_start = t.start_secs();
                    self.fx_trim_end = t.end_secs();
                }
                None => {
                    // 没有裁剪记录:终点先记 0,等用户按"读视频时长"填。
                    self.fx_trim_start = 0.0;
                    self.fx_trim_end = 0.0;
                }
            }
        }
        ui.heading("视频裁剪");
        ui.small("起点 / 终点(秒)。原版单位为 100 纳秒,此处按秒操作。");
        let mut changed = false;
        changed |= ui
            .add(
                egui::DragValue::new(&mut self.fx_trim_start)
                    .range(0.0..=f64::MAX)
                    .speed(0.1)
                    .prefix("起点 "),
            )
            .changed();
        changed |= ui
            .add(
                egui::DragValue::new(&mut self.fx_trim_end)
                    .range(0.0..=f64::MAX)
                    .speed(0.1)
                    .prefix("终点 "),
            )
            .changed();
        changed |= ui
            .add(
                egui::DragValue::new(&mut self.fx_trim_total)
                    .range(0.0..=f64::MAX)
                    .speed(0.5)
                    .prefix("总时长(0=不限) "),
            )
            .changed();
        let _ = changed;
        let raw = crate::video::MovieTrim::from_secs(self.fx_trim_start, self.fx_trim_end);
        // 填了总时长就按它钳位(并顺手把反序区间理顺)。
        let t = if self.fx_trim_total > 0.0 {
            raw.clamped_to(self.fx_trim_total)
        } else {
            raw
        };
        ui.small(format!(
            "区间 {} 秒({})",
            format!("{:.1}", t.duration_secs()),
            t.label()
        ));
        ui.small(format!("落库片段:{}", t.serialize()));
        ui.horizontal(|ui| {
            if ui.button("应用裁剪").clicked() {
                if t.is_empty() {
                    self.fx_msg = "终点需大于起点".into();
                    return;
                }
                self.fx_commit_effect(
                    id,
                    "视频裁剪",
                    crate::effects::Effect::oneclick("movie", t.to_params()),
                );
                self.fx_msg = "已应用裁剪".into();
            }
            if ui.button("清除裁剪").clicked() {
                self.fx_remove_effect(id, "视频裁剪", "movie");
                self.fx_trim_start = 0.0;
                self.fx_trim_end = 0.0;
            }
        });
    }

    /// 把一个效果项写进目标照片的效果栈并记入撤销栈。
    fn fx_commit_effect(&mut self, id: i64, label: &str, eff: crate::effects::Effect) {        let before = self.fx_stack_for(id);
        let mut after = before.clone();
        after.push(eff);
        if let Err(e) = self.store.set_filters(id, &after.serialize()) {
            self.fx_msg = format!("写入失败: {e}");
            return;
        }
        let stack = self
            .fx_undo
            .entry(id)
            .or_insert_with(crate::effects::EditStack::new);
        stack.commit(label, before, after);
        self.fx_applied_sig = None;
        self.fx_msg = format!("已应用: {label}");
    }

    /// 从目标照片的效果栈移除指定名字的效果项(并记入撤销栈)。
    fn fx_remove_effect(&mut self, id: i64, label: &str, name: &str) {
        let before = self.fx_stack_for(id);
        let mut after = before.clone();
        if after.remove(name).is_none() {
            return;
        }
        if let Err(e) = self.store.set_filters(id, &after.serialize()) {
            self.fx_msg = format!("写入失败: {e}");
            return;
        }
        let stack = self
            .fx_undo
            .entry(id)
            .or_insert_with(crate::effects::EditStack::new);
        stack.commit(label, before, after);
        self.fx_applied_sig = None;
    }

    /// P1-9:派发滚动事件(键盘翻页 / 跳到顶底)。
    ///
    /// 按事件名派发给对应的滚动处理;未匹配的事件直接忽略。
    /// 这里把事件语义映射到本项目的画廊滚动。
    fn dispatch_throttle_event(&mut self, event: &str) {
        // 目标偏移:0 = 顶,1 = 底,其它 = 翻一屏。
        let vh = self.last_viewport_h.max(1.0);
        let content = self.last_content_h.max(vh);
        let target = match event {
            "scroll/album_top" => 0.0,
            "scroll/album_bottom" => (content - vh).max(0.0),
            "scroll/page_up" => (self.last_scroll_y - vh).max(0.0),
            "scroll/page_down" => (self.last_scroll_y + vh).min((content - vh).max(0.0)),
            _ => return,
        };
        self.gallery_scroll_target = Some(target);
        self.status = format!("已派发 {event}");
    }

    /// P1-9:用位移阻尼曲线整形本帧滚动速度,
    /// 并据此决定"本帧允许同步解码多少张缩略图"。
    ///
    /// 设计要点:
    /// - 滚动时把"这一步该滚多少"整形成**平滑速度**,
    ///   保证缩略图网格快速滚动时不掉帧;
    /// - 其余量:`0.5` 是包围盒中点系数、`2.7` 是调用方增益、
    ///   `-2.4` 是阻尼系数、`0.3`/`4.25` 是两个 `powf` 指数、`100` 是分支阈值。
    ///
    /// 本项目用 egui `ScrollArea`(自带虚拟化与惯性),因此把原版的
    /// "限速"作用等价实现为**解码预算**:滚动越快 → 每帧同步解码越少。
    fn update_scroll_throttle(&mut self, offset_y: f32, viewport_h: f32, content_h: f32) {
        self.last_viewport_h = viewport_h;
        self.last_content_h = content_h;
        self.scroll_throttle.set_viewport(viewport_h);
        // 逐帧的质心位移 × 2.7 → 原版阻尼曲线 → 平滑速度。
        // 这条路径对应缩略图滚动事件
        // (唯一走缩略图质心位移算法质心算法的事件,滚动事件处理)。
        let vel = self.scroll_throttle.feed_offset(offset_y, content_h);
        self.thumb_decode_budget =
            crate::scroll::decode_budget(vel, self.last_viewport_h.max(1.0) as f64);
        self.last_scroll_y = offset_y;
        // 滚动停了(传 0 复位)→ 限速器复位。
        if vel == 0.0 {
            self.scroll_throttle.reset();
        }
    }

    /// P1-7:添加一个自定义排除目录名(写回 `settings.exclude_dirs`)。
    ///
    /// 包含 / 排除文件夹设置
    /// 的用户自定义部分;内置默认值不可删。
    fn add_exclude_dir(&mut self, name: &str) {
        let mut cur = self
            .store
            .get_setting("exclude_dirs")
            .unwrap_or_default();
        let already = cur
            .lines()
            .any(|l| l.trim().eq_ignore_ascii_case(name));
        let is_default = crate::scan::Exclusions::default()
            .names()
            .iter()
            .any(|d| d.eq_ignore_ascii_case(name));
        if already || is_default {
            self.status = format!("排除规则「{name}」已存在");
            return;
        }
        if !cur.is_empty() && !cur.ends_with('\n') {
            cur.push('\n');
        }
        cur.push_str(name);
        match self.store.set_setting("exclude_dirs", &cur) {
            Ok(()) => self.status = format!("已添加排除目录「{name}」"),
            Err(e) => self.status = format!("添加失败: {e}"),
        }
    }

    /// P1-7:移除一个自定义排除目录名。
    fn remove_exclude_dir(&mut self, name: &str) {
        let cur = self.store.get_setting("exclude_dirs").unwrap_or_default();
        let kept: Vec<&str> = cur
            .lines()
            .filter(|l| !l.trim().eq_ignore_ascii_case(name))
            .collect();
        let newv = kept.join("\n");
        match self.store.set_setting("exclude_dirs", &newv) {
            Ok(()) => self.status = format!("已移除排除目录「{name}」"),
            Err(e) => self.status = format!("移除失败: {e}"),
        }
    }

    /// 文件夹管理器 > 从图库移除:删该文件夹(含子文件夹)索引 + 取消监听。
    fn remove_folder_from_library(&mut self, folder: &str) {
        match timeline::remove_photos_under(&self.store, folder) {
            Ok(n) => {
                let _ = self.store.remove_watched(Path::new(folder));
                self.status = format!("已移除 {folder} 下 {n} 张索引(未删除磁盘文件)");
                self.sel.clear();
                self.preview = None;
                self.preview_tex = None;
                self.rebuild();
                self.watched = self.store.watched_stats();
            }
            Err(e) => self.status = format!("移除失败: {e}"),
        }
    }

    /// 工具 > 图库维护 > 备份索引到…(VACUUM INTO,后台线程)。
    fn start_backup(&mut self) {
        if self.maint_rx.is_some() {
            self.status = "维护操作进行中…".into();
            return;
        }
        let Some(dest) = rfd::FileDialog::new()
            .set_title("备份索引数据库")
            .add_filter("SQLite 数据库", &["db"])
            .set_file_name("maliang-index-backup.db")
            .save_file()
        else {
            return;
        };
        let store = self.store.clone();
        let (tx, rx) = mpsc::channel();
        self.maint_rx = Some(rx);
        self.status = "正在备份索引…".into();
        std::thread::spawn(move || {
            let r = store
                .backup_to(&dest)
                .map(|_| format!("索引已备份到 {}", dest.display()))
                .map_err(|e| e.to_string());
            let _ = tx.send(r);
        });
    }

    /// 工具 > 图库维护 > 压缩索引(VACUUM,后台线程)。
    fn start_compact(&mut self) {
        if self.maint_rx.is_some() {
            self.status = "维护操作进行中…".into();
            return;
        }
        let store = self.store.clone();
        let (tx, rx) = mpsc::channel();
        self.maint_rx = Some(rx);
        self.status = "正在压缩索引…".into();
        std::thread::spawn(move || {
            let r = store.compact().map(|_| "索引已压缩(VACUUM)".to_string()).map_err(|e| e.to_string());
            let _ = tx.send(r);
        });
    }

    /// 工具 > 图库维护 > 从 XMP 导入标签…(后台线程)。
    /// reset=true 全量重导(菜单手动触发);false 只处理新扫描照片(启动静默增量)。
    /// 语义见 timeline::import_xmp_batch:一次性导入,DB 有值不复活。
    fn start_xmp_import(&mut self, reset: bool) {
        if self.maint_rx.is_some() {
            self.status = "维护操作进行中…".into();
            return;
        }
        let store = self.store.clone();
        let (tx, rx) = mpsc::channel();
        self.maint_rx = Some(rx);
        self.status = "正在从 XMP 导入标签/星标…".into();
        std::thread::spawn(move || {
            let r = (|| -> anyhow::Result<String> {
                if reset {
                    store
                        .0
                        .lock()
                        .unwrap()
                        .execute("UPDATE photos SET xmp_imported=0", [])?;
                }
                let (stars, tagged) = timeline::import_xmp_batch(&store, 1_000_000)?;
                Ok(format!("XMP 导入完成:{stars} 张星标,{tagged} 张标签"))
            })()
            .map_err(|e| e.to_string());
            let _ = tx.send(r);
        });
    }

    /// 工具 > 图库维护 > 清理已缺失文件的索引(后台线程)。
    fn clean_missing(&mut self) {
        if self.maint_rx.is_some() {
            self.status = "维护操作进行中…".into();
            return;
        }
        let store = self.store.clone();
        let (tx, rx) = mpsc::channel();
        self.maint_rx = Some(rx);
        self.status = "正在检查缺失文件…".into();
        std::thread::spawn(move || {
            let r = store
                .mark_missing()
                .map(|v| format!("已清理 {} 条缺失文件的索引", v.len()))
                .map_err(|e| e.to_string());
            let _ = tx.send(r);
        });
    }

    /// 工具 > 导出地理标记(KML):带 GPS 的照片 -> Google Earth 可打开的 KML。
    fn export_kml(&mut self) {
        let pts = timeline::gps_points(&self.store).unwrap_or_default();
        if pts.is_empty() {
            self.status = "没有带 GPS 坐标的照片(需原图 EXIF 含定位信息)".into();
            return;
        }
        let Some(dest) = rfd::FileDialog::new()
            .set_title("导出地理标记 KML")
            .add_filter("KML", &["kml"])
            .set_file_name("geotag.kml")
            .save_file()
        else {
            return;
        };
        let xml_esc = |s: &str| {
            s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
        };
        let mut out = String::new();
        out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>
");
        out.push_str("<kml xmlns=\"http://www.opengis.net/kml/2.2\">
<Document>
");
        out.push_str("  <name>Relyto.Maliang 地理标记</name>
");
        for p in &pts {
            let name = Path::new(&p.path)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.path.clone());
            out.push_str("  <Placemark>
");
            out.push_str(&format!("    <name>{}</name>
", xml_esc(&name)));
            if let Some(t) = &p.taken {
                out.push_str(&format!("    <description>拍摄于 {}</description>
", xml_esc(t)));
            }
            if p.star != 0 {
                out.push_str("    <description>★ 星标</description>
");
            }
            out.push_str(&format!(
                "    <Point><coordinates>{:.6},{:.6}</coordinates></Point>
",
                p.lon, p.lat
            ));
            out.push_str("  </Placemark>
");
        }
        out.push_str("</Document>
</kml>
");
        match std::fs::write(&dest, out.as_bytes()) {
            Ok(()) => self.status = format!("已导出 {} 个地理标记到 {}", pts.len(), dest.display()),
            Err(e) => self.status = format!("KML 导出失败: {e}"),
        }
    }

    /// 视图 > 在资源管理器中显示:优先预览照片,其次第一个选中项。
    fn show_in_explorer_current(&mut self) {
        let path = if let Some(id) = self.preview {
            self.id_row.get(&id).map(|&i| self.rows[i].path.clone())
        } else {
            self.sel
                .iter()
                .next()
                .and_then(|id| self.rows.iter().find(|r| r.id == *id).map(|r| r.path.clone()))
        };
        match path {
            Some(p) => show_in_explorer(&p),
            None => self.status = "没有可定位的照片(先预览或选中一张)".into(),
        }
    }

    /// 预览窗上一张/下一张:按当前视图的照片序列循环(与网格显示范围一致)。
    fn step_preview(&mut self, delta: i32) {
        let Some(cur) = self.preview else { return };
        let ids = self.view_ids();
        if ids.is_empty() {
            return;
        }
        let pos = ids.iter().position(|&x| x == cur).unwrap_or(0);
        let n = ids.len() as i32;
        let next_id = ids[(pos as i32 + delta).rem_euclid(n) as usize];
        if let Some(&row) = self.id_row.get(&next_id) {
            self.preview = Some(next_id);
            self.preview_tex = None;
            debug_assert!(row < self.rows.len());
        }
    }

    /// 照片网格(按缩略图档位决定列数与格子大小;选中项加蓝框)。
    ///
    /// P1-9:缩略图档位(Picasa 3 用 8 档,本项目取 4 档常用值)。
    /// 列数按格子大小反推,保证一行铺满可用宽度。
    fn photo_grid(&mut self, ui: &mut egui::Ui, salt: &str, cells: &[usize]) {
        let tier = self.thumb_tier;
        let cell = tier.size() as f32;
        // 列数:按可用宽度算,至少 2 列,最多 16 列。
        let avail = ui.available_width();
        let cols = ((avail + 12.0) / (cell + 12.0)).floor().clamp(2.0, 16.0) as usize;
        egui::Grid::new(egui::Id::new(salt))
            .num_columns(cols)
            .spacing([12.0, 12.0])
            .show(ui, |ui| {
                for chunk in cells.chunks(cols) {
                    for &i in chunk {
                        self.photo_cell(ui, i, cell);
                    }
                    ui.end_row();
                }
            });
    }

    /// 照片右键上下文菜单(参照 Picasa 图片菜单的常用项)。
    fn photo_ctx_menu(
        &mut self,
        ui: &mut egui::Ui,
        idx: usize,
        star0: bool,
        selected: bool,
        star_clicked: &mut bool,
        preview_clicked: &mut bool,
    ) {
        if ui
            .button(if self.rows[idx].kind == crate::video::kind::MOVIE {
                "播放"
            } else {
                "预览"
            })
            .clicked()
        {
            *preview_clicked = true;
            ui.close();
        }
        let target = if star0 { 0 } else { 1 };
        let batch = selected && self.sel.len() > 1;
        let label = if batch {
            if target == 1 {
                format!("给选中的 {} 张加星标", self.sel.len())
            } else {
                format!("给选中的 {} 张取消星标", self.sel.len())
            }
        } else if target == 1 {
            "加星标".to_string()
        } else {
            "取消星标".to_string()
        };
        if ui.button(label).clicked() {
            if batch {
                self.star_selection(target);
            } else {
                *star_clicked = true;
            }
            ui.close();
        }
        if ui.button("在资源管理器中显示").clicked() {
            show_in_explorer(&self.rows[idx].path);
            ui.close();
        }
        ui.separator();
        if ui.button("从图库中移除…").clicked() {
            let id = self.rows[idx].id;
            self.sel.clear();
            self.sel.insert(id);
            self.confirm_remove = true;
            ui.close();
        }
    }

    fn photo_cell(&mut self, ui: &mut egui::Ui, idx: usize, cell: f32) {
        let id = self.rows[idx].id;
        let star0 = self.rows[idx].star != 0;
        let selected = self.sel.contains(&id);
        let tex = match self.texs.get(id) {
            Some(t) => Some(t),
            None if self.texs.is_pending(id) => None, // 后台正在生成,先占位
            None => {
                // P1-9:滚动限速 —— 本帧解码配额已用尽时,跳过同步解码,
                // 只画灰色占位块(下一帧再补)。滚动越快配额越小 → 滚动不掉帧。
                // 注意:此处**不能** mark_pending(那是"磁盘上还没有、交给后台生成"
                // 的语义),否则已落盘的缩略图会被误判为待生成而不再加载。
                if self.thumb_decode_count >= self.thumb_decode_budget {
                    None
                } else {
                    self.thumb_decode_count += 1;
                    // 只从落盘的该档缩略图读(绝不重解码原图)。
                    match thumb_tex(ui.ctx(), id, self.thumb_tier) {
                        Some(t) => {
                            self.texs.put(id, t.clone());
                            Some(t)
                        }
                        None => {
                            // 磁盘上还没有 → 交给后台 worker 生成,本帧先显示占位块。
                            self.texs.mark_pending(id);
                            None
                        }
                    }
                }
            }
        };
        let mut star_clicked = false;
        let mut preview_clicked = false;
        let ctrl_click = |ui: &egui::Ui| ui.input(|i| i.modifiers.ctrl || i.modifiers.command);
        ui.vertical(|v| {
            if let Some(ref t) = tex {
                let [tw, th] = t.size();
                let scale = cell / tw.max(th).max(1) as f32;
                let size = egui::vec2(tw as f32 * scale, th as f32 * scale);
                let resp = v.add(egui::Image::new((t.id(), size)).sense(egui::Sense::click()));
                // P2-17:视频条目左上角 ▶ 徽标(与 Picasa 3 一致的"missing/loading movie"
                // 同样给视频专门占位/标识的语义,媒体类型判定)。
                if self.rows[idx].kind == crate::video::kind::MOVIE {
                    v.painter().text(
                        resp.rect.left_top() + egui::vec2(5.0, 4.0),
                        egui::Align2::LEFT_TOP,
                        "▶",
                        egui::FontId::proportional(15.0),
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 235),
                    );
                }
                if selected {
                    v.painter().rect_stroke(
                        resp.rect.expand(2.0),
                        3.0,
                        (2.0, egui::Color32::from_rgb(90, 160, 255)),
                        egui::StrokeKind::Outside,
                    );
                }
                if resp.clicked() {
                    if ctrl_click(v) {
                        if !self.sel.remove(&id) {
                            self.sel.insert(id);
                        }
                    } else {
                        preview_clicked = true;
                    }
                }
                resp.context_menu(|ui| {
                    self.photo_ctx_menu(ui, idx, star0, selected, &mut star_clicked, &mut preview_clicked)
                });
            } else {
                let (rect, resp) = v.allocate_exact_size(egui::vec2(cell, cell), egui::Sense::click());
                v.painter().rect_filled(rect, 4.0, egui::Color32::from_gray(64));
                if selected {
                    v.painter().rect_stroke(
                        rect.expand(2.0),
                        3.0,
                        (2.0, egui::Color32::from_rgb(90, 160, 255)),
                        egui::StrokeKind::Outside,
                    );
                }
                if resp.clicked() {
                    if ctrl_click(v) {
                        if !self.sel.remove(&id) {
                            self.sel.insert(id);
                        }
                    } else {
                        preview_clicked = true;
                    }
                }
                resp.context_menu(|ui| {
                    self.photo_ctx_menu(ui, idx, star0, selected, &mut star_clicked, &mut preview_clicked)
                });
            }
            if v.small_button(if star0 { "★" } else { "☆" }).clicked() {
                star_clicked = true;
            }
        });
        if star_clicked {
            let new = if star0 { 0 } else { 1 };
            let _ = timeline::set_star(&self.store, id, new);
            self.rows[idx].star = new;
            self.sync_sidecars(id, new);
        }
        if preview_clicked {
            if self.rows[idx].kind == crate::video::kind::MOVIE {
                // P2-17 视频:交给系统关联播放器(库内渲染管线体量过大,不在本轮实现)。
                let p = std::path::PathBuf::from(&self.rows[idx].path);
                if !crate::win::shell_open(&p) {
                    self.status = format!("无法打开视频(无关联播放器):{}", p.display());
                }
            } else {
                self.preview = Some(id);
                self.preview_tex = None;
            }
        }
    }

    /// 预览大图纹理(最长边 ≤ 1024;首次打开同步加载一次,按 EXIF 方向摆正)。
    ///
    /// P0-7:相机 RAW 由 `thumbs::load_for_preview` 借 Windows Shell 缩略图扩展解码,
    /// 不再因为 image crate 不认识格式而显示"加载失败"。
    fn preview_tex(&mut self, ctx: &egui::Context) -> Option<egui::TextureHandle> {
        let id = self.preview?;
        let idx = *self.id_row.get(&id)?;
        // P2-16:预览渲染要叠加该照片的效果栈(op 树实时渲染)。
        // 签名 = 照片 id + 效果栈序列化;变了才重算纹理。
        let stack = self.fx_preview_stack(id);
        let sig = format!("{id}|{}", stack.serialize());
        if self.preview_tex.as_ref().map(|(k, _)| *k) != Some(id)
            || self.fx_applied_sig.as_deref() != Some(sig.as_str())
        {
            // P1-10:预览**不阻塞 UI**。
            // 1) 先查落盘缓存;
            // 2) 没有 → 发请求给后台 worker(后台解码 + 落盘),本帧先不显示;
            // 3) worker 完成后回报,UI 再加载纹理。
            // 原来这里同步 `image::open` 4K 原图,会卡住界面几百毫秒。
            let path = self.rows[idx].path.clone();
            // 预览底图(未加效果)缓存 —— 滑块连续拖动时复用,避免每帧重解码 jpeg。
            if self.preview_base.as_ref().map(|(k, _)| *k) != Some(id) {
                self.preview_base = None;
                match crate::thumbs::load_preview_cached(id) {
                    Some(img) => self.preview_base = Some((id, img)),
                    None => {
                        // 请求后台生成(重复请求会被 worker 的存在性检查短路)。
                        if let Some(w) = &self.preview_bg {
                            let _ = w.req.send((id, path));
                        }
                        return None;
                    }
                }
            }
            if let Some((_, img)) = self.preview_base.as_ref() {
                // P2-16:在解码后的像素上套用效果栈(op 树在此位置执行)。
                let mut rgba = img.to_rgba8();
                crate::effects::apply_stack(&mut rgba, &stack);
                let size = [rgba.width() as usize, rgba.height() as usize];
                let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                let tex = ctx.load_texture(
                    format!("preview-{id}-fx"),
                    color,
                    egui::TextureOptions::default(),
                );
                self.preview_tex = Some((id, tex));
                self.fx_applied_sig = Some(sig);
            }
        }
        self.preview_tex.as_ref().map(|(_, t)| t.clone())
    }

    /// 预览叠框数据(OCR/人脸框图上标注)。按照片 id 缓存。
    /// 返回纹理相对坐标 (0..1) 的框 + 标签文字。
    /// 坐标链路:OCR Txt / FaceHit 均为**未旋转原图像素** → 经 EXIF orientation
    /// 映射到显示方向 → 归一化(纹理与旋转后图同比例,resize 不改变归一化坐标)。
    fn preview_boxes_for(
        &mut self,
        id: i64,
    ) -> Vec<((f32, f32, f32, f32), String)> {
        if self.preview_boxes.as_ref().map(|(k, _)| *k) == Some(id) {
            return self.preview_boxes.as_ref().map(|(_, v)| v.clone()).unwrap_or_default();
        }
        #[allow(unused_mut)]
        let mut out: Vec<((f32, f32, f32, f32), String)> = Vec::new();
        #[cfg(feature = "ai")]
        let idx = match self.id_row.get(&id) {
            Some(&i) => i,
            None => return Vec::new(),
        };
        #[cfg(feature = "ai")]
        {
            let path = self.rows[idx].path.clone();
            let orient = crate::exif::read_orientation(Path::new(&path));
            let dims = image::ImageReader::open(&path)
                .ok()
                .and_then(|r| r.with_guessed_format().ok())
                .and_then(|r| r.into_dimensions().ok());
            if let Some((w0, h0)) = dims {
                let (w, h) = (w0 as f32, h0 as f32);
                if self.show_face_boxes {
                    for f in crate::faces::faces_for_photo(&self.store, id) {
                        let (r, _, _) = orient_rect(f.x1, f.y1, f.x2, f.y2, w, h, orient);
                        out.push((r, "人脸".to_string()));
                    }
                }
                if self.show_ocr_boxes && self.ocr_photo == Some(id) {
                    for t in &self.ocr_results {
                        let (r, _, _) = orient_rect(
                            t.x0 as f32,
                            t.y0 as f32,
                            t.x1 as f32,
                            t.y1 as f32,
                            w,
                            h,
                            orient,
                        );
                        out.push((r, t.text.clone()));
                    }
                }
            }
        }
        self.preview_boxes = Some((id, out.clone()));
        out
    }
}

/// EXIF orientation 下,未旋转原图 (w×h) 中的点 (x,y) 在显示方向上的坐标。
/// 语义与 thumbs::apply_exif_rotation 一致(2/4 翻转,5-8 旋转)。
/// (无 ai 构建时预览无框可画,仅测试引用。)
#[allow(dead_code)]
fn map_orient(x: f32, y: f32, w: f32, h: f32, orient: u32) -> (f32, f32) {
    match orient {
        2 => (w - 1.0 - x, y),
        3 => (w - 1.0 - x, h - 1.0 - y),
        4 => (x, h - 1.0 - y),
        5 => (y, x),
        6 => (h - 1.0 - y, x),
        7 => (h - 1.0 - y, w - 1.0 - x),
        8 => (y, w - 1.0 - x),
        _ => (x, y),
    }
}

/// 未旋转原图像素矩形 → (纹理归一化矩形, 显示宽, 显示高)。
/// (无 ai 构建时仅测试引用。)
#[allow(dead_code)]
fn orient_rect(
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    w: f32,
    h: f32,
    orient: u32,
) -> ((f32, f32, f32, f32), f32, f32) {
    let (ax, ay) = map_orient(x0, y0, w, h, orient);
    let (bx, by) = map_orient(x1, y1, w, h, orient);
    let (nx0, nx1) = if ax <= bx { (ax, bx) } else { (bx, ax) };
    let (ny0, ny1) = if ay <= by { (ay, by) } else { (by, ay) };
    let (rw, rh) = match orient {
        5..=8 => (h, w),
        _ => (w, h),
    };
    // 像素坐标:右/下边缘 +1(框住最后一个像素,整幅矩形恰归一化到 1.0)
    ((nx0 / rw, ny0 / rh, (nx1 + 1.0) / rw, (ny1 + 1.0) / rh), rw, rh)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// orientation 映射与 thumbs::apply_exif_rotation 语义一致
    /// (以左上角点为基准;3=180°,6=顺时针 90°,8=逆时针 90°)。
    #[test]
    fn map_orient_matches_rotation() {
        let (w, h) = (100.0f32, 50.0f32);
        // 1: 恒等
        assert_eq!(map_orient(10.0, 20.0, w, h, 1), (10.0, 20.0));
        // 3: 180° 旋转 → 对角
        assert_eq!(map_orient(10.0, 20.0, w, h, 3), (89.0, 29.0));
        // 6: 顺时针 90° → 显示尺寸 (h, w) = (50, 100)
        assert_eq!(map_orient(10.0, 20.0, w, h, 6), (29.0, 10.0));
        // 8: 逆时针 90°
        assert_eq!(map_orient(10.0, 20.0, w, h, 8), (20.0, 89.0));
        // 矩形归一化:整幅图在任何方向都应归一化到全幅
        let (r, rw, rh) = orient_rect(0.0, 0.0, w - 1.0, h - 1.0, w, h, 6);
        assert_eq!((rw, rh), (50.0, 100.0));
        assert!((r.0 - 0.0).abs() < 1e-4 && (r.2 - 1.0).abs() < 1e-4);
        assert!((r.1 - 0.0).abs() < 1e-4 && (r.3 - 1.0).abs() < 1e-4);
        // 90° 旋转下一个 19x30 窄高条变成 30x19(宽来自原高 30px,占显示宽 50 的 60%)
        let (r2, _, _) = orient_rect(80.0, 10.0, 99.0, 40.0, w, h, 6);
        assert!(r2.1 > r2.0, "旋转后 x 区间应来自原 y 区间");
        assert!((r2.2 - r2.0 - 0.6).abs() < 0.02, "实际宽占比 {}", r2.2 - r2.0);
    }
}

impl eframe::App for App {
    /// 退出时停掉后台缩略图 worker(不留游离线程)。
    fn on_exit(&mut self) {
        if let Some(w) = self.thumb_bg.take() {
            w.stop();
        }
        // P1-10:预览 worker 也要停(否则后台线程会随进程一起被砍,留下半张图)。
        if let Some(w) = self.preview_bg.take() {
            w.stop();
        }
        // P1-13:设备监听线程(隐藏窗口 + 消息循环)也要停。
        if let Some(w) = self.device_watch.take() {
            w.stop();
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // P1-9:每帧重置缩略图解码配额计数(预算由滚动速度在 CentralPanel 里定)。
        self.thumb_decode_count = 0;
        // 轮询扫描结果
        if let Some(rx) = &self.scan_rx {
            if let Ok(r) = rx.try_recv() {
                self.scan_rx = None;
                self.scan_ctrl = None;
                self.status = match r.err {
                    Some(e) => format!("扫描{}失败: {e}", r.scope),
                    None if r.cancelled => format!(
                        "扫描{}已取消:已索引 {} 张(已处理 {} 个文件)",
                        r.scope, r.indexed, r.scanned
                    ),
                    None => format!(
                        "扫描{}完成:发现 {} 个文件,索引 {} 张(未变跳过 {} 张,排除目录 {} 个)",
                        r.scope, r.scanned, r.indexed, r.unchanged, r.excluded_dirs
                    ),
                };
                self.rebuild();
            }
        }
        // P1-13:设备热插拔事件("置刷新标志 + 触发刷新")。
        // 设备节点树变化时重建设备列表;本项目对应动作是
        // "重新枚举可移动卷 → 新出现的卷自动扫描"。
        if let Some(dw) = &self.device_watch {
            let mut hit: Option<crate::device::DeviceEvent> = None;
            while let Ok(ev) = dw.rx.try_recv() {
                hit = Some(ev);
            }
            if let Some(ev) = hit {
                let vols = crate::device::removable_volumes();
                let added = crate::device::new_volumes(&self.last_volumes, &vols);
                self.last_volumes = vols;
                if !added.is_empty() {
                    // 新设备接入 → 扫描新卷(设备到达 → 分派 → 重建列表 → 导入)。
                    let roots = added.clone();
                    self.status = format!(
                        "检测到新设备({}),正在扫描 {}",
                        ev.label(),
                        added
                            .iter()
                            .map(|p| p.to_string_lossy().to_string())
                            .collect::<Vec<_>>()
                            .join("、")
                    );
                    // P1-13:设备列表缓存失效(原版 WM_DEVICECHANGE 链路同样
                    // 会重建设备列表);若导入窗口开着,下次重绘即看到新设备。
                    self.refresh_import_devices();
                    self.start_scan_roots(roots, "新接入的设备");
                } else {
                    self.status = format!("检测到设备变化({})", ev.label());
                    self.refresh_import_devices();
                }
            }
        }
        // 缩略图后台 worker:完成一批 → 清占位标记 + 触发重绘
        if let Some(w) = &self.thumb_bg {
            let mut got: Vec<i64> = Vec::new();
            while let Ok(ids) = w.done.try_recv() {
                got.extend(ids);
            }
            if !got.is_empty() {
                self.texs.clear_pending(&got);
                self.status = format!("缩略图已生成 {} 张", got.len());
                ctx.request_repaint();
            }
        }
        // P1-10:预览后台完成信号 → 清掉预览纹理缓存,让下一帧重新加载。
        if let Some(w) = &self.preview_bg {
            let mut got: Vec<i64> = Vec::new();
            let mut failed = 0usize;
            while let Ok((id, res)) = w.done.try_recv() {
                match res {
                    Ok(_) => got.push(id),
                    Err(_) => failed += 1,
                }
            }
            if !got.is_empty() {
                // 预览图已落盘 → 作废当前纹理,下一帧从缓存重读。
                if let Some((pid, _)) = &self.preview_tex {
                    if got.contains(pid) {
                        self.preview_tex = None;
                    }
                }
                ctx.request_repaint();
            }
            if failed > 0 {
                self.status = format!("{failed} 张预览生成失败(格式不支持或文件已移动)");
            }
        }
        // 烘焙(保存)结果
        if let Some(rx) = &self.bake_rx {
            if let Ok(res) = rx.try_recv() {
                self.bake_rx = None;
                self.status = match res {
                    Ok(()) => "保存完成:已备份原稿并重写原图".into(),
                    Err(e) => format!("保存失败: {e}"),
                };
                self.rebuild();
            }
        }
        if self.bake_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        // 导出(复制)结果
        if let Some(rx) = &self.export_rx {
            if let Ok((ok, errs)) = rx.try_recv() {
                self.export_rx = None;
                self.status = if errs.is_empty() {
                    format!("导出完成 {ok} 张")
                } else {
                    let first: Vec<String> = errs.iter().take(2).cloned().collect();
                    format!("导出完成 {ok} 张,失败 {} 个: {}", errs.len(), first.join("; "))
                };
                self.status = self.status.replace('\n', " ");
            }
        }
        if self.export_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        // P2-18:邮件/光盘后台任务结果
        if let Some(rx) = &self.share_rx {
            if let Ok(res) = rx.try_recv() {
                self.share_rx = None;
                self.status = match res {
                    Ok(msg) => msg,
                    Err(e) => format!("分享失败: {e}"),
                };
                self.status = self.status.replace('\n', " ");
            }
        }
        if self.share_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
        // 图库维护(备份/压缩/清理缺失)结果轮询
        if let Some(rx) = &self.maint_rx {
            if let Ok(res) = rx.try_recv() {
                self.maint_rx = None;
                self.status = match res {
                    Ok(msg) => msg,
                    Err(e) => format!("维护失败: {e}"),
                };
                self.rebuild();
            }
        }
        if self.maint_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        // 监听文件夹:收到变化汇总 → 刷新视图
        if let Some(rx) = &self.watch_rx {
            if let Ok(msg) = rx.try_recv() {
                self.status = msg;
                self.rebuild();
            }
        }
        if self.watch_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_secs(2));
        }
        // OCR(M2b)结果轮询
        #[cfg(feature = "ai")]
        if let Some(rx) = &self.ocr_rx {
            if let Ok(res) = rx.try_recv() {
                self.ocr_rx = None;
                match res {
                    Ok((pid, rows)) => {
                        self.ocr_photo = Some(pid);
                        self.ocr_results = rows;
                        self.ocr_panel = true;
                        self.status = if self.ocr_results.is_empty() {
                            "OCR 完成:未识别到文本".into()
                        } else {
                            format!("OCR 完成:识别到 {} 处文本", self.ocr_results.len())
                        };
                    }
                    Err(e) => self.status = format!("OCR 失败: {e}"),
                }
            }
        }
        #[cfg(feature = "ai")]
        if self.ocr_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        // 人脸(M2c)结果轮询
        #[cfg(feature = "ai")]
        if let Some(rx) = &self.face_rx {
            if let Ok(res) = rx.try_recv() {
                self.face_rx = None;
                match res {
                    Ok((np, nf)) => {
                        self.status = format!("人脸扫描完成:{np} 张照片,检出 {nf} 张人脸")
                    }
                    Err(e) => self.status = format!("人脸扫描失败: {e}"),
                }
                self.refresh_people_and_suggestions();
                if let Some(c) = self.sel_cluster {
                    self.face_hits = crate::faces::faces_for_cluster(
                        &self.store,
                        c,
                        self.face_show_ignored,
                        if self.face_hide_low { 0.4 } else { 0.0 },
                    );
                }
            }
        }
        #[cfg(feature = "ai")]
        if self.face_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        // CLIP(M4b)语义索引轮询
        #[cfg(feature = "ai")]
        if let Some(rx) = &self.clip_index_rx {
            if let Ok(res) = rx.try_recv() {
                self.clip_index_rx = None;
                match res {
                    Ok((n, ok)) => {
                        self.status = format!("语义索引完成:{n} 张照片,嵌入 {ok} 张")
                    }
                    Err(e) => self.status = format!("语义索引失败: {e}"),
                }
            }
        }
        #[cfg(feature = "ai")]
        if self.clip_index_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        // CLIP(M4b)语义搜索轮询
        #[cfg(feature = "ai")]
        if let Some(rx) = &self.clip_search_rx {
            if let Ok(res) = rx.try_recv() {
                self.clip_search_rx = None;
                match res {
                    Ok(hits) => {
                        let n = hits.len();
                        self.clip_results = hits;
                        self.status = format!("语义搜索完成:{n} 个结果");
                    }
                    Err(e) => self.status = format!("语义搜索失败: {e}"),
                }
            }
        }
        #[cfg(feature = "ai")]
        if self.clip_search_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        // WiFi 导入服务器存活时定时刷新接收计数
        if self.wifi.is_some() || self.wifi_panel {
            ctx.request_repaint_after(std::time::Duration::from_millis(1000));
        }
        // 后台任务进行中则定时重绘
        if self.scan_busy.load(Ordering::Relaxed) || self.thumb_bg.is_some() {
            // 扫描中用更短的间隔,让进度条流畅(持续回调驱动)。
            let ms = if self.scan_busy.load(Ordering::Relaxed) { 100 } else { 300 };
            ctx.request_repaint_after(std::time::Duration::from_millis(ms));
        }
        // 键盘快捷键(参照 Picasa;有文本输入焦点时不拦截)
        if !ctx.wants_keyboard_input() {
            let (sel_all, save, esc, next, prev, page_up, page_down, home, end) =
                ctx.input(|i| {
                    (
                        i.modifiers.command && i.key_pressed(egui::Key::A),
                        i.key_pressed(egui::Key::F2),
                        i.key_pressed(egui::Key::Escape),
                        i.key_pressed(egui::Key::ArrowRight),
                        i.key_pressed(egui::Key::ArrowLeft),
                        i.key_pressed(egui::Key::PageUp),
                        i.key_pressed(egui::Key::PageDown),
                        i.key_pressed(egui::Key::Home),
                        i.key_pressed(egui::Key::End),
                    )
                });
            if sel_all {
                self.sel = self.view_ids().into_iter().collect();
            }
            if save && self.preview.is_some() && self.bake_rx.is_none() {
                if let Some(id) = self.preview {
                    self.start_bake(id);
                }
            }
            if esc {
                // P1-8:ESC 优先取消正在进行的扫描
                // (ESC 取消绑定);
                // 没有扫描时才关闭预览窗。
                if let Some(ctrl) = &self.scan_ctrl {
                    ctrl.cancel();
                    self.status = "正在取消扫描…".into();
                } else {
                    self.preview = None;
                    self.preview_tex = None;
                }
            }
            if self.preview.is_some() {
                if next {
                    self.step_preview(1);
                }
                if prev {
                    self.step_preview(-1);
                }
            }
            // P1-9:7 个滚动事件在这里派发
            // (按事件名匹配 + 派发;
            //  缩略图滚动事件走缩略图质心位移算法)。
            // 本项目把前 6 个接成键盘翻页/跳转,语义与原版一一对应。
            if self.preview.is_none() {
                if page_up {
                    self.dispatch_throttle_event(crate::scroll::SCROLL_EVENTS[2]);
                }
                if page_down {
                    self.dispatch_throttle_event(crate::scroll::SCROLL_EVENTS[3]);
                }
                if home {
                    self.dispatch_throttle_event(crate::scroll::SCROLL_EVENTS[0]);
                }
                if end {
                    self.dispatch_throttle_event(crate::scroll::SCROLL_EVENTS[1]);
                }
            }
        }

        // ===== 菜单栏(结构参照 Picasa 3:文件/编辑/视图/工具/帮助) =====
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                // —— 文件 ——
                ui.menu_button("文件(F)", |ui| {
                    if ui.button("添加文件夹到图库…").clicked() {
                        self.add_folder_to_library();
                        ui.close();
                    }
                    let scanning = self.scan_busy.load(Ordering::Relaxed);
                    if ui
                        .add_enabled(!scanning, egui::Button::new("重新扫描监听目录…"))
                        .clicked()
                    {
                        self.start_scan_full();
                        ui.close();
                    }
                    if scanning {
                        ui.spinner();
                    }
                    // 参考件没有「扫全盘」概念:范围只经文件夹管理器扩大,
                    // 这里只重扫已登记的监听目录,不新增范围。
                    ui.small(format!(
                        "监听 {} 个目录;扩大范围请用「添加文件夹到图库」",
                        self.watched.len()
                    ));
                    ui.separator();
                    ui.menu_button("导入(I)", |ui| {
                        // P1-13:从设备导入(设备选择窗口)。
                        if ui.button("从设备(相机/手机)…").clicked() {
                            self.open_import_devices();
                            ui.close();
                        }
                        let label = if self.wifi.is_some() {
                            "从手机(WiFi)导入…(运行中,点击停止)"
                        } else {
                            "从手机(WiFi)导入…"
                        };
                        if ui.button(label).clicked() {
                            self.toggle_wifi();
                            ui.close();
                        }
                    });
                    ui.menu_button("导出(E)", |ui| {
                        if ui
                            .add_enabled(self.export_rx.is_none(), egui::Button::new("导出到文件夹…"))
                            .clicked()
                        {
                            self.start_export();
                            ui.close();
                        }
                        // P2-16:导出时套用已保存的效果(不修改原图)。
                        ui.checkbox(&mut self.export_with_effects, "套用已保存的效果");
                    });
                    // P2-18:分享(邮件 / 礼品光盘;上传目标服务已关停,不实现)。
                    ui.menu_button("分享(S)", |ui| {
                        let busy = self.share_rx.is_some();
                        if ui
                            .add_enabled(!busy, egui::Button::new("以邮件发送…"))
                            .clicked()
                        {
                            self.email_win = true;
                            ui.close();
                        }
                        if ui
                            .add_enabled(!busy, egui::Button::new("制作礼品光盘…"))
                            .clicked()
                        {
                            self.cd_win = true;
                            ui.close();
                        }
                        ui.separator();
                        ui.small(
                            "网络上传:原版依赖已停运的云相册服务,目前无可用对等实现。",
                        );
                    });
                    ui.separator();
                    if ui
                        .add_enabled(
                            self.preview.is_some() && self.bake_rx.is_none(),
                            egui::Button::new("保存  F2"),
                        )
                        .clicked()
                    {
                        if let Some(id) = self.preview {
                            self.start_bake(id);
                        }
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("退出(X)").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        ui.close();
                    }
                });
                // —— 编辑 ——
                ui.menu_button("编辑(E)", |ui| {
                    let ids = self.view_ids();
                    let empty_view = ids.is_empty();
                    let starred: Vec<i64> = self.rows.iter().filter(|r| r.star != 0).map(|r| r.id).collect();
                    if ui
                        .add_enabled(!empty_view, egui::Button::new("全选  Ctrl+A"))
                        .clicked()
                    {
                        self.sel = ids.iter().copied().collect();
                        ui.close();
                    }
                    if ui
                        .add_enabled(!empty_view, egui::Button::new("选择加星标的照片"))
                        .clicked()
                    {
                        let view_set: BTreeSet<i64> = ids.iter().copied().collect();
                        self.sel = starred
                            .iter()
                            .filter(|id| view_set.contains(id))
                            .copied()
                            .collect();
                        ui.close();
                    }
                    if ui
                        .add_enabled(!empty_view, egui::Button::new("反向选择"))
                        .clicked()
                    {
                        let all: BTreeSet<i64> = ids.iter().copied().collect();
                        self.sel = all.difference(&self.sel).copied().collect();
                        ui.close();
                    }
                    if ui
                        .add_enabled(!self.sel.is_empty(), egui::Button::new("清除选择"))
                        .clicked()
                    {
                        self.sel.clear();
                        ui.close();
                    }
                    ui.separator();
                    // P2-16:效果面板。
                    if ui.selectable_label(self.fx_panel, "效果面板").clicked() {
                        self.fx_panel = !self.fx_panel;
                        ui.close();
                    }
                    if ui
                        .add_enabled(!self.sel.is_empty(), egui::Button::new("从图库中移除…"))
                        .clicked()
                    {
                        self.confirm_remove = true;
                        ui.close();
                    }
                });
                // —— 视图 ——
                ui.menu_button("视图(V)", |ui| {
                    if ui
                        .selectable_label(self.view == ViewKind::Timeline, "时间线")
                        .clicked()
                    {
                        self.view = ViewKind::Timeline;
                        ui.close();
                    }
                    #[cfg(feature = "ai")]
                    if ui
                        .selectable_label(self.view == ViewKind::Faces, "人物")
                        .clicked()
                    {
                        self.view = ViewKind::Faces;
                        ui.close();
                    }
                    #[cfg(feature = "ai")]
                    if ui
                        .selectable_label(self.view == ViewKind::Semantic, "语义搜索")
                        .clicked()
                    {
                        self.view = ViewKind::Semantic;
                        ui.close();
                    }
                    if ui
                        .selectable_label(self.view == ViewKind::Tags, "标签")
                        .clicked()
                    {
                        self.view = ViewKind::Tags;
                        ui.close();
                    }
                    if ui
                        .selectable_label(self.view == ViewKind::Folder, "文件夹")
                        .clicked()
                    {
                        self.view = ViewKind::Folder;
                        ui.close();
                    }
                    ui.separator();
                    ui.menu_button("排序方式", |ui| {
                        if ui
                            .selectable_label(self.sort_mode == SortMode::Date, "按拍摄日期")
                            .clicked()
                        {
                            self.sort_mode = SortMode::Date;
                            self.rebuild();
                            ui.close();
                        }
                        if ui
                            .selectable_label(self.sort_mode == SortMode::Name, "按文件名")
                            .clicked()
                        {
                            self.sort_mode = SortMode::Name;
                            self.rebuild();
                            ui.close();
                        }
                    });
                    if ui.selectable_label(self.sort_rev, "反向排序").clicked() {
                        self.sort_rev = !self.sort_rev;
                        self.rebuild();
                        ui.close();
                    }
                    ui.menu_button("缩略图", |ui| {
                        // P1-9:多档缩略图(与 Picasa 3 一致的四个视图尺寸菜单项)。
                        for tier in crate::thumbs::ALL_TIERS {
                            if ui
                                .selectable_label(self.thumb_tier == tier, tier.label())
                                .clicked()
                            {
                                self.set_thumb_tier(tier);
                                ui.close();
                            }
                        }
                    });
                    ui.separator();
                    if ui
                        .add_enabled(
                            self.preview.is_some() || !self.sel.is_empty(),
                            egui::Button::new("在资源管理器中显示"),
                        )
                        .clicked()
                    {
                        self.show_in_explorer_current();
                        ui.close();
                    }
                });
                // —— 工具 ——
                ui.menu_button("工具(T)", |ui| {
                    #[cfg(feature = "ai")]
                    {
                        if ui
                            .add_enabled(self.face_rx.is_none(), egui::Button::new("人脸识别扫描…"))
                            .clicked()
                        {
                            self.view = ViewKind::Faces;
                            self.start_face_scan();
                            ui.close();
                        }
                        if self.face_rx.is_some() {
                            ui.spinner();
                        }
                        if ui
                            .add_enabled(
                                self.preview.is_some() && self.ocr_rx.is_none(),
                                egui::Button::new("文字识别(OCR)…"),
                            )
                            .clicked()
                        {
                            if let Some(id) = self.preview {
                                if let Some(&i) = self.id_row.get(&id) {
                                    let p = PathBuf::from(&self.rows[i].path);
                                    self.start_ocr(id, p);
                                }
                            }
                            ui.close();
                        }
                        if ui
                            .add_enabled(
                                self.clip_index_rx.is_none(),
                                egui::Button::new("语义索引…"),
                            )
                            .clicked()
                        {
                            self.view = ViewKind::Semantic;
                            self.start_clip_index();
                            ui.close();
                        }
                        if self.clip_index_rx.is_some() {
                            ui.spinner();
                        }
                        ui.separator();
                    }
                    ui.separator();
                    if ui.button("文件夹管理器…").clicked() {
                        self.open_folder_mgr();
                        ui.close();
                    }
                    let mut watching = self.watch_on.load(Ordering::SeqCst);
                    if ui.checkbox(&mut watching, "监听文件夹变化").clicked() {
                        self.watch_on.store(watching, Ordering::SeqCst);
                        self.status = if watching {
                            "已开启文件夹监听(事件驱动,不可用时自动降级轮询)".into()
                        } else {
                            "已暂停文件夹监听".into()
                        };
                        ui.close();
                    }
                    if ui
                        .selectable_label(self.view == ViewKind::Duplicates, "重复照片")
                        .clicked()
                    {
                        self.view = ViewKind::Duplicates;
                        ui.close();
                    }
                    ui.separator();
                    ui.menu_button("图库维护", |ui| {
                        if ui
                            .add_enabled(self.maint_rx.is_none(), egui::Button::new("备份索引到…"))
                            .clicked()
                        {
                            self.start_backup();
                            ui.close();
                        }
                        if ui
                            .add_enabled(self.maint_rx.is_none(), egui::Button::new("压缩索引"))
                            .clicked()
                        {
                            self.start_compact();
                            ui.close();
                        }
                        if ui
                            .add_enabled(self.maint_rx.is_none(), egui::Button::new("清理已缺失文件的索引"))
                            .clicked()
                        {
                            self.clean_missing();
                            ui.close();
                        }
                        if ui
                            .add_enabled(self.maint_rx.is_none(), egui::Button::new("从 XMP 导入标签…"))
                            .clicked()
                        {
                            self.start_xmp_import(true);
                            ui.close();
                        }
                        // P1-12:变更历史(可回溯的变更日志)。
                        if ui.button("变更历史…").clicked() {
                            self.show_changes = true;
                            self.reload_changes();
                            ui.close();
                        }
                        // P1-11:给还没有 uid64 的照片补一个持久唯一标识。
                        if ui.button("分配唯一标识(uid64)…").clicked() {
                            match self.store.backfill_uid64() {
                                Ok(n) => {
                                    self.status = format!("已为 {n} 张照片分配 uid64");
                                }
                                Err(e) => self.status = format!("分配 uid64 失败: {e}"),
                            }
                            ui.close();
                        }
                        if ui.button("按唯一标识查找…").clicked() {
                            self.show_uid_lookup = true;
                            self.uid_lookup_result = None;
                            ui.close();
                        }
                        if self.maint_rx.is_some() {
                            ui.spinner();
                        }
                    });
                    if ui.button("导出地理标记(KML)…").clicked() {
                        self.export_kml();
                        ui.close();
                    }
                });
                // —— 帮助 ——
                ui.menu_button("帮助(H)", |ui| {
                    if ui.button("键盘快捷键…").clicked() {
                        self.show_shortcuts = true;
                        ui.close();
                    }
                    if ui.button("关于 Relyto.Maliang…").clicked() {
                        self.show_about = true;
                        ui.close();
                    }
                });
            });
        });

        // 右侧面板:效果系统
        if self.fx_panel {
            egui::SidePanel::right("fx").show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.fx_panel_ui(ui);
                });
            });
        }

        // 左侧面板:文件夹视图 / 人物视图 / 标签视图
        if self.view == ViewKind::Folder {            egui::SidePanel::left("folders").show(ctx, |ui| {
                ui.heading("文件夹");
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (i, f) in self.folders.clone().iter().enumerate() {
                        if ui.selectable_label(self.selected_folder == i, f).clicked() {
                            self.selected_folder = i;
                        }
                    }
                });
            });
        }
        if self.view == ViewKind::Tags {
            egui::SidePanel::left("tags").show(ctx, |ui| {
                ui.heading("标签");
                ui.separator();
                ui.label(format!("共 {} 个(右键设颜色)", self.tags.len()));
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for t in self.tags.clone() {
                        ui.horizontal(|ui| {
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(10.0, 10.0),
                                egui::Sense::hover(),
                            );
                            ui.painter().rect_filled(
                                rect,
                                2.0,
                                tag_color(t.color.as_deref())
                                    .unwrap_or_else(|| egui::Color32::from_gray(90)),
                            );
                            let resp = ui.selectable_label(
                                self.sel_tag.as_deref() == Some(t.name.as_str()),
                                format!("{}  ·  {} 张", t.name, t.count),
                            );
                            if resp.clicked() {
                                // 再次点击已选中的标签 = 取消筛选(回到全部)
                                if self.sel_tag.as_deref() == Some(t.name.as_str()) {
                                    self.sel_tag = None;
                                } else {
                                    self.sel_tag = Some(t.name.clone());
                                }
                            }
                            resp.context_menu(|ui| {
                                ui.label("标签颜色");
                                for (cname, cval) in TAG_COLORS {
                                    let mark =
                                        if Some(cval) == tag_color(t.color.as_deref()) { "● " } else { "○ " };
                                    if ui.button(format!("{mark}{cname}")).clicked() {
                                        let _ = timeline::set_tag_color(
                                            &self.store,
                                            &t.name,
                                            Some(cname),
                                        );
                                        self.tags = timeline::all_tags(&self.store)
                                            .unwrap_or_default();
                                        ui.close();
                                    }
                                }
                                if ui.button("无颜色").clicked() {
                                    let _ =
                                        timeline::set_tag_color(&self.store, &t.name, None);
                                    self.tags =
                                        timeline::all_tags(&self.store).unwrap_or_default();
                                    ui.close();
                                }
                            });
                        });
                    }
                });
            });
        }
        #[cfg(feature = "ai")]
        if self.view == ViewKind::Faces {
            if self.face_people.is_empty() && self.face_rx.is_none() {
                self.refresh_people_and_suggestions();
            }
            egui::SidePanel::left("people").show(ctx, |ui| {
                ui.heading("人脸簇");
                ui.separator();
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.face_hide_low, "隐藏低质量");
                    ui.checkbox(&mut self.face_show_ignored, "显示已忽略");
                });
                ui.label(format!("共 {} 簇", self.face_people.len()));
                // 命名建议(未命名簇按质心相似度推荐)
                if !self.face_sugs.is_empty() {
                    ui.separator();
                    ui.label("命名建议:");
                    for s in self.face_sugs.clone() {
                        ui.horizontal(|ui| {
                            ui.label(format!("簇 {} → {} ({:.2} · {} 张)", s.cluster, s.name, s.score, s.count));
                            if ui.small_button("应用").clicked() {
                                let _ = crate::faces::rename_cluster(&self.store, s.cluster, &s.name);
                                self.refresh_people_and_suggestions();
                            }
                        });
                    }
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for p in self.face_people.clone() {
                        let display = p.person.clone().unwrap_or_else(|| format!("簇 {}", p.cluster));
                        let label =
                            format!("{display}  ·  {} 张  · 质量 {:.2}", p.count, p.quality);
                        if ui
                            .selectable_label(self.sel_cluster == Some(p.cluster), label)
                            .clicked()
                        {
                            self.sel_cluster = Some(p.cluster);
                            self.face_hits = crate::faces::faces_for_cluster(
                                &self.store,
                                p.cluster,
                                self.face_show_ignored,
                                if self.face_hide_low { 0.4 } else { 0.0 },
                            );
                        }
                    }
                    // 已忽略簇(可选中后恢复)
                    if self.face_show_ignored {
                        for cid in crate::faces::ignored_clusters(&self.store) {
                            let label = format!("簇 {cid}  · 已忽略");
                            if ui.selectable_label(self.sel_cluster == Some(cid), label).clicked()
                            {
                                self.sel_cluster = Some(cid);
                                self.face_hits = crate::faces::faces_for_cluster(
                                    &self.store,
                                    cid,
                                    true,
                                    0.0,
                                );
                            }
                        }
                    }
                });
            });
        }
        #[cfg(feature = "ai")]
        if self.view == ViewKind::Semantic {
            egui::SidePanel::left("semantic").show(ctx, |ui| {
                ui.heading("语义搜索");
                ui.separator();
                let resp = ui.text_edit_singleline(&mut self.input_query);
                let enter =
                    resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (enter || ui.button("搜索").clicked()) && self.clip_search_rx.is_none() {
                    let q = self.input_query.trim().to_string();
                    if !q.is_empty() {
                        self.start_clip_search(q);
                    }
                }
                if self.clip_search_rx.is_some() {
                    ui.spinner();
                    ui.label("搜索中…");
                }
                ui.separator();
                // 索引语言单选(中英双模型;搜索按查询含 CJK 自动匹配)
                ui.horizontal(|ui| {
                    ui.label("模型:");
                    ui.selectable_value(
                        &mut self.clip_lang,
                        crate::clip::Lang::En,
                        "英文",
                    );
                    ui.selectable_value(
                        &mut self.clip_lang,
                        crate::clip::Lang::Zh,
                        "中文",
                    );
                });
                // 模型目录里实际放了哪些 .onnx:一个都没有就直说,
                // 免得用户点了「语义索引」才在后台线程里撞到"模型缺失"。
                let models = crate::ai::available_models();
                if models.is_empty() {
                    ui.small("模型目录还没有 .onnx 模型文件,请先放入后再索引");
                } else {
                    ui.small(format!("已装模型:{}", models.join("、")));
                }
                let indexed = crate::clip::clip_count_lang(&self.store, self.clip_lang);
                let total_zh = crate::clip::clip_count_lang(&self.store, crate::clip::Lang::Zh);
                let total_en = crate::clip::clip_count_lang(&self.store, crate::clip::Lang::En);
                ui.label(format!(
                    "已索引({}){indexed} / {} 张",
                    self.clip_lang.label(),
                    self.rows.len()
                ));
                if indexed < self.rows.len() {
                    ui.label(format!(
                        "点 工具 > 语义索引 补嵌(当前{}模型,较慢)",
                        self.clip_lang.label()
                    ));
                }
                if total_en + total_zh > 0 {
                    ui.label(format!("中/英索引量:{total_zh} / {total_en}"));
                }
                ui.separator();
                ui.label(format!("结果 {} 张", self.clip_results.len()));
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (pid, score) in self.clip_results.clone() {
                        let path = self
                            .id_row
                            .get(&pid)
                            .map(|&i| self.rows[i].path.clone())
                            .unwrap_or_default();
                        let fname = Path::new(&path)
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or(path);
                        if ui
                            .selectable_label(false, format!("{score:.3}  {fname}"))
                            .clicked()
                        {
                            if self.id_row.contains_key(&pid) {
                                self.preview = Some(pid);
                            }
                        }
                    }
                });
            });
        }

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("共 {} 张", self.rows.len()));
                if !self.sel.is_empty() {
                    ui.label(format!("已选 {} 张", self.sel.len()));
                }
                ui.separator();
                ui.label(&self.status);
                // P1-9:当前档位的缓存进度(已生成 / 总数)。
                // 切档时后台 worker 才开始补该档,这里实时反映。
                let tier = self.thumb_tier;
                let cached = self.store.tier_count(tier as u8);
                let total = self.rows.len();
                if cached < total {
                    ui.separator();
                    ui.label(format!("{}缓存 {cached}/{total}", tier.label()));
                }
                // P1-9:滚动限速状态(平滑速度)。
                // 正在滚动时显示整形后的速度与本帧解码预算;
                // 静止时显示"限速待机"。
                {
                    let vel = self.scroll_throttle.velocity();
                    if vel != 0.0 {
                        ui.separator();
                        ui.label(format!(
                            "限速 {:.0}px/帧({} 位移 {:.0}px,本帧解码≤{})",
                            vel,
                            crate::scroll::thumb_event_name(),
                            self.scroll_throttle.last_raw(),
                            self.thumb_decode_budget
                        ));
                    } else if self.thumb_decode_budget < crate::scroll::MAX_THUMB_DECODE_PER_FRAME
                    {
                        ui.separator();
                        ui.label(format!("限速待机(本帧解码≤{})", self.thumb_decode_budget));
                    }
                }
                // P1-8:扫描进度条 + 取消按钮。
                if let Some(ctrl) = &self.scan_ctrl {
                    let (done, total) = ctrl.counts();
                    let frac = ctrl.fraction();
                    ui.separator();
                    ui.label(format!("扫描进度 {done}/{total}"));
                    ui.add(egui::ProgressBar::new(frac).desired_width(160.0));
                    if ui.button("取消扫描").clicked() {
                        ctrl.cancel();
                        self.status = format!("正在取消扫描{}…", self.scan_scope);
                    }
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            // P1-9:滚动限速(位移阻尼限速)。
            // 上一帧记下的滚动偏移 + 视口高 + 内容高 → 逐帧质心位移 × 2.7
            // → 原版阻尼曲线 → 平滑速度 → 换算成本帧的缩略图解码预算。
            // 滚动越快 → 预算越低 → 大图库滚动不掉帧(原版限速节点的等价作用)。
            self.update_scroll_throttle(
                self.last_scroll_y,
                self.last_viewport_h,
                self.last_content_h,
            );
            // P1-9:原版滚动事件派发后的目标偏移 → 作为 ScrollArea 的初始偏移
            // (egui 会用 `state.offset` 记忆滚动位置,这里只在有目标时覆盖一次)。
            let mut gallery = egui::ScrollArea::vertical().id_salt("gallery");
            if let Some(t) = self.gallery_scroll_target.take() {
                gallery = gallery.vertical_scroll_offset(t);
            }
            let out = gallery.show(ui, |ui| {
                match self.view {
                    ViewKind::Timeline => {
                        let months = std::mem::take(&mut self.months);
                        for (label, idxs) in &months {
                            ui.heading(label);
                            self.photo_grid(ui, &format!("tl-{label}"), idxs);
                            ui.add_space(12.0);
                        }
                        self.months = months;
                    }
                    ViewKind::Duplicates => {
                        let near_label = if self.dup_near {
                            format!("重复照片 {} 组(含近似)", self.dups.len())
                        } else {
                            format!("重复照片 {} 组", self.dups.len())
                        };
                        ui.horizontal(|ui| {
                            ui.heading(near_label);
                            if ui
                                .checkbox(&mut self.dup_near, format!("包含近似重复(hamming≤{NEAR_DUP_DIST})"))
                                .changed()
                            {
                                self.dups = if self.dup_near {
                                    timeline::dup_groups_near(&self.store, NEAR_DUP_DIST)
                                        .unwrap_or_default()
                                } else {
                                    timeline::dup_groups(&self.store).unwrap_or_default()
                                };
                            }
                        });
                        ui.add_space(4.0);
                        if self.dups.is_empty() {
                            ui.label(if self.dup_near {
                                "没有检测到重复或近似重复"
                            } else {
                                "没有检测到重复(哈希精确一致才计为一组;勾选下方可含近似重复)"
                            });
                        }
                        let dups = std::mem::take(&mut self.dups);
                        for (h, ids) in &dups {
                            ui.separator();
                            if self.dup_near {
                                ui.heading(format!("近似组  ·  {} 张", ids.len()));
                            } else {
                                ui.heading(format!("组 {h:016x}  ·  {} 张", ids.len()));
                            }
                            let cells: Vec<usize> = ids
                                .iter()
                                .filter_map(|id| self.id_row.get(id).copied())
                                .collect();
                            self.photo_grid(ui, &format!("dup-{h}"), &cells);
                        }
                        self.dups = dups;
                    }
                    ViewKind::Faces => {
                        #[cfg(feature = "ai")]
                        match self.sel_cluster {
                            None => {
                                ui.heading("人脸");
                                ui.label("左侧选择一个人脸簇查看照片;识别用 工具 > 人脸识别扫描。");
                            }
                            Some(c) => {
                                let cells: Vec<usize> = {
                                    let mut seen = BTreeSet::new();
                                    self.face_hits
                                        .iter()
                                        .filter_map(|h| self.id_row.get(&h.photo_id).copied())
                                        .filter(|i| seen.insert(*i))
                                        .collect()
                                };
                                let q_txt = self
                                    .face_people
                                    .iter()
                                    .find(|p| p.cluster == c)
                                    .map(|p| format!(" · 平均质量 {:.2}", p.quality))
                                    .unwrap_or_default();
                                ui.heading(format!(
                                    "簇 {c} · {} 张照片 / {} 个人脸框{q_txt}",
                                    cells.len(),
                                    self.face_hits.len()
                                ));
                                self.photo_grid(ui, "faces-grid", &cells);
                                ui.add_space(8.0);
                                ui.separator();
                                ui.label("人脸框明细(按质量降序;质量低的可整簇忽略):");
                                let mut qs: Vec<(String, f32)> = self
                                    .face_hits
                                    .iter()
                                    .map(|h| {
                                        let fname = self
                                            .id_row
                                            .get(&h.photo_id)
                                            .map(|&i| {
                                                Path::new(&self.rows[i].path)
                                                    .file_name()
                                                    .map(|f| f.to_string_lossy().into_owned())
                                                    .unwrap_or_default()
                                            })
                                            .unwrap_or_default();
                                        (fname, h.quality)
                                    })
                                    .collect();
                                qs.sort_by(|a, b| {
                                    b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
                                });
                                egui::ScrollArea::vertical()
                                    .id_salt("face-hits-detail")
                                    .max_height(150.0)
                                    .show(ui, |ui| {
                                        for (fname, q) in qs {
                                            ui.label(format!("{fname} · 质量 {q:.2}"));
                                        }
                                    });
                                ui.add_space(8.0);
                                ui.separator();
                                ui.heading("重命名这个簇");
                                ui.horizontal(|ui| {
                                    let resp = ui.text_edit_singleline(&mut self.input_rename);
                                    if (resp.lost_focus()
                                        && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                                        || ui.button("重命名").clicked()
                                    {
                                        let name = std::mem::take(&mut self.input_rename);
                                        if !name.is_empty() {
                                            let _ =
                                                crate::faces::rename_cluster(&self.store, c, &name);
                                            self.refresh_people_and_suggestions();
                                        }
                                    }
                                });
                                // 忽略/恢复(对应原版 ignoreface:误检簇一键排除)
                                ui.horizontal(|ui| {
                                    if ui.button("忽略此簇").clicked() {
                                        let _ = crate::faces::ignore_cluster(&self.store, c, true);
                                        self.face_show_ignored = true;
                                        self.refresh_people_and_suggestions();
                                        self.face_hits = crate::faces::faces_for_cluster(
                                            &self.store,
                                            c,
                                            true,
                                            0.0,
                                        );
                                    }
                                    if ui.button("恢复此簇").clicked() {
                                        let _ = crate::faces::ignore_cluster(&self.store, c, false);
                                        self.refresh_people_and_suggestions();
                                        self.face_hits = crate::faces::faces_for_cluster(
                                            &self.store,
                                            c,
                                            self.face_show_ignored,
                                            if self.face_hide_low { 0.4 } else { 0.0 },
                                        );
                                    }
                                });
                            }
                        }
                        #[cfg(not(feature = "ai"))]
                        {
                            ui.heading("人物");
                            ui.label(
                                "人脸功能未编译进当前构建(构建时用了 --no-default-features)。",
                            );
                        }
                    }
                    ViewKind::Tags => match self.sel_tag.clone() {
                        None => {
                            ui.heading("标签");
                            ui.label("左侧选择一个标签,查看标有该标签的照片。");
                        }
                        Some(tag) => {
                            let ids = timeline::photos_by_tag(&self.store, &tag).unwrap_or_default();
                            let cells: Vec<usize> = ids
                                .iter()
                                .filter_map(|id| self.id_row.get(id).copied())
                                .collect();
                            ui.heading(format!("标签 «{tag}» · {} 张", cells.len()));
                            self.photo_grid(ui, "tags-grid", &cells);
                        }
                    },
                    ViewKind::Semantic => {
                        #[cfg(feature = "ai")]
                        {
                            if self.clip_results.is_empty() {
                                ui.heading("语义搜索");
                                ui.label(
                                    "左侧输入关键词(英文效果最佳,如 \"a cat\"、\"beach\");\
                                     首次使用请先 工具 > 语义索引。",
                                );
                            } else {
                                let q = self.input_query.trim().to_string();
                                ui.heading(format!("语义结果 {} 张(查询:{q})", self.clip_results.len()));
                                let cells: Vec<usize> = self
                                    .clip_results
                                    .iter()
                                    .filter_map(|(id, _)| self.id_row.get(id).copied())
                                    .collect();
                                self.photo_grid(ui, "semantic-grid", &cells);
                            }
                        }
                        #[cfg(not(feature = "ai"))]
                        {
                            ui.heading("语义搜索");
                            ui.label(
                                "语义搜索未编译进当前构建(构建时用了 --no-default-features)。",
                            );
                        }
                    }
                    ViewKind::Folder => {
                        let folder = self.folders.get(self.selected_folder).cloned().unwrap_or_default();
                        let idxs: Vec<usize> = self
                            .rows
                            .iter()
                            .enumerate()
                            .filter(|(_, r)| folder_of(&r.path) == folder)
                            .map(|(i, _)| i)
                            .collect();
                        self.photo_grid(ui, "folder-grid", &idxs);
                    }
                }
            });
            // P1-9:把本帧的实际滚动偏移 / 视口高 / 内容高记下来,
            // 下一帧用来算逐帧质心位移(原版缩略图质心位移算法的算法)。
            self.last_scroll_y = out.state.offset.y;
            self.last_viewport_h = out.inner_rect.height();
            self.last_content_h = out.content_size.y;
        });

        // 预览编辑窗
        let mut preview_open = self.preview.is_some();
        if let Some(id) = self.preview {
            if !self.id_row.contains_key(&id) {
                self.preview = None; // 照片已不在图库(被移除或删除)
            } else {
                let &idx = self.id_row.get(&id).unwrap();
                egui::Window::new("照片预览")
                    .open(&mut preview_open)
                    .resizable(true)
                    .default_width(720.0)
                    .show(ctx, |ui| {
                        // 导航行:翻页后本帧直接返回,下一帧按新索引渲染
                        let mut stepped = false;
                        ui.horizontal(|ui| {
                            if ui.button("◀ 上一张").clicked() {
                                self.step_preview(-1);
                                stepped = true;
                            }
                            if ui.button("下一张 ▶").clicked() {
                                self.step_preview(1);
                                stepped = true;
                            }
                            #[cfg(feature = "ai")]
                            {
                                if ui
                                    .checkbox(&mut self.show_face_boxes, "人脸框")
                                    .changed()
                                {
                                    self.preview_boxes = None;
                                }
                                if ui
                                    .checkbox(&mut self.show_ocr_boxes, "识别框")
                                    .changed()
                                {
                                    self.preview_boxes = None;
                                }
                            }
                        });
                        if stepped {
                            return;
                        }
                        ui.separator();
                        let tex = self.preview_tex(ctx);
                        if let Some(t) = tex {
                            let [tw, th] = t.size();
                            let scale = 640.0 / tw.max(th).max(1) as f32;
                            let size = egui::vec2(tw as f32 * scale, th as f32 * scale);
                            let resp = ui.image((t.id(), size));
                            // OCR/人脸框图上标注
                            let boxes = self.preview_boxes_for(id);
                            if !boxes.is_empty() {
                                let r = resp.rect;
                                let stroke = egui::Stroke::new(
                                    1.5_f32,
                                    egui::Color32::from_rgb(255, 90, 90),
                                );
                                for ((a, b, c, d), label) in &boxes {
                                    let p0 = egui::pos2(
                                        r.min.x + a * r.width(),
                                        r.min.y + b * r.height(),
                                    );
                                    let p1 = egui::pos2(
                                        r.min.x + c * r.width(),
                                        r.min.y + d * r.height(),
                                    );
                                    let rect = egui::Rect::from_min_max(p0, p1);
                                    ui.painter()
                                        .rect_stroke(rect, 0.0, stroke, egui::StrokeKind::Inside);
                                    if !label.is_empty() {
                                        let galley = ui.painter().layout_no_wrap(
                                            label.clone(),
                                            egui::FontId::proportional(11.0),
                                            egui::Color32::WHITE,
                                        );
                                        let bg = egui::Rect::from_min_size(
                                            rect.min,
                                            galley.size() + egui::vec2(4.0, 2.0),
                                        );
                                        ui.painter()
                                            .rect_filled(bg, 0.0, egui::Color32::from_black_alpha(170));
                                        ui.painter().galley(
                                            bg.min + egui::vec2(2.0, 1.0),
                                            galley,
                                            egui::Color32::WHITE,
                                        );
                                    }
                                }
                            }
                        } else {
                            // P1-10:预览改为后台生成 —— 缓存未就绪时显示"生成中",
                            // 而不是"加载失败"(解码在后台线程,失败才会走这里)。
                            if crate::thumbs::preview_path(id).exists() {
                                ui.label("(预览加载失败)");
                            } else {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label("预览生成中…(后台解码并缓存)");
                                });
                                // 后台完成后需要重绘,否则停在"生成中"。
                                ctx.request_repaint_after(std::time::Duration::from_millis(150));
                            }
                        }
                        let path = self.rows[idx].path.clone();
                        let star = self.rows[idx].star != 0;
                        ui.horizontal(|ui| {
                            if ui.button(if star { "★ 取消星标" } else { "☆ 加星" }).clicked() {
                                let new = if star { 0 } else { 1 };
                                let _ = timeline::set_star(&self.store, id, new);
                                self.rows[idx].star = new;
                                self.sync_sidecars(id, new);
                            }
                            if ui.button("保存(烘培)").clicked() {
                                self.start_bake(id);
                            }
                            if self.bake_rx.is_some() {
                                ui.spinner();
                            }
                            if ui.button("在资源管理器中显示").clicked() {
                                show_in_explorer(&path);
                            }
                        });
                        // 信息行:尺寸 / 拍摄时间 / 相机 / GPS
                        let r = &self.rows[idx];
                        let mut parts: Vec<String> = Vec::new();
                        if let (Some(w), Some(h)) = (r.width, r.height) {
                            parts.push(format!("{w}×{h}"));
                        }
                        if let Some(t) = &r.taken {
                            parts.push(t.replace('T', " "));
                        }
                        if !r.cam.is_empty() {
                            parts.push(r.cam.clone());
                        }
                        if let (Some(lat), Some(lon)) = (r.lat, r.lon) {
                            parts.push(format!("GPS {lat:.5}, {lon:.5}"));
                        }
                        if !parts.is_empty() {
                            ui.label(parts.join(" · "));
                        }
                        ui.label(format!("文件: {path}"));
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.label("标签:");
                            if ui.text_edit_singleline(&mut self.input_tag).lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter))
                            {
                                let t = std::mem::take(&mut self.input_tag);
                                if !t.is_empty() {
                                    let _ = timeline::add_tag(&self.store, id, &t);
                                    self.sync_sidecars(id, self.rows[idx].star);
                                }
                            }
                            if ui.button("添加").clicked() {
                                let t = std::mem::take(&mut self.input_tag);
                                if !t.is_empty() {
                                    let _ = timeline::add_tag(&self.store, id, &t);
                                    self.sync_sidecars(id, self.rows[idx].star);
                                }
                            }
                        });
                        if let Ok(tags) = timeline::tags_of(&self.store, id) {
                            if !tags.is_empty() {
                                ui.horizontal_wrapped(|ui| {
                                    for t in tags.clone() {
                                        let col = self
                                            .tags
                                            .iter()
                                            .find(|x| x.name == t)
                                            .and_then(|x| tag_color(x.color.as_deref()));
                                        ui.horizontal(|ui| {
                                            if let Some(col) = col {
                                                let (rect, _) = ui.allocate_exact_size(
                                                    egui::vec2(8.0, 8.0),
                                                    egui::Sense::hover(),
                                                );
                                                ui.painter().rect_filled(rect, 1.0, col);
                                            }
                                            if ui.small_button(format!("{t} ✕")).clicked() {
                                                let _ = timeline::remove_tag(&self.store, id, &t);
                                                self.sync_sidecars(id, self.rows[idx].star);
                                            }
                                        });
                                    }
                                });
                            }
                        }
                    });
                if !preview_open {
                    self.preview = None;
                    self.preview_tex = None;
                }
            }
        }

        // WiFi 导入面板(M4a)
        let mut wifi_open = self.wifi_panel;
        if wifi_open {
            egui::Window::new("WiFi 导入")
                .open(&mut wifi_open)
                .default_width(480.0)
                .show(ctx, |ui| {
                    let running = self
                        .wifi
                        .as_ref()
                        .map(|h| (h.addr.clone(), h.received.load(Ordering::SeqCst)));
                    match running {
                        Some((addr, n)) => {
                            ui.label(format!("运行中:http://{addr}/"));
                            ui.label(format!("已接收 {n} 张"));
                            ui.label("手机浏览器打开以上地址,选择照片即自动上传到本机 Incoming 目录并入库。");
                            ui.add_space(8.0);
                            if ui.button("停止").clicked() {
                                self.toggle_wifi();
                            }
                        }
                        None => {
                            ui.label("未启动。启动后手机需与电脑在同一局域网。");
                            if ui.button("启动").clicked() {
                                self.toggle_wifi();
                            }
                        }
                    }
                });
            if self.wifi.is_some() {
                ctx.request_repaint_after(std::time::Duration::from_millis(1000));
            }
        }
        self.wifi_panel = wifi_open;

        // P1-13:从设备导入窗口(统一设备列表)。
        if self.show_import_devices {
            let mut open = self.show_import_devices;
            egui::Window::new("从设备导入")
                .open(&mut open)
                .default_width(520.0)
                .show(ctx, |ui| {
                    ui.small(
                        "WPD 便携设备(相机/手机)+ 可移动磁盘统一列表;原版设备列表合成",
                    );
                    ui.horizontal(|ui| {
                        if ui.button("刷新").clicked() {
                            self.refresh_import_devices();
                        }
                        ui.small(format!(
                            "共 {} 台(便携 {} + 磁盘 {})",
                            self.import_devices.len(),
                            self.import_devices.iter().filter(|d| matches!(d, crate::wpd::DeviceEntry::Portable(_))).count(),
                            self.import_devices.iter().filter(|d| matches!(d, crate::wpd::DeviceEntry::Volume(_))).count(),
                        ));
                    });
                    if !self.import_devices_err.is_empty() {
                        ui.colored_label(egui::Color32::RED, self.import_devices_err.clone());
                    }
                    ui.separator();
                    if self.import_devices.is_empty() {
                        ui.label("未发现设备。接入相机/手机(U 盘、SD 卡读卡器也可以),或点「刷新」重新枚举。");
                        ui.small("提示:WPD 服务不可用时只显示盘符卷 —— 原版同样是尽力而为(WPD 设备枚举)。");
                    } else {
                        // 索引先行:避免闭包里同时借 self.import_devices 和 self.store
                        let entries: Vec<crate::wpd::DeviceEntry> = self.import_devices.clone();
                        // 用户点「扫描此卷」先记下来,循环结束后再执行
                        // (egui 闭包里不能同时借列表与 &mut self)。
                        let mut scan_request: Option<std::path::PathBuf> = None;
                        egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                            for d in &entries {
                                ui.horizontal(|ui| {
                                    ui.small(d.kind());
                                    ui.label(d.display_name());
                                    match d {
                                        crate::wpd::DeviceEntry::Portable(w) => {
                                            ui.small(w.device_id.clone());
                                        }
                                        crate::wpd::DeviceEntry::Volume(p) => {
                                            if ui.button("扫描此卷").clicked() {
                                                scan_request = Some(p.clone());
                                            }
                                        }
                                    }
                                });
                            }
                        });
                        if let Some(root) = scan_request {
                            self.start_scan_roots(vec![root], "设备导入");
                            self.show_import_devices = false;
                        }
                        ui.separator();
                        ui.small("便携设备(MTP/PTP)的照片传输需 WPD 对象流,当前版本未实现—— U 盘/读卡器请用「扫描此卷」。");
                    }
                });
            self.show_import_devices = open;
        }

        // P2-18:邮件发送窗口(尺寸档与原版 1600/1024/800 一致)。
        if self.email_win {
            let mut open = self.email_win;
            egui::Window::new("以邮件发送")
                .open(&mut open)
                .default_width(420.0)
                .show(ctx, |ui| {
                    ui.small(
                        "把当前视图的照片缩放为 JPEG,交给系统默认邮件客户端(原版 Simple MAPI 链路,MAPI 动态加载)。",
                    );
                    ui.separator();
                    ui.label("附件尺寸:");
                    for s in crate::share::ShareSize::ALL {
                        ui.radio_value(&mut self.email_size, s, s.label());
                    }
                    ui.separator();
                    let n = self.view_ids().len();
                    if ui
                        .add_enabled(
                            self.share_rx.is_none(),
                            egui::Button::new(format!("发送当前视图 {n} 张")),
                        )
                        .clicked()
                    {
                        self.start_email();
                    }
                    if self.share_rx.is_some() {
                        ui.small("后台准备附件/等待邮件客户端中…");
                    }
                });
            self.email_win = open;
        }

        // P2-18:礼品光盘窗口(原版「Create a Gift CD…」,IMAPI2 刻录)。
        if self.cd_win {
            let mut open = self.cd_win;
            egui::Window::new("制作礼品光盘")
                .open(&mut open)
                .default_width(420.0)
                .show(ctx, |ui| {
                    ui.small(
                        "把当前视图的照片写入光盘(走 IMAPI2 引擎;卷标沿用默认命名)。",
                    );
                    ui.separator();
                    ui.label("照片尺寸:");
                    for s in crate::share::ShareSize::ALL {
                        ui.radio_value(&mut self.cd_size, s, s.label());
                    }
                    ui.separator();
                    let n = self.view_ids().len();
                    if ui
                        .add_enabled(
                            self.share_rx.is_none(),
                            egui::Button::new(format!("刻录当前视图 {n} 张")),
                        )
                        .clicked()
                    {
                        self.start_gift_cd();
                    }
                    if self.share_rx.is_some() {
                        ui.small("后台暂存内容/刻录中…(请放入空盘)");
                    }
                });
            self.cd_win = open;
        }

        // P1-12:变更历史窗口(可回溯的变更日志)。
        if self.show_changes {
            let mut open = self.show_changes;
            egui::Window::new("变更历史")
                .open(&mut open)
                .default_width(720.0)
                .min_height(360.0)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!("共 {} 条记录", self.store.change_count()));
                        if ui.button("刷新").clicked() {
                            self.reload_changes();
                        }
                        if ui.button("清空").clicked() {
                            let _ = self.store.clear_changes();
                            self.reload_changes();
                            self.status = "变更历史已清空".into();
                        }
                    });
                    ui.separator();
                    if self.change_log.is_empty() {
                        ui.label("(暂无变更记录 —— 扫描或监听到变化后会记录在这里)");
                    } else {
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            for (ts, path, action) in &self.change_log {
                                ui.horizontal(|ui| {
                                    let t = format_ts(*ts);
                                    ui.monospace(t);
                                    let color = match action.as_str() {
                                        "added" => egui::Color32::from_rgb(80, 200, 120),
                                        "modified" => egui::Color32::from_rgb(240, 180, 60),
                                        "removed" => egui::Color32::from_rgb(240, 90, 90),
                                        _ => egui::Color32::GRAY,
                                    };
                                    ui.colored_label(color, action);
                                    ui.label(path);
                                });
                            }
                        });
                    }
                    ui.separator();
                    ui.small("说明:原版的变更日志只存内存,不可回溯;此处为持久化增强版。");
                });
            self.show_changes = open;
        }

        // P1-11:按 uid64 定位照片窗口(外部工具可用持久标识引用照片)。
        if self.show_uid_lookup {
            let mut open = self.show_uid_lookup;
            egui::Window::new("按唯一标识(uid64)查找")
                .open(&mut open)
                .default_width(520.0)
                .show(ctx, |ui| {
                    ui.label("uid64 是照片的持久唯一标识(内容+路径派生,重新扫描不变)。");
                    ui.horizontal(|ui| {
                        let mut input = self.uid_input.clone();
                        if ui
                            .add(egui::TextEdit::singleline(&mut input).hint_text("十进制或 0x 十六进制"))
                            .changed()
                        {
                            self.uid_input = input.clone();
                        }
                        if ui.button("查找").clicked() {
                            self.uid_lookup_result = Some(self.lookup_uid());
                        }
                    });
                    match &self.uid_lookup_result {
                        Some(Ok(path)) => {
                            ui.separator();
                            ui.label(egui::RichText::new(path).strong());
                            if ui.button("在资源管理器中显示").clicked() {
                                show_in_explorer(path);
                            }
                        }
                        Some(Err(e)) => {
                            ui.separator();
                            ui.colored_label(egui::Color32::from_rgb(240, 90, 90), e);
                        }
                        None => {}
                    }
                });
            self.show_uid_lookup = open;
        }

        // OCR 结果窗(M2b)
        #[cfg(feature = "ai")]
        {
            let mut ocr_open = self.ocr_panel;
            if ocr_open {
                egui::Window::new("OCR 识别结果")
                    .open(&mut ocr_open)
                    .default_width(420.0)
                    .show(ctx, |ui| {
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            if self.ocr_results.is_empty() {
                                ui.label("(未识别到文本)");
                            }
                            for t in &self.ocr_results {
                                ui.label(format!("{}   置信度 {:.2}", t.text, t.conf));
                            }
                        });
                    });
            }
            self.ocr_panel = ocr_open;
        }

        // 帮助 > 键盘快捷键
        if self.show_shortcuts {
            let mut open = self.show_shortcuts;
            egui::Window::new("键盘快捷键")
                .open(&mut open)
                .default_width(380.0)
                .show(ctx, |ui| {
                    ui.label("Ctrl+A        全选当前视图的照片");
                    ui.label("Ctrl+点击     选中 / 取消选中单张照片");
                    ui.label("F2            保存(烘培)当前预览照片");
                    ui.label("← / →         预览上一张 / 下一张");
                    ui.label("Esc           关闭预览");
                    ui.label("右键          照片上下文菜单");
                });
            self.show_shortcuts = open;
        }

        // 帮助 > 关于
        if self.show_about {
            let mut open = self.show_about;
            egui::Window::new("关于 Relyto.Maliang")
                .open(&mut open)
                .default_width(480.0)
                .show(ctx, |ui| {
                    ui.heading("Relyto.Maliang");
                    ui.label("版本 0.1.0 · Rust + egui 重写的 Picasa 风格本地图库");
                    ui.separator();
                    ui.label("功能:全盘扫描索引 / 时间线 / 文件夹 / 彩色标签 / 重复检测 / GPS 地理标记(KML)/ 文件夹管理器 / 索引备份与压缩 / 烘焙保存 / 导出 / WiFi 导入 / OCR / 人脸分类");
                    ui.label("人脸:质量评分、半自动命名建议、忽略簇、算法版本迁移(见 视图 > 人脸)。");
                    ui.label("原则:全本地、无联网;文件夹优先,绝不复制或移动原图。");
                    ui.label("菜单结构参照 Picasa 3(文件 / 编辑 / 视图 / 工具 / 帮助)。");
                });
            self.show_about = open;
        }

        // 文件夹管理器:监听文件夹列表 + 添加/全扫/清理 + 每行重新扫描/移除
        if self.show_folder_mgr {
            let mut open = self.show_folder_mgr;
            let mut close = false;
            egui::Window::new("文件夹管理器")
                .open(&mut open)
                .default_width(660.0)
                .min_height(340.0)
                .show(ctx, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("添加文件夹…").clicked() {
                            self.add_folder_to_library();
                            close = true;
                        }
                        if ui.button("全部重新扫描").clicked() {
                            let roots: Vec<PathBuf> =
                                self.watched.iter().map(|(p, _)| PathBuf::from(p)).collect();
                            if roots.is_empty() {
                                self.status = "图库中还没有监听任何文件夹".into();
                            } else {
                                self.start_scan_roots(roots, "全部文件夹");
                                close = true;
                            }
                        }
                        if ui.button("清理已缺失文件").clicked() {
                            self.clean_missing();
                            close = true;
                        }
                    });
                    ui.separator();
                    if self.watched.is_empty() {
                        ui.label("(空)用上面的「添加文件夹…」把图片文件夹加入图库。");
                    }
                    let watched = self.watched.clone();
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for (path, count) in watched {
                            let exists = Path::new(&path).is_dir();
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    ui.label(egui::RichText::new(&path).strong());
                                    ui.small(if exists {
                                        format!("{count} 张照片 · 状态正常")
                                    } else {
                                        format!(
                                            "{count} 张照片 · 文件夹已不在磁盘上(可清理缺失索引)"
                                        )
                                    });
                                });
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.button("从图库移除…").clicked() {
                                            self.confirm_folder_remove = Some(path.clone());
                                            close = true;
                                        }
                                        if ui.button("重新扫描").clicked() {
                                            self.start_scan_roots(vec![PathBuf::from(&path)], &path);
                                            close = true;
                                        }
                                    },
                                );
                            });
                            ui.separator();
                        }
                    });
                    ui.small("提示:移除只删除数据库索引,磁盘文件原样保留;重新扫描会再次加入。");
                    // 扫描排除规则(对齐 Picasa 3 的目录排除规则)+ 监听模式。
                    let excl = crate::scan::Exclusions::from_store(&self.store);
                    ui.separator();
                    ui.label("扫描时排除的目录");
                    ui.small("对齐 Picasa 3 的目录排除规则;可增删自定义项。");
                    let mut to_remove: Option<String> = None;
                    egui::ScrollArea::vertical()
                        .max_height(120.0)
                        .show(ui, |ui| {
                            for name in excl.names() {
                                ui.horizontal(|ui| {
                                    ui.label(name);
                                    // 内置默认项不可删 —— 与 Picasa 3 行为一致。
                                    let is_default = crate::scan::Exclusions::default()
                                        .names()
                                        .iter()
                                        .any(|d| d.eq_ignore_ascii_case(name));
                                    if is_default {
                                        ui.small("(原版默认)");
                                    } else if ui.button("移除").clicked() {
                                        to_remove = Some(name.clone());
                                    }
                                });
                            }
                        });
                    if let Some(rm) = to_remove {
                        self.remove_exclude_dir(&rm);
                    }
                    ui.horizontal(|ui| {
                        let mut input = self.exclude_input.clone();
                        if ui
                            .add(egui::TextEdit::singleline(&mut input).hint_text("目录名,如 Cache"))
                            .changed()
                        {
                            self.exclude_input = input.clone();
                        }
                        if ui.button("添加排除目录").clicked() {
                            let t = self.exclude_input.trim().to_string();
                            if !t.is_empty() {
                                self.add_exclude_dir(&t);
                                self.exclude_input.clear();
                            }
                        }
                    });
                    if let Some(m) = &self.watch_mode {
                        let mode = if m.load(Ordering::SeqCst) == 1 {
                            crate::watch::Mode::Poll
                        } else {
                            crate::watch::Mode::Event
                        };
                        ui.small(format!("文件夹监听模式:{}", mode.label()));
                    }
                });
            self.show_folder_mgr = open && !close;
        }

        // 文件夹管理器 > 从图库移除(二次确认;只删索引,不删文件)
        if let Some(folder) = self.confirm_folder_remove.clone() {
            let mut open = true;
            egui::Window::new("从图库中移除文件夹")
                .open(&mut open)
                .collapsible(false)
                .default_width(460.0)
                .show(ctx, |ui| {
                    ui.label(format!("确定把「{folder}」及其子文件夹下的所有照片从图库索引中移除吗?"));
                    ui.label("只移除索引记录,不会删除磁盘上的任何文件。");
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("移除").clicked() {
                            self.remove_folder_from_library(&folder);
                            self.confirm_folder_remove = None;
                        }
                        if ui.button("取消").clicked() {
                            self.confirm_folder_remove = None;
                        }
                    });
                });
            if !open {
                self.confirm_folder_remove = None;
            }
        }

        // 人脸算法版本不一致:旧向量无法与新算法结果比较
        #[cfg(feature = "ai")]
        if self.face_version_warn {
            let mut open = self.face_version_warn;
            egui::Window::new("人脸数据需要更新")
                .open(&mut open)
                .collapsible(false)
                .default_width(480.0)
                .show(ctx, |ui| {
                    ui.label("检测到人脸识别算法已升级(现有数据由旧版本生成),旧的人脸簇和特征向量无法与新算法的结果正确比较。");
                    ui.label("建议清除旧人脸数据并重新扫描;也可以暂时保留(新旧簇可能分开显示)。");
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("清除并重新扫描").clicked() {
                            let _ = crate::faces::wipe_faces(&self.store);
                            let _ = self
                                .store
                                .set_setting("face_algo_version", crate::faces::FACE_ALGO_VERSION);
                            self.face_version_warn = false;
                            self.start_face_scan();
                        }
                        if ui.button("保留旧数据").clicked() {
                            let _ = self
                                .store
                                .set_setting("face_algo_version", crate::faces::FACE_ALGO_VERSION);
                            self.face_version_warn = false;
                        }
                    });
                });
            self.face_version_warn = open;
        }

        // 编辑 > 从图库中移除(二次确认;只删索引,不删文件)
        if self.confirm_remove {
            egui::Window::new("从图库中移除")
                .collapsible(false)
                .default_width(400.0)
                .show(ctx, |ui| {
                    ui.label(format!("确定把选中的 {} 张照片从图库索引中移除吗?", self.sel.len()));
                    ui.label("只移除索引记录,不会删除磁盘上的原文件。");
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("移除").clicked() {
                            self.remove_selection();
                            self.confirm_remove = false;
                        }
                        if ui.button("取消").clicked() {
                            self.confirm_remove = false;
                        }
                    });
                });
        }
    }
}
