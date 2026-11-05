use crate::db::Store;
use crate::timeline::{self, PhotoRow};
use eframe::egui;
use std::collections::{BTreeSet, HashMap, HashSet};
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
    /// 相簿(参考件 Album:照片的命名集合,不动文件)。
    Albums,
    #[allow(dead_code)]
    Faces,
    Tags,
    #[allow(dead_code)]
    Semantic,
    /// 快速查找:全盘文件名索引检索(USN/MFT,Windows 提权子进程产出)。
    Search,
}

impl ViewKind {
    /// 状态栏/历史提示用的一词名(左栏导航项的文案另有取舍,见导航列表)。
    fn label(self) -> &'static str {
        match self {
            ViewKind::Timeline => "时间线",
            ViewKind::Folder => "文件夹",
            ViewKind::Duplicates => "重复照片",
            ViewKind::Albums => "相簿",
            ViewKind::Faces => "人物",
            ViewKind::Tags => "标签",
            ViewKind::Semantic => "语义搜索",
            ViewKind::Search => "快速查找",
        }
    }
}

/// 整页(参考件把"看照片"和"取照片"做成两套整页面板互切:
/// 主页是图库,导入页占满中央区并换掉左栏;菜单/工具栏/状态栏不动)。
#[derive(PartialEq, Eq, Clone, Copy)]
enum PageKind {
    /// 图库页(时间线/文件夹/相簿/人物… 都在这里)。
    Library,
    /// 导入页(选源 → 看清单 → 入库 → 返回)。
    Import,
}

impl PageKind {
    fn label(self) -> &'static str {
        match self {
            PageKind::Library => "图库",
            PageKind::Import => "导入",
        }
    }
}

/// 导航历史的一站:后/前进按钮恢复的东西(整页 + 视图 + 左栏选中)。
#[derive(Clone, PartialEq)]
struct NavStop {
    page: PageKind,
    view: ViewKind,
    folder: usize,
    show_all: bool,
}

/// 导入页列出的一行(源目录还没入库,只能现读文件系统)。
#[derive(Debug)]
struct ImportEntry {
    path: PathBuf,
    name: String,
    /// 右列:大小(读盘时一次算好,不逐帧格式化)。
    size_s: String,
    /// 右列:修改日期(同上)。
    date: String,
    is_dir: bool,
    /// 已在图库(行灰显并标「已入库」,免得用户以为漏导又点一遍)。
    in_library: bool,
}

/// 排序方式(`SortBy`)与视图过滤(`Filters`)定义在 `timeline`,
/// 由 SQL 谓词一次完成 —— 大图库不在 UI 层筛全表。

/// 人物列表排序(工具栏「查看选项」> Sort People 三项;
/// 菜单顺序即此处声明顺序)。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum PeopleSort {
    /// 按姓名(未命名簇排最后)。
    Name,
    /// 按人脸张数,多的在前。
    Amount,
    /// 只要张数最多的前 10 个簇。
    Top10,
}

impl PeopleSort {
    const ALL: [PeopleSort; 3] = [PeopleSort::Name, PeopleSort::Amount, PeopleSort::Top10];

    fn label(self) -> &'static str {
        match self {
            PeopleSort::Name => "按姓名给人物排序",
            PeopleSort::Amount => "按数量给人物排序",
            PeopleSort::Top10 => "给人物排序并只看前 10",
        }
    }

    fn key(self) -> &'static str {
        match self {
            PeopleSort::Name => "name",
            PeopleSort::Amount => "amount",
            PeopleSort::Top10 => "top10",
        }
    }

    fn from_key(s: &str) -> Option<PeopleSort> {
        PeopleSort::ALL.into_iter().find(|x| x.key() == s)
    }
}

/// 右栏抽屉的四个面板(参考件右抽屉的属性 / 标签 / 人物 / 地点四页)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum DrawerTab {
    /// 照片属性 + 标题。
    #[default]
    Props,
    /// 本张的标签增删与常用标签。
    Tags,
    /// 本张的人脸与人物命名。
    People,
    /// 本张的 GPS 与地图入口。
    Places,
}

impl DrawerTab {
    const ALL: [DrawerTab; 4] = [
        DrawerTab::Props,
        DrawerTab::Tags,
        DrawerTab::People,
        DrawerTab::Places,
    ];

    fn label(self) -> &'static str {
        match self {
            DrawerTab::Props => "属性",
            DrawerTab::Tags => "标签",
            DrawerTab::People => "人物",
            DrawerTab::Places => "地点",
        }
    }

    fn key(self) -> &'static str {
        match self {
            DrawerTab::Props => "props",
            DrawerTab::Tags => "tags",
            DrawerTab::People => "people",
            DrawerTab::Places => "places",
        }
    }

    fn from_key(s: &str) -> Option<DrawerTab> {
        DrawerTab::ALL.into_iter().find(|x| x.key() == s)
    }
}

struct ScanResult {
    scanned: usize,
    indexed: usize,
    /// 因 path+size+mtime 未变而跳过(快速路径命中)。
    unchanged: usize,
    /// 被排除规则跳过的目录数。
    excluded_dirs: usize,
    /// 删除对账移出的行数(仅启动增量对账做;以磁盘为准)。
    removed: usize,
    /// 被用户取消(而非正常完成)。
    cancelled: bool,
    err: Option<String>,
    scope: String,
}

type Dup = (i64, Vec<i64>);

/// 人脸扫描后台线程 → UI 的回报:逐张进度 + 收尾结果。
#[cfg(feature = "ai")]
enum FaceScan {
    Progress(usize, usize),
    Done(Result<(usize, usize), String>),
}

/// 左栏文件夹树的一个目录节点(节点号即 `FolderTree::nodes` 的下标)。
struct TreeNode {
    /// 行内显示名(末级目录名;根节点是盘符/`/`)。
    name: String,
    /// 完整路径(展开状态与选中判定的键)。
    path: String,
    kids: Vec<usize>,
    /// 该目录自己在 `App::folders` 里的下标 —— 只有装照片的目录才可选。
    folder_index: Option<usize>,
    /// 行缩略图用哪张照片代表本目录(本目录优先,否则取子树里第一个命中的)。
    photo_id: Option<i64>,
}

/// 由 `App::folders` 反推的目录树(含中间层目录,它们本身可以没有照片)。
#[derive(Default)]
struct FolderTree {
    /// 对应的 `rows_gen`;不等即视为过期,重建。
    gen: u64,
    nodes: Vec<TreeNode>,
    roots: Vec<usize>,
}

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
    /// 后台入库任务的动词(整库/目录扫描是「扫描」,导入页勾选取用「导入」):
    /// 两条路共用同一个结果通道与状态栏进度,措辞得跟着任务变。
    scan_verb: &'static str,
    status: String,
    dups: Vec<Dup>,
    /// 重复视图:包含近似重复(hamming ≤ NEAR_DUP_DIST)。
    dup_near: bool,
    id_row: HashMap<i64, usize>,
    /// 预览中的照片 id(存 id 而非行索引:后台扫描/移除触发 rebuild 后不错位)。
    preview: Option<i64>,
    preview_tex: Option<(i64, egui::TextureHandle)>,
    /// P1-10:预览后台 worker(解码 + 落盘缓存,不阻塞 UI)。
    preview_bg: Option<crate::thumbs::PreviewWorker>,
    /// 预览中的照片原文件已不存在:预览窗显示明确提示,不再无限转圈。
    preview_missing: Option<i64>,
    /// 预览后台解码失败(非"文件不存在"):同样要终止"生成中"状态。
    preview_failed: Option<i64>,
    /// 待移出图库的 (id, 路径, 首次发现缺失的时刻)。预览路径发现,
    /// 统一在下一帧预 UI 阶段清理 —— 不能在预览窗闭包里直接重建 rows
    /// (闭包捕获的行索引会失效)。等一小段确认期再删,避免网络盘/移动盘
    /// 瞬时不可用时误清索引(星标/标签等元数据随之丢失)。
    pending_missing: Vec<(i64, String, std::time::Instant)>,
    bake_rx: Option<mpsc::Receiver<Result<(), String>>>,
    input_tag: String,
    export_rx: Option<mpsc::Receiver<(usize, Vec<String>)>>,
    wifi: Option<crate::net::WifiHandle>,
    wifi_panel: bool,
    // —— 菜单相关状态 ——
    sel: BTreeSet<i64>,
    sort_by: timeline::SortBy,
    sort_rev: bool,
    /// 工具栏过滤开关(星标/人物/视频/地理标记;全 false = 显示全部)。
    filters: timeline::Filters,
    /// 当前缩略图档位(P1-9:小 96 / 普通 256 / 大 512 / 预览 1024)。
    thumb_tier: crate::thumbs::Tier,
    tags: Vec<timeline::TagInfo>,
    sel_tag: Option<String>,
    show_about: bool,
    /// 「关于」窗口里 Markdown 更新日志的渲染缓存(egui_commonmark)。
    about_cache: egui_commonmark::CommonMarkCache,
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
    /// 坏文件台账窗口开关(方案 P3-22)。
    show_bad_media: bool,
    /// 台账快照(开窗与点「刷新」时重读)。
    bad_media_rows: Vec<crate::db::BadMedia>,
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
    /// 本帧允许同步解码的缩略图**像素体积**(RGBA8 字节数)。
    ///
    /// 方案 P4-23③:原来是「每帧 64 张」的张数上限,在中档(256px,256KB/张)
    /// 下约 16MB —— 没问题;但预览档(1024px,4MB/张)同样 64 张就是 256MB 的
    /// 解码 + 纹理上传,一帧几百毫秒,「UI 永不阻塞」在大档上等于没做。
    /// 改成字节口径后各档的每帧耗时才拉平(见 [`crate::scroll::decode_budget_for`])。
    thumb_decode_budget: usize,
    /// 本帧已用的解码配额(字节;每帧开头清零)。
    thumb_decode_count: usize,
    /// 当前档位已生成缩略图的数量(状态栏用)。
    /// 只在切档 / 批次回报 / 重建图库时刷新 —— 逐帧查库会抢锁且白跑 COUNT(*)。
    tier_cached: usize,
    /// 图库行集的版本号(每次 rebuild 自增)。缓存的派生数据靠它判新旧。
    rows_gen: u64,
    /// 「文件夹」视图的行下标缓存:(版本号, 文件夹, 行下标, 组头日期)。
    /// 每帧从全部行里按目录筛一遍是 O(行数) 次路径字符串运算,没必要。
    folder_cells: (u64, String, Vec<usize>, String),
    /// 「文件夹」视图一次铺开所有文件夹时的分组缓存:(版本号, [(文件夹, 行下标, 组头日期)])。
    /// `by_folder` 每行都要 clone 一个目录串,逐帧重跑大图库必掉帧。
    folder_groups: (u64, Vec<(String, Vec<usize>, String)>),
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
    /// 右侧属性面板是否展开(可伸缩,平时收起;设置项 drawer_open)。
    drawer_open: bool,
    /// 右栏抽屉当前页签(属性/标签/人物/地点;设置项 drawer_tab)。
    drawer_tab: DrawerTab,
    /// 效果面板当前标签页(调整/曲线/一键/裁剪/维护)。
    fx_tab: usize,
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
    // —— 预览缩放/平移(参考件 View and Edit 单图视图:滚轮缩放、拖拽平移、
    // 双击切换适应窗口/100% 像素)——
    /// 缩放系数:1.0 = 适应窗口;上限 8 倍。
    preview_zoom: f32,
    /// 平移偏移(屏幕像素,相对图区中心)。
    preview_pan: egui::Vec2,
    /// 裁剪模式开关(拖拽画裁剪框;开启时平移手势让位)。
    crop_mode: bool,
    /// 放大超过预览档(1024px)分辨率时按需解码的全图缓存(照片 id, RGBA)。
    /// 内存上限约 40MP,超出则停在预览档(状态栏提示)。
    preview_full: Option<(i64, image::RgbaImage)>,
    /// 全图解码在途照片 id。
    zoom_pending: Option<i64>,
    // —— 裁剪工具(编辑视图的 Crop;比例预设取经典图库常用宽高比清单)——
    /// 裁剪框草稿(显示图像的归一化坐标 0..1;[x0, y0, x1, y1],用前归一)。
    crop_rect: Option<[f32; 4]>,
    /// 裁剪比例锁定(像素比 w / h;None = 自由)。
    crop_ratio: Option<(f32, f32)>,
    /// 裁剪拖拽锚点(屏幕坐标;None = 未在拖拽)。
    crop_drag: Option<egui::Pos2>,
    /// 自定义比例(设置项「custom_crop_ratios」,name|w|h 分号分隔)。
    custom_ratios: Vec<(String, f32, f32)>,
    /// 自定义比例录入框(宽 / 高 / 名称)。
    crop_cw: String,
    crop_ch: String,
    crop_cname: String,
    // —— 幻灯片放映(对应原版 视图 > 幻灯片放映:可调间隔与循环开关)——
    /// 放映中(全屏接管主区)。
    slideshow: bool,
    /// 上次自动切换时刻。
    slide_last: std::time::Instant,
    /// 暂停(空格切换)。
    slide_pause: bool,
    /// 每张停留秒数(原版默认间隔暂无对应资料,取 3 秒)。
    slide_interval: f32,
    /// 循环放映(设置项 slideshow_loop,默认开)。
    slide_loop: bool,
    // —— 标题(Caption,参考件)——
    /// 预览窗标题编辑缓冲(照片 id, 文本)。
    caption_edit: Option<(i64, String)>,
    /// 缩略图下方显示标题(参考件 Thumbnail captions;设置项 thumb_captions)。
    thumb_captions: bool,
    // —— 相簿(参考件 Album)——
    /// 当前查看的相簿。
    sel_album: Option<i64>,
    /// 新建相簿输入框。
    album_input: String,
    // —— 属性 / 选项窗口(原版 File > Properties / Tools > Options 等价物)——
    show_props: bool,
    show_options: bool,
    /// 选项窗口当前页签(0 常规 / 1 电子邮件 / 2 幻灯片)。
    options_tab: usize,
    /// 缓存目录占用(文件数, 字节数)+ 采样时刻。整树遍历不放在每帧里跑,
    /// 选项页打开时按时间片刷新(迁移完立刻重采)。
    cache_usage: Option<(usize, u64, std::time::Instant)>,
    /// 文件夹视图树形形态(参考件左栏可逐级展开的那棵树;设置项 folder_tree,缺省树形)。
    folder_tree: bool,
    /// 中间栏是否一次铺开所有文件夹(每组一个相册组头)。
    /// 左栏点了具体文件夹即收窄为该组;左栏「全部文件夹」再铺回去。
    folder_show_all: bool,
    /// 左栏每行画该目录的缩略图(参考件 View > Show Thumbnails in Library;
    /// 设置项 tree_thumbs)。
    tree_thumbs: bool,
    /// 文件夹树的节点缓存(键 = rows_gen;逐帧重建是 O(目录数×层级) 的白活)。
    folder_tree_cache: FolderTree,
    /// 展开中的目录路径(点三角切换;选中项的祖先链自动展开)。
    tree_open: HashSet<String>,
    /// 树行缩略图纹理缓存(按照片 id;随当前档位变化,数量有上限就整体丢弃)。
    tree_texs: HashMap<i64, egui::TextureHandle>,
    /// 当前整页(参考件是主面板与导入面板两套整页互切,不是弹窗)。
    page: PageKind,
    /// 导航历史栈(后/前进逐站回退;每站记住整页 + 视图 + 左栏选中)。
    nav_hist: Vec<NavStop>,
    /// 历史游标(指向当前站;新跳转会截断游标之后的"前进"分支)。
    nav_at: usize,
    /// 导入页正在浏览的源目录(None = 还没选源)。
    import_src: Option<PathBuf>,
    /// 导入页目录清单缓存(只在切源/点刷新时读盘,不逐帧 read_dir)。
    import_entries: Vec<ImportEntry>,
    /// 导入页勾选的行号(下标对应 `import_entries`)。
    import_sel: BTreeSet<usize>,
    /// 最近一次浏览过的导入源(历史站退回导入页时用它恢复清单)。
    last_import_src: String,
    /// 人物列表排序方式(参考件 Sort People by Name / Amount / Top 10)。
    people_sort: PeopleSort,
    /// 工具栏「新建相册」弹窗(选中照片时把选中项一并放进新相册)。
    album_new_win: bool,
    /// 当前视图里尚未生成缩略图的张数(>0 → 工具栏最右的动画 spinner)。
    pending_thumbs: usize,
    /// 壁纸设置结果通道(后台解码 + 落盘 + SystemParametersInfoW)。
    wallpaper_rx: Option<mpsc::Receiver<Result<String, String>>>,
    /// 放大态专用纹理(全图 + 效果栈合成;键 = 照片 id + 栈签名,平移/缩放复用)。
    zoom_tex: Option<(i64, String, egui::TextureHandle)>,
    /// 缩放/平移状态归属的照片 id(换照片时自动复位)。
    zoom_active_for: Option<i64>,
    /// 全图解码结果通道(Option = 解码失败或超过内存上限);防重复派发的在途照片 id。
    zoom_rx: Option<mpsc::Receiver<(i64, Option<image::RgbaImage>)>>,
    #[cfg(feature = "ai")]
    face_rx: Option<mpsc::Receiver<FaceScan>>,
    /// 人脸扫描本批进度(已走张数, 本批总张数)。
    #[cfg(feature = "ai")]
    face_prog: (usize, usize),
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
    /// 右栏「人物」页正在改名的脸(face_id, 草稿名);失焦/回车即写回所属人物。
    #[cfg(feature = "ai")]
    face_name_draft: Option<(i64, String)>,
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
    // —— 快速查找(索引服务管道客户端) ——
    /// 索引服务链路状态。降级横幅、选项页、状态页都读它。
    ///
    /// 索引本身在服务进程里:UI 不再持有 `IndexFile`,
    /// 也不再读 `nameindex.bin`,一切走 `\\.\pipe\maliang-idx`。
    link: LinkInfo,
    /// 链路线程回报(服务概况 / 掉线原因 / 重建与装卸回执)。
    link_rx: Option<mpsc::Receiver<LinkMsg>>,
    /// 发给链路线程的命令(重连 / 装卸服务 / 全量重建 / 落盘)。
    link_tx: Option<mpsc::Sender<LinkCmd>>,
    /// 用户拒绝装服务后本次会话不再自动发起(可从搜索页/选项页手动再试)。
    link_declined: bool,
    /// 授权说明弹窗待示(没决定过就先问,问明白再动手)。
    svc_ask_grant: bool,
    /// 授权窗里「不再提示」勾选(仅窗口打开期间的临时态;拒绝/关窗且勾选才落库)。
    svc_never_ask: bool,
    /// 服务安装/卸载在途(提权子进程最长等 120s,期间不能重复发起)。
    svc_busy: bool,
    /// 首启范围对话框待示(方案 P3-18:两档单选,**无取消** —— 必须答一次)。
    scope_ask: bool,
    /// 对话框里当前选中的那一档。
    scope_choice: InitialScope,
    /// 快速查找输入与结果。
    name_query: String,
    name_results: Vec<NameRow>,
    /// 最近一次检索耗时(毫秒;状态栏与结果面板展示)。
    name_search_ms: f64,
    /// 类型过滤档(顶栏/搜索页共用,换档即重查)。
    name_class: crate::nameidx::FileClass,
    /// 搜索行缩略图缓存(路径 → 纹理;超上限按入序逐出)。
    name_thumbs: HashMap<String, egui::TextureHandle>,
    /// 缩略图缓存逐出顺序(与 name_thumbs 同步维护)。
    name_thumb_order: std::collections::VecDeque<String>,
    /// 后台检索线程的收发两端(tx 发请求,rx 收结果)。
    search_tx: Option<mpsc::Sender<SearchReq>>,
    search_rx: Option<mpsc::Receiver<SearchResp>>,
    /// 请求代数:每次 dispatch 递增,过期响应直接丢弃。
    search_gen: u64,
    /// 有一批检索在途(结果面板显示"检索中…")。
    search_inflight: bool,
    /// 查询缓存:(查询, 档位) → (结果, 耗时 ms)。重复输入零成本回显。
    name_search_cache: HashMap<(String, u8), (Vec<NameRow>, f64)>,
    /// 后台缩略图解码线程的收发两端。
    thumb_tx: Option<mpsc::Sender<String>>,
    thumb_rx: Option<mpsc::Receiver<(String, Option<image::DynamicImage>)>>,
    /// 已请求未回图的路径(去重 + 在途上限)。
    thumb_inflight: std::collections::HashSet<String>,
    /// 快速查找列表当前选中行(视觉反馈;右键菜单直接作用于所在行)。
    name_sel: Option<String>,
    /// 「屏幕上出现过、已经纳过」的目录(大写归一路径)。每个目录本进程只纳一次。
    adopt_seen: std::collections::HashSet<String>,
    /// 待纳入目录队列(原样路径):渲染帧只入队,`photo_grid` 每帧消化少量。
    adopt_queue: std::collections::VecDeque<String>,
    /// 本帧真正画在屏幕上的目录(原样路径,按出现顺序、去重)。帧首整批发给后台 worker。
    frame_dirs: Vec<String>,
    /// `frame_dirs` 的去重键(大写归一路径)。
    frame_dirs_seen: std::collections::HashSet<String>,
}

/// 索引服务的链路状态(由链路线程按心跳回报)。
#[derive(Debug, Clone)]
struct LinkInfo {
    /// 管道连着且握手通过 —— 有全盘快查。
    online: bool,
    /// 连不上的原因(降级横幅原样显示)。
    why: String,
    /// SCM 侧的服务状态:区分「没装」「装了没起」「正在起」。
    svc: crate::win::ServiceState,
    /// 索引条目总数与卷数。
    entries: u64,
    vols: u32,
    /// 正在全量建库的卷数(>0 时结果面板该说"正在建库"而不是"没有命中")。
    building: usize,
    /// 走不了 USN/MFT 的卷 → 原因(方案 P4-30:这些卷走兼容扫描,无实时快查)。
    compat: Vec<(char, String)>,
    /// 索引里有未落盘的变更。
    dirty: bool,
    /// 服务已运行秒数。
    uptime: u64,
    /// 上次拿到概况的时刻(状态页显示"多久前")。
    seen: Option<std::time::Instant>,
}

impl LinkInfo {
    fn new() -> Self {
        LinkInfo {
            online: false,
            why: String::new(),
            svc: crate::win::ServiceState::NotInstalled,
            entries: 0,
            vols: 0,
            building: 0,
            compat: Vec::new(),
            dirty: false,
            uptime: 0,
            seen: None,
        }
    }

    /// 把服务概况并进链路状态。
    fn apply(&mut self, s: &crate::ipc::StatusInfo) {
        self.online = true;
        self.why.clear();
        self.entries = s.vols.iter().map(|v| v.entries).sum();
        self.vols = s.vols.len() as u32;
        self.building = s.vols.iter().filter(|v| v.building).count();
        self.compat = s
            .vols
            .iter()
            .filter(|v| !v.why.is_empty())
            .map(|v| (v.letter, v.why.clone()))
            .collect();
        self.dirty = s.dirty;
        self.uptime = s.uptime_secs;
        self.seen = Some(std::time::Instant::now());
    }
}

/// 链路线程 → UI 的回报。
#[derive(Debug)]
enum LinkMsg {
    /// 服务在线:概况 + SCM 状态。
    Up(crate::ipc::StatusInfo, crate::win::ServiceState),
    /// 服务离线:连不上的原因 + SCM 状态(判「没装」还是「装了没起」)。
    Down(String, crate::win::ServiceState),
    /// 全量重建请求的回执(None = 已受理)。
    Rebuild(Option<String>),
    /// 服务动作的回执:`op` = 哪个动作(「安装」「卸载」「停止」「启动」),
    /// 第二项 None = 成功。停/起只改进程状态,不该动用户的授权意向。
    Service(&'static str, Option<String>),
}

/// UI → 链路线程的命令。装卸与停起服务都要等 UAC(最长 120s),必须放线程里做。
#[derive(Debug)]
enum LinkCmd {
    /// 立刻重连(用户点了「重试」,或服务刚装好)。
    Retry,
    /// 提权安装并启动索引服务 —— 本功能唯一一次 UAC。
    Install,
    /// 提权停止并删除索引服务。
    Uninstall,
    /// 提权停止服务,保留注册项(为了让程序自己换二进制/临时让磁盘歇口)。
    Stop,
    /// 提权启动已注册的服务,和 `Stop` 成对。
    Start,
    /// 请求全量重建;空列表 = 全部可扫卷。
    Rebuild(Vec<char>),
    /// 让服务立刻把内存索引落盘。
    Flush,
}

/// 链路心跳间隔:在线时按这个节奏问一次 `Status`。
const LINK_TICK: std::time::Duration = std::time::Duration::from_secs(3);
/// 连不上时的退避上限(服务可能正在被安装,别把重连打成忙等)。
const LINK_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// 启动索引服务链路线程:维持一根管道连接、周期问 `Status`、掉线退避重连,
/// 并代办装卸服务(要等 UAC)与全量重建。UI 只读回报,自己不碰管道。
fn spawn_link_worker(
    ctx: egui::Context,
) -> (mpsc::Sender<LinkCmd>, mpsc::Receiver<LinkMsg>) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<LinkCmd>();
    let (msg_tx, msg_rx) = mpsc::channel::<LinkMsg>();
    std::thread::Builder::new()
        .name("idx-link".into())
        .spawn(move || {
            crate::win::set_current_thread_priority(crate::win::Priority::BelowNormal);
            let mut client: Option<crate::ipc::Client> = None;
            let mut backoff = std::time::Duration::from_secs(1);
            // 同一句失败原因只报一次:退避期间每拍都失败,逐拍刷屏没意义。
            let mut last_down = String::new();
            // 首拍立刻探一次,别让用户等满一个心跳才看见状态。
            let mut tick_now = true;
            loop {
                if !tick_now {
                    match cmd_rx.recv_timeout(LINK_TICK) {
                        Ok(cmd) => {
                            tick_now = link_cmd(cmd, &mut client, &mut backoff, &msg_tx, &ctx);
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        // 发送端随 App 一起没了 = 收工。
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                tick_now = false;
                if client.is_none() {
                    match link_connect() {
                        Ok(c) => {
                            client = Some(c);
                            backoff = std::time::Duration::from_secs(1);
                        }
                        Err(e) => {
                            if e != last_down {
                                last_down = e.clone();
                                let _ = msg_tx.send(LinkMsg::Down(e, crate::win::service_state()));
                                ctx.request_repaint();
                            }
                            // 退避期间来了命令(比如刚装完服务的 Retry)要能立刻醒。
                            match cmd_rx.recv_timeout(backoff) {
                                Ok(cmd) => {
                                    tick_now =
                                        link_cmd(cmd, &mut client, &mut backoff, &msg_tx, &ctx);
                                }
                                Err(mpsc::RecvTimeoutError::Timeout) => tick_now = true,
                                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                            }
                            backoff = (backoff * 2).min(LINK_BACKOFF_MAX);
                            continue;
                        }
                    }
                }
                // 连上了就清掉旧原因:下一句失败要如实报出来。
                last_down.clear();
                let c = client.as_mut().expect("上面刚确保连接在位");
                match c.call_ok(&crate::ipc::Req::Status) {
                    Ok(crate::ipc::Resp::Status(s)) => {
                        let _ = msg_tx.send(LinkMsg::Up(s, crate::win::service_state()));
                        ctx.request_repaint();
                    }
                    Ok(other) => {
                        client = None;
                        let _ = msg_tx.send(LinkMsg::Down(
                            format!("服务对 Status 回了意外答复:{other:?}"),
                            crate::win::service_state(),
                        ));
                        ctx.request_repaint();
                    }
                    Err(e) => {
                        // 管道断了(服务停了/重启了):丢掉连接,下一拍重连。
                        client = None;
                        let _ = msg_tx.send(LinkMsg::Down(e, crate::win::service_state()));
                        ctx.request_repaint();
                    }
                }
                // tick_now 在本拍开头已置 false:落回循环顶部等一个心跳,
                // 别把 3 秒一次的 Status 打成忙等。
            }
        })
        .expect("启动索引服务链路线程失败");
    (cmd_tx, msg_rx)
}

/// 处理一条 UI 命令。返回 true = 立刻再跑一拍心跳(重试/装卸/重建后马上看状态)。
fn link_cmd(
    cmd: LinkCmd,
    client: &mut Option<crate::ipc::Client>,
    backoff: &mut std::time::Duration,
    msg_tx: &mpsc::Sender<LinkMsg>,
    ctx: &egui::Context,
) -> bool {
    match cmd {
        LinkCmd::Retry => {
            *client = None;
            *backoff = std::time::Duration::from_secs(1);
            true
        }
        LinkCmd::Install => {
            let r = crate::win::service_install_elevated();
            let _ = msg_tx.send(LinkMsg::Service("安装", r.err()));
            ctx.request_repaint();
            // 装完立刻试连:服务是被安装步骤顺手起来的,不必等退避。
            *client = None;
            *backoff = std::time::Duration::from_secs(1);
            true
        }
        LinkCmd::Uninstall => {
            let r = crate::win::service_uninstall_elevated();
            *client = None;
            let _ = msg_tx.send(LinkMsg::Service("卸载", r.err()));
            ctx.request_repaint();
            true
        }
        // 停与起:提权子进程只动 SCM 里的进程状态,注册项和映像路径都不碰。
        // 断掉管道再按 1s 退避重连 —— 停下来的服务不会应答,别等下一个心跳周期。
        LinkCmd::Stop => {
            let r = crate::win::service_stop_elevated();
            *client = None;
            *backoff = std::time::Duration::from_secs(1);
            let _ = msg_tx.send(LinkMsg::Service("停止", r.err()));
            ctx.request_repaint();
            true
        }
        LinkCmd::Start => {
            let r = crate::win::service_start_elevated();
            *client = None;
            *backoff = std::time::Duration::from_secs(1);
            let _ = msg_tx.send(LinkMsg::Service("启动", r.err()));
            ctx.request_repaint();
            true
        }
        LinkCmd::Rebuild(letters) => {
            let r = link_call(client, &crate::ipc::Req::Rebuild { letters });
            let _ = msg_tx.send(LinkMsg::Rebuild(r.err()));
            ctx.request_repaint();
            true
        }
        LinkCmd::Flush => {
            let _ = link_call(client, &crate::ipc::Req::Flush);
            false
        }
    }
}

/// 借现有连接发一次请求;连接不在就先连(命令不该因为心跳没到点而失败)。
fn link_call(
    client: &mut Option<crate::ipc::Client>,
    req: &crate::ipc::Req,
) -> Result<crate::ipc::Resp, String> {
    if client.is_none() {
        *client = Some(link_connect()?);
    }
    let c = client.as_mut().expect("上面刚连上");
    match c.call_ok(req) {
        Ok(r) => Ok(r),
        Err(e) => {
            // 失败即视为这根连接不可信,丢掉让下一拍重连。
            *client = None;
            Err(e)
        }
    }
}

/// 连上管道并完成握手(协议版本对不上视为连不上)。
fn link_connect() -> Result<crate::ipc::Client, String> {
    let mut c = crate::ipc::Client::connect(500)?;
    match c.call(&crate::ipc::Req::Hello {
        proto: crate::ipc::PROTO_VERSION,
    }) {
        Ok(crate::ipc::Resp::Hello { .. }) => Ok(c),
        Ok(other) => Err(format!("握手答复异常:{other:?}")),
        Err(e) => Err(e),
    }
}

/// 快速查找结果行。
///
/// 服务回的命中自带 size/mtime(这几列服务端本来就在内存里握着),
/// 所以 UI 侧不再逐条 `stat` —— 省掉一趟 500 次文件 IO,冷目录上差别很明显。
/// 投影在服务内完成(方案 P4-29):派生状态也由服务批量查库后随命中一起回。
#[derive(Debug, Clone)]
struct NameRow {
    hit: crate::nameidx::SearchHit,
    /// 字节数;目录为 0。
    size: u64,
    /// 修改时间,UNIX 秒;0 = 这卷走 USN 降级路径,索引里没有权威时间。
    mtime: u64,
    /// 派生状态([`crate::db::RICH_*`]);[`crate::ipc::RICH_UNKNOWN`] = 这条
    /// 不在图库数据库里(非媒体文件、或还没落库),与「未纳入派生作用域」不是一回事。
    rich_state: u8,
}

impl NameRow {
    fn from_hit(h: crate::ipc::Hit) -> Self {
        NameRow {
            hit: crate::nameidx::SearchHit {
                letter: h.letter,
                idx: h.idx,
                name: h.name,
                path: h.path,
                is_dir: h.is_dir,
            },
            size: h.size,
            mtime: h.mtime,
            rich_state: h.rich_state,
        }
    }

    /// 结果表「图库」列的短标签。
    ///
    /// 快查是 L1(内存文件名索引)的东西,它看得见全盘;图库只收媒体且派生还受
    /// 作用域门控。这一列就是把两者的差集讲清楚,免得用户以为「搜到了但打不开缩略图」
    /// 是坏了。
    fn rich_label(&self) -> &'static str {
        if self.hit.is_dir {
            return "—";
        }
        // 常量不能带 `as` 当模式用,所以把被匹配的值转成 i32(db 侧的原类型)。
        match i32::from(self.rich_state) {
            crate::db::RICH_OUT => "未纳入范围",
            crate::db::RICH_PENDING => "待派生",
            crate::db::RICH_DONE => "已入库",
            crate::db::RICH_BAD => "读取失败",
            _ => "非图库文件",
        }
    }
}

/// 后台检索请求。
struct SearchReq {
    gen: u64,
    q: String,
    class: crate::nameidx::FileClass,
}

/// 后台检索结果。
enum SearchResp {
    /// 命中列表(毫秒级)。
    Hits {
        gen: u64,
        hits: Vec<NameRow>,
        ms: f64,
    },
    /// 服务连不上:这一轮出不来结果。UI 据此显示降级,而不是"没有命中"。
    Down {
        gen: u64,
        why: String,
    },
}

/// 启动常驻检索线程:收请求(合并到最新)→ 走管道问服务 → 回报命中。
///
/// 线程自己持一根管道连接,与链路线程那根互不干扰(服务端每连接一线程);
/// 断了就地重连,连不上如实回报 `Down`。
fn spawn_search_worker() -> (
    mpsc::Sender<SearchReq>,
    mpsc::Receiver<SearchResp>,
    std::thread::JoinHandle<()>,
) {
    let (req_tx, req_rx) = mpsc::channel::<SearchReq>();
    let (res_tx, res_rx) = mpsc::channel::<SearchResp>();
    let h = std::thread::Builder::new()
        .name("search".into())
        .spawn(move || {
            crate::win::set_current_thread_priority(crate::win::Priority::BelowNormal);
            let mut client: Option<crate::ipc::Client> = None;
            while let Ok(req) = req_rx.recv() {
                // 合并:队列里还有更新的请求就只跑最新(打字流不逐键全跑)。
                let mut req = req;
                while let Ok(latest) = req_rx.try_recv() {
                    req = latest;
                }
                if client.is_none() {
                    if let Err(e) = link_connect().map(|c| client = Some(c)) {
                        if res_tx.send(SearchResp::Down { gen: req.gen, why: e }).is_err() {
                            return;
                        }
                        continue;
                    }
                }
                let c = client.as_mut().expect("上面刚连上");
                let t0 = std::time::Instant::now();
                let resp = c.call_ok(&crate::ipc::Req::Search {
                    text: req.q.clone(),
                    limit: 500,
                    class: crate::ipc::class_to_wire(req.class),
                });
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                let hits = match resp {
                    Ok(crate::ipc::Resp::Hits(h)) => h,
                    Ok(other) => {
                        client = None;
                        let why = format!("服务对 Search 回了意外答复:{other:?}");
                        if res_tx.send(SearchResp::Down { gen: req.gen, why }).is_err() {
                            return;
                        }
                        continue;
                    }
                    Err(e) => {
                        client = None;
                        if res_tx.send(SearchResp::Down { gen: req.gen, why: e }).is_err() {
                            return;
                        }
                        continue;
                    }
                };
                let rows: Vec<NameRow> = hits.into_iter().map(NameRow::from_hit).collect();
                if res_tx
                    .send(SearchResp::Hits {
                        gen: req.gen,
                        hits: rows,
                        ms,
                    })
                    .is_err()
                {
                    return;
                }
            }
        })
        .expect("启动检索线程失败");
    (req_tx, res_rx, h)
}

/// 字节数的短格式(与导入面板的 KB/MB 口径一致,补 GB/B 两档)。
fn fmt_size(bytes: u64) -> String {
    let b = bytes as f64;
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{b:.0} KB")
    } else {
        format!("{bytes} B")
    }
}

/// 修改时间(UNIX 秒,来自 L1 索引)的短格式;0 = 这卷没有权威时间。
fn fmt_mtime(secs: u64) -> String {
    use chrono::TimeZone;
    if secs == 0 {
        return "—".to_string();
    }
    match chrono::Local.timestamp_opt(secs as i64, 0).single() {
        Some(t) => t.format("%Y-%m-%d %H:%M").to_string(),
        None => String::new(),
    }
}

/// 运行时长的短格式(选项页显示索引服务跑了多久)。
fn fmt_uptime(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, (secs % 86400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("{d} 天 {h} 小时")
    } else if h > 0 {
        format!("{h} 小时 {m} 分")
    } else if m > 0 {
        format!("{m} 分")
    } else {
        format!("{secs} 秒")
    }
}

/// 启动常驻缩略图解码线程:收路径 → 纯路径解码 128px 预览(可失败)。
/// 与图库缩略图 worker 无关:这些文件多半未入库,走文件系统直读。
fn spawn_thumb_worker() -> (
    mpsc::Sender<String>,
    mpsc::Receiver<(String, Option<image::DynamicImage>)>,
    ) {
    let (tx, rx) = mpsc::channel::<String>();
    let (out_tx, out_rx) = mpsc::channel::<(String, Option<image::DynamicImage>)>();
    std::thread::Builder::new()
        .name("search-thumb".into())
        .spawn(move || {
            crate::win::set_current_thread_priority(crate::win::Priority::BelowNormal);
            while let Ok(p) = rx.recv() {
                let img = crate::thumbs::load_for_preview(Path::new(&p), 128)
                    .map(|(img, _)| img);
                if out_tx.send((p, img)).is_err() {
                    return;
                }
            }
        })
        .expect("启动搜索缩略图线程失败");
    (tx, out_rx)
}

/// 用户对口全盘索引服务的意向(设置项 `svc_wanted`)。
///
/// 进程模型换成常驻提权服务后,原来那三档授权
/// (计划任务 / 每次提权 / 不做)塌成两档:装 or 不装。装服务只过一次 UAC,
/// 之后由 SCM 自动拉起,所以"每次单独提权"这一档没有存在意义了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SvcWant {
    /// 没问过(首次启动先说明再动手)。
    Unasked,
    /// 要:装上服务,以后静默。
    Yes,
    /// 不要:走 walkdir 兜底,没有全盘快查。
    No,
}

impl SvcWant {
    fn as_setting(self) -> &'static str {
        match self {
            SvcWant::Unasked => "",
            SvcWant::Yes => "1",
            SvcWant::No => "0",
        }
    }

    fn from_setting(s: &str) -> Self {
        match s {
            "1" => SvcWant::Yes,
            "0" => SvcWant::No,
            _ => SvcWant::Unasked,
        }
    }
}

/// 首启范围对话框的两档(方案 P3-18:全盘完整扫描 / 仅常见目录)。
///
/// 这一档决定的是**派生**作用域,不是库的作用域:两档都全盘发现、全盘入库,
/// 区别只在"哪些目录的图要出缩略图 / 读 EXIF"。没进作用域的行结构列齐全、
/// 派生列全空,网格里显示成占位,点进去才补(方案 P3-19)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitialScope {
    /// 全盘:每个可扫卷根各落一条意图。
    Complete,
    /// 常用目录:我的文档 / 图片 / 桌面(不含视频,用户已拍)。
    Limited,
}

/// 近似重复的 dHash Hamming 阈值(64 bit 中 ≤10 不同 ≈ 肉眼难辨)。
const NEAR_DUP_DIST: u32 = 10;

/// 预览发现原文件消失后的确认期:到期仍不存在才移出图库。
/// 网络盘/移动盘瞬时不可用时,一次 `exists()` 失败不足以判定删除。
const MISSING_CONFIRM: std::time::Duration = std::time::Duration::from_secs(2);

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

/// 导入页源列表的一行:图标 + 名称,选中态高亮。返回是否被点中。
fn source_row(
    ui: &mut egui::Ui,
    icon: &'static str,
    label: &str,
    path: &Path,
    cur: &Option<PathBuf>,
) -> bool {
    let active = cur.as_deref() == Some(path);
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.add(toolbar_icon(icon));
        if ui.selectable_label(active, label).clicked() {
            clicked = true;
        }
    });
    clicked
}

/// 小节标题(导入页源列表分组:我的文件夹 / 设备 / 我的电脑)。
fn muted_title(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(12.0)
            .color(crate::theme::p().muted)
            .strong(),
    );
}

/// 列一个导入源目录:可入库的图片/RAW 文件 + 可继续下探的子目录。
///
/// `indexed` 是该目录已入库的文件路径集合(调用方查库得到);
/// 只有这一步读盘,结果由 `set_import_source` 缓存,不逐帧重读。
/// 子目录排前面、文件按名排;不递归(下探交给行点击)。
fn list_import_dir(dir: &Path, indexed: &HashSet<String>) -> Vec<ImportEntry> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<ImportEntry> = Vec::new();
    for e in rd.flatten() {
        let Ok(md) = e.metadata() else {
            continue;
        };
        let path = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let date = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| format_ts(d.as_millis() as i64)[..10].to_string())
            .unwrap_or_default();
        if md.is_dir() {
            out.push(ImportEntry {
                path,
                name,
                size_s: String::new(),
                date,
                is_dir: true,
                in_library: false,
            });
            continue;
        }
        if !crate::scan::is_indexable(&path) {
            continue;
        }
        let kb = md.len() as f64 / 1024.0;
        let size_s = if kb >= 1024.0 {
            format!("{:.1} MB", kb / 1024.0)
        } else {
            format!("{kb:.0} KB")
        };
        let key = path.to_string_lossy().into_owned();
        out.push(ImportEntry {
            path,
            name,
            size_s,
            date,
            is_dir: false,
            in_library: indexed.contains(&key),
        });
    }
    out.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    out
}

/// 由「装了照片的目录」列表反推目录树:链上每一层都建节点,所以中间层目录
/// (本身没有照片)也在树里,能一路展开到底层相册;`rep` 给出每个目录的代表照片,
/// 没有照片的中间层借用子树里第一个命中的,行上才不会是一片空位。
/// 返回 (节点表, 根节点号)。
fn build_folder_tree(
    folders: &[String],
    rep: &HashMap<String, i64>,
) -> (Vec<TreeNode>, Vec<usize>) {
    let mut nodes: Vec<TreeNode> = Vec::new();
    let mut by_path: HashMap<String, usize> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    // 每个目录的祖先链(根 → 自身)与它的照片/下标,先记下来再统一回填。
    let mut chains: Vec<(Vec<usize>, Option<i64>, usize)> = Vec::new();
    for (fi, f) in folders.iter().enumerate() {
        let mut chain: Vec<String> = Path::new(f)
            .ancestors()
            .take(64)
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        chain.reverse();
        chain.dedup();
        let mut ids: Vec<usize> = Vec::with_capacity(chain.len());
        let mut parent: Option<usize> = None;
        for seg in &chain {
            let id = match by_path.get(seg) {
                Some(&i) => i,
                None => {
                    let name = Path::new(seg)
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| seg.clone());
                    nodes.push(TreeNode {
                        name,
                        path: seg.clone(),
                        kids: Vec::new(),
                        folder_index: None,
                        photo_id: None,
                    });
                    let id = nodes.len() - 1;
                    by_path.insert(seg.clone(), id);
                    match parent {
                        Some(pi) => nodes[pi].kids.push(id),
                        None => roots.push(id),
                    }
                    id
                }
            };
            ids.push(id);
            parent = Some(id);
        }
        chains.push((ids, rep.get(f).copied(), fi));
    }
    // 第一遍:目录自己的照片与 `folders` 下标只挂在链尾(即该目录节点)。
    for (ids, pid, fi) in &chains {
        if let (Some(&last), Some(pid)) = (ids.last(), pid) {
            nodes[last].folder_index = Some(*fi);
            nodes[last].photo_id = Some(*pid);
        }
    }
    // 第二遍:还空着的(中间层)借子树第一个命中。
    for (ids, pid, _) in &chains {
        if pid.is_none() {
            continue;
        }
        for &id in ids {
            if nodes[id].photo_id.is_none() {
                nodes[id].photo_id = *pid;
            }
        }
    }
    (nodes, roots)
}

/// 从**已落盘的缩略图**加载纹理(不碰原图)。
///
/// P0-1 关键:原实现 `image::open(path)` 直接解原图,网格每帧每个可见格子
/// 都重解码一次 JPEG,是帧率杀手。现在只读缓存里的小图。
///
/// P1-9:按档位读(`tier`)。不同档位落在不同目录
/// (三张缩略图缓存表)。
fn thumb_tex(
    ctx: &egui::Context,
    id: i64,
    tier: crate::thumbs::Tier,
) -> Option<egui::TextureHandle> {
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

/// 网格预取的行数(视口上下各这么多行,方案 P4-23④)。
///
/// 预取线程口径:滚到哪就把前后几行的缩略图先备好。
/// 只吃当帧剩余的解码配额,不与真正可见的格子抢时间。
const PREFETCH_ROWS: usize = 2;

/// 一帧最多消化几个「屏幕上看到但还没纳入」的目录。
///
/// 首屏一次铺开上百个目录是常态(「最近更新」按天分组),不能让渲染帧去写上百次账;
/// 队列下一帧继续消化,人眼看不出先后。
const MAX_ADOPT_PER_FRAME: usize = 4;

/// 「看到但还没纳入」队列的长度上限:快速滚动一屏可能铺出上百个目录,
/// 排到 5 帧以后就没人等了 —— 后面的请求直接丢弃,那一格仍显示「未纳入范围」。
const MAX_ADOPT_QUEUE: usize = 24;

/// 一帧最多上报几个「屏幕上的目录」给后台 worker(显示优先的清单长度上限)。
const MAX_VISIBLE_DIRS: usize = 32;

/// 把任意长宽比的缩略图裁成正方形(取中间部分),用于网格瓦片。
fn cover_uv([tw, th]: [usize; 2]) -> egui::Rect {
    let (w, h) = (tw.max(1) as f32, th.max(1) as f32);
    if w > h {
        let f = h / w;
        let x = (1.0 - f) * 0.5;
        egui::Rect::from_min_max(egui::pos2(x, 0.0), egui::pos2(x + f, 1.0))
    } else {
        let f = w / h;
        let y = (1.0 - f) * 0.5;
        egui::Rect::from_min_max(egui::pos2(0.0, y), egui::pos2(1.0, y + f))
    }
}

/// 缩略图右上角的星标:已收藏常显,未收藏悬停时才出现;点击切换。
fn thumb_star(
    ui: &mut egui::Ui,
    id: i64,
    on: bool,
    image_rect: egui::Rect,
    image_hovered: bool,
    clicked: &mut bool,
) {
    if !on && !image_hovered {
        return;
    }
    let side = 22.0;
    let rect = egui::Rect::from_min_size(
        image_rect.right_top() + egui::vec2(-side - 6.0, 6.0),
        egui::vec2(side, side),
    );
    let resp = ui.interact(
        rect,
        egui::Id::new(("thumb-star", id)),
        egui::Sense::click(),
    );
    let t = crate::theme::p();
    let (fill, color) = if resp.hovered() {
        (t.badge, t.accent_strong)
    } else if on {
        (t.badge, t.accent)
    } else {
        (t.badge, t.badge_mute)
    };
    ui.painter().circle_filled(rect.center(), side * 0.5, fill);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        if on { "★" } else { "☆" },
        egui::FontId::proportional(13.0),
        color,
    );
    if resp.clicked() {
        *clicked = true;
    }
    resp.on_hover_text(if on { "取消星标" } else { "加星标" });
}

/// 主题化图标源表(Lucide,ISC 许可,见 assets/icons/LICENSE-lucide.txt):
/// 每个图标按主题烤好两套描边色,SVG 运行时栅格化,高 DPI 不糊。
const ICON_LIGHT: &[(&str, &[u8])] = &[
    (
        "arrow-left",
        include_bytes!("../assets/icons/light/arrow-left.svg"),
    ),
    (
        "arrow-right",
        include_bytes!("../assets/icons/light/arrow-right.svg"),
    ),
    (
        "hard-drive",
        include_bytes!("../assets/icons/light/hard-drive.svg"),
    ),
    (
        "download",
        include_bytes!("../assets/icons/light/download.svg"),
    ),
    ("folder", include_bytes!("../assets/icons/light/folder.svg")),
    (
        "folder-open",
        include_bytes!("../assets/icons/light/folder-open.svg"),
    ),
    (
        "folder-plus",
        include_bytes!("../assets/icons/light/folder-plus.svg"),
    ),
    (
        "smartphone",
        include_bytes!("../assets/icons/light/smartphone.svg"),
    ),
    ("images", include_bytes!("../assets/icons/light/images.svg")),
    (
        "image-plus",
        include_bytes!("../assets/icons/light/image-plus.svg"),
    ),
    ("play", include_bytes!("../assets/icons/light/play.svg")),
    ("mail", include_bytes!("../assets/icons/light/mail.svg")),
    (
        "wand-sparkles",
        include_bytes!("../assets/icons/light/wand-sparkles.svg"),
    ),
    (
        "panel-right",
        include_bytes!("../assets/icons/light/panel-right.svg"),
    ),
    ("moon", include_bytes!("../assets/icons/light/moon.svg")),
    (
        "layout-grid",
        include_bytes!("../assets/icons/light/layout-grid.svg"),
    ),
    (
        "list-tree",
        include_bytes!("../assets/icons/light/list-tree.svg"),
    ),
    (
        "sliders-horizontal",
        include_bytes!("../assets/icons/light/sliders-horizontal.svg"),
    ),
    ("star", include_bytes!("../assets/icons/light/star.svg")),
    ("smile", include_bytes!("../assets/icons/light/smile.svg")),
    ("video", include_bytes!("../assets/icons/light/video.svg")),
    (
        "map-pin",
        include_bytes!("../assets/icons/light/map-pin.svg"),
    ),
    ("search", include_bytes!("../assets/icons/light/search.svg")),
    ("x", include_bytes!("../assets/icons/light/x.svg")),
];
const ICON_DARK: &[(&str, &[u8])] = &[
    (
        "arrow-left",
        include_bytes!("../assets/icons/dark/arrow-left.svg"),
    ),
    (
        "arrow-right",
        include_bytes!("../assets/icons/dark/arrow-right.svg"),
    ),
    (
        "hard-drive",
        include_bytes!("../assets/icons/dark/hard-drive.svg"),
    ),
    (
        "download",
        include_bytes!("../assets/icons/dark/download.svg"),
    ),
    ("folder", include_bytes!("../assets/icons/dark/folder.svg")),
    (
        "folder-open",
        include_bytes!("../assets/icons/dark/folder-open.svg"),
    ),
    (
        "folder-plus",
        include_bytes!("../assets/icons/dark/folder-plus.svg"),
    ),
    (
        "smartphone",
        include_bytes!("../assets/icons/dark/smartphone.svg"),
    ),
    ("images", include_bytes!("../assets/icons/dark/images.svg")),
    (
        "image-plus",
        include_bytes!("../assets/icons/dark/image-plus.svg"),
    ),
    ("play", include_bytes!("../assets/icons/dark/play.svg")),
    ("mail", include_bytes!("../assets/icons/dark/mail.svg")),
    (
        "wand-sparkles",
        include_bytes!("../assets/icons/dark/wand-sparkles.svg"),
    ),
    (
        "panel-right",
        include_bytes!("../assets/icons/dark/panel-right.svg"),
    ),
    ("sun", include_bytes!("../assets/icons/dark/sun.svg")),
    (
        "layout-grid",
        include_bytes!("../assets/icons/dark/layout-grid.svg"),
    ),
    (
        "list-tree",
        include_bytes!("../assets/icons/dark/list-tree.svg"),
    ),
    (
        "sliders-horizontal",
        include_bytes!("../assets/icons/dark/sliders-horizontal.svg"),
    ),
    ("star", include_bytes!("../assets/icons/dark/star.svg")),
    ("smile", include_bytes!("../assets/icons/dark/smile.svg")),
    ("video", include_bytes!("../assets/icons/dark/video.svg")),
    (
        "map-pin",
        include_bytes!("../assets/icons/dark/map-pin.svg"),
    ),
    ("search", include_bytes!("../assets/icons/dark/search.svg")),
    ("x", include_bytes!("../assets/icons/dark/x.svg")),
];

/// 内置更新日志全文,「帮助 > 关于」里以 Markdown 渲染。
const CHANGELOG_MD: &str = include_str!("../CHANGELOG.md");

/// 取当前主题的图标字节;URI 带主题段,切主题即重载纹理。
fn icon_source(name: &str) -> (&'static str, &'static [u8]) {
    let table = if crate::theme::p().is_light {
        ICON_LIGHT
    } else {
        ICON_DARK
    };
    let (tag, bytes) = table
        .iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("未知工具栏图标: {name}"));
    (tag, bytes)
}

/// 工具栏图标(18px)。
fn toolbar_icon(name: &str) -> egui::Image<'static> {
    let (tag, bytes) = icon_source(name);
    let half = if crate::theme::p().is_light { "l" } else { "d" };
    egui::Image::from_bytes(format!("bytes://{half}/{tag}.svg"), bytes)
        .fit_to_exact_size(egui::vec2(18.0, 18.0))
}

/// 组头图标(参考件组头那枚大一号的目录图标)。
fn album_icon(name: &str, size: f32) -> egui::Image<'static> {
    let (tag, bytes) = icon_source(name);
    let half = if crate::theme::p().is_light { "l" } else { "d" };
    egui::Image::from_bytes(format!("bytes://big-{half}/{tag}-{size}.svg"), bytes)
        .fit_to_exact_size(egui::vec2(size, size))
}

/// 工具栏图标按钮(选中态即"这个视图档/过滤正生效")。
// 0.33 起 ImageButton 标了 deprecated,推荐 `Button::image`;但后者会把图片
// 高度限制到字体高度(见 button.rs 的 atom_max_height_font_size),
// 18px 工具栏图标与 26px 组头图标都会缩一号,所以这里继续用 ImageButton。
#[allow(deprecated)]
fn tool_button(name: &str, selected: bool) -> egui::ImageButton<'static> {
    egui::ImageButton::new(toolbar_icon(name))
        .selected(selected)
        .corner_radius(4)
}

fn load_cjk_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    // 主字体:内置阿里普惠体 3.0 Regular(免费可商用),拉丁与中文观感统一;
    // 系统微软雅黑等保留为 CJK 兜底。strong() 在 egui 里只是变色,一档字重即可。
    fonts.font_data.insert(
        "puhui".into(),
        std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/AlibabaPuHuiTi-3-55-Regular.ttf"
        ))),
    );
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "puhui".into());
    let candidates = [
        r"C:\Windows\Fonts\msyh.ttc",   // 微软雅黑(ttc,ab_glyph 兼容时优先)
        r"C:\Windows\Fonts\simhei.ttf", // 黑体(纯 ttf,最稳妥)
        r"C:\Windows\Fonts\simsun.ttc", // 宋体
    ];
    for path in candidates {
        let Ok(data) = std::fs::read(path) else {
            continue;
        };
        fonts.font_data.insert(
            "cjk".into(),
            std::sync::Arc::new(egui::FontData::from_owned(data)),
        );
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push("cjk".into());
        }
        ctx.set_fonts(fonts);
        return;
    }
    ctx.set_fonts(fonts);
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
    format!("{y:04}-{:02}-{:02} {h:02}:{m:02}:{s:02}", m_idx + 1, d + 1)
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

    /// 是否已有纹理(**不动** LRU 顺序 —— 预取路径用它判重,
    /// 预取到的格子不该因此变得比真正可见的格子更"新")。
    fn has(&self, id: i64) -> bool {
        self.map.contains_key(&id)
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
        // 配色:默认 Picasa 灰,可切蓝黑(设置项 ui_theme,入口在 选项 > 常规)。
        let ui_theme = store
            .get_setting("ui_theme")
            .and_then(|v| crate::theme::Kind::from_key(&v))
            .unwrap_or(crate::theme::Kind::Gray);
        crate::theme::set_kind(ui_theme);
        crate::theme::apply(&cc.egui_ctx);
        // SVG 图标加载器(工具栏图标等),必须在首帧前装好。
        egui_extras::install_image_loaders(&cc.egui_ctx);
        load_cjk_font(&cc.egui_ctx);
        // 自定义裁剪比例 / 缩略图说明开关在 move store 之前读取。
        let custom_ratios = Self::load_custom_ratios(&store);
        let thumb_captions = store
            .get_setting("thumb_captions")
            .map(|v| v != "0")
            .unwrap_or(false);
        // 缺省树形(参考件左栏即树);显式选过平铺的库存的是 "0",不受影响。
        let folder_tree = store
            .get_setting("folder_tree")
            .map(|v| v != "0")
            .unwrap_or(true);
        // 左栏缩略图:参考件默认在库里显示,这里同样默认开。
        let tree_thumbs = store
            .get_setting("tree_thumbs")
            .map(|v| v != "0")
            .unwrap_or(true);
        let people_sort = store
            .get_setting("people_sort")
            .and_then(|v| PeopleSort::from_key(&v))
            .unwrap_or(PeopleSort::Amount);
        // 右侧属性面板的收展状态(平时收起,记住上次的选择)。
        let drawer_open = store
            .get_setting("drawer_open")
            .map(|v| v == "1")
            .unwrap_or(false);
        let drawer_tab = store
            .get_setting("drawer_tab")
            .and_then(|v| DrawerTab::from_key(&v))
            .unwrap_or_default();
        // 排序与视图过滤同属"怎么看图库",重启后保持(参考件亦记忆视图状态)。
        let sort_by = store
            .get_setting("sort_by")
            .and_then(|v| timeline::SortBy::from_key(&v))
            .unwrap_or_default();
        let filters =
            timeline::Filters::from_setting(&store.get_setting("view_filters").unwrap_or_default());
        let sort_rev = store.get_setting("sort_rev").as_deref() == Some("1");
        // P3-ui:邮件默认尺寸从设置读(选项 > 电子邮件;参考件默认 1024)。
        let email_size = crate::share::ShareSize::from_px(
            store
                .get_setting("email_size_px")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1024),
        );
        Self {
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
            thumb_decode_budget: crate::scroll::MAX_THUMB_DECODE_BYTES_PER_FRAME,
            thumb_decode_count: 0,
            tier_cached: 0,
            rows_gen: 0,
            folder_cells: Default::default(),
            folder_groups: Default::default(),
            last_scroll_y: 0.0,
            last_viewport_h: 600.0,
            last_content_h: 0.0,
            gallery_scroll_target: None,
            device_watch: None,
            last_volumes: Vec::new(),
            fx_panel: false,
            drawer_open,
            drawer_tab,
            fx_tab: 0,
            fx_draft: crate::effects::Adjust::default(),
            fx_draft_id: None,
            fx_clipboard: None,
            fx_undo: HashMap::new(),
            fx_msg: String::new(),
            export_with_effects: false,
            email_win: false,
            email_size,
            cd_win: false,
            cd_size: crate::share::ShareSize::Px800,
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
            preview_missing: None,
            preview_failed: None,
            pending_missing: Vec::new(),
            bake_rx: None,
            input_tag: String::new(),
            export_rx: None,
            wifi: None,
            wifi_panel: false,
            sel: BTreeSet::new(),
            sort_by,
            sort_rev,
            filters,
            thumb_tier: crate::thumbs::Tier::Normal,
            tags: Vec::new(),
            sel_tag: None,
            show_about: false,
            about_cache: Default::default(),
            show_shortcuts: false,
            confirm_remove: false,
            show_folder_mgr: false,
            exclude_input: String::new(),
            watched: Vec::new(),
            confirm_folder_remove: None,
            maint_rx: None,
            show_changes: false,
            change_log: Vec::new(),
            show_bad_media: false,
            bad_media_rows: Vec::new(),
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
            preview_zoom: 1.0,
            preview_pan: egui::Vec2::ZERO,
            crop_mode: false,
            preview_full: None,
            zoom_rx: None,
            zoom_pending: None,
            crop_rect: None,
            crop_ratio: None,
            crop_drag: None,
            custom_ratios,
            crop_cw: String::new(),
            crop_ch: String::new(),
            crop_cname: String::new(),
            slideshow: false,
            slide_last: std::time::Instant::now(),
            slide_pause: false,
            slide_interval: 3.0,
            slide_loop: true,
            caption_edit: None,
            thumb_captions,
            sel_album: None,
            album_input: String::new(),
            show_props: false,
            show_options: false,
            options_tab: 0,
            cache_usage: None,
            folder_tree,
            folder_show_all: true,
            tree_thumbs,
            folder_tree_cache: FolderTree::default(),
            tree_open: HashSet::new(),
            tree_texs: HashMap::new(),
            page: PageKind::Library,
            nav_hist: Vec::new(),
            nav_at: 0,
            import_src: None,
            import_entries: Vec::new(),
            import_sel: BTreeSet::new(),
            last_import_src: String::new(),
            scan_verb: "扫描",
            people_sort,
            album_new_win: false,
            pending_thumbs: 0,
            wallpaper_rx: None,
            zoom_tex: None,
            zoom_active_for: None,
            #[cfg(feature = "ai")]
            face_rx: None,
            #[cfg(feature = "ai")]
            face_prog: (0, 0),
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
            face_name_draft: None,
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
            link: LinkInfo::new(),
            link_rx: None,
            link_tx: None,
            link_declined: false,
            svc_ask_grant: false,
            svc_never_ask: false,
            svc_busy: false,
            scope_ask: false,
            scope_choice: InitialScope::Complete,
            name_query: String::new(),
            name_results: Vec::new(),
            name_search_ms: 0.0,
            name_class: crate::nameidx::FileClass::All,
            name_thumbs: HashMap::new(),
            name_thumb_order: std::collections::VecDeque::new(),
            search_tx: None,
            search_rx: None,
            search_gen: 0,
            search_inflight: false,
            name_search_cache: HashMap::new(),
            thumb_tx: None,
            thumb_rx: None,
            thumb_inflight: std::collections::HashSet::new(),
            name_sel: None,
            adopt_seen: std::collections::HashSet::new(),
            adopt_queue: std::collections::VecDeque::new(),
            frame_dirs: Vec::new(),
            frame_dirs_seen: std::collections::HashSet::new(),
        }
    }

    pub fn start_initial_tasks(&mut self, ctx: egui::Context) {
        // 快速查找后台线程(即输即搜:走管道问索引服务,毫秒级回命中)。
        let (stx, srx, _) = spawn_search_worker();
        self.search_tx = Some(stx);
        self.search_rx = Some(srx);
        // 索引服务链路线程(心跳问 Status、掉线重连、代办装卸与重建)。
        let (ltx, lrx) = spawn_link_worker(ctx);
        self.link_tx = Some(ltx);
        self.link_rx = Some(lrx);
        // 上次拒绝过装服务 → 本次会话不自动发起(入口仍在,手动可再试)。
        self.link_declined = self.store.get_setting("svc_declined").as_deref() == Some("1");
        // 搜索行缩略图解码线程(命中列表先出,缩略图异步补)。
        let (ttx, trx) = spawn_thumb_worker();
        self.thumb_tx = Some(ttx);
        self.thumb_rx = Some(trx);
        // 后台缩略图补齐(持续 worker:只补派生门放行的行,可见目录优先)
        self.thumb_bg = Some(crate::thumbs::spawn_worker(
            self.store.clone(),
            crate::thumbs::visible_dirs(),
        ));
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
        if !pending.is_empty() {
            let roots: Vec<PathBuf> = pending.iter().map(|(r, _)| PathBuf::from(r)).collect();
            let scope = if pending.len() == 1 {
                format!("续扫 {}", pending[0].0)
            } else {
                format!("续扫 {} 个上次中断的目录", pending.len())
            };
            self.start_scan_roots(roots, &scope);
        }
        // 方案 P3-18:首次启动先问派生作用域(两档单选、无取消,必须答一次)。
        // 答完才落账 `dir_intent` 并发起首扫;`skip_initial_scan` 偏好短路 ——
        // 明确说过不要首扫的用户按保守档静默落账,不弹窗。
        if !self.store.scope_initialized() {
            if self.store.get_setting("skip_initial_scan").as_deref() == Some("1") {
                self.apply_initial_scope(InitialScope::Limited, false);
            } else {
                self.scope_ask = true;
            }
        }
        // 方案 P3-21:上次点了「重建全部缩略图」但进程中途退出 → 续做完。
        self.resume_force_thumb_update();
        // 全盘快查的数据源是**索引服务**(常驻提权进程):
        // 服务在位就由 SCM 自动拉起,UI 侧全程静默,链路线程自己会连上。
        // 这里只处理"该不该问用户装服务":没问过 → 弹说明窗取得同意;
        // 问过且要 → 若服务不在了(换机/被卸)直接重装,不再啰嗦;
        // 问过且不要 / 上次装失败被拒 → 不打扰,走 walkdir 兜底。
        #[cfg(windows)]
        match self.svc_want() {
            SvcWant::Unasked => {
                if !self.link_declined
                    && self.store.get_setting("svc_never_ask").as_deref() != Some("1")
                {
                    self.svc_open_grant();
                }
            }
            SvcWant::Yes => {
                if crate::win::service_state() != crate::win::ServiceState::NotInstalled {
                    // 已注册:SCM 负责拉起,链路线程退避重连即可,别抢着弹 UAC。
                    self.link.svc = crate::win::service_state();
                } else if !self.link_declined {
                    self.svc_install();
                }
            }
            SvcWant::No => {}
        }
    }

    /// 用户对口索引服务的意向(没存过就是「未问过」)。
    fn svc_want(&self) -> SvcWant {
        SvcWant::from_setting(self.store.get_setting("svc_wanted").as_deref().unwrap_or(""))
    }

    /// 打开授权说明窗(「不再提示」勾选每次打开时复位)。
    #[cfg(windows)]
    fn svc_open_grant(&mut self) {
        self.svc_ask_grant = true;
        self.svc_never_ask = false;
    }

    /// 首启范围落账(方案 P3-18):写 `dir_intent` + 打标记,按需发起首扫。
    ///
    /// `scan=false` 用于 `skip_initial_scan` 偏好短路 —— 作用域照样落账
    /// (不落账派生门是空的,谁都不出缩略图),只是不发起扫描。
    fn apply_initial_scope(&mut self, choice: InitialScope, scan: bool) {
        let roots = self.store.apply_initial_scope(choice == InitialScope::Complete);
        self.scope_ask = false;
        self.scope_choice = choice;
        if roots.is_empty() {
            return;
        }
        // 落账改变了派生门:已入库的行按新作用域重算一遍 rich_state。
        let gate = crate::db::ScopeGate::load(&self.store);
        let _ = self.store.reconcile_rich_state(&gate);
        if !scan {
            self.status = "已按偏好跳过首扫;派生作用域已落账,可在文件夹管理器里手动扫。".into();
            return;
        }
        let label = match choice {
            InitialScope::Complete => "全盘",
            InitialScope::Limited => "常用目录",
        };
        self.start_scan_roots(roots, label);
    }

    /// 发起安装/启动索引服务(UAC 与等待都在链路线程里,不挡 UI 帧)。
    #[cfg(windows)]
    fn svc_install(&mut self) {
        if self.svc_busy {
            return;
        }
        self.svc_busy = true;
        if let Some(tx) = &self.link_tx {
            let _ = tx.send(LinkCmd::Install);
        } else {
            self.svc_busy = false;
        }
    }

    /// 卸载索引服务(停 + 删,同样一次 UAC)。
    #[cfg(windows)]
    fn svc_uninstall(&mut self) {
        if self.svc_busy {
            return;
        }
        self.svc_busy = true;
        if let Some(tx) = &self.link_tx {
            let _ = tx.send(LinkCmd::Uninstall);
        } else {
            self.svc_busy = false;
        }
    }

    /// 只停服务、保留注册项。存在的理由很实际:服务在跑就锁着自己的 exe 映像,
    /// 程序升级/重编/搬目录全都动不了 —— 这一步必须能在界面里点到,
    /// 而不是让用户去开 services.msc 或敲 `net stop RelytoMaliangIndex`。
    #[cfg(windows)]
    fn svc_stop(&mut self) {
        if self.svc_busy {
            return;
        }
        self.svc_busy = true;
        if let Some(tx) = &self.link_tx {
            let _ = tx.send(LinkCmd::Stop);
        } else {
            self.svc_busy = false;
        }
    }

    /// 启动已注册的服务(和 `svc_stop` 成对;停过之后只有这一条回头路)。
    #[cfg(windows)]
    fn svc_start(&mut self) {
        if self.svc_busy {
            return;
        }
        self.svc_busy = true;
        if let Some(tx) = &self.link_tx {
            let _ = tx.send(LinkCmd::Start);
        } else {
            self.svc_busy = false;
        }
    }

    /// 用户在说明窗/选项页定了意向之后的落地动作:落库 + 该动手的动手。
    #[cfg(windows)]
    fn svc_apply_want(&mut self, want: SvcWant) {
        let _ = self.store.set_setting("svc_wanted", want.as_setting());
        self.svc_ask_grant = false;
        self.svc_never_ask = false;
        self.link_declined = false;
        let _ = self.store.set_setting("svc_declined", "0");
        // 明确选了要装 = 撤回「不再提示」,服务丢失时还要恢复询问。
        let _ = self.store.set_setting("svc_never_ask", "0");
        match want {
            SvcWant::Yes => self.svc_install(),
            SvcWant::No => {
                // 已经装着的服务不会因为用户改主意就自动消失:如实告知,
                // 卸载是破坏性动作(要停服务、删注册项),交给用户自己点。
                if crate::win::service_installed() {
                    self.status = "已选「不用全盘索引」;服务仍在运行,要彻底移除请点「卸载索引服务」。"
                        .into();
                }
            }
            SvcWant::Unasked => {}
        }
    }

    /// 手动「启用全盘快查」(搜索页/选项页/顶栏切换入口)。
    /// 还没定意向 → 先弹说明窗取得同意,不背着一声不响就弹 UAC;
    /// 服务在位但管道连不上 → 只是重连,不必再问一遍。
    #[cfg(windows)]
    fn svc_try_start(&mut self) {
        let want = self.svc_want();
        if want == SvcWant::Unasked {
            self.svc_open_grant();
            return;
        }
        if want == SvcWant::No {
            // 明确选了不用的人,切页面时不反复拉弹窗,只给一句指路。
            self.status = "全盘快查已关闭(在 选项 > 常规 可改)".into();
            return;
        }
        match self.link.svc {
            // 没装(或被卸了)→ 重新装一次;装了没起 → 让 SCM/重连去处理,
            // 起不来再退回安装(install 对已存在的服务是改配置 + 启动,幂等)。
            crate::win::ServiceState::NotInstalled | crate::win::ServiceState::Stopped => {
                self.svc_install()
            }
            _ => {
                if let Some(tx) = &self.link_tx {
                    let _ = tx.send(LinkCmd::Retry);
                }
                self.status = "索引服务在运行,正在重连…".into();
            }
        }
    }

    /// 非 Windows:没有索引服务这回事,入口只负责说明。
    #[cfg(not(windows))]
    fn svc_try_start(&mut self) {
        self.status = "全盘快查仅 Windows 可用(NTFS USN/MFT 索引服务)".into();
    }

    /// 进入快速查找页时的引导判定。
    ///
    /// 连得上就不打扰;连不上、且用户没明确拒绝过也没勾「不再提示」,才走
    /// [`Self::svc_try_start`] —— 意向未定时它先弹说明窗,不是直接弹 UAC。
    #[cfg(windows)]
    fn svc_nudge_on_search_entry(&mut self) {
        if self.link.online || self.link_declined || self.svc_busy || self.svc_ask_grant {
            return;
        }
        if self.svc_want() == SvcWant::Unasked
            && self.store.get_setting("svc_never_ask").as_deref() == Some("1")
        {
            return;
        }
        self.svc_try_start();
    }

    /// 请求全量重建索引(选项页的手动入口)。服务异步执行,立刻回受理。
    #[cfg(windows)]
    fn svc_rebuild(&mut self) {
        let Some(tx) = self.link_tx.clone() else {
            return;
        };
        let _ = tx.send(LinkCmd::Rebuild(Vec::new()));
        self.status = "已请求索引服务全量重建(进度见下方状态)".into();
    }

    /// 泵索引服务链路事件(概况 / 掉线 / 重建与装卸回执)。
    /// 放在 update 顶层而不是快速查找视图里:人在选项页也要看得到状态和失败原因。
    fn pump_link_events(&mut self) {
        // 接收端先摘出来:处理途中要改 self(清缓存、重发查询),不能同时借着它。
        let Some(rx) = self.link_rx.take() else {
            return;
        };
        let mut alive = true;
        loop {
            match rx.try_recv() {
                Ok(LinkMsg::Up(s, svc)) => {
                    let was_down = !self.link.online;
                    self.link.svc = svc;
                    self.link.apply(&s);
                    if was_down {
                        // 刚接上:旧缓存都是降级期间攒的,清掉重查当前输入。
                        self.name_search_cache.clear();
                        // 这句只在「刚接上」那一刻写一次,所以不能带条数:
                        // 条数会随建库一直涨,写死在这儿就成了过期读数
                        // (2026-11-05 的「2 卷 0 条」就是这么来的)。
                        self.status =
                            format!("全盘快查已接上索引服务:{} 卷(条数见快查页)", self.link.vols);
                        if !self.name_query.trim().is_empty() {
                            self.dispatch_name_search();
                        }
                    }
                }
                Ok(LinkMsg::Down(why, svc)) => {
                    let was_up = self.link.online;
                    self.link.online = false;
                    self.link.why = why.clone();
                    self.link.svc = svc;
                    self.link.entries = 0;
                    self.link.vols = 0;
                    self.link.building = 0;
                    self.link.compat.clear();
                    if was_up {
                        self.name_search_cache.clear();
                        self.status = format!("索引服务断开,已降级为逐目录扫描:{why}");
                    }
                }
                Ok(LinkMsg::Rebuild(err)) => {
                    self.status = match err {
                        None => "索引服务已受理全量重建".into(),
                        Some(e) => format!("请求重建失败:{e}"),
                    };
                }
                Ok(LinkMsg::Service(op, err)) => {
                    self.svc_busy = false;
                    // 「停止/启动」只是运维动作(换二进制、让磁盘歇口),
                    // 不代表用户不想要全盘快查 —— 别把它记成拒绝授权。
                    let state_only = matches!(op, "停止" | "启动");
                    if state_only {
                        self.status = match (&err, op) {
                            (None, "停止") => "索引服务已停止(注册项保留):全盘快查暂不可用,同页可再启动".into(),
                            (None, _) => "索引服务已启动,正在重连…".into(),
                            (Some(e), _) => format!("{op}索引服务未完成:{e}(可在 选项 > 常规 重试)"),
                        };
                    } else {
                        match err {
                            None => {
                                self.link_declined = false;
                                let _ = self.store.set_setting("svc_declined", "0");
                            }
                            Some(e) => {
                                // 装不成就别把意向留在"要",否则每次启动都白跑一趟 UAC。
                                let _ = self.store.set_setting("svc_wanted", "");
                                self.link_declined = true;
                                let _ = self.store.set_setting("svc_declined", "1");
                                self.status = format!("索引服务未完成:{e}(可在 选项 > 常规 重试)");
                            }
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                // 链路线程不会先于 App 退出;真断了就不放回,免得每帧空转。
                Err(mpsc::TryRecvError::Disconnected) => {
                    alive = false;
                    break;
                }
            }
        }
        if alive {
            self.link_rx = Some(rx);
        }
    }

    /// 即输即搜:把当前输入交给后台检索线程(走管道问索引服务,不挡 UI)。
    /// 命中缓存直接回显;空输入清空结果。换档/服务重连后缓存自动失效。
    fn dispatch_name_search(&mut self) {
        let q = self.name_query.trim().to_string();
        if q.is_empty() {
            self.name_results.clear();
            self.search_inflight = false;
            return;
        }
        let key = (q.clone(), self.name_class as u8);
        if let Some((hits, ms)) = self.name_search_cache.get(&key) {
            self.name_results = hits.clone();
            self.name_search_ms = *ms;
        }
        self.search_gen += 1;
        if let Some(tx) = &self.search_tx {
            let _ = tx.send(SearchReq {
                gen: self.search_gen,
                q,
                class: self.name_class,
            });
            self.search_inflight = true;
        }
    }

    /// 泵后台检索结果与搜索行缩略图(每帧调用,非阻塞)。
    fn pump_search(&mut self, ctx: &egui::Context) {
        // —— 检索结果:同代才收,过期代直接丢 ——
        if let Some(rx) = &self.search_rx {
            let mut hits: Option<(u64, Vec<NameRow>, f64)> = None;
            let mut down: Option<(u64, String)> = None;
            while let Ok(resp) = rx.try_recv() {
                match resp {
                    SearchResp::Hits { gen, hits: h, ms } => hits = Some((gen, h, ms)),
                    SearchResp::Down { gen, why } => down = Some((gen, why)),
                }
            }
            if let Some((gen, h, ms)) = hits {
                if gen == self.search_gen {
                    self.search_inflight = false;
                    // 进查询缓存(重复输入零成本回显)。
                    let q = self.name_query.trim().to_string();
                    if !q.is_empty() {
                        let key = (q, self.name_class as u8);
                        if self.name_search_cache.len() >= 64 {
                            self.name_search_cache.clear();
                        }
                        self.name_search_cache.insert(key, (h.clone(), ms));
                    }
                    self.name_results = h;
                    self.name_search_ms = ms;
                    self.status = format!(
                        "快速查找 \"{}\"({}):{} 条命中,耗时 {:.1} ms",
                        self.name_query.trim(),
                        self.name_class.label(),
                        self.name_results.len(),
                        ms
                    );
                    ctx.request_repaint();
                }
            }
            if let Some((gen, why)) = down {
                if gen == self.search_gen {
                    self.search_inflight = false;
                    self.name_results.clear();
                    // 链路状态由链路线程维护,这里只落一句用户看得懂的原因。
                    self.link.why = why;
                    ctx.request_repaint();
                }
            }
        }
        // —— 搜索行缩略图:解码完成的建纹理进缓存 ——
        if let Some(rx) = &self.thumb_rx {
            let mut got = false;
            while let Ok((path, img)) = rx.try_recv() {
                self.thumb_inflight.remove(&path);
                if let Some(img) = img {
                    let rgba = img.to_rgba8();
                    let size = [rgba.width() as usize, rgba.height() as usize];
                    let tex = ctx.load_texture(
                        "name-thumb",
                        egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()),
                        egui::TextureOptions::LINEAR,
                    );
                    self.name_thumbs.insert(path.clone(), tex);
                    self.name_thumb_order.push_back(path);
                    // 超上限按入序逐出(整表清空会闪,搜索页翻页时很难受)。
                    while self.name_thumbs.len() > 512 {
                        if let Some(old) = self.name_thumb_order.pop_front() {
                            self.name_thumbs.remove(&old);
                        } else {
                            break;
                        }
                    }
                    got = true;
                }
            }
            if got {
                ctx.request_repaint();
            }
        }
    }

    /// 降级横幅:索引服务连不上时说清楚「少了什么、为什么、怎么办」。
    ///
    /// 方案 P0-4 要求连接失败必须明示降级 —— 静默退化成"搜不到东西"最坑人:
    /// 用户会以为盘上真没有这个文件。图库侧的发现/保鲜另有 walkdir + watch
    /// 兜底(方案 P4-30),这里丢的只是"全盘按名字快查"这一项能力。
    fn svc_degraded_ui(&mut self, ui: &mut egui::Ui) {
        use crate::win::ServiceState as S;
        let svc = self.link.svc;
        let why = self.link.why.clone();
        let busy = self.svc_busy;
        let head = match svc {
            S::NotInstalled => "全盘快查未启用(索引服务未安装)",
            S::Stopped => "索引服务已停止",
            S::Transition(_) => "索引服务正在启动…",
            S::Running => "索引服务在运行,但连不上它的管道",
        };
        ui.horizontal(|ui| {
            // ⚠ 字形默认字体里没有,用橙色加粗文字代替图标(项目里同口径)。
            ui.label(egui::RichText::new(head).color(egui::Color32::from_rgb(230, 145, 56)).strong());
        });
        ui.small("现在只有图库里的照片能按文件名查;全盘文件与文件夹的查找、以及未导入目录的实时发现都不可用。");
        if !why.is_empty() {
            ui.small(egui::RichText::new(format!("原因:{why}")).weak());
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            #[cfg(windows)]
            {
                let label = match svc {
                    S::NotInstalled => "安装索引服务…",
                    S::Stopped | S::Transition(_) => "重新安装并启动服务…",
                    S::Running => "重试连接",
                };
                if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                    self.link_declined = false;
                    let _ = self.store.set_setting("svc_declined", "0");
                    if svc == S::Running {
                        if let Some(tx) = &self.link_tx {
                            let _ = tx.send(LinkCmd::Retry);
                        }
                    } else {
                        self.svc_try_start();
                    }
                }
                if busy {
                    ui.spinner();
                    ui.small("等待你在 UAC 授权窗口点「是」…");
                }
            }
            #[cfg(not(windows))]
            ui.small("全盘快查仅支持 Windows(NTFS USN/MFT)。");
        });
        if matches!(svc, S::Stopped | S::Transition(_)) {
            ui.small("服务是「自动启动」的:停在这里通常是被手动停过或上次启动失败,重装一次即可恢复。");
        }
    }

    /// 快速查找视图(自绘 CentralPanel:任意文件/文件夹结果列表)。
    fn search_ui(&mut self, ctx: &egui::Context) {
        // 索引服务链路事件已在 update 顶层统一泵(见 pump_link_events),这里只画。
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("快速查找");
            // A9:三层数据必须标来源。名字/路径/大小/日期这一路走 L1(服务内存里的
            // 文件名索引,不经 SQLite);图库条件查询走 L2;派生列只可能来自 L3。
            // 只有「图库」这一列是 L2 的投影,由服务批量查库后随命中一起回。
            ui.small(
                "结果来自 L1 内存文件名索引(NTFS USN/MFT;由常驻索引服务维护,安装一次后开机自动更新)\
                 —— 名字、路径、大小、修改日期都在这里查,不经图库数据库;「图库」一列取自 L2。\
                 要按拍摄时间、相机、评分等条件查,请用图库搜索(走 L2 SQL)。",
            );
            ui.separator();

            // 服务连不上 = 全盘快查没有数据源:明示降级(方案 P0-4),本页只剩说明。
            // 图库内照片仍可从顶栏的图库搜索查(那条路走 SQL,与这里无关)。
            if !self.link.online {
                self.svc_degraded_ui(ui);
                return;
            }
            if self.link.building > 0 {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!(
                        "索引服务正在建库({} 卷进行中),已建好 {} 条 —— 可以先搜,结果会越查越全",
                        self.link.building, self.link.entries
                    ));
                });
            }
            for (letter, why) in self.link.compat.clone() {
                ui.small(format!("{letter}: 盘走兼容扫描,无实时快查({why})"));
            }
            if self.link.entries == 0 {
                ui.label("索引还是空的:服务正在扫第一轮全盘,建好后这里自动可用。");
                return;
            }

            // 输入框在顶部整条快速查找栏(gsearch),这里只出结果面板;
            // 即输即搜由 gsearch 的 changed() 触发,本页被动刷新。
            if self.name_query.trim().is_empty() {
                ui.label("在上方搜索栏输入关键字开始查找(文件与文件夹都搜,大小写不敏感)");
                return;
            }
            if self.name_results.is_empty() {
                if self.search_inflight {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("正在检索…");
                    });
                } else {
                    ui.label("没有命中的文件");
                }
                return;
            }

            let results = std::mem::take(&mut self.name_results);
            let mut opened: Option<String> = None;
            let mut menu_for: Option<String> = None;
            egui_extras::TableBuilder::new(ui)
                .striped(true)
                .vscroll(true)
                .min_scrolled_height(320.0)
                .column(
                    egui_extras::Column::initial(220.0)
                        .resizable(true)
                        .clip(true),
                )
                .column(egui_extras::Column::remainder())
                .column(egui_extras::Column::auto())
                .column(egui_extras::Column::auto())
                .column(egui_extras::Column::auto())
                .column(egui_extras::Column::auto())
                .header(26.0, |mut h| {
                    for title in ["文件名称", "路径", "大小", "修改时间", "图库", "预览"] {
                        h.col(|ui| {
                            ui.strong(title);
                        });
                    }
                })
                .body(|mut body| {
                    for r in &results {
                        let h = &r.hit;
                        body.row(64.0, |mut row| {
                            row.set_selected(self.name_sel.as_deref() == Some(h.path.as_str()));
                            row.col(|ui| {
                                ui.strong(&h.name);
                            });
                            row.col(|ui| {
                                ui.small(&h.path);
                            });
                            row.col(|ui| {
                                // size/mtime 由服务随命中一起回,不再逐条 stat。
                                if h.is_dir {
                                    ui.label("");
                                } else {
                                    ui.label(fmt_size(r.size));
                                }
                            });
                            row.col(|ui| {
                                ui.label(fmt_mtime(r.mtime));
                            });
                            row.col(|ui| {
                                // 快查看得见全盘,图库只收作用域内的媒体 —— 这一列把
                                // 「搜到了为什么网格里没有」讲明白(方案 P4-29)。
                                let label = r.rich_label();
                                let why = match i32::from(r.rich_state) {
                                    crate::db::RICH_OUT => {
                                        "已在图库数据库里,但它所在的目录未纳入派生作用域:\
                                         不生成缩略图与元数据。点进该目录、在「工具 → \
                                         文件夹管理器」里添加它,或在网格里点开这张图,\
                                         就会开始生成。"
                                    }
                                    crate::db::RICH_PENDING => "已纳入范围,派生还没跑到这一条。",
                                    crate::db::RICH_DONE => "已纳入范围并完成派生,网格里能看到。",
                                    crate::db::RICH_BAD => "文件读取失败(损坏或无权限),已隔离,不再重试。",
                                    _ if r.hit.is_dir => "文件夹不进图库。",
                                    _ => "不是图库收录的媒体类型,或还没被扫描入库。",
                                };
                                ui.label(label).on_hover_text(why);
                            });
                            row.col(|ui| {
                                // 缩略图按需异步解码(未入库文件走纯路径入口)。
                                let p = PathBuf::from(&h.path);
                                if let Some(tex) = self.name_thumb(ctx, &p) {
                                    ui.add(
                                        egui::Image::new(&tex)
                                            .fit_to_exact_size(egui::vec2(56.0, 56.0)),
                                    );
                                } else {
                                    let icon = if h.is_dir { "📁" } else { "📄" };
                                    ui.add_sized([56.0, 56.0], egui::Label::new(icon));
                                }
                            });
                            let resp = row.response();
                            if resp.clicked() {
                                self.name_sel = Some(h.path.clone());
                            }
                            if resp.double_clicked() {
                                opened = Some(h.path.clone());
                            }
                            if resp.secondary_clicked() {
                                menu_for = Some(h.path.clone());
                            }
                        });
                    }
                });
            self.name_results = results;
            // 打开/菜单在表外执行:右键菜单是模态的,不在遍历中途弹。
            if let Some(p) = opened {
                crate::win::shell_open(std::path::Path::new(&p));
            }
            if let Some(p) = menu_for {
                // 与资源管理器同一份系统右键菜单(Shell IContextMenu)。
                crate::win::shell_context_menu(std::path::Path::new(&p));
            }
        });
    }

    /// 搜索行缩略图:命中缓存直接回显;未命中丢给后台解码线程(去重 + 在途
    /// 上限),下一帧才出现。UI 线程绝不解码 —— 翻长列表不卡。
    fn name_thumb(&mut self, _ctx: &egui::Context, p: &Path) -> Option<egui::TextureHandle> {
        let key = p.to_string_lossy().into_owned();
        if let Some(t) = self.name_thumbs.get(&key) {
            return Some(t.clone());
        }
        if !crate::scan::is_indexable(p) {
            return None;
        }
        if self.thumb_inflight.len() < 16 && self.thumb_inflight.insert(key.clone()) {
            match &self.thumb_tx {
                Some(tx) => {
                    let _ = tx.send(key);
                }
                None => {
                    self.thumb_inflight.remove(&key);
                }
            }
        }
        None
    }

    /// 平铺 / 树形一对互斥档(参考件同一开关的两态:平铺列表或多级树)。
    fn set_folder_tree(&mut self, tree: bool) {
        if self.folder_tree == tree {
            return;
        }
        self.folder_tree = tree;
        let _ = self
            .store
            .set_setting("folder_tree", if tree { "1" } else { "0" });
    }

    /// 后台是否还有活在干(工具栏最右的动画指示灯)。
    fn busy(&self) -> bool {
        self.scan_busy.load(Ordering::Relaxed)
            || self.pending_thumbs > 0
            // 装卸索引服务要等 UAC + 提权子进程(最长 120s),这期间算"在忙"。
            || self.svc_busy
            || self.export_rx.is_some()
    }

    /// 「查看选项」里的壳目录快捷键(参考件 Desktop / My Docs / My Pictures / My Computer):
    /// 目录已在图库里 → 跳到该文件夹页;没导入 → 只给提示,不擅自扩扫描范围。
    /// 进入一个目录(方案 P3-19 点击懒加载 + P3-20 可见优先)。
    ///
    /// 库里早已收录、但没落在派生作用域里的目录,点进来才补一条 `clicked`
    /// 账,并把该目录 `rich_state=0` 的行置 1 —— 眼前的图先清晰。
    /// 同时把该目录交给后台 worker 当"可见目录"(排序键)。
    ///
    /// 这一句只服务**导航帧**:点完树/列表立刻点名这个目录,不必等它被画到屏幕上
    /// 的那一帧才进优先队列。网格自己的可见清单在帧首整批发(见 `update`)。
    fn enter_dir(&mut self, dir: &str) -> bool {
        let ok = self.include_dir(dir, crate::db::INTENT_CLICKED, crate::db::SRC_CLICK);
        if ok {
            self.publish_visible(dir);
        }
        ok
    }

    /// 把目录交给后台 worker 当「可见目录」(派生排序的第一优先键)。
    ///
    /// worker 那边是**整表替换**,所以导航时只发这一个目录;网格攒出一整帧的清单
    /// 后在帧首发一批(`update` 里),逐目录发会把前面刚上屏的目录挤出优先。
    fn publish_visible(&self, dir: &str) {
        if dir.is_empty() {
            return;
        }
        if let Some(w) = &self.thumb_bg {
            w.publish_visible(vec![dir.to_string()]);
        }
    }

    /// 记下「这一帧画在屏幕上的这个目录」(去重、限量)。
    fn note_frame_dir(&mut self, dir: &str) {
        if dir.is_empty() || self.frame_dirs.len() >= MAX_VISIBLE_DIRS {
            return;
        }
        let key = crate::db::norm_dir(dir).to_ascii_uppercase();
        if self.frame_dirs_seen.insert(key) {
            self.frame_dirs.push(dir.to_string());
        }
    }

    /// 屏幕上出现一张未纳入的图 → 把它的目录排进「纳入」队列(每个目录只排一次)。
    ///
    /// 不在渲染帧里直接写库:首屏一次可能铺开几十上百个目录,那就变成几十次
    /// `UPDATE` + 回读,帧会被自己拖死。排队,由 `photo_grid` 每帧消化少量。
    /// 去重键用大写归一路径(`adopt_seen`),队列里保留界面上那串原样大小写。
    /// 队列排满时**不**登记去重(返回 false),这个目录下一帧还会再来敲门 ——
    /// 登记了却不出队,就等于本进程永远不再纳它。
    fn note_visible_out(&mut self, dir: &str) -> bool {
        if dir.is_empty() || self.adopt_queue.len() >= MAX_ADOPT_QUEUE {
            return false;
        }
        let key = crate::db::norm_dir(dir).to_ascii_uppercase();
        if self.adopt_seen.contains(&key) {
            return false;
        }
        self.adopt_seen.insert(key);
        self.adopt_queue.push_back(dir.to_string());
        true
    }

    /// 消化「看到但还没纳入」的队列:每帧最多 `max` 个目录。
    ///
    /// 口径与点进目录一致(写 `clicked` 账 + 放行该目录),只是账本来源记 `visible`,
    /// 好跟用户亲手点进去的分开。可见清单不用在这里发:帧首按整帧上屏目录一次发完。
    fn drain_adopts(&mut self, max: usize) {
        for _ in 0..max {
            let Some(dir) = self.adopt_queue.pop_front() else {
                break;
            };
            self.include_dir(&dir, crate::db::INTENT_CLICKED, crate::db::SRC_VIEW);
        }
    }

    /// 把一个目录纳进派生作用域并立即放行它的行(方案 P3-19)。
    ///
    /// 返回是否真的纳进去了。**被排除规则挡住的目录一律不写账本**(2026-11-05 拍板):
    /// 系统目录、应用缓存、回收站不该因为用户滚到过、或点过一张图,就变成
    /// 「以后这个目录的图都要派生」。
    ///
    /// 已纳过的目录不重写账本 —— 免得把用户显式设过的来源(首启对话框 /
    /// 手动导入)冲成 click;但 `promote_dir` 照样跑一遍:并发扫描可能刚
    /// 插进一批 `rich_state=0` 的新行。
    fn include_dir(&mut self, dir: &str, state: &str, source: &str) -> bool {
        if !self.store.adoptable_dir(dir) {
            return false;
        }
        if !self.store.dir_intent(dir).is_some_and(|d| d.included()) {
            let _ = self
                .store
                .set_dir_intent(&crate::db::DirIntent::new(dir, state, source));
        }
        if self.store.promote_dir(dir).unwrap_or(0) > 0 {
            self.sync_dir_promoted(dir);
        }
        true
    }

    /// `promote_dir` 落库后,把库里的真相搬进内存视图。
    ///
    /// 网格每帧只读 `self.rows`,而这里的 `rich_state` 要等 `rebuild` 才跟库对齐 ——
    /// 不搬这一趟,点进目录后整屏仍是「未纳入范围」的占位块,用户看到的就是
    /// 「懒加载根本没动」。**真相一律回读库**(`Store::promoted_states`),界面不按
    /// 排除名单自己再判一遍门:上一轮那么做,把库里早已派生完的 `AppData` 目录
    /// 永远锁在占位块上(2026-11-05 现场)。
    fn sync_dir_promoted(&mut self, dir: &str) {
        let done = self.store.promoted_states(dir);
        for (id, state) in done {
            if let Some(&i) = self.id_row.get(&id) {
                self.rows[i].rich_state = state;
            }
        }
    }

    fn goto_known_dir(&mut self, dir: Option<std::path::PathBuf>, label: &str) {
        let Some(p) = dir else {
            self.status = format!("{label}:图库树只覆盖已导入的目录");
            return;
        };
        let want = p
            .to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_string();
        // 该目录本身或其任意子目录在库里,取路径最短的那个作为落点。
        let hit = self
            .folders
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                let f = f.trim_end_matches(['\\', '/']);
                f.eq_ignore_ascii_case(&want)
                    || f.to_ascii_lowercase()
                        .starts_with(&want.to_ascii_lowercase())
            })
            .min_by_key(|(_, f)| f.len());
        match hit {
            Some((i, _)) => {
                self.selected_folder = i;
                self.folder_show_all = false;
                self.set_view(ViewKind::Folder);
                let p = self.folders[i].clone();
                self.enter_dir(&p);
                self.tree_reveal(&p);
                self.status = format!("已定位到 {label}");
            }
            None => self.status = format!("{label} 未导入图库:文件夹管理器 > 添加文件夹"),
        }
    }

    /// 顶部工具条(参考件主界面顶部那一排),左→右:
    /// 导入 · 新建相簿 · 放映 · 平铺/树形 · 查看选项 ▾ …… 过滤四开关 · 尺寸滑块 · 搜索框 · 活动灯。
    fn toolbar_ui(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(2.0);
            // 前后导航(浏览器式历史;Alt+←/→ 同一条路)。
            if ui
                .add_enabled(self.nav_at > 0, tool_button("arrow-left", false))
                .on_hover_text("后退(Alt+←)")
                .clicked()
            {
                self.nav_back();
            }
            if ui
                .add_enabled(
                    self.nav_at + 1 < self.nav_hist.len(),
                    tool_button("arrow-right", false),
                )
                .on_hover_text("前进(Alt+→)")
                .clicked()
            {
                self.nav_forward();
            }
            ui.separator();
            // 参考件的主「导入」按钮开的是整页导入面板(选源 → 看清单 → 入库),
            // 直接挑文件夹的一次性入口仍在 文件 菜单里。
            let on_import = self.page == PageKind::Import;
            if ui
                .add(tool_button("folder-plus", on_import))
                .on_hover_text("导入(打开导入页:选源 → 看清单 → 入库)")
                .clicked()
            {
                self.go_page(PageKind::Import);
            }
            if ui
                .add(tool_button("smartphone", false))
                .on_hover_text("从设备导入…")
                .clicked()
            {
                self.open_import_devices();
            }
            if ui
                .add(tool_button("image-plus", false))
                .on_hover_text("新建相簿(有选中照片则一并放入)")
                .clicked()
            {
                self.album_input.clear();
                self.album_new_win = true;
            }
            ui.separator();
            if ui
                .add(tool_button("images", false))
                .on_hover_text("幻灯片放映(选中照片;未选中则放映当前视图)")
                .clicked()
            {
                self.start_slideshow();
            }
            ui.separator();
            if ui
                .add(tool_button("layout-grid", !self.folder_tree))
                .on_hover_text("平铺文件夹视图")
                .clicked()
            {
                self.set_folder_tree(false);
            }
            if ui
                .add(tool_button("list-tree", self.folder_tree))
                .on_hover_text("树形视图(只显示有照片的目录)")
                .clicked()
            {
                self.set_folder_tree(true);
            }
            ui.menu_button("查看选项", |ui| self.view_options_menu(ui));
            ui.separator();
            if ui
                .add(tool_button("wand-sparkles", self.fx_panel))
                .on_hover_text("效果面板")
                .clicked()
            {
                self.fx_panel = !self.fx_panel;
            }
            // —— 右端成组控件:参考件从右往左依次是 活动灯 → 搜索框 → 滑块 → 过滤开关 ——
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // 二选一的配色开关:浅色显月亮(点了变暗),暗色显太阳(点了变亮)。
                let dark = !crate::theme::p().is_light;
                let tip = if dark {
                    "切回灰色配色"
                } else {
                    "切到蓝黑配色"
                };
                if ui
                    .add(tool_button(if dark { "sun" } else { "moon" }, false))
                    .on_hover_text(tip)
                    .clicked()
                {
                    let next = if dark {
                        crate::theme::Kind::Gray
                    } else {
                        crate::theme::Kind::Blue
                    };
                    crate::theme::switch(ctx, next);
                    let _ = self.store.set_setting("ui_theme", next.key());
                }
                // 右栏抽屉开关(参考件工具栏的属性面板按钮;抽屉内四页签见右栏)。
                if ui
                    .add(tool_button("panel-right", self.drawer_open))
                    .on_hover_text("属性面板")
                    .clicked()
                {
                    self.drawer_open = !self.drawer_open;
                    let _ = self
                        .store
                        .set_setting("drawer_open", if self.drawer_open { "1" } else { "0" });
                }
                ui.separator();
                if self.busy() {
                    ui.add(egui::Spinner::new().size(17.0))
                        .on_hover_text("后台正在扫描 / 生成缩略图 / 建立索引");
                } else {
                    // 不忙也占住同样的宽度,免得指示灯出现时整行左右抖。
                    ui.add_space(23.0);
                }
                // 库内搜索框已取消(与全盘快速查找重复,统一走顶部整条搜索)。
                ui.separator();
                // 滑块(参考件的缩放滑块);档位即已缓存的缩略图尺寸。
                let mut idx = self.thumb_tier as i32;
                let slider = egui::Slider::new(&mut idx, 0..=3)
                    .show_value(false)
                    .text("图");
                // egui 的滑块宽度取自 spacing.slider_width,工具栏里要收窄。
                let old_w = ui.spacing().slider_width;
                ui.spacing_mut().slider_width = 96.0;
                let moved = ui
                    .add(slider)
                    .on_hover_text("缩略图大小:小 · 普通 · 大 · 预览")
                    .changed();
                ui.spacing_mut().slider_width = old_w;
                if moved {
                    if let Some(t) = crate::thumbs::Tier::from_u8(idx.clamp(0, 3) as u8) {
                        self.set_thumb_tier(t);
                    }
                }
                ui.separator();
                // 四个可叠加的过滤开关(星标 / 人脸 / 视频 / 地理标记)。
                // 右到左布局里后加的在左边,所以倒序添加,视觉顺序才是 星标·人物·视频·地理标记。
                for i in (0..timeline::FILTER_BTNS.len()).rev() {
                    let (name, tip) = timeline::FILTER_BTNS[i];
                    if ui
                        .add(tool_button(name, self.filters.flag(i)))
                        .on_hover_text(tip)
                        .clicked()
                    {
                        self.filters.flip(i);
                        self.apply_view_state();
                    }
                }
            });
        });
    }

    /// 「查看选项」下拉:条目与顺序沿用经典图库的查看菜单
    /// (View 组 → 排序 → Sort People 组 → 壳目录快捷键 → 缩略图/树形选项)。
    fn view_options_menu(&mut self, ui: &mut egui::Ui) {
        if ui
            .selectable_label(!self.folder_tree, "平铺文件夹视图")
            .clicked()
        {
            self.set_folder_tree(false);
            ui.close();
        }
        if ui.selectable_label(self.folder_tree, "树形视图").clicked() {
            self.set_folder_tree(true);
            ui.close();
        }
        ui.separator();
        for s in timeline::SortBy::ALL {
            if ui.selectable_label(self.sort_by == s, s.label()).clicked() {
                self.sort_by = s;
                self.apply_view_state();
                ui.close();
            }
        }
        if ui.selectable_label(self.sort_rev, "反向排序").clicked() {
            self.sort_rev = !self.sort_rev;
            self.apply_view_state();
            ui.close();
        }
        ui.separator();
        for p in PeopleSort::ALL {
            if ui
                .selectable_label(self.people_sort == p, p.label())
                .clicked()
            {
                self.people_sort = p;
                let _ = self.store.set_setting("people_sort", p.key());
                self.rebuild();
                ui.close();
            }
        }
        ui.separator();
        for (label, dir) in crate::win::shell_shortcuts() {
            let tip = if dir.is_some() {
                "跳到该目录(需已导入图库)"
            } else {
                "图库树只覆盖已导入的目录"
            };
            if ui
                .add_enabled(dir.is_some(), egui::Button::new(label))
                .on_hover_text(tip)
                .clicked()
            {
                self.goto_known_dir(dir, label);
                ui.close();
            }
        }
        ui.separator();
        if ui
            .selectable_label(self.tree_thumbs, "在图库中显示缩略图")
            .clicked()
        {
            self.tree_thumbs = !self.tree_thumbs;
            let _ = self
                .store
                .set_setting("tree_thumbs", if self.tree_thumbs { "1" } else { "0" });
            ui.close();
        }
        if ui
            .selectable_label(self.thumb_captions, "缩略图说明")
            .clicked()
        {
            self.thumb_captions = !self.thumb_captions;
            let _ = self.store.set_setting(
                "thumb_captions",
                if self.thumb_captions { "1" } else { "0" },
            );
            ui.close();
        }
    }

    /// 视图状态(排序/反向/过滤)变更后统一落库并重建。
    /// 调用方改完 `self.sort_by` / `self.sort_rev` / `self.filters` 再调这里,
    /// 避免多处入口漏存设置导致重启后视图回到默认。
    fn apply_view_state(&mut self) {
        let _ = self.store.set_setting("sort_by", self.sort_by.key());
        let _ = self
            .store
            .set_setting("sort_rev", if self.sort_rev { "1" } else { "0" });
        let _ = self
            .store
            .set_setting("view_filters", &self.filters.as_setting());
        self.rebuild();
    }

    /// 人物列表按「查看选项 > 给人物排序」重排(前 10 是截断显示,不是重新统计)。
    #[cfg(feature = "ai")]
    fn sorted_people(&self) -> Vec<crate::faces::PeopleRow> {
        let mut v = self.face_people.clone();
        let by_count = |a: &crate::faces::PeopleRow, b: &crate::faces::PeopleRow| {
            b.count.cmp(&a.count).then(a.cluster.cmp(&b.cluster))
        };
        match self.people_sort {
            PeopleSort::Name => v.sort_by(|a, b| match (&a.person, &b.person) {
                (Some(x), Some(y)) => x.cmp(y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => a.cluster.cmp(&b.cluster),
            }),
            PeopleSort::Amount => v.sort_by(by_count),
            PeopleSort::Top10 => {
                v.sort_by(by_count);
                v.truncate(10);
            }
        }
        v
    }

    fn rebuild(&mut self) {
        // 排序与过滤都在 SQL 侧完成:rows 即"当前视图该显示的行",
        // UI 层不再拿全表二次排序(大图库下每帧搬全表不可接受)。
        let rows = timeline::load_query(&self.store, self.sort_by, self.sort_rev, &self.filters)
            .unwrap_or_default();
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
        self.rows_gen = self.rows_gen.wrapping_add(1);
        // 工具栏 spinner:本视图还有多少张缩略图没落地(后台 worker 在补就是"有活")。
        self.pending_thumbs = self.rows.iter().filter(|r| r.thumb_ok == 0).count();
        self.refresh_tier_count();
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
        self.start_scan_roots_ext(roots, scope, false)
    }

    /// 带删除对账开关的扫描入口:仅启动增量对账传 `true` —— 扫描干净完成后
    /// 在同一线程里把「库里有行、磁盘已消失」的行移出图库(以磁盘为准,
    /// 离线根整根跳过)。常规扫描/导入/设备扫描不做删除,维持原口径。
    fn start_scan_roots_ext(&mut self, roots: Vec<PathBuf>, scope: &str, reconcile: bool) {
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
        self.scan_verb = "扫描";
        self.status = format!("正在{}{scope}…", self.scan_verb);
        std::thread::spawn(move || {
            // L1 索引住在服务进程里(方案 P0 决策 5),UI 侧不再持有 `IndexFile`,
            // 所以监听根的发现走 walkdir —— 这正是方案 P4-30 的口径:全盘发现与
            // 结构入库由服务负责,监听根只是用户挑的那几个目录,walkdir 足够。
            let res = crate::scan::scan_dir_ctrl(&store, &roots, &ctrl, None);
            let mut removed = 0usize;
            if reconcile {
                if let Ok(s) = &res {
                    if !s.cancelled {
                        let dropped = crate::scan::reconcile_missing(&store, &roots);
                        removed = dropped.len();
                        for (id, _) in &dropped {
                            crate::thumbs::purge_cached(*id);
                        }
                    }
                }
            }
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
                    removed,
                    cancelled: s.cancelled,
                    err: None,
                    scope: scope_owned,
                },
                Err(e) => ScanResult {
                    scanned: 0,
                    indexed: 0,
                    unchanged: 0,
                    excluded_dirs: 0,
                    removed,
                    cancelled: false,
                    err: Some(e.to_string()),
                    scope: scope_owned,
                },
            };
            let _ = tx.send(msg);
        });
    }

    // ===== 页面模型(图库页 ⇄ 导入页)与前后导航 =====

    /// 当前状态快照(压入历史用)。
    fn cur_stop(&self) -> NavStop {
        NavStop {
            page: self.page,
            view: self.view,
            folder: self.selected_folder,
            show_all: self.folder_show_all,
        }
    }

    /// 用户主动换页/换视图 → 记一站:截断游标之后的"前进"分支,
    /// 与栈顶同形的站不重复记,栈只留最近 50 站。
    fn push_nav(&mut self) {
        let stop = self.cur_stop();
        self.nav_hist.truncate(self.nav_at + 1);
        if self.nav_hist.last() != Some(&stop) {
            self.nav_hist.push(stop);
        }
        if self.nav_hist.len() > 50 {
            // 再早的站里存的目录号多半已被增删改作废,留着也回不去。
            let cut = self.nav_hist.len() - 50;
            self.nav_hist.drain(..cut);
        }
        self.nav_at = self.nav_hist.len() - 1;
    }

    /// 整页切换(参考件把看图与取图做成两套整页面板互切,不是弹窗)。
    fn go_page(&mut self, page: PageKind) {
        if self.page == page {
            return;
        }
        self.page = page;
        if page == PageKind::Import && self.import_src.is_none() {
            // 进页面先给个能用的默认源(我的图片 → 家目录 → 首个盘根),省一次点击。
            let default = crate::win::known_dirs()
                .into_iter()
                .next()
                .or_else(|| std::env::var("HOME").map(PathBuf::from).ok())
                .or_else(|| crate::win::drive_roots().into_iter().next());
            if let Some(d) = default {
                self.set_import_source(d);
            }
        }
        self.push_nav();
        self.status = format!("切到{}页", page.label());
    }

    /// 选视图:分类只在图库页有,所以顺带把页面切回去并记站。
    fn set_view(&mut self, v: ViewKind) {
        if self.view == v && self.page == PageKind::Library {
            return;
        }
        self.view = v;
        self.page = PageKind::Library;
        self.push_nav();
    }

    /// 后退一站(工具栏左箭头 / Alt+←)。
    fn nav_back(&mut self) {
        if self.nav_at == 0 {
            return;
        }
        self.nav_at -= 1;
        self.apply_nav();
    }

    /// 前进一站(工具栏右箭头 / Alt+→)。
    fn nav_forward(&mut self) {
        if self.nav_at + 1 >= self.nav_hist.len() {
            return;
        }
        self.nav_at += 1;
        self.apply_nav();
    }

    /// 恢复一站(不再压栈)。
    fn apply_nav(&mut self) {
        let Some(s) = self.nav_hist.get(self.nav_at).cloned() else {
            return;
        };
        self.page = s.page;
        self.view = s.view;
        self.folder_show_all = s.show_all;
        self.selected_folder = if self.folders.is_empty() {
            0
        } else {
            s.folder.min(self.folders.len() - 1)
        };
        // 前后导航同样算"进目录":该目录若没纳过派生作用域,这里补账。
        if let Some(f) = self.folders.get(self.selected_folder).cloned() {
            self.enter_dir(&f);
        }
        if s.page == PageKind::Import && self.import_src.is_none() {
            self.set_import_source(PathBuf::from(&self.last_import_src));
        }
        self.status = format!("回到 {} · {}", s.page.label(), s.view.label());
    }

    // ===== 导入页 =====

    /// 换导入源目录:清勾选 + 现读一次清单(只有这里碰磁盘)。
    fn set_import_source(&mut self, dir: PathBuf) {
        let indexed = timeline::indexed_paths_in(&self.store, &dir.to_string_lossy());
        self.import_entries = list_import_dir(&dir, &indexed);
        let n = self.import_entries.iter().filter(|e| !e.is_dir).count();
        self.last_import_src = dir.to_string_lossy().into_owned();
        self.import_src = Some(dir);
        self.import_sel.clear();
        self.status = format!("导入源:{n} 个可入库文件");
    }

    /// 重读当前源目录(清单按钮「刷新」;新增/删除文件后不必重开页面)。
    fn refresh_import_entries(&mut self) {
        let Some(dir) = self.import_src.clone() else {
            self.import_entries.clear();
            return;
        };
        self.set_import_source(dir);
    }

    /// 导入页左栏:源列表(我的文件夹 / 设备 / 盘根 / 浏览其他文件夹)。
    ///
    /// 占掉图库页那栏的位置 —— 参考件的导入面板同样把左侧换成源选择,
    /// 菜单、工具栏与状态栏保持原位。
    fn import_sources_ui(&mut self, ui: &mut egui::Ui) {
        crate::theme::section_title(ui, "选择源");
        let cur = self.import_src.clone();
        let mut pick: Option<PathBuf> = None;

        let shell = crate::win::known_dirs();
        if !shell.is_empty() {
            muted_title(ui, "我的文件夹");
            for p in shell {
                let name = p
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.display().to_string());
                if source_row(ui, "folder", &name, &p, &cur) {
                    pick = Some(p);
                }
            }
        }
        // 设备:可移动卷(U 盘 / 相机卡)。
        let devs = crate::device::removable_volumes();
        if !devs.is_empty() {
            muted_title(ui, "设备");
            for v in devs {
                let label = crate::win::volume_label(&v)
                    .map(|l| format!("{l} ({})", v.display()))
                    .unwrap_or_else(|| format!("可移动磁盘 {}", v.display()));
                if source_row(ui, "smartphone", &label, &v, &cur) {
                    pick = Some(v);
                }
            }
        }
        // 我的电脑:本机盘根(网络盘/光驱按扫描策略不列)。
        let roots: Vec<PathBuf> = crate::win::drive_roots()
            .into_iter()
            .filter(|r| {
                matches!(
                    crate::win::drive_type(r),
                    Some(crate::win::DriveType::Fixed)
                )
            })
            .collect();
        if !roots.is_empty() {
            muted_title(ui, "我的电脑");
            for r in roots {
                let label = crate::win::volume_label(&r)
                    .map(|l| format!("{l} ({})", r.display()))
                    .unwrap_or_else(|| format!("本地磁盘 {}", r.display()));
                if source_row(ui, "hard-drive", &label, &r, &cur) {
                    pick = Some(r);
                }
            }
        }

        ui.separator();
        if ui.button("浏览其他文件夹…").clicked() {
            if let Some(dir) = rfd::FileDialog::new()
                .set_title("选择导入源文件夹")
                .pick_folder()
            {
                pick = Some(dir);
            }
        }
        if ui.button("刷新清单").clicked() {
            self.refresh_import_entries();
        }
        if let Some(d) = pick {
            self.set_import_source(d);
        }
    }

    /// 导入页中央区:源 + 文件清单 + 导入动作 + 「返回图库」。
    fn import_page_ui(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("导入照片");
                if let Some(src) = &self.import_src {
                    ui.small(src.display().to_string())
                        .on_hover_text("当前导入源目录");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("返回图库").clicked() {
                        self.go_page(PageKind::Library);
                    }
                });
            });
            ui.separator();

            if self.import_src.is_none() {
                ui.small("左侧还没有可浏览的源:选一个文件夹,或点「浏览其他文件夹…」。");
                return;
            }

            // 清单头:选择工具 + 计数(计数读的是缓存,不重复读盘)。
            let pics = self.import_entries.iter().filter(|e| !e.is_dir).count();
            let fresh = self
                .import_entries
                .iter()
                .filter(|e| !e.is_dir && !e.in_library)
                .count();
            ui.horizontal(|ui| {
                if ui.button("全选照片").clicked() {
                    self.import_sel = (0..self.import_entries.len())
                        .filter(|&i| !self.import_entries[i].is_dir)
                        .collect();
                }
                if ui.button("全不选").clicked() {
                    self.import_sel.clear();
                }
                ui.small(format!(
                    "{pics} 个可入库文件(未入库 {fresh} 个),已勾选 {} 个",
                    self.import_sel.len()
                ));
            });

            // 清单本体:把表取出来画,画完回填 —— 逐帧 clone 大目录是白活。
            let entries = std::mem::take(&mut self.import_entries);
            let mut descend: Option<PathBuf> = None;
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for (i, e) in entries.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if e.is_dir {
                                ui.add(toolbar_icon("folder-open"));
                                let text = format!("{} \\", e.name);
                                if ui
                                    .selectable_label(false, &text)
                                    .on_hover_text("进入该子目录")
                                    .clicked()
                                {
                                    descend = Some(e.path.clone());
                                }
                            } else {
                                ui.add(toolbar_icon("images"));
                                let mut on = self.import_sel.contains(&i);
                                let name = if e.in_library {
                                    format!("{}  (已入库)", e.name)
                                } else {
                                    e.name.clone()
                                };
                                let chk =
                                    ui.checkbox(&mut on, &name).on_hover_text(if e.in_library {
                                        "已在图库里,导入时会跳过"
                                    } else {
                                        "勾选后「导入所选」只入这几张"
                                    });
                                if chk.changed() {
                                    if on {
                                        self.import_sel.insert(i);
                                    } else {
                                        self.import_sel.remove(&i);
                                    }
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.small(&e.date);
                                        ui.small(&e.size_s);
                                    },
                                );
                            }
                        });
                    }
                    if entries.is_empty() {
                        ui.small("这个目录里没有可入库的图片(或子目录)。");
                    }
                });
            let sel: Vec<PathBuf> = self
                .import_sel
                .iter()
                .filter_map(|&i| entries.get(i))
                .filter(|e| !e.is_dir && !e.in_library)
                .map(|e| e.path.clone())
                .collect();
            self.import_entries = entries;
            if let Some(d) = descend {
                self.set_import_source(d);
            }

            // 动作行。
            ui.separator();
            ui.horizontal(|ui| {
                ui.small("导入 = 登记该目录进图库并建索引,不移动、不改写原文件。");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            !sel.is_empty(),
                            egui::Button::new(format!("导入所选({})", sel.len())),
                        )
                        .on_hover_text("只把勾选的文件逐张入库(有进度,可取消)")
                        .clicked()
                    {
                        self.import_selected(sel.clone());
                    }
                    if ui
                        .button("导入整个文件夹")
                        .on_hover_text("登记该目录并扫描(含子目录)")
                        .clicked()
                    {
                        self.import_whole_folder();
                    }
                });
            });
        });
    }

    /// 「导入整个文件夹」:登记目录 + 常规扫描(进度/取消/断点续扫都复用同一套)。
    fn import_whole_folder(&mut self) {
        let Some(dir) = self.import_src.clone() else {
            return;
        };
        if let Err(e) = self.store.upsert_watched(&dir) {
            self.status = format!("记录文件夹失败(本次仍会扫描): {e}");
        }
        let scope = dir.display().to_string();
        // 用户明确要这个目录 = 它该出缩略图:落 `imported` 账(方案 P3-19)。
        // 必须在起扫之前落 —— 扫描线程入库时读这份账本算 `rich_state`。
        self.include_dir(&scope, crate::db::INTENT_IMPORTED, crate::db::SRC_IMPORT);
        self.start_scan_roots(vec![dir], &scope);
    }

    /// 「导入所选」:只入勾选的那几张(目录仍登记进监听列表,
    /// 否则左栏看不到刚落库的图)。
    fn import_selected(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            self.status = "没有待导入的勾选(已入库的会跳过)".into();
            return;
        }
        if self.scan_busy.swap(true, Ordering::SeqCst) {
            self.status = "已有扫描在跑,请等它结束后再导入所选".into();
            return;
        }
        let Some(dir) = self.import_src.clone() else {
            self.scan_busy.store(false, Ordering::SeqCst);
            return;
        };
        let _ = self.store.upsert_watched(&dir);
        // 用户挑中的这几张 = 它所在的目录该出缩略图(方案 P3-19)。
        self.include_dir(
            &dir.display().to_string(),
            crate::db::INTENT_IMPORTED,
            crate::db::SRC_IMPORT,
        );
        let store = self.store.clone();
        let busy = self.scan_busy.clone();
        let scope = format!("所选 {} 张", paths.len());
        self.scan_scope = scope.clone();
        self.scan_verb = "导入";
        let scope_owned = scope.clone();
        let (tx, rx) = mpsc::channel();
        self.scan_rx = Some(rx);
        let ctrl = Arc::new(crate::scan::ScanControl::new());
        ctrl.set_total(paths.len());
        self.scan_ctrl = Some(ctrl.clone());
        self.status = format!("正在{scope}…");
        std::thread::spawn(move || {
            let mut scanned = 0usize;
            let mut indexed = 0usize;
            let mut skipped = 0usize;
            for p in &paths {
                if ctrl.is_cancelled() {
                    break;
                }
                scanned += 1;
                match crate::scan::index_file_ctrl(&store, p, Some(&ctrl)) {
                    Ok(true) => indexed += 1,
                    Ok(false) => skipped += 1,
                    Err(_) => {}
                }
                ctrl.advance(1);
            }
            let cancelled = ctrl.is_cancelled();
            busy.store(false, Ordering::SeqCst);
            let msg = ScanResult {
                scanned,
                indexed,
                unchanged: skipped,
                excluded_dirs: 0,
                removed: 0,
                cancelled,
                err: None,
                scope: scope_owned,
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
        // 用户明确要这个目录 = 它该出缩略图:落 `imported` 账(方案 P3-19)。
        // 必须在起扫之前落 —— 扫描线程入库时读这份账本算 `rich_state`。
        self.include_dir(&scope, crate::db::INTENT_IMPORTED, crate::db::SRC_IMPORT);
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
        let name = photo
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
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
        // 模型缺失时直说怎么放,不进后台线程撞裸错误。
        let algo = crate::faces::current_algo(&self.store);
        if !crate::ai::face::Faces::models_present(algo) {
            let dir = crate::ai::models_dir()
                .map(|d| d.display().to_string())
                .unwrap_or_default();
            let (det_f, rec_f) = algo.files();
            self.status = format!(
                "缺少人脸模型:请将 {det_f} 与 {rec_f} 放入模型目录 {dir},获取方式见 README"
            );
            return;
        }
        let store = self.store.clone();
        let (tx, rx) = mpsc::channel();
        self.face_rx = Some(rx);
        self.face_prog = (0, 0);
        self.status = "正在人脸识别(扫描 200 张照片)…".into();
        std::thread::spawn(move || {
            let res = crate::faces::scan_batch(&store, 200, &|done, total| {
                let _ = tx.send(FaceScan::Progress(done, total));
            })
            .map_err(|e| e.to_string());
            let _ = tx.send(FaceScan::Done(res));
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

    /// 人脸算法版本检查(算法版本迁移):库中记录的版本+方案标识与当前
    /// 不一致时弹窗,建议清除旧人脸数据重扫。
    #[cfg(feature = "ai")]
    fn check_face_algo_version(&mut self) {
        let tag = crate::faces::algo_tag(&self.store);
        match self.store.get_setting("face_algo_version") {
            None => {
                let _ = self.store.set_setting("face_algo_version", &tag);
            }
            Some(v) if v != tag => {
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
                // 铺开全部文件夹时,选择/导出/放映的范围就是整个图库
                // (与中间栏看到的组头一致,不能只算 selected_folder 那一组)。
                if self.folder_show_all {
                    return self.rows.iter().map(|r| r.id).collect();
                }
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
            ViewKind::Albums => self
                .sel_album
                .map(|a| self.store.album_photo_ids(a))
                .unwrap_or_default(),
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
            // 快速查找结果不是图库照片,选择/导出范围不适用。
            ViewKind::Search => Vec::new(),
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
            Some(hex) => {
                u64::from_str_radix(hex, 16).map_err(|e| format!("十六进制解析失败: {e}"))?
            }
            None => t.parse::<u64>().map_err(|e| format!("数字解析失败: {e}"))?,
        };
        // uid64 存的是 i64(高位为 1 时是负数)→ 还原成 u64。
        let id = self
            .store
            .photo_by_uid64(uid)
            .ok_or_else(|| format!("未找到 uid64 = {uid} 的照片"))?;
        let path: String = self
            .store
            .read(|conn| {
                conn.query_row("SELECT path FROM photos WHERE id=?1", [id], |r| r.get(0))
            })
            .map_err(|e| e.to_string())?;
        Ok(path)
    }

    /// 刷新当前档位「已生成缩略图数」(状态栏进度用)。
    ///
    /// 这个数只在切档 / 后台批次回报 / 重建图库时变,
    /// 逐帧查是一次 `COUNT(*)` 还要和后台线程抢库锁,所以缓存起来。
    fn refresh_tier_count(&mut self) {
        self.tier_cached = self.store.tier_count(self.thumb_tier as u8);
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
        self.refresh_tier_count();
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
                st.push(crate::effects::Effect::oneclick(name, Default::default()));
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
        ui.heading("效果");
        // 标签页(编辑面板按功能分页,不堆长列表)。
        ui.horizontal(|ui| {
            for (i, name) in ["调整", "曲线", "一键", "裁剪", "维护"].iter().enumerate() {
                if ui.selectable_label(self.fx_tab == i, *name).clicked() {
                    self.fx_tab = i;
                }
            }
        });
        ui.separator();
        match self.fx_tab {
            0 => {
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
            }
            1 => self.fx_curves_ui(ui, id),
            2 => self.fx_oneclick_ui(ui, id),
            3 => self.fx_trim_ui(ui, id),
            _ => self.pool_maintenance_ui(ui),
        }
        ui.separator();
        ui.horizontal(|ui| {
            let can_undo = self.fx_undo.get(&id).map(|s| s.can_undo()).unwrap_or(false);
            let can_redo = self.fx_undo.get(&id).map(|s| s.can_redo()).unwrap_or(false);
            if ui
                .add_enabled(can_undo, egui::Button::new("撤销"))
                .clicked()
            {
                self.fx_undo_step(id);
            }
            if ui
                .add_enabled(can_redo, egui::Button::new("重做"))
                .clicked()
            {
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
                if *refs > 0 {
                    *n as f64 * 100.0 / *refs as f64
                } else {
                    0.0
                }
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
                    ui.small(format!(
                        "  · {pool}:可回收 {} 条孤儿({cur} → {alive})",
                        cur - alive
                    ));
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
                        self.fx_msg =
                            format!("池重建完成:回收孤儿 {removed} 条,重映射 {remapped} 行");
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
                self.fx_commit_effect(
                    id,
                    "曲线",
                    crate::effects::Effect::curves(&self.fx_curve_draft),
                );
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
                .add(egui::Slider::new(&mut self.fx_grain_amount, 0.0..=1.0).text("颗粒强度"))
                .changed()
            {
                self.fx_applied_sig = None;
            }
        }
        if self.fx_oneclick.as_deref() == Some("unsharp") {
            if ui
                .add(egui::Slider::new(&mut self.fx_sharpen_amount, 0.0..=100.0).text("锐化强度"))
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
                        format!(
                            "{:02x}{:02x}{:02x}",
                            highlight[0], highlight[1], highlight[2]
                        ),
                    );
                }
                if name == "unsharp" {
                    params.insert("amount".into(), format!("{:.4}", self.fx_sharpen_amount));
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
                    "bw", "sepia", "grain", "unsharp", "warm", "cool", "vignette", "faded", "tilt",
                    "shadow",
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
                if ui
                    .selectable_label(on, crate::recipes::label(key))
                    .clicked()
                {
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
    fn fx_commit_effect(&mut self, id: i64, label: &str, eff: crate::effects::Effect) {
        let before = self.fx_stack_for(id);
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

    /// 90° 直角旋转(参考件的「Rotate Clockwise / Counterclockwise」)。
    ///
    /// 效果栈的同名项会被替换,所以先把既有 `rot` 的 quarter 累加再提交 ——
    /// 连点两次顺时针 = 180°,四次回到原位(quarter 归零时直接移除该项)。
    fn preview_rotate(&mut self, id: i64, quarter_delta: u32) {
        let cur = self
            .fx_preview_stack(id)
            .items
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case("rot"))
            .and_then(|e| e.params.get("quarter").and_then(|v| v.parse::<u32>().ok()))
            .unwrap_or(0);
        let q = (cur + quarter_delta) % 4;
        if q == 0 {
            self.fx_remove_effect(id, "旋转复位", "rot");
            self.fx_msg = "已复位旋转".into();
        } else {
            let mut params = std::collections::BTreeMap::new();
            params.insert("quarter".into(), q.to_string());
            self.fx_commit_effect(
                id,
                if quarter_delta % 2 == 1 {
                    "旋转90°"
                } else {
                    "旋转180°"
                },
                crate::effects::Effect {
                    name: "rot".into(),
                    params,
                },
            );
        }
    }

    /// 读取自定义裁剪比例(设置项「custom_crop_ratios」,格式 name|w|h 分号分隔)。
    /// 对应参考件 Add Custom Aspect Ratio 对话框的持久化。
    fn load_custom_ratios(store: &Store) -> Vec<(String, f32, f32)> {
        let Some(s) = store.get_setting("custom_crop_ratios") else {
            return Vec::new();
        };
        s.split(';')
            .filter_map(|e| {
                let mut it = e.split('|');
                let name = it.next()?.to_string();
                let w = it.next()?.parse::<f32>().ok()?;
                let h = it.next()?.parse::<f32>().ok()?;
                Some((name, w, h))
            })
            .collect()
    }

    /// 把自定义比例写回设置项。
    fn save_custom_ratios(&self) {
        let s: Vec<String> = self
            .custom_ratios
            .iter()
            .map(|(n, w, h)| format!("{n}|{w}|{h}"))
            .collect();
        let _ = self.store.set_setting("custom_crop_ratios", &s.join(";"));
    }

    /// 进入幻灯片放映(参考件 View > Slideshow:当前视图逐张全屏放映)。
    /// 间隔 / 循环来自设置项(选项对话框可改)。
    fn start_slideshow(&mut self) {
        if self.view_ids().is_empty() {
            self.fx_msg = "当前视图没有照片".into();
            return;
        }
        self.slide_interval = self
            .store
            .get_setting("slideshow_interval")
            .and_then(|v| v.parse().ok())
            .unwrap_or(3.0);
        self.slide_loop = self
            .store
            .get_setting("slideshow_loop")
            .map(|v| v != "0")
            .unwrap_or(true);
        if self.preview.is_none() {
            if let Some(first) = self.view_ids().first() {
                self.preview = Some(*first);
                self.preview_tex = None;
                self.preview_failed = None;
            }
        }
        self.slideshow = true;
        self.slide_pause = false;
        self.slide_last = std::time::Instant::now();
    }

    /// 设为桌面壁纸:后台解码原图 → EXIF 摆正 → 存临时 JPEG → SystemParametersInfoW。
    fn set_wallpaper_for(&mut self, idx: usize) {
        if self.wallpaper_rx.is_some() {
            self.status = "上一个壁纸设置还在进行中…".into();
            return;
        }
        let path = PathBuf::from(&self.rows[idx].path);
        self.status = format!("正在设为桌面壁纸:{}…", path.display());
        let (tx, rx) = mpsc::channel();
        self.wallpaper_rx = Some(rx);
        std::thread::spawn(move || {
            let res = (|| -> Result<String, String> {
                let img = image::open(&path).map_err(|e| e.to_string())?;
                let img = crate::thumbs::apply_exif_rotation(
                    img,
                    crate::exif::read_orientation(Path::new(&path)),
                );
                let dst = std::env::temp_dir().join("maliang_wallpaper.jpg");
                // image 0.25 的 JPEG 编码器要可 seek 的目标,先编进内存缓冲。
                let rgb = img.to_rgb8();
                let mut buf = std::io::Cursor::new(Vec::new());
                let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 92);
                enc.encode(
                    rgb.as_raw(),
                    rgb.width(),
                    rgb.height(),
                    image::ExtendedColorType::Rgb8,
                )
                .map_err(|e| e.to_string())?;
                std::fs::write(&dst, buf.into_inner()).map_err(|e| e.to_string())?;
                if crate::win::set_wallpaper(&dst) {
                    Ok("已设为桌面壁纸".into())
                } else {
                    Err("SystemParametersInfoW 调用失败".into())
                }
            })();
            let _ = tx.send(res);
        });
    }

    /// 本张照片的文件路径(不在当前视图时 None)。
    fn photo_path(&self, id: i64) -> Option<String> {
        self.id_row.get(&id).map(|&idx| self.rows[idx].path.clone())
    }

    /// 属性详情:名称 / 位置 / 大小+修改日期 + Property-Value 两列清单。
    /// 浮动属性窗与右栏抽屉「属性」页共用(参考件属性窗上半部)。
    fn props_detail_ui(&mut self, ui: &mut egui::Ui, id: i64) {
        let Some(&idx) = self.id_row.get(&id) else {
            ui.small("该照片不在当前视图。");
            return;
        };
        // 先取齐数据再排版(避免闭包里借用 self.rows 冲突)。
        let (path, taken, cam, caption, star, kind) = {
            let r = &self.rows[idx];
            (
                r.path.clone(),
                r.taken.clone(),
                r.cam.clone(),
                r.caption.clone(),
                r.star != 0,
                r.kind,
            )
        };
        let dims = (self.rows[idx].width, self.rows[idx].height);
        let (lat, lon) = (self.rows[idx].lat, self.rows[idx].lon);
        let fx_count = self.fx_preview_stack(id).items.len();
        let name = Path::new(&path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone());
        ui.strong(name);
        ui.small(
            Path::new(&path)
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
        // 大小 + 修改日期(参考件 Size 行)。
        if let Ok(md) = std::fs::metadata(&path) {
            let kb = md.len() as f64 / 1024.0;
            let size_s = if kb >= 1024.0 {
                format!("{:.1} MB", kb / 1024.0)
            } else {
                format!("{kb:.0} KB")
            };
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| {
                    chrono::DateTime::<chrono::Local>::from(t)
                        .format("%Y-%m-%d %H:%M")
                        .to_string()
                        .into()
                })
                .unwrap_or_default();
            ui.small(format!("{size_s}    修改:{mtime}"));
        }
        ui.separator();
        // Property / Value 两列清单(参考件 list 控件)。
        egui::Grid::new("props-grid").num_columns(2).show(ui, |ui| {
            ui.label("类型");
            ui.label(if kind == crate::video::kind::MOVIE {
                "视频".into()
            } else {
                Path::new(&path)
                    .extension()
                    .map(|e| format!("图片({})", e.to_string_lossy().to_uppercase()))
                    .unwrap_or_else(|| "图片".into())
            });
            ui.end_row();
            ui.label("像素");
            ui.label(match dims {
                (Some(w), Some(h)) => format!("{w} × {h}"),
                _ => "—".into(),
            });
            ui.end_row();
            ui.label("拍摄时间");
            ui.label(taken.unwrap_or_default().replace('T', " "));
            ui.end_row();
            ui.label("相机");
            ui.label(if cam.is_empty() { "—".into() } else { cam });
            ui.end_row();
            ui.label("GPS");
            ui.label(match (lat, lon) {
                (Some(a), Some(b)) => format!("{a:.5}, {b:.5}"),
                _ => "—".into(),
            });
            ui.end_row();
            ui.label("星标");
            ui.label(if star {
                "★ 已加星"
            } else {
                "☆ 未加星"
            });
            ui.end_row();
            ui.label("标题");
            ui.label(if caption.is_empty() {
                "—".into()
            } else {
                caption
            });
            ui.end_row();
            ui.label("效果项");
            ui.label(format!("{fx_count} 项"));
            ui.end_row();
        });
    }

    /// 属性窗口(参考件 File > Properties:文件名 / 位置 / 大小+日期 /
    /// Property-Value 列表 / 上一张-下一张)。
    fn props_ui(&mut self, ctx: &egui::Context) {
        let id = match self.preview {
            Some(id) => id,
            // 没开预览时取第一个选中的照片。
            None => match self.sel.iter().next() {
                Some(&id) => id,
                None => {
                    self.show_props = false;
                    return;
                }
            },
        };
        if !self.id_row.contains_key(&id) {
            self.show_props = false;
            return;
        }
        let total = self.view_ids().len();
        let pos = self
            .preview
            .and_then(|p| self.view_ids().iter().position(|&x| x == p))
            .map(|i| i + 1)
            .unwrap_or(1);
        let mut props_open = self.show_props;
        egui::Window::new("属性")
            .open(&mut props_open)
            .default_width(460.0)
            .show(ctx, |ui| {
                self.props_detail_ui(ui, id);
                ui.separator();
                // 上一张 / 下一张(参考件 nav 行:"x of x files")。
                ui.horizontal(|ui| {
                    if ui.button("◀ 上一张").clicked() {
                        self.step_preview(-1);
                    }
                    ui.label(format!("{pos} / {total} 张"));
                    if ui.button("下一张 ▶").clicked() {
                        self.step_preview(1);
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("关闭").clicked() {
                        self.show_props = false;
                    }
                    if ui.button("在资源管理器中显示").clicked() {
                        if let Some(path) = self.photo_path(id) {
                            show_in_explorer(&path);
                        }
                    }
                });
            });
        // 标题栏 X(props_open=false)或关闭按钮都会关掉窗口。
        self.show_props = props_open && self.show_props;
    }

    /// 标题编辑(参考件 Caption:失焦或回车即保存)。
    /// 单图预览与右栏抽屉「属性」页共用同一份 caption_edit 草稿。
    fn caption_editor(&mut self, ui: &mut egui::Ui, id: i64) {
        if self.caption_edit.as_ref().map(|(k, _)| *k) != Some(id) {
            self.caption_edit = Some((id, self.store.get_caption(id)));
        }
        if let Some((_, cap)) = &mut self.caption_edit {
            let resp = ui
                .horizontal(|ui| {
                    ui.label("标题:");
                    ui.add(
                        egui::TextEdit::singleline(cap).desired_width(ui.available_width() - 60.0),
                    )
                })
                .inner;
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter)) && resp.has_focus();
            if resp.lost_focus() || enter {
                if *cap != self.store.get_caption(id) {
                    let _ = self.store.set_caption(id, cap);
                    self.fx_msg = "标题已保存".into();
                }
            }
        }
    }

    /// 标签编辑(参考件 Tag 面板:输入框 + 色标 + 点 ✕ 移除)。
    fn tags_editor(&mut self, ui: &mut egui::Ui, id: i64) {
        ui.horizontal(|ui| {
            ui.label("标签:");
            if ui.text_edit_singleline(&mut self.input_tag).lost_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter))
            {
                let t = std::mem::take(&mut self.input_tag);
                if !t.is_empty() {
                    let _ = timeline::add_tag(&self.store, id, &t);
                    self.sync_sidecars(id, self.star_of(id));
                }
            }
            if ui.button("添加").clicked() {
                let t = std::mem::take(&mut self.input_tag);
                if !t.is_empty() {
                    let _ = timeline::add_tag(&self.store, id, &t);
                    self.sync_sidecars(id, self.star_of(id));
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
                                self.sync_sidecars(id, self.star_of(id));
                            }
                        });
                    }
                });
            }
        }
    }

    /// 本张当前星标值(不在视图里时按未加星处理)。
    fn star_of(&self, id: i64) -> i32 {
        self.id_row
            .get(&id)
            .map(|&idx| self.rows[idx].star)
            .unwrap_or(0)
    }

    /// 切换单张星标(网格右键、预览窗与右栏抽屉共用同一条写库 + 旁文件链路)。
    fn toggle_star(&mut self, id: i64) {
        let new = if self.star_of(id) != 0 { 0 } else { 1 };
        let _ = timeline::set_star(&self.store, id, new);
        if let Some(&idx) = self.id_row.get(&id) {
            self.rows[idx].star = new;
        }
        self.sync_sidecars(id, new);
    }

    /// 抽屉作用的照片:优先单图预览里那张,其次第一个选中项。
    fn drawer_target(&self) -> Option<i64> {
        self.preview.or_else(|| self.sel.iter().next().copied())
    }

    /// 没选中照片时四个页签共用的提示。
    fn drawer_need_photo(ui: &mut egui::Ui) {
        ui.small("先在图库点选一张照片(或双击打开单图)。");
        ui.small("这里显示它的属性、标签、人物与拍摄地点。");
    }

    /// 右栏抽屉(参考件右栏面板:属性 / 标签 / 人物 / 地点 四页签,
    /// 面板头「»」收起,当前页签记忆到设置项 drawer_tab)。
    fn drawer_ui(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("props")
            .resizable(true)
            .default_width(280.0)
            .width_range(220.0..=420.0)
            .frame(crate::theme::side_frame())
            .show(ctx, |ui| {
                let cur = self.drawer_tab;
                ui.horizontal(|ui| {
                    for t in DrawerTab::ALL {
                        // A9:抽屉里这几页的字段只可能来自 L3(派生层)—— EXIF、
                        // 缩略图、标签、人物、地理都是逐条打开文件抽出来的,L1/L2 里
                        // 没有。标注挂在页签的悬停上,不新增可见控件。
                        if ui
                            .selectable_label(cur == t, t.label())
                            .on_hover_text(
                                "来源 L3:派生数据(EXIF / 缩略图 / 标签 / 人物 / 地理),\
                                 由图库按作用域门控逐条抽取。\
                                 未纳入派生作用域的目录这里没有内容 ——\
                                 点进该目录、在「工具 → 文件夹管理器」里添加它,\
                                 或在网格里点开这张图即可纳入。",
                            )
                            .clicked()
                        {
                            self.drawer_tab = t;
                            let _ = self.store.set_setting("drawer_tab", t.key());
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("»").on_hover_text("收起属性面板").clicked() {
                            self.drawer_open = false;
                            let _ = self.store.set_setting("drawer_open", "0");
                        }
                    });
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt("drawer")
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.drawer_tab {
                        DrawerTab::Props => self.drawer_props_ui(ui),
                        DrawerTab::Tags => self.drawer_tags_ui(ui),
                        #[cfg(feature = "ai")]
                        DrawerTab::People => self.drawer_people_ui(ui),
                        #[cfg(not(feature = "ai"))]
                        DrawerTab::People => {
                            ui.small("此构建未开启人脸功能(ai 特性)。");
                        }
                        DrawerTab::Places => self.drawer_places_ui(ui, ctx),
                    });
            });
    }

    /// 抽屉「属性」页:属性清单 + 标题编辑 + 星标/定位。
    fn drawer_props_ui(&mut self, ui: &mut egui::Ui) {
        let Some(id) = self.drawer_target() else {
            Self::drawer_need_photo(ui);
            return;
        };
        self.props_detail_ui(ui, id);
        ui.add_space(4.0);
        self.caption_editor(ui, id);
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let starred = self.star_of(id) != 0;
            if ui
                .small_button(if starred {
                    "★ 取消星标"
                } else {
                    "☆ 加星"
                })
                .clicked()
            {
                self.toggle_star(id);
            }
            if ui.small_button("在资源管理器中显示").clicked() {
                if let Some(path) = self.photo_path(id) {
                    show_in_explorer(&path);
                }
            }
        });
    }

    /// 抽屉「标签」页:本张标签增删 + 库内已有标签一键套用(参考件 Tag 面板)。
    fn drawer_tags_ui(&mut self, ui: &mut egui::Ui) {
        let Some(id) = self.drawer_target() else {
            Self::drawer_need_photo(ui);
            return;
        };
        self.tags_editor(ui, id);
        ui.add_space(6.0);
        ui.separator();
        ui.small("库内标签(点一下即加到本张):");
        let names: Vec<String> = self.tags.iter().map(|t| t.name.clone()).collect();
        if names.is_empty() {
            ui.small("(还没有标签)");
            return;
        }
        ui.horizontal_wrapped(|ui| {
            for n in names {
                let col = self
                    .tags
                    .iter()
                    .find(|x| x.name == n)
                    .and_then(|x| tag_color(x.color.as_deref()));
                ui.horizontal(|ui| {
                    if let Some(col) = col {
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                        ui.painter().rect_filled(rect, 1.0, col);
                    }
                    if ui.small_button(&n).clicked() {
                        let _ = timeline::add_tag(&self.store, id, &n);
                        self.sync_sidecars(id, self.star_of(id));
                    }
                });
            }
        });
    }

    /// 抽屉「人物」页:本张检测到的人脸 + 命名 / 忽略 / 跳到该人物。
    #[cfg(feature = "ai")]
    fn drawer_people_ui(&mut self, ui: &mut egui::Ui) {
        let Some(id) = self.drawer_target() else {
            Self::drawer_need_photo(ui);
            return;
        };
        let mut faces = crate::faces::photo_faces(&self.store, id);
        // 大脸在前(参考件把最大的人脸当主脸显示)。
        faces.sort_by(|a, b| {
            let area = |r: &(f32, f32, f32, f32)| (r.2 - r.0).max(0.0) * (r.3 - r.1).max(0.0);
            area(&b.rect)
                .partial_cmp(&area(&a.rect))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if faces.is_empty() {
            ui.small("本张照片还没有人脸数据。");
            ui.small("左栏「人物」里可以扫描图库并自动聚类。");
            return;
        }
        for f in faces {
            let Some(cluster) = f.cluster else {
                ui.small(format!(
                    "未归入人物的脸  · 质量 {:.2}{}",
                    f.quality,
                    if f.ignored { "(已忽略)" } else { "" }
                ));
                continue;
            };
            ui.separator();
            let named = f.person.clone().unwrap_or_default();
            ui.horizontal(|ui| {
                if named.is_empty() {
                    ui.strong(format!("簇 {cluster}"));
                } else {
                    ui.strong(named.as_str());
                }
                ui.small(format!("质量 {:.2}", f.quality));
                if f.ignored {
                    ui.small("(已忽略)");
                }
            });
            ui.horizontal(|ui| {
                if ui.small_button("命名").clicked() {
                    self.face_name_draft = Some((f.face_id, named.clone()));
                }
                if ui.small_button("查看该人物").clicked() {
                    if self.face_people.is_empty() {
                        self.refresh_people_and_suggestions();
                    }
                    self.set_view(ViewKind::Faces);
                    self.sel_cluster = Some(cluster);
                    self.face_hits = crate::faces::faces_for_cluster(
                        &self.store,
                        cluster,
                        self.face_show_ignored,
                        if self.face_hide_low { 0.4 } else { 0.0 },
                    );
                }
                if ui
                    .small_button(if f.ignored { "恢复" } else { "忽略" })
                    .clicked()
                {
                    let _ = crate::faces::ignore_cluster(&self.store, cluster, !f.ignored);
                    self.face_show_ignored = true;
                    self.refresh_people_and_suggestions();
                }
            });
            if self.face_name_draft.as_ref().map(|(k, _)| *k) == Some(f.face_id) {
                let mut commit: Option<String> = None;
                if let Some((_, draft)) = &mut self.face_name_draft {
                    let resp = ui.text_edit_singleline(draft);
                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter)) && resp.has_focus();
                    if resp.lost_focus() || enter {
                        commit = Some(draft.clone());
                    }
                }
                if let Some(name) = commit {
                    if !name.is_empty() {
                        let _ = crate::faces::rename_cluster(&self.store, cluster, &name);
                        self.refresh_people_and_suggestions();
                        self.status = format!("已命名:{name}");
                    }
                    self.face_name_draft = None;
                }
            }
        }
    }

    /// 抽屉「地点」页:GPS 坐标、复制、地图入口与全库 KML 导出。
    fn drawer_places_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(id) = self.drawer_target() else {
            Self::drawer_need_photo(ui);
            return;
        };
        let Some(&idx) = self.id_row.get(&id) else {
            ui.small("该照片不在当前视图。");
            return;
        };
        let (lat, lon) = (self.rows[idx].lat, self.rows[idx].lon);
        let taken = self.rows[idx].taken.clone();
        match (lat, lon) {
            (Some(a), Some(b)) => {
                ui.label(format!("GPS:{a:.6}, {b:.6}"));
                if let Some(t) = taken {
                    ui.small(format!("拍摄时间:{}", t.replace('T', " ")));
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.small_button("复制坐标").clicked() {
                        ctx.copy_text(format!("{a:.6}, {b:.6}"));
                        self.status = "已复制 GPS 坐标".into();
                    }
                    if ui.small_button("在地图中打开").clicked() {
                        // 交给系统默认浏览器(OpenStreetMap,无需第三方密钥)。
                        let url = format!(
                            "https://www.openstreetmap.org/?mlat={a}&mlon={b}#map=17/{a}/{b}"
                        );
                        if !crate::win::shell_open(Path::new(&url)) {
                            self.status = "无法打开地图(没有默认浏览器?)".into();
                        }
                    }
                });
            }
            (Some(a), None) => {
                ui.small("只有纬度,坐标不完整(原图 EXIF 可能损坏)。");
                ui.small(format!("纬度:{a:.6}"));
            }
            (None, Some(b)) => {
                ui.small("只有经度,坐标不完整(原图 EXIF 可能损坏)。");
                ui.small(format!("经度:{b:.6}"));
            }
            (None, None) => {
                ui.small("本张照片的 EXIF 里没有 GPS 坐标。");
                ui.small("相机未定位或导出时丢掉了定位信息;可在原图属性里补写。");
            }
        }
        ui.add_space(6.0);
        ui.separator();
        ui.small("整个图库的定位数据:");
        if ui.small_button("导出地理标记(KML)…").clicked() {
            self.export_kml();
        }
    }

    /// 选项窗口(参考件 options 窗:General / E-Mail / Slideshow 等页签;
    /// 本项目为全本地,Network / Web Albums / Name Tags 页签不适用,不设)。
    fn options_ui(&mut self, ctx: &egui::Context) {
        let mut opts_open = self.show_options;
        egui::Window::new("选项")
            .open(&mut opts_open)
            .default_width(420.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (i, name) in ["常规", "电子邮件", "幻灯片"].iter().enumerate() {
                        if ui.selectable_label(self.options_tab == i, *name).clicked() {
                            self.options_tab = i;
                        }
                    }
                });
                ui.separator();
                match self.options_tab {
                    0 => {
                        // 常规(参考件 General:本项目取本地适用项)。
                        if ui
                            .checkbox(&mut self.export_with_effects, "导出时套用已保存的效果")
                            .changed()
                        {
                            self.fx_msg = if self.export_with_effects {
                                "导出将带上已保存的效果".into()
                            } else {
                                "导出 = 原图副本".into()
                            };
                        }
                        if ui
                            .checkbox(&mut self.thumb_captions, "缩略图下方显示标题")
                            .changed()
                        {
                            let _ = self.store.set_setting(
                                "thumb_captions",
                                if self.thumb_captions { "1" } else { "0" },
                            );
                        }
                        if ui
                            .checkbox(&mut self.dup_near, "重复照片检测包含近似重复")
                            .changed()
                        {
                            self.rebuild();
                        }
                        ui.separator();
                        // 配色:默认 Picasa 灰;暗色方案为浅蓝点缀。切换即存。
                        ui.heading(egui::RichText::new("配色").size(14.0));
                        ui.horizontal(|ui| {
                            for k in [crate::theme::Kind::Gray, crate::theme::Kind::Blue] {
                                if ui
                                    .selectable_label(crate::theme::kind() == k, k.label())
                                    .clicked()
                                {
                                    crate::theme::switch(ctx, k);
                                    let _ = self.store.set_setting("ui_theme", k.key());
                                }
                            }
                        });
                        ui.separator();
                        // 人脸识别方案:两套模型都支持,默认推荐 YuNet + AdaFace
                        // (商业友好许可);antelopev2 权重不可商用,自备自用。
                        // 切换即存;两方案的向量不通用,会触发重扫提示。
                        #[cfg(feature = "ai")]
                        {
                            ui.heading(egui::RichText::new("人脸识别方案").size(14.0));
                            let cur = crate::faces::current_algo(&self.store);
                            for algo in [
                                crate::ai::face::FaceAlgo::AdaFace,
                                crate::ai::face::FaceAlgo::AntelopeV2,
                            ] {
                                ui.horizontal(|ui| {
                                    if ui.radio(cur == algo, algo.label()).clicked() && cur != algo
                                    {
                                        let _ = self.store.set_setting("face_algo", algo.key());
                                        self.refresh_people();
                                    }
                                });
                                let (det_f, rec_f) = algo.files();
                                let ready = if crate::ai::face::Faces::models_present(algo) {
                                    "模型已就位".to_string()
                                } else {
                                    format!("缺少 {det_f} / {rec_f}(获取方式见 README)")
                                };
                                ui.small(format!("　{ready}"));
                            }
                            ui.separator();
                        }
                        // 全盘快查的数据源是常驻索引服务(Windows 专属):
                        // 状态一览 + 启用意向 + 手动重建/卸载入口。
                        ui.heading(egui::RichText::new("全盘快查(索引服务)").size(14.0));
                        let link = self.link.clone();
                        let svc_text = match link.svc {
                            crate::win::ServiceState::NotInstalled => "未安装".to_string(),
                            crate::win::ServiceState::Stopped => "已停止".to_string(),
                            // 过渡态的原始码只给排障看,别当成确定结论。
                            crate::win::ServiceState::Transition(c) => format!("过渡态(码 {c})"),
                            crate::win::ServiceState::Running => "运行中".to_string(),
                        };
                        let status = if link.online {
                            let mut s = format!("服务{svc_text} · {} 卷 {} 条", link.vols, link.entries);
                            if link.building > 0 {
                                s.push_str(&format!(" · {} 卷正在建库", link.building));
                            }
                            if link.dirty {
                                s.push_str(" · 有未落盘的变更");
                            }
                            s.push_str(&format!(" · 已运行 {}", fmt_uptime(link.uptime)));
                            if let Some(t) = link.seen {
                                s.push_str(&format!(" · {} 秒前刷新", t.elapsed().as_secs()));
                            }
                            s
                        } else if link.svc == crate::win::ServiceState::Running {
                            format!("服务{svc_text},但连不上它的管道(全盘快查暂不可用)")
                        } else {
                            format!("服务{svc_text}:全盘快查不可用,发现与保鲜走逐目录扫描兜底")
                        };
                        ui.horizontal(|ui| {
                            ui.label("状态:");
                            ui.label(&status);
                        });
                        if link.building > 0 {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.small(format!(
                                    "{} 卷正在全量建库,期间快查结果可能不全",
                                    link.building
                                ));
                            });
                        }
                        for (letter, why) in &link.compat {
                            ui.small(format!(
                                "{letter}: 盘走兼容扫描(walkdir + 目录监听),无实时快查 —— {why}"
                            ));
                        }
                        #[cfg(windows)]
                        {
                            let cur = self.svc_want();
                            ui.horizontal(|ui| {
                                ui.label("全盘快查:");
                                if ui
                                    .radio(cur != SvcWant::No, "启用(推荐)")
                                    .clicked()
                                {
                                    self.svc_apply_want(SvcWant::Yes);
                                }
                                if ui.radio(cur == SvcWant::No, "不用").clicked() {
                                    self.svc_apply_want(SvcWant::No);
                                }
                            });
                            ui.small("启用 = 装一个自动启动的索引服务:安装与卸载各过一次 UAC,之后开机自启、全程静默。");
                            if self.store.get_setting("svc_never_ask").as_deref() == Some("1") {
                                ui.small("已勾「不再提示」:启动不再自动询问,点上面任一档即可恢复。");
                            }
                            ui.horizontal(|ui| {
                                if ui
                                    .add_enabled(link.online, egui::Button::new("立即重建全盘索引"))
                                    .clicked()
                                {
                                    self.svc_rebuild();
                                }
                                if ui
                                    .add_enabled(link.online, egui::Button::new("立刻保存索引"))
                                    .clicked()
                                {
                                    if let Some(tx) = &self.link_tx {
                                        let _ = tx.send(LinkCmd::Flush);
                                    }
                                    self.status = "已请求索引服务把内存索引落盘".into();
                                }
                                if ui
                                    .add_enabled(!self.svc_busy, egui::Button::new("重试连接"))
                                    .clicked()
                                {
                                    if let Some(tx) = &self.link_tx {
                                        let _ = tx.send(LinkCmd::Retry);
                                    }
                                }
                            });
                            // 停与起必须成对出现:只给「停止」等于把人关在门外 ——
                            // 停完之后只剩「卸载再重装」这一条回头路。
                            let stoppable = link.svc == crate::win::ServiceState::Running;
                            let startable = link.svc == crate::win::ServiceState::Stopped;
                            if stoppable || startable {
                                ui.horizontal(|ui| {
                                    if stoppable
                                        && ui
                                            .add_enabled(
                                                !self.svc_busy,
                                                egui::Button::new("停止索引服务…"),
                                            )
                                            .clicked()
                                    {
                                        self.svc_stop();
                                    }
                                    if startable
                                        && ui
                                            .add_enabled(
                                                !self.svc_busy,
                                                egui::Button::new("启动索引服务…"),
                                            )
                                            .clicked()
                                    {
                                        self.svc_start();
                                    }
                                });
                                ui.small(if stoppable {
                                    "停一下服务能让程序自己腾出手来:升级、搬家或想把硬盘歇口气的时候用,索引记录都保留,在同一页点「启动」就接着跑。"
                                } else {
                                    "服务停着时全盘快查不可用,发现与保鲜退回逐目录扫描;索引文件都留在数据目录,启动后接着用。"
                                });
                            }
                            if link.svc != crate::win::ServiceState::NotInstalled
                                && ui
                                    .add_enabled(!self.svc_busy, egui::Button::new("卸载索引服务…"))
                                    .clicked()
                            {
                                self.svc_uninstall();
                            }
                            if link.svc != crate::win::ServiceState::NotInstalled {
                                ui.small("卸载会停掉服务并删除注册项(需要一次 UAC);索引文件本身留在数据目录。");
                            }
                            if self.svc_busy {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.small("等待你在 UAC 授权窗口点「是」…");
                                });
                            }
                        }
                        #[cfg(not(windows))]
                        ui.small("全盘快查仅支持 Windows(NTFS USN journal)。");
                        ui.small("服务装卸失败后不再自动打扰;可在此或快速查找页重新发起。");
                        ui.separator();
                        // 数据目录:防 C 盘写满 —— 库/索引/模型整体迁走。
                        ui.heading(egui::RichText::new("数据目录").size(14.0));
                        ui.small(crate::db::data_dir().to_string_lossy().to_string());
                        ui.horizontal(|ui| {
                            if ui.button("更改数据目录…").clicked() {
                                if let Some(d) = rfd::FileDialog::new().pick_folder() {
                                    if d != crate::db::data_dir() {
                                        match crate::db::set_data_dir(Some(&d)) {
                                            Ok(()) => {
                                                self.status = format!(
                                                    "数据目录已指向 {},下次启动时自动迁移文件",
                                                    d.display()
                                                );
                                            }
                                            Err(e) => {
                                                self.status =
                                                    format!("写入数据目录配置失败: {e}");
                                            }
                                        }
                                    }
                                }
                            }
                            if ui.button("恢复默认").clicked() {
                                match crate::db::set_data_dir(None) {
                                    Ok(()) => {
                                        self.status =
                                            "数据目录已恢复默认(旧目录文件保留,不自动删)"
                                                .into();
                                    }
                                    Err(e) => {
                                        self.status = format!("清除数据目录配置失败: {e}");
                                    }
                                }
                            }
                        });
                        ui.small(
                            "更改后,数据库/全盘索引/模型在下次启动时复制到新目录(原目录文件保留);本会话继续用原目录。",
                        );
                        ui.small(
                            "索引服务认的是安装时记下的数据目录;改过目录后要先「卸载索引服务…」再重新安装,它才会跟着搬。",
                        );
                        ui.separator();
                        // 缩略图缓存:库搬到大盘时缓存跟着走,不再默默写系统盘。
                        ui.heading(egui::RichText::new("缩略图缓存").size(14.0));
                        let cdir = crate::db::cache_dir();
                        let sdir = crate::db::system_cache_dir();
                        ui.small(cdir.to_string_lossy().to_string());
                        ui.small(if cdir == sdir {
                            "在系统缓存目录;数据目录迁走后会自动跟随新数据目录。"
                        } else {
                            "跟随数据目录(可用 MALIANG_CACHE_DIR 单独指定)。"
                        });
                        // 整树遍历有成本:按 2 秒时间片采样,不每帧重算。
                        let fresh = match self.cache_usage {
                            Some((_, _, at)) => at.elapsed().as_secs_f32() < 2.0,
                            None => false,
                        };
                        if !fresh {
                            let (files, bytes) = crate::db::tree_usage(&cdir);
                            self.cache_usage =
                                Some((files, bytes, std::time::Instant::now()));
                        }
                        if let Some((files, bytes, _)) = self.cache_usage {
                            ui.small(format!("{files} 个文件 · {}", fmt_size(bytes)));
                        }
                        // 旧位置还有东西就提供一次性的搬运(可再生,但省一轮重解)。
                        if cdir != sdir && crate::db::system_cache_has_content() {
                            if ui.button("把旧缓存搬过来").clicked() {
                                match crate::db::migrate_cache_from(&sdir) {
                                    Ok((0, _)) => {
                                        self.status =
                                            "旧缓存无新内容可搬(目标目录已是最新)".into();
                                    }
                                    Ok((n, cleared)) => {
                                        self.status = format!(
                                            "已搬动 {n} 个缓存文件{}",
                                            if cleared { "" } else { "(旧目录尚有占用中的文件)" }
                                        );
                                    }
                                    Err(e) => {
                                        self.status = format!("缓存迁移失败: {e}");
                                    }
                                }
                                // 搬完立刻重采,别让面板显示旧数字。
                                self.cache_usage = None;
                            }
                        }
                        ui.separator();
                        ui.small("排除规则、文件夹监听在「工具 > 文件夹管理器」里配置。");
                        ui.small("索引备份 / 压缩在「工具 > 图库维护」里。");
                    }
                    1 => {
                        // 电子邮件(参考件 E-Mail:8 档尺寸列表,默认 1024)。
                        ui.label("邮件附件默认尺寸(发送窗口仍可临时改):");
                        let cur_px = self.email_size.px();
                        for s in crate::share::ShareSize::ALL {
                            if ui
                                .selectable_label(cur_px == s.px(), s.label())
                                .clicked()
                            {
                                self.email_size = s;
                                let _ = self
                                    .store
                                    .set_setting("email_size_px", &s.px().to_string());
                            }
                        }
                    }
                    _ => {
                        // 幻灯片放映:原版同样提供循环开关与间隔设置;
                        // 间隔的具体数值暂无对应资料,默认 3 秒。
                        let mut itv = self
                            .store
                            .get_setting("slideshow_interval")
                            .and_then(|v| v.parse::<f32>().ok())
                            .unwrap_or(3.0);
                        if ui
                            .add(egui::Slider::new(&mut itv, 1.0..=10.0).text("每张停留(秒)"))
                            .changed()
                        {
                            let _ = self.store.set_setting("slideshow_interval", &format!("{itv:.1}"));
                        }
                        let mut lp = self
                            .store
                            .get_setting("slideshow_loop")
                            .map(|v| v != "0")
                            .unwrap_or(true);
                        if ui.checkbox(&mut lp, "循环放映").changed() {
                            let _ =
                                self.store.set_setting("slideshow_loop", if lp { "1" } else { "0" });
                        }
                    }
                }
            });
        self.show_options = opts_open && self.show_options;
    }

    /// 幻灯片全屏渲染(接管整帧;空格暂停、Esc 退出、点击前进)。
    fn slideshow_ui(&mut self, ctx: &egui::Context) {
        let ids = self.view_ids();
        if ids.is_empty() {
            self.slideshow = false;
            return;
        }
        let interval = self.slide_interval.max(0.5) as f64;
        if !self.slide_pause {
            let elapsed = self.slide_last.elapsed().as_secs_f64();
            if elapsed >= interval {
                // 不循环时放到尾张即退出(与原版行为一致)。
                let last = *ids.last().unwrap();
                if !self.slide_loop && self.preview == Some(last) {
                    self.slideshow = false;
                    return;
                }
                self.step_preview(1);
                self.slide_last = std::time::Instant::now();
            }
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(
                (interval - elapsed).max(0.05),
            ));
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(egui::Color32::BLACK))
            .show(ctx, |ui| {
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    self.slideshow = false;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Space)) {
                    self.slide_pause = !self.slide_pause;
                    self.slide_last = std::time::Instant::now();
                }
                if ui.input(|i| i.key_pressed(egui::Key::ArrowLeft)) {
                    self.step_preview(-1);
                    self.slide_last = std::time::Instant::now();
                }
                if ui.input(|i| i.key_pressed(egui::Key::ArrowRight)) {
                    self.step_preview(1);
                    self.slide_last = std::time::Instant::now();
                }
                let tex = self.preview_tex(ctx);
                let avail = ui.available_size();
                let (rect, resp) =
                    ui.allocate_exact_size(avail - egui::vec2(0.0, 34.0), egui::Sense::click());
                if let Some(t) = tex {
                    let [tw, th] = t.size();
                    let fit = (rect.width() / tw.max(1) as f32)
                        .min(rect.height() / th.max(1) as f32)
                        .min(1.0);
                    let size = egui::vec2(tw as f32 * fit, th as f32 * fit);
                    let img = egui::Rect::from_center_size(rect.center(), size);
                    ui.painter_at(rect).image(
                        t.id(),
                        img,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                }
                // 点击前进(参考件放映的翻页交互)。
                if resp.clicked() {
                    self.step_preview(1);
                    self.slide_last = std::time::Instant::now();
                }
                // 底部信息行:标题 + 序号 + 暂停提示。
                let n = ids.len();
                let pos = self
                    .preview
                    .and_then(|p| ids.iter().position(|&x| x == p))
                    .map(|i| i + 1)
                    .unwrap_or(0);
                let mut line = format!("{} / {}    空格 暂停 · Esc 退出", pos, n);
                if self.slide_pause {
                    line = format!("[已暂停]  {line}");
                }
                let cap = self
                    .preview
                    .map(|id| self.store.get_caption(id))
                    .unwrap_or_default();
                ui.horizontal(|ui| {
                    if !cap.is_empty() {
                        ui.label(
                            egui::RichText::new(&cap)
                                .color(egui::Color32::WHITE)
                                .size(15.0),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(line)
                                .color(egui::Color32::GRAY)
                                .size(12.0),
                        );
                    });
                });
            });
    }

    /// 应用裁剪(把归一化矩形写进效果链;对应参考件 Crop 工具的 Apply)。
    fn commit_crop(&mut self, id: i64) {
        let Some([ax, ay, bx, by]) = self.crop_rect.take() else {
            return;
        };
        let (x0, x1) = (ax.min(bx), ax.max(bx));
        let (y0, y1) = (ay.min(by), ay.max(by));
        let (w, h) = (x1 - x0, y1 - y0);
        if w < 0.02 || h < 0.02 {
            self.fx_msg = "裁剪区域太小".into();
            return;
        }
        let mut params = std::collections::BTreeMap::new();
        params.insert("x".into(), format!("{x0:.4}"));
        params.insert("y".into(), format!("{y0:.4}"));
        params.insert("w".into(), format!("{w:.4}"));
        params.insert("h".into(), format!("{h:.4}"));
        self.crop_mode = false;
        self.crop_drag = None;
        self.fx_commit_effect(
            id,
            "裁剪",
            crate::effects::Effect {
                name: "crop".into(),
                params,
            },
        );
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
        // 张数口径 → 字节口径:大档一张就是 4MB,只按张数限流会让 UI 卡死(方案 P4-23③)。
        self.thumb_decode_budget = crate::scroll::decode_budget_for(
            vel,
            self.last_viewport_h.max(1.0) as f64,
            self.thumb_tier.bytes(),
        );
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
        let mut cur = self.store.get_setting("exclude_dirs").unwrap_or_default();
        let already = cur.lines().any(|l| l.trim().eq_ignore_ascii_case(name));
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

    /// 强制刷新缩略图(方案 P3-21 的 `force_thumb_update` 偏好)。
    ///
    /// 先落偏好再动手:动作中途进程被杀,下次启动还能补做完。
    /// 账本里每个目录挂失效位 → 子树全部退回待派生(含已派生与坏文件 ——
    /// 用户明确要求刷新时坏文件也该再试一次)→ 清位销账。
    fn force_thumb_update(&mut self) {
        let _ = self.store.set_setting("force_thumb_update", "1");
        let dirs = self.store.mark_all_dirs_needs_refresh().unwrap_or(0);
        let rows = self.store.refresh_pending_dirs().unwrap_or(0);
        let _ = self.store.set_setting("force_thumb_update", "0");
        self.rebuild();
        self.status = format!("{dirs} 个目录 / {rows} 行已退回待派生,后台正在重出缩略图");
    }

    /// 启动时续做上次没跑完的强制刷新(偏好还挂着 = 上次中途退出)。
    fn resume_force_thumb_update(&mut self) {
        if self.store.get_setting("force_thumb_update").as_deref() != Some("1") {
            return;
        }
        let _ = self.store.mark_all_dirs_needs_refresh();
        let rows = self.store.refresh_pending_dirs().unwrap_or(0);
        let _ = self.store.set_setting("force_thumb_update", "0");
        if rows > 0 {
            self.status = format!("上次未完成的强制刷新已续做:{rows} 行退回待派生");
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
            let r = store
                .compact()
                .map(|_| "索引已压缩(VACUUM)".to_string())
                .map_err(|e| e.to_string());
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
                        .write(|conn| -> anyhow::Result<()> {
                            conn.execute("UPDATE photos SET xmp_imported=0", [])?;
                            Ok(())
                        })?;
                }
                let (stars, tagged) = timeline::import_xmp_batch(&store, 1_000_000)?;
                Ok(format!("XMP 导入完成:{stars} 张星标,{tagged} 张标签"))
            })()
            .map_err(|e| e.to_string());
            let _ = tx.send(r);
        });
    }

    /// 记录一张"疑似已删除"的照片,过确认期(仍是缺失)再移出图库。
    fn defer_missing(&mut self, id: i64, path: String) {
        if !self.pending_missing.iter().any(|(pid, _, _)| *pid == id) {
            self.pending_missing
                .push((id, path, std::time::Instant::now()));
        }
    }

    /// 把已确认不存在的照片移出图库(索引行 + 全部缩略图档缓存)并刷新视图。
    ///
    /// 由预览过程发现(打开预览、后台解码失败)。调用点必须在 UI 构建之前:
    /// `rebuild` 会换掉 `rows`,预览窗闭包里捕获的行索引随即失效。
    fn drop_missing(&mut self, items: Vec<(i64, String)>) {
        if items.is_empty() {
            return;
        }
        let paths: Vec<String> = items.into_iter().map(|(_, p)| p).collect();
        let gone = match self.store.drop_missing_paths(&paths) {
            Ok(v) => v,
            Err(e) => {
                self.status = format!("清理缺失文件失败: {e}");
                return;
            }
        };
        if gone.is_empty() {
            return;
        }
        for (id, _) in &gone {
            crate::thumbs::purge_cached(*id);
        }
        // 记下被移出照片在当前视图里的位置:幻灯片放映时要接着往下放,
        // 否则 preview 变空 → step_preview 直接返回,停在全黑画面。
        let removed_preview_pos = self.preview.and_then(|pid| {
            gone.iter()
                .any(|(id, _)| *id == pid)
                .then(|| self.view_ids().iter().position(|&x| x == pid).unwrap_or(0))
        });
        self.rebuild();
        if let Some(pos) = removed_preview_pos {
            self.preview = None;
            self.preview_tex = None;
            self.preview_base = None;
            self.preview_missing = None;
            self.preview_failed = None;
            if self.slideshow {
                let ids = self.view_ids();
                if !ids.is_empty() {
                    self.preview = Some(ids[pos % ids.len()]);
                }
            }
        }
        self.status = format!("{} 张照片的原文件已不存在,已移出图库", gone.len());
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
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
        };
        let mut out = String::new();
        out.push_str(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
",
        );
        out.push_str(
            "<kml xmlns=\"http://www.opengis.net/kml/2.2\">
<Document>
",
        );
        out.push_str(
            "  <name>Relyto.Maliang 地理标记</name>
",
        );
        for p in &pts {
            let name = Path::new(&p.path)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.path.clone());
            out.push_str(
                "  <Placemark>
",
            );
            out.push_str(&format!(
                "    <name>{}</name>
",
                xml_esc(&name)
            ));
            if let Some(t) = &p.taken {
                out.push_str(&format!(
                    "    <description>拍摄于 {}</description>
",
                    xml_esc(t)
                ));
            }
            if p.star != 0 {
                out.push_str(
                    "    <description>★ 星标</description>
",
                );
            }
            out.push_str(&format!(
                "    <Point><coordinates>{:.6},{:.6}</coordinates></Point>
",
                p.lon, p.lat
            ));
            out.push_str(
                "  </Placemark>
",
            );
        }
        out.push_str(
            "</Document>
</kml>
",
        );
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
            self.sel.iter().next().and_then(|id| {
                self.rows
                    .iter()
                    .find(|r| r.id == *id)
                    .map(|r| r.path.clone())
            })
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
            self.preview_failed = None;
            self.preview_reset_zoom();
            debug_assert!(row < self.rows.len());
        }
    }

    /// 预览缩放/平移复位(换照片、关闭预览时调用;参考件换图回到适应窗口)。
    fn preview_reset_zoom(&mut self) {
        self.preview_zoom = 1.0;
        self.preview_pan = egui::Vec2::ZERO;
        self.crop_mode = false;
        // 全图缓存内存可观(几十 MB),换图即释放;在途解码结果按 id 校验丢弃。
        self.preview_full = None;
        self.zoom_tex = None;
        self.zoom_rx = None;
        self.zoom_pending = None;
    }

    /// 中间栏按文件夹分组的缓存:`(文件夹, 行下标, 组头日期)`。
    ///
    /// `by_folder` 每行都要 clone 一个目录串,`group_date` 每组再扫一遍行,
    /// 逐帧重跑大图库会掉帧 —— 只在图库版本(rows_gen)变化时重算。
    /// 借出后由调用方在同一帧末尾归还(与 `months` / `folder_cells` 同一套路)。
    fn take_folder_groups(&mut self) -> Vec<(String, Vec<usize>, String)> {
        if self.folder_groups.0 != self.rows_gen {
            let groups = timeline::by_folder(&self.rows);
            let mut dated = Vec::with_capacity(groups.len());
            for (folder, idxs) in groups {
                let date = timeline::group_date(&self.rows, &idxs);
                dated.push((folder, idxs, date));
            }
            self.folder_groups = (self.rows_gen, dated);
        }
        std::mem::take(&mut self.folder_groups.1)
    }

    /// 由 `folders` 反推目录树并接管展开状态(树本体见 `build_folder_tree`)。
    fn rebuild_folder_tree(&mut self) {
        // 每个目录的代表照片:当前排序下第一张命中的行(新图优先)。
        let mut rep: HashMap<String, i64> = HashMap::new();
        for r in &self.rows {
            rep.entry(folder_of(&r.path)).or_insert(r.id);
        }
        let (nodes, roots) = build_folder_tree(&self.folders, &rep);
        // 展开状态:丢掉已不存在的目录,盘根默认摊开,选中项的祖先链补开。
        let alive: HashSet<&str> = nodes.iter().map(|n| n.path.as_str()).collect();
        self.tree_open.retain(|p| alive.contains(p.as_str()));
        for &r in &roots {
            self.tree_open.insert(nodes[r].path.clone());
        }
        if let Some(f) = self.folders.get(self.selected_folder).cloned() {
            self.tree_reveal(&f);
        }
        self.folder_tree_cache = FolderTree {
            gen: self.rows_gen,
            nodes,
            roots,
        };
    }

    /// 展开某目录的全部祖先(选中后树上看得见选中的那一条)。
    fn tree_reveal(&mut self, path: &str) {
        for p in Path::new(path).ancestors().take(64) {
            self.tree_open.insert(p.to_string_lossy().into_owned());
        }
    }

    /// 左栏文件夹树入口:树本体按 rows_gen 缓存,逐帧只走可见节点。
    fn folder_tree_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if self.folder_tree_cache.gen != self.rows_gen {
            self.rebuild_folder_tree();
        }
        let cache = std::mem::take(&mut self.folder_tree_cache);
        for &r in &cache.roots {
            self.walk_tree(ui, ctx, &cache.nodes, r, 0);
        }
        self.folder_tree_cache = cache;
    }

    /// 递归画一个目录节点及其展开中的子树。
    fn walk_tree(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        nodes: &[TreeNode],
        id: usize,
        depth: usize,
    ) {
        let n = &nodes[id];
        let kids = n.kids.clone();
        let path = n.path.clone();
        let name = n.name.clone();
        let folder_index = n.folder_index;
        let thumb = n.photo_id;
        let open = self.tree_open.contains(&path);
        let active = folder_index.is_some()
            && !self.folder_show_all
            && self.selected_folder == folder_index.unwrap_or(usize::MAX);
        ui.horizontal(|ui| {
            ui.add_space((depth as f32) * 10.0);
            if !kids.is_empty() {
                if ui
                    .small_button(if open { "▾" } else { "▸" })
                    .on_hover_text("展开 / 收起")
                    .clicked()
                {
                    if open {
                        self.tree_open.remove(&path);
                    } else {
                        self.tree_open.insert(path.clone());
                    }
                }
            } else {
                ui.add_space(26.0);
            }
            if self.tree_thumbs {
                if let Some(pid) = thumb {
                    self.tree_thumb_ui(ui, ctx, pid);
                }
            }
            let text = egui::RichText::new(name.as_str()).color(if folder_index.is_some() {
                crate::theme::p().text
            } else {
                crate::theme::p().muted
            });
            if ui.selectable_label(active, text).clicked() {
                match folder_index {
                    Some(fi) => {
                        self.selected_folder = fi;
                        self.folder_show_all = false;
                        self.enter_dir(&path);
                        self.tree_reveal(&path);
                    }
                    // 中间层目录本身没有照片:点名字即展开/收起,不当作选中。
                    None => {
                        if open {
                            self.tree_open.remove(&path);
                        } else {
                            self.tree_open.insert(path.clone());
                        }
                    }
                }
            }
        });
        if open {
            for k in kids {
                self.walk_tree(ui, ctx, nodes, k, depth + 1);
            }
        }
    }

    /// 树行的 18px 代表图。
    ///
    /// 取的是**当前档**的已落盘缩略图(后台 worker 只补当前档,单开一档会做成
    /// 一片空位);纹理按照片 id 缓存,不逐帧重传。
    fn tree_thumb_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, id: i64) {
        if self.tree_texs.len() > 256 {
            self.tree_texs.clear();
        }
        let tier = self.thumb_tier;
        let tex = match self.tree_texs.get(&id) {
            Some(t) => Some(t.clone()),
            None => {
                let t = thumb_tex(ctx, id, tier);
                if let Some(t) = &t {
                    self.tree_texs.insert(id, t.clone());
                }
                t
            }
        };
        match tex {
            Some(t) => {
                ui.add(egui::Image::new(&t).fit_to_exact_size(egui::vec2(18.0, 18.0)));
            }
            // 还没生成缩略图时留同宽的空位,行不会左右跳。
            None => ui.add_space(18.0),
        }
    }

    /// 组头(参考件每个相册上方那行:大图标 + 名称 + 日期,图标偏移照原版排版):
    /// 大号目录图标 + 文件夹名 + 组内最早拍摄日期与张数,右侧挂本组动作按钮。
    ///
    /// `date` 由调用方缓存好传进来(见 `take_folder_groups`):逐帧扫全组找最早日期
    /// 在大图库里是 O(行数) 的白活。
    ///
    /// 放映与邮件复用视图级入口:先把视图范围收窄到本组,再调现成的
    /// `start_slideshow` / 邮件窗口 —— 不必为幻灯片另开一条 id 队列。
    fn album_label(&mut self, ui: &mut egui::Ui, folder: &str, date: &str, count: usize) {
        let mut name = Path::new(folder.trim_end_matches(['\\', '/']))
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| folder.to_string());
        if name.is_empty() {
            name = "(未知位置)".into();
        }
        // 组内全无 EXIF 拍摄时间时 date 是空串:只报张数,不编日期。
        let sub = if date.is_empty() {
            format!("{count} 张")
        } else {
            format!("{date} · {count} 张")
        };
        let muted = crate::theme::p().muted;
        let mut act_play = false;
        let mut act_mail = false;
        let mut act_open = false;
        ui.horizontal(|ui| {
            ui.add(album_icon("folder", 26.0));
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.add(
                    egui::Label::new(egui::RichText::new(&name).size(17.0).strong())
                        .selectable(false),
                );
                ui.add(
                    egui::Label::new(egui::RichText::new(&sub).size(12.0).color(muted))
                        .selectable(false),
                );
            });
            // 参考件把动作按钮钉在组头右侧(槽位序照原版排版,从右往左)。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(tool_button("mail", false))
                    .on_hover_text("以邮件发送本组照片")
                    .clicked()
                {
                    act_mail = true;
                }
                if ui
                    .add(tool_button("folder-open", false))
                    .on_hover_text("在资源管理器中打开该文件夹")
                    .clicked()
                {
                    act_open = true;
                }
                if ui
                    .add(tool_button("play", false))
                    .on_hover_text("放映本组幻灯片")
                    .clicked()
                {
                    act_play = true;
                }
            });
        });
        if act_open && !crate::win::shell_open(Path::new(folder)) {
            self.status = format!("无法在资源管理器中打开:{folder}");
        }
        if act_play || act_mail {
            // 只在点击时反查一次左栏槽位(逐帧扫 folders 列表没必要)。
            if let Some(slot) = self.folders.iter().position(|f| f == folder) {
                self.selected_folder = slot;
                self.enter_dir(folder);
                self.tree_reveal(folder);
            }
            self.set_view(ViewKind::Folder);
            self.folder_show_all = false;
            if act_play {
                // 预览停在别的文件夹时,start_slideshow 会接着放那张不相关的;
                // 不在本组范围内就清掉,让它从本组第一张起步。
                let ids = self.view_ids();
                if self.preview.is_some_and(|p| !ids.contains(&p)) {
                    self.preview = None;
                    self.preview_tex = None;
                }
                self.start_slideshow();
            } else {
                self.email_win = true;
            }
        }
    }

    /// 照片网格(按缩略图档位决定列数与格子大小;选中项加蓝框)。
    ///
    /// P1-9:缩略图档位(Picasa 3 用 8 档,本项目取 4 档常用值)。
    /// 列数按格子大小反推,保证一行铺满可用宽度。
    fn photo_grid(&mut self, ui: &mut egui::Ui, salt: &str, cells: &[usize]) {
        // 显示优先:先把屏幕上排队等纳入的目录消化掉几个(写账 + 放行 + 交给后台优先派生),
        // 再画格子。渲染帧不一次写几十个目录的账,所以有上限。
        self.drain_adopts(MAX_ADOPT_PER_FRAME);
        let tier = self.thumb_tier;
        let cell = tier.size() as f32;
        // 列数:按卡片外框(图片 + 4px 内边距×2)与间距反推,保证一行铺满。
        let gap = 14.0;
        let card_outer = cell + 8.0;
        let avail = ui.available_width();
        let cols = ((avail + gap) / (card_outer + gap))
            .floor()
            .clamp(2.0, 16.0) as usize;
        // 行级虚拟化:视口外的行不建 widget,只按同一高度占位。
        // 行高是确定的(卡片外框 + 该行有说明文字时再加 20px)。
        // 占位记法:每行连它后面的那个间距一起记(行高 + gap),这样后续行
        // 依旧落在原来的位置;若占位段结尾就是整个网格,得多退一次间距,
        // 否则内容总高会比真实值多出一个 gap。
        let clip = ui.clip_rect();
        // 方案 P4-23④:预取带 —— 视口上下各留这么高的一段,里面的行不建 widget,
        // 但会用当帧**剩余**的解码配额把纹理先暖进缓存(滚动时少闪一帧占位块)。
        let band = (card_outer + gap) * PREFETCH_ROWS as f32;
        let ctx = ui.ctx().clone();
        ui.push_id(salt, |ui| {
            ui.spacing_mut().item_spacing.y = gap;
            // 逐行维护虚拟坐标:跳过的行不建 widget,光标不会前移,
            // 若继续用 `ui.cursor()` 判视口,滚动后光标始终停在本网格顶部,
            // 整片网格都会被判成"在视口上方"而不画(滚着滚着就全空)。
            let mut y = ui.cursor().top();
            let mut hidden = 0.0f32;
            for chunk in cells.chunks(cols) {
                let cap = if self.thumb_captions
                    && chunk
                        .iter()
                        .any(|&i| !self.rows[i].caption.trim().is_empty())
                {
                    20.0
                } else {
                    0.0
                };
                let row_h = card_outer + cap;
                if y + row_h < clip.min.y || y > clip.max.y {
                    // 整行在视口外:记进占位高度,这一帧不建行。
                    let near = y + row_h + band >= clip.min.y && y <= clip.max.y + band;
                    hidden += row_h + gap;
                    y += row_h + gap;
                    if near {
                        self.prefetch_row(&ctx, chunk);
                    }
                    continue;
                }
                if hidden > 0.0 {
                    ui.add_space(hidden);
                    hidden = 0.0;
                }
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for &i in chunk {
                        self.photo_cell(ui, i, cell);
                    }
                });
                y += row_h + gap;
            }
            if hidden > 0.0 {
                ui.add_space(hidden - gap);
            }
        });
    }

    /// 预取一行的缩略图纹理(方案 P4-23④)。
    ///
    /// 用的是当帧**剩余**的解码配额:真正可见的格子先吃配额,预取只捡剩下的,
    /// 所以它永远不会把可见格子挤成占位块。快速滚动时配额被限速器压到 0
    /// ([`crate::scroll::decode_budget_for`]),预取自然「丢弃过期请求」——
    /// 队列是当帧同步消费的,不留跨帧的陈旧请求。
    fn prefetch_row(&mut self, ctx: &egui::Context, chunk: &[usize]) {
        for &i in chunk {
            if self.thumb_decode_count >= self.thumb_decode_budget {
                return;
            }
            // 未纳入/已立案的行不吃这一帧的配额,也不 `mark_pending`(那是
            // 「磁盘还没有,交给后台生成」的语义);排队纳入后它们自己会回到正常路径。
            let rich = self.rows[i].rich_state;
            if rich == crate::db::RICH_OUT || rich == crate::db::RICH_BAD {
                continue;
            }
            let id = self.rows[i].id;
            if self.texs.has(id) || self.texs.is_pending(id) {
                continue;
            }
            self.thumb_decode_count += self.thumb_tier.bytes();
            match thumb_tex(ctx, id, self.thumb_tier) {
                Some(t) => self.texs.put(id, t),
                // 磁盘上还没有 → 交给后台 worker 生成(与可见格子同一套语义)。
                None => self.texs.mark_pending(id),
            }
        }
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
        // 加入相簿(参考件 Add to Album;相簿不移动文件;已加入的打勾,点击移除)。
        ui.menu_button("加入相簿…", |ui| {
            let albums = self.store.albums();
            if albums.is_empty() {
                ui.label("(还没有相簿:视图 > 相簿 里新建)");
            }
            let member: Vec<i64> = self
                .store
                .albums_of_photo(self.rows[idx].id)
                .into_iter()
                .map(|(a, _)| a)
                .collect();
            for (aid, name, _) in albums {
                let in_album = member.contains(&aid);
                let label = if in_album {
                    format!("✓ 「{name}」(点击移出)")
                } else {
                    format!("「{name}」")
                };
                if ui.button(label).clicked() {
                    if in_album {
                        let _ = self.store.album_remove_photo(aid, self.rows[idx].id);
                        self.fx_msg = format!("已移出相簿「{name}」");
                    } else {
                        let _ = self.store.album_add_photo(aid, self.rows[idx].id);
                        self.fx_msg = format!("已加入相簿「{name}」");
                    }
                    ui.close();
                }
            }
        });
        if self.view == ViewKind::Albums {
            if let Some(a) = self.sel_album {
                if ui.button("从相簿移除").clicked() {
                    let _ = self.store.album_remove_photo(a, self.rows[idx].id);
                    ui.close();
                }
            }
        }
        ui.separator();
        // 用编辑器打开(参考件 Open File(s) in an Editor)。
        if ui.button("用编辑器打开").clicked() {
            let p = PathBuf::from(&self.rows[idx].path);
            if !crate::win::shell_edit(&p) {
                self.status = format!("无法打开(未关联编辑器):{}", p.display());
            }
            ui.close();
        }
        // 设为桌面壁纸(参考件 Set as Desktop)。
        if ui.button("设为桌面壁纸").clicked() {
            self.set_wallpaper_for(idx);
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
        // 坏文件出口(方案 P3-22,对齐经典图库坏图清单的
        // Hide / Don't Hide):立过案的文件不再每轮重试,这里给两个方向的开关。
        let p = self.rows[idx].path.clone();
        if self.store.is_bad_media(&p) {
            if ui.button("不隐藏此文件(重试缩略图)").clicked() {
                self.status = match self.store.unmark_bad_media(&p) {
                    Ok(()) => format!("已销案,重新排队派生:{p}"),
                    Err(e) => format!("销案失败: {e}"),
                };
                ui.close();
            }
        } else if ui.button("隐藏此文件(不再重试缩略图)").clicked() {
            self.status = match self.store.mark_bad_media(&p, "用户手动隐藏") {
                Ok(()) => format!("已隐藏,不再重试:{p}"),
                Err(e) => format!("隐藏失败: {e}"),
            };
            ui.close();
        }
        // 属性(参考件右键菜单含属性项,置于末项;未开预览时属性窗取选中项)。
        if ui.button("属性…").clicked() {
            let id = self.rows[idx].id;
            self.sel.clear();
            self.sel.insert(id);
            self.show_props = true;
            ui.close();
        }
    }

    fn photo_cell(&mut self, ui: &mut egui::Ui, idx: usize, cell: f32) {
        let id = self.rows[idx].id;
        let star0 = self.rows[idx].star != 0;
        let selected = self.sel.contains(&id);
        // 方案 P4-29:派生状态决定占位文案,也决定要不要去要缩略图。
        // RICH_OUT(未纳入作用域)/ RICH_BAD(已立案失败)这两类在本帧磁盘上还没有
        // 缩略图,不能进「交给后台生成」那条路 —— 那是白跑一趟解码再失败一次,
        // 还会把滚动配额吃掉。(RICH_OUT 只是**这一帧**的状态:下面的排队纳入会让
        // 它在下一帧变成待派生,显示优先。)
        let rich = self.rows[idx].rich_state;
        let no_derive = rich == crate::db::RICH_OUT || rich == crate::db::RICH_BAD;
        // 显示优先(2026-11-05 拍板):屏幕上出现过的目录决定后台派生的先后;
        // 屏幕上出现一张还没纳入的图,它的目录当场排队纳入 —— 用户看到的是图,
        // 不是作用域账本。排队后要自己把下一帧叫回来:站着不滚动时 egui 不重绘,
        // 队列就会空等。
        let dir = self.rows[idx].parent.clone();
        self.note_frame_dir(&dir);
        if rich == crate::db::RICH_OUT && self.note_visible_out(&dir) {
            ui.ctx().request_repaint();
        }
        let tex = if no_derive {
            None
        } else {
            match self.texs.get(id) {
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
                        self.thumb_decode_count += self.thumb_tier.bytes();
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
            }
        };
        let mut star_clicked = false;
        let mut preview_clicked = false;
        let ctrl_click = |ui: &egui::Ui| ui.input(|i| i.modifiers.ctrl || i.modifiers.command);
        // 缩略图说明(参考件 Thumbnail captions 视图开关)。
        let caption: Option<String> = if self.thumb_captions {
            let cap = self.rows[idx].caption.trim();
            if cap.is_empty() {
                None
            } else {
                let max_chars = ((cell / 7.0) as usize).max(6);
                Some(if cap.chars().count() > max_chars {
                    format!("{}…", cap.chars().take(max_chars - 1).collect::<String>())
                } else {
                    cap.to_string()
                })
            }
        } else {
            None
        };
        let pad = 4.0;
        let card = cell + pad * 2.0;
        let cap_h = if caption.is_some() { 20.0 } else { 0.0 };
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(card, card + cap_h), egui::Sense::hover());
        // 先取悬停态再登记图片控件,否则图片会盖住整卡的命中区域。
        let hovered = ui.rect_contains_pointer(rect);
        let card_fill = if hovered {
            crate::theme::p().card_hover
        } else {
            crate::theme::p().card
        };
        ui.painter()
            .rect_filled(rect, crate::theme::CARD_RADIUS, card_fill);
        let img_rect =
            egui::Rect::from_min_size(rect.min + egui::vec2(pad, pad), egui::vec2(cell, cell));
        if let Some(ref t) = tex {
            // 居中裁切成正方形(参考件缩略图网格等宽等高)。
            ui.put(
                img_rect,
                egui::Image::new((t.id(), img_rect.size()))
                    .uv(cover_uv(t.size()))
                    .corner_radius(crate::theme::IMAGE_RADIUS),
            );
        } else {
            ui.painter().rect_filled(
                img_rect,
                crate::theme::IMAGE_RADIUS,
                crate::theme::p().placeholder,
            );
            // 占位文案按派生状态分三种(方案 P4-29):
            // RICH_OUT 是「作用域门挡住了」,**不是失败**,必须写成占位而不是错误;
            // RICH_BAD 才是真的读不出来。小档格子(96px)放不下长句,退成短版,
            // 全称挂在悬停上。
            let ph = match rich {
                crate::db::RICH_OUT if cell >= 100.0 => "未纳入派生作用域",
                crate::db::RICH_OUT => "未纳入范围",
                crate::db::RICH_BAD => "读取失败",
                _ => "载入中…",
            };
            ui.painter().text(
                img_rect.center(),
                egui::Align2::CENTER_CENTER,
                ph,
                egui::FontId::proportional(11.0),
                crate::theme::p().faint,
            );
        }
        // P2-17:视频条目左上角 ▶ 徽标。
        if self.rows[idx].kind == crate::video::kind::MOVIE {
            ui.painter().text(
                img_rect.left_top() + egui::vec2(6.0, 5.0),
                egui::Align2::LEFT_TOP,
                "▶",
                egui::FontId::proportional(15.0),
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 235),
            );
        }
        // 星标悬浮在缩略图右上角(常驻显示已加星,悬停才露出空心星)。
        let hit = ui.interact(rect, egui::Id::new(("thumb-hit", id)), egui::Sense::click());
        // 占位格的悬停给全称与出路;正常格不挂(避免翻格子时到处弹提示)。
        let hit = if no_derive {
            hit.on_hover_text(if rich == crate::db::RICH_BAD {
                "这张照片读取失败(文件损坏或无权限),已记入坏文件台账并隔离,不再重试。\
                 在「工具 → 图库维护 → 坏文件台账…」里可以看明细或整批销案。"
            } else {
                "这张照片所在的目录未纳入派生作用域:图库收了它的名字与路径,\
                 但不生成缩略图、不抽 EXIF —— 这是门控,不是失败。\
                 点一下这张图(或点进它所在的目录)就会开始生成;\
                 排除名单里的目录(系统目录、应用缓存、回收站一类)不会。"
            })
        } else {
            hit
        };
        thumb_star(ui, id, star0, img_rect, hovered, &mut star_clicked);
        let ring = if selected {
            crate::theme::p().accent
        } else if hovered {
            crate::theme::p().stroke_light
        } else {
            egui::Color32::TRANSPARENT
        };
        if ring != egui::Color32::TRANSPARENT {
            let width = if selected { 2.0_f32 } else { 1.0_f32 };
            ui.painter().rect_stroke(
                rect,
                crate::theme::CARD_RADIUS,
                egui::Stroke::new(width, ring),
                egui::StrokeKind::Inside,
            );
        }
        if let Some(cap) = &caption {
            ui.painter().text(
                egui::pos2(rect.center().x, rect.bottom() - 5.0),
                egui::Align2::CENTER_BOTTOM,
                cap,
                egui::FontId::proportional(11.0),
                if selected {
                    crate::theme::p().text
                } else {
                    crate::theme::p().muted
                },
            );
        }
        if hit.clicked() {
            // 点到这张图 = 用户已经明确表态「我要看它」:作用域外的目录当场纳入。
            // 但**排除名单里的目录不写账本**(系统目录 / 应用缓存 / 回收站),这种情况
            // 给一句准话,而不是让人对着永远不亮的格子反复点。
            // RICH_BAD 不走这条路 —— 那是已立案的读取失败,再点一次也不会好。
            if self.rows[idx].rich_state == crate::db::RICH_OUT {
                let dir = self.rows[idx].parent.clone();
                if !self.enter_dir(&dir) {
                    self.status = format!(
                        "{dir} 属于排除名单(系统目录、应用缓存、回收站一类),图库不为它生成缩略图;\
                         要收进来请在「工具 → 文件夹管理器」里调整「扫描时排除的目录」。"
                    );
                }
            }
            if ctrl_click(ui) {
                if !self.sel.remove(&id) {
                    self.sel.insert(id);
                }
            } else {
                preview_clicked = true;
            }
        }
        hit.context_menu(|ui| {
            self.photo_ctx_menu(
                ui,
                idx,
                star0,
                selected,
                &mut star_clicked,
                &mut preview_clicked,
            )
        });
        if star_clicked {
            self.toggle_star(id);
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
                self.preview_failed = None;
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
        // 原文件已在外部消失:给出明确结论,不再展示内存里的旧纹理,
        // 也不再请求后台解码(worker 对缺失文件只会返回失败)。
        // 移出图库排到下一帧统一做(此处可能位于预览窗闭包内)。
        if !std::path::Path::new(&self.rows[idx].path).exists() {
            self.preview_missing = Some(id);
            let path = self.rows[idx].path.clone();
            self.defer_missing(id, path);
            self.preview_tex = None;
            return None;
        }
        self.preview_missing = None;
        // 后台解码已判失败:停在"加载失败",不再每帧重发请求(否则 worker 会被
        // 不支持格式的文件反复唤醒);换图后由 step/打开处清空重试。
        if self.preview_failed == Some(id) {
            return None;
        }
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

    /// 放大态纹理:全图缓存 + 当前效果栈合成,按(照片 id, 栈签名)缓存。
    ///
    /// 只在效果栈变化时重算一次全图合成(大图一次几百毫秒),平移/缩放重绘
    /// 直接复用缓存纹理;平移缩放的几何与分辨率无关(等比),不需重算。
    fn zoom_tex_get(&mut self, ctx: &egui::Context, id: i64) -> Option<egui::TextureHandle> {
        if self.preview_full.as_ref()?.0 != id {
            return None;
        }
        let stack = self.fx_preview_stack(id);
        let sig = format!("full|{}", stack.serialize());
        if let Some((kid, ksig, tex)) = &self.zoom_tex {
            if *kid == id && ksig == &sig {
                return Some(tex.clone());
            }
        }
        let img = &self.preview_full.as_ref().unwrap().1;
        let mut rgba = img.clone();
        crate::effects::apply_stack(&mut rgba, &stack);
        let size = [rgba.width() as usize, rgba.height() as usize];
        let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
        let tex = ctx.load_texture(
            format!("preview-{id}-full"),
            color,
            egui::TextureOptions::default(),
        );
        self.zoom_tex = Some((id, sig, tex.clone()));
        Some(tex)
    }

    /// 预览叠框数据(OCR/人脸框图上标注)。按照片 id 缓存。
    /// 返回纹理相对坐标 (0..1) 的框 + 标签文字。
    /// 坐标链路:OCR Txt / FaceHit 均为**未旋转原图像素** → 经 EXIF orientation
    /// 映射到显示方向 → 归一化(纹理与旋转后图同比例,resize 不改变归一化坐标)。
    fn preview_boxes_for(&mut self, id: i64) -> Vec<((f32, f32, f32, f32), String)> {
        if self.preview_boxes.as_ref().map(|(k, _)| *k) == Some(id) {
            return self
                .preview_boxes
                .as_ref()
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
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
    (
        (nx0 / rw, ny0 / rh, (nx1 + 1.0) / rw, (ny1 + 1.0) / rh),
        rw,
        rh,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 导入页清单三件事:
    /// 1) 子目录排最前(可下探),txt 之类不可入库的文件不列;
    /// 2) 已入库的文件带 `in_library` 标记,行上灰显;
    /// 3) 文件按名排序。
    #[test]
    fn import_dir_lists_dirs_first_and_marks_indexed() {
        let base = std::env::temp_dir().join("maliang-import-case");
        let dir = base.join("cards");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("b.jpg"), b"x").unwrap();
        std::fs::write(dir.join("a.jpg"), b"y").unwrap();
        std::fs::write(dir.join("c.txt"), b"z").unwrap();
        let mut indexed = HashSet::new();
        indexed.insert(dir.join("b.jpg").to_string_lossy().into_owned());
        let rows = list_import_dir(&dir, &indexed);
        assert_eq!(rows.len(), 3, "子目录 + 两张图;txt 不列: {rows:?}");
        assert!(rows[0].is_dir, "子目录排最前: {rows:?}");
        assert_eq!(rows[1].name, "a.jpg", "文件按名排");
        assert!(rows[2].in_library, "已在库的标出来");
        assert!(!rows[1].size_s.is_empty(), "大小列要有值");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 目录树两件事必须成立,左栏的真递归树全靠它们:
    /// 1) 中间层目录(本身没有照片)也成节点,并且借到子树里的一张代表图;
    /// 2) 只有装了照片的目录带 `folder_index`,即树上只有它可选。
    #[test]
    fn folder_tree_adds_intermediate_dirs() {
        fn walk(nodes: &[TreeNode], id: usize, out: &mut Vec<String>) {
            out.push(nodes[id].path.clone());
            for &k in &nodes[id].kids {
                walk(nodes, k, out);
            }
        }
        // 用系统临时目录起头,两种平台的祖先链形状一致(单一根)。
        let base = std::env::temp_dir().join("maliang-tree-case");
        let folders: Vec<String> = [
            base.join("2024").join("jan"),
            base.join("2024").join("feb"),
            base.join("solo"),
        ]
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
        let mut rep: HashMap<String, i64> = HashMap::new();
        for (fi, f) in folders.iter().enumerate() {
            rep.insert(f.clone(), 11 + fi as i64);
        }
        let (nodes, roots) = build_folder_tree(&folders, &rep);

        let uniq: HashSet<&str> = nodes.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(uniq.len(), nodes.len(), "同一路径不应出现两个节点");
        assert_eq!(roots.len(), 1, "同一盘/同一根应收敛成一个根");

        let mut seen = Vec::new();
        walk(&nodes, roots[0], &mut seen);
        assert_eq!(seen.len(), nodes.len(), "所有节点都应能从根走到");
        for f in &folders {
            assert!(seen.contains(f), "{f} 应在树上");
        }

        let mid_path = base.join("2024").to_string_lossy().into_owned();
        let mid = nodes
            .iter()
            .find(|n| n.path == mid_path)
            .expect("中间层目录 2024 应成节点");
        assert!(mid.folder_index.is_none(), "本身没有照片的目录不该是可选项");
        assert_eq!(mid.photo_id, Some(11), "应借到子树里的第一张代表图");
        assert_eq!(mid.kids.len(), 2);

        for (fi, f) in folders.iter().enumerate() {
            let n = nodes.iter().find(|n| &n.path == f).expect("目录自身节点");
            assert_eq!(n.folder_index, Some(fi), "目录应记回它在 folders 里的下标");
            assert_eq!(n.photo_id, Some(11 + fi as i64));
        }
    }

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
        assert!(
            (r2.2 - r2.0 - 0.6).abs() < 0.02,
            "实际宽占比 {}",
            r2.2 - r2.0
        );
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
        // 显示优先(2026-11-05 拍板):上一帧真正画在屏幕上的目录整批发给后台 worker
        // 当第一优先 —— 用户看哪儿,缩略图就先补哪儿。在帧首发是为了拿得到「上一帧
        // 的完整清单」:网格分多个 section 画,帧尾没有自然的收口点。
        if !self.frame_dirs.is_empty() {
            if let Some(w) = &self.thumb_bg {
                w.publish_visible(self.frame_dirs.clone());
            }
        }
        self.frame_dirs.clear();
        self.frame_dirs_seen.clear();
        // 索引服务链路事件(概况 / 掉线 / 装卸回执)在顶层泵:人在哪个页面
        // 都收得到,不要求停在快速查找页。
        self.pump_link_events();
        // 即输即搜的回流(命中结果 / 搜索行缩略图)每帧非阻塞泵。
        self.pump_search(ctx);
        // 服务在途(装/卸等 UAC)或正在建库时自刷一帧,状态行与指示灯才动。
        if self.svc_busy || self.link.building > 0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
        // 预览过程发现原文件已消失 → 先在 UI 构建前移出图库,
        // 保证本帧后续按行索引取数据时不会读到已删行。
        if !self.pending_missing.is_empty() {
            let now = std::time::Instant::now();
            let mut due: Vec<(i64, String)> = Vec::new();
            self.pending_missing.retain(|(id, p, t)| {
                if now.duration_since(*t) >= MISSING_CONFIRM {
                    due.push((*id, p.clone()));
                    false
                } else {
                    true
                }
            });
            if !due.is_empty() {
                self.drop_missing(due);
            }
            if !self.pending_missing.is_empty() {
                ctx.request_repaint_after(std::time::Duration::from_millis(250));
            }
        }
        // 幻灯片放映:全屏接管整帧,跳过常规面板。
        if self.slideshow {
            self.slideshow_ui(ctx);
            return;
        }
        // 轮询扫描结果
        if let Some(rx) = &self.scan_rx {
            if let Ok(r) = rx.try_recv() {
                self.scan_rx = None;
                self.scan_ctrl = None;
                self.status = match r.err {
                    Some(e) => format!("{}{}失败: {e}", self.scan_verb, r.scope),
                    None if r.cancelled => format!(
                        "{}{}已取消:已索引 {} 张(已处理 {} 个文件)",
                        self.scan_verb, r.scope, r.indexed, r.scanned
                    ),
                    None => {
                        let mut msg = format!(
                            "{}{}完成:发现 {} 个文件,索引 {} 张(未变跳过 {} 张,排除目录 {} 个)",
                            self.scan_verb, r.scope, r.scanned, r.indexed, r.unchanged, r.excluded_dirs
                        );
                        if r.removed > 0 {
                            msg.push_str(&format!(";{} 个已消失文件移出图库", r.removed));
                        }
                        msg
                    }
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
        // 缩略图后台 worker:完成一批 → 清占位标记 + 刷新缓存计数 + 触发重绘
        if let Some(w) = &self.thumb_bg {
            let mut got: Vec<i64> = Vec::new();
            while let Ok(ids) = w.done.try_recv() {
                got.extend(ids);
            }
            if !got.is_empty() {
                self.texs.clear_pending(&got);
                // 库里刚多了一批已生成记录,顺手刷新;不写 status ——
                // 逐批覆盖会让状态栏文字一直跳,进度看「N缓存 x/y」就够了。
                self.refresh_tier_count();
                ctx.request_repaint();
            }
        }
        // P1-10:预览后台完成信号 → 清掉预览纹理缓存,让下一帧重新加载。
        if let Some(w) = &self.preview_bg {
            let mut got: Vec<i64> = Vec::new();
            let mut failed = 0usize;
            let mut missing: Vec<(i64, String)> = Vec::new();
            while let Ok((id, res)) = w.done.try_recv() {
                match res {
                    Ok(_) => got.push(id),
                    Err(_) => {
                        failed += 1;
                        // 终止"预览生成中":正在看的这张解码失败 → 标记失败。
                        if self.preview == Some(id) {
                            self.preview_failed = Some(id);
                            self.preview_tex = None;
                        }
                        // 解码失败里只有"文件不存在"可以做确定结论;
                        // 其余(格式不支持等)保持原提示,不能误删索引。
                        if let Some(&idx) = self.id_row.get(&id) {
                            let p = self.rows[idx].path.clone();
                            if !std::path::Path::new(&p).exists() {
                                missing.push((id, p));
                            }
                        }
                    }
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
            if !missing.is_empty() {
                for (id, p) in missing {
                    self.defer_missing(id, p);
                }
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
                    format!(
                        "导出完成 {ok} 张,失败 {} 个: {}",
                        errs.len(),
                        first.join("; ")
                    )
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
        // 壁纸设置结果(后台线程解码原图并落盘临时 JPEG)。
        if let Some(rx) = &self.wallpaper_rx {
            if let Ok(res) = rx.try_recv() {
                self.wallpaper_rx = None;
                self.status = match res {
                    Ok(msg) => msg,
                    Err(e) => format!("设为壁纸失败: {e}"),
                };
            }
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
            let msgs: Vec<FaceScan> = rx.try_iter().collect();
            let mut finished: Option<Result<(usize, usize), String>> = None;
            for m in msgs {
                match m {
                    FaceScan::Progress(done, total) => self.face_prog = (done, total),
                    FaceScan::Done(res) => finished = Some(res),
                }
            }
            if let Some(res) = finished {
                self.face_rx = None;
                self.face_prog = (0, 0);
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
                    Ok((n, ok)) => self.status = format!("语义索引完成:{n} 张照片,嵌入 {ok} 张"),
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
            let ms = if self.scan_busy.load(Ordering::Relaxed) {
                100
            } else {
                300
            };
            ctx.request_repaint_after(std::time::Duration::from_millis(ms));
        }
        // 键盘快捷键(参照 Picasa;有文本输入焦点时不拦截)
        if !ctx.wants_keyboard_input() {
            let (
                sel_all,
                save,
                esc,
                next,
                prev,
                page_up,
                page_down,
                home,
                end,
                rot_cw,
                rot_ccw,
                nav_back_k,
                nav_fwd_k,
            ) = ctx.input(|i| {
                let alt = i.modifiers.alt;
                (
                    i.modifiers.command && i.key_pressed(egui::Key::A),
                    i.key_pressed(egui::Key::F2),
                    i.key_pressed(egui::Key::Escape),
                    !alt && i.key_pressed(egui::Key::ArrowRight),
                    !alt && i.key_pressed(egui::Key::ArrowLeft),
                    i.key_pressed(egui::Key::PageUp),
                    i.key_pressed(egui::Key::PageDown),
                    i.key_pressed(egui::Key::Home),
                    i.key_pressed(egui::Key::End),
                    i.modifiers.command && !i.modifiers.shift && i.key_pressed(egui::Key::R),
                    i.modifiers.command && i.modifiers.shift && i.key_pressed(egui::Key::R),
                    alt && i.key_pressed(egui::Key::ArrowLeft),
                    alt && i.key_pressed(egui::Key::ArrowRight),
                )
            });
            // 页面前后导航(Alt+← / Alt+→)。
            if nav_back_k {
                self.nav_back();
            }
            if nav_fwd_k {
                self.nav_forward();
            }
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
                    self.status = format!("正在取消{}…", self.scan_verb);
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
                if rot_cw || rot_ccw {
                    if let Some(id) = self.preview {
                        // 自定绑定:原版旋转命令无键盘快捷键(主循环加速表恒空,
                        // 旋转命令名仅被菜单构建器引用);按 Windows 图库惯例补
                        // Ctrl+R / Ctrl+Shift+R,只在预览窗生效。
                        self.preview_rotate(id, if rot_cw { 1 } else { 3 });
                    }
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
        egui::TopBottomPanel::top("menu")
            .frame(crate::theme::bar_frame())
            .show(ctx, |ui| {
                egui::MenuBar::new().ui(ui, |ui| {
                    ui.label(
                        egui::RichText::new("马良")
                            .size(15.0)
                            .strong()
                            .color(crate::theme::p().accent),
                    );
                    ui.label(
                        egui::RichText::new("MALIANG")
                            .size(9.0)
                            .color(crate::theme::p().faint),
                    );
                    ui.add_space(2.0);
                    ui.separator();
                    // —— 文件 ——
                    ui.menu_button("文件(F)", |ui| {
                        // 属性(参考件 File > Properties 窗)。
                        if ui
                            .add_enabled(
                                self.preview.is_some() || !self.sel.is_empty(),
                                egui::Button::new("属性…"),
                            )
                            .clicked()
                        {
                            self.show_props = true;
                            ui.close();
                        }
                        ui.separator();
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
                            // 导入页:整页面板(选源 → 看清单 → 入库 → 返回)。
                            if ui.button("导入页(选源入库)…").clicked() {
                                self.go_page(PageKind::Import);
                                ui.close();
                            }
                            // 直接挑文件夹的一次性入口(不开页面)。
                            if ui.button("添加文件夹到图库…").clicked() {
                                self.add_folder_to_library();
                                ui.close();
                            }
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
                                .add_enabled(
                                    self.export_rx.is_none(),
                                    egui::Button::new("导出到文件夹…"),
                                )
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
                            ui.small("网络上传:原版依赖已停运的云相册服务,目前无可用对等实现。");
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
                        let starred: Vec<i64> = self
                            .rows
                            .iter()
                            .filter(|r| r.star != 0)
                            .map(|r| r.id)
                            .collect();
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
                            self.set_view(ViewKind::Timeline);
                            ui.close();
                        }
                        // 相簿(参考件 Album 列表)。
                        if ui
                            .selectable_label(self.view == ViewKind::Albums, "相簿")
                            .clicked()
                        {
                            self.set_view(ViewKind::Albums);
                            ui.close();
                        }
                        #[cfg(feature = "ai")]
                        if ui
                            .selectable_label(self.view == ViewKind::Faces, "人物")
                            .clicked()
                        {
                            self.set_view(ViewKind::Faces);
                            ui.close();
                        }
                        #[cfg(feature = "ai")]
                        if ui
                            .selectable_label(self.view == ViewKind::Semantic, "语义搜索")
                            .clicked()
                        {
                            self.set_view(ViewKind::Semantic);
                            ui.close();
                        }
                        if ui
                            .selectable_label(self.view == ViewKind::Search, "快速查找")
                            .clicked()
                        {
                            self.set_view(ViewKind::Search);
                            #[cfg(windows)]
                            self.svc_nudge_on_search_entry();
                            ui.close();
                        }
                        if ui
                            .selectable_label(self.view == ViewKind::Tags, "标签")
                            .clicked()
                        {
                            self.set_view(ViewKind::Tags);
                            ui.close();
                        }
                        if ui
                            .selectable_label(self.view == ViewKind::Folder, "文件夹")
                            .clicked()
                        {
                            self.set_view(ViewKind::Folder);
                            ui.close();
                        }
                        ui.separator();
                        // 幻灯片放映(参考件 View > Slideshow:当前视图逐张全屏)。
                        if ui.button("幻灯片放映").clicked() {
                            self.start_slideshow();
                            ui.close();
                        }
                        // 缩略图说明(参考件 Thumbnail captions)。
                        if ui
                            .checkbox(&mut self.thumb_captions, "缩略图说明")
                            .changed()
                        {
                            let _ = self.store.set_setting(
                                "thumb_captions",
                                if self.thumb_captions { "1" } else { "0" },
                            );
                        }
                        // 文件夹视图形态(树形 / 平铺)。
                        ui.menu_button("文件夹视图", |ui| {
                            if ui
                                .selectable_label(!self.folder_tree, "平铺文件夹视图")
                                .clicked()
                            {
                                self.set_folder_tree(false);
                                ui.close();
                            }
                            if ui.selectable_label(self.folder_tree, "树形视图").clicked() {
                                self.set_folder_tree(true);
                                ui.close();
                            }
                        });
                        ui.separator();
                        // 四种排序是同一层菜单项(不套子菜单),顺序沿用经典布局。
                        for s in timeline::SortBy::ALL {
                            if ui.selectable_label(self.sort_by == s, s.label()).clicked() {
                                self.sort_by = s;
                                self.apply_view_state();
                                ui.close();
                            }
                        }
                        if ui.selectable_label(self.sort_rev, "反向排序").clicked() {
                            self.sort_rev = !self.sort_rev;
                            self.apply_view_state();
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
                        // 选项(参考件 Tools > Options,分页设置窗)。
                        if ui.button("选项…").clicked() {
                            self.show_options = true;
                            ui.close();
                        }
                        ui.separator();
                        #[cfg(feature = "ai")]
                        {
                            if ui
                                .add_enabled(
                                    self.face_rx.is_none(),
                                    egui::Button::new("人脸识别扫描…"),
                                )
                                .clicked()
                            {
                                self.set_view(ViewKind::Faces);
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
                                self.set_view(ViewKind::Semantic);
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
                            self.set_view(ViewKind::Duplicates);
                            ui.close();
                        }
                        ui.separator();
                        ui.menu_button("图库维护", |ui| {
                            if ui
                                .add_enabled(
                                    self.maint_rx.is_none(),
                                    egui::Button::new("备份索引到…"),
                                )
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
                                .add_enabled(
                                    self.maint_rx.is_none(),
                                    egui::Button::new("清理已缺失文件的索引"),
                                )
                                .clicked()
                            {
                                self.clean_missing();
                                ui.close();
                            }
                            // 方案 P3-21:目录级失效的用户出口。
                            if ui.button("重建全部缩略图").clicked() {
                                self.force_thumb_update();
                                ui.close();
                            }
                            // 方案 P3-22:坏文件台账(整批销案在这里)。
                            if ui.button("坏文件台账…").clicked() {
                                self.bad_media_rows = self.store.bad_media();
                                self.show_bad_media = true;
                                ui.close();
                            }
                            if ui
                                .add_enabled(
                                    self.maint_rx.is_none(),
                                    egui::Button::new("从 XMP 导入标签…"),
                                )
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

        // ===== 工具栏:菜单下面一排常用操作,一键直达 =====
        egui::TopBottomPanel::top("toolbar")
            .frame(crate::theme::bar_frame())
            .show(ctx, |ui| self.toolbar_ui(ctx, ui));

        // ===== 全局快速查找:独占一行,输入框拉通整行,右侧档位 =====
        // 即输即搜:每敲一个字就检索,越输越准(后台线程合并连击,UI 不卡);
        // 清空则退出查找页回时间线。
        egui::TopBottomPanel::top("gsearch")
            .frame(crate::theme::bar_frame())
            .show(ctx, |ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    egui::ComboBox::from_id_salt("top-name-class")
                        .width(84.0)
                        .selected_text(self.name_class.label())
                        .show_ui(ui, |ui| {
                            for c in crate::nameidx::FileClass::all() {
                                if ui
                                    .selectable_label(self.name_class == c, c.label())
                                    .clicked()
                                {
                                    self.name_class = c;
                                    // 换档即重查(缓存未命中时走后台线程)。
                                    if !self.name_query.trim().is_empty() {
                                        self.dispatch_name_search();
                                    }
                                }
                            }
                        });
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.name_query)
                            .hint_text("全盘快速查找:文件名 / 文件夹名片段,即输即搜…")
                            .desired_width(ui.available_width() - 6.0),
                    );
                    if resp.changed() {
                        if self.name_query.trim().is_empty() {
                            self.dispatch_name_search();
                            if self.view == ViewKind::Search {
                                self.set_view(ViewKind::Timeline);
                            }
                        } else {
                            self.set_view(ViewKind::Search);
                            self.dispatch_name_search();
                            #[cfg(windows)]
                            self.svc_nudge_on_search_entry();
                        }
                    }
                });
            });

        // ===== 底部状态栏:先于左右侧面板添加 → 拉通整个窗口宽度,
        // 左右面板最低只到状态栏上沿 =====
        egui::TopBottomPanel::bottom("status")
            .frame(crate::theme::bar_frame())
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(format!("共 {} 张", self.rows.len()))
                        // A9:图库视图是 L2(SQLite)的结果,与顶部整条快速查找(L1
                        // 内存文件名索引)不是同一条路 —— 标出来,免得两处张数对不上时
                        // 用户以为是丢图。
                        .on_hover_text(
                            "来源 L2:图库数据库(SQLite photos 表)。\
                             星级、人物、视频、地理标记等条件过滤都在这一层查;\
                             顶部的全盘快速查找走的是另一条路(L1 内存文件名索引)。",
                        );
                    // 过滤开着时必须说清"这是过滤后的张数",否则看起来像丢图。
                    if !self.filters.is_none() {
                        ui.label("(视图过滤中)");
                        if ui.small_button("清除过滤").clicked() {
                            self.filters = Default::default();
                            self.apply_view_state();
                        }
                    }
                    if !self.sel.is_empty() {
                        ui.label(format!("已选 {} 张", self.sel.len()));
                    }
                    ui.separator();
                    ui.label(&self.status);
                    // P1-9:当前档位的缓存进度(已生成 / 总数)。
                    // 计数只在切档 / 批次回报 / 重建图库时刷新,不逐帧查库。
                    let cached = self.tier_cached;
                    let total = self.rows.len();
                    if cached < total {
                        ui.separator();
                        ui.label(format!("{}缓存 {cached}/{total}", self.thumb_tier.label()));
                    }
                    // P1-8:进度条 + 取消按钮(百分比照原版说法,明细挂悬停;
                    // 扫描与导入共用通道,动词跟着任务走)。
                    if let Some(ctrl) = &self.scan_ctrl {
                        let (done, total) = ctrl.counts();
                        let frac = ctrl.fraction();
                        let verb = self.scan_verb;
                        ui.separator();
                        let pct = (frac * 100.0).round() as i32;
                        ui.label(format!("正在{verb}… {pct}%"))
                            .on_hover_text(format!("{done}/{total} 张已完成"));
                        ui.add(egui::ProgressBar::new(frac).desired_width(140.0));
                        if ui.button(format!("取消{verb}")).clicked() {
                            ctrl.cancel();
                            self.status = format!("正在取消{verb}{}…", self.scan_scope);
                        }
                    }
                    // 人脸扫描进度(分母 = 本批待分析张数;逐张回报,不会卡在 0%)。
                    #[cfg(feature = "ai")]
                    if self.face_rx.is_some() {
                        let (d, t) = self.face_prog;
                        let frac = if t > 0 { d as f32 / t as f32 } else { 0.0 };
                        ui.separator();
                        ui.label(format!("正在扫描人物… {}%", (frac * 100.0).round() as i32))
                            .on_hover_text(format!("本批 {d}/{t} 张"));
                        ui.add(egui::ProgressBar::new(frac).desired_width(140.0));
                    }
                });
            });

        // ===== 右栏抽屉:可伸缩,平时收起,工具栏「属性面板」或面板头「»」收展;
        // 属性 / 标签 / 人物 / 地点 四页签共用同一张目标照片 =====
        if self.drawer_open && self.page == PageKind::Library {
            self.drawer_ui(ctx);
        }

        // 右侧面板:效果系统
        if self.fx_panel && self.page == PageKind::Library {
            egui::SidePanel::right("fx")
                .resizable(true)
                .default_width(300.0)
                .width_range(260.0..=460.0)
                .frame(crate::theme::side_frame())
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        self.fx_panel_ui(ui);
                    });
                });
        }

        // 左侧导航面板(参考件左栏分类列表:常驻,内容随选中项切换)。
        #[cfg(feature = "ai")]
        if self.view == ViewKind::Faces && self.face_people.is_empty() && self.face_rx.is_none() {
            self.refresh_people_and_suggestions();
        }
        egui::SidePanel::left("nav")
            .resizable(true)
            .default_width(232.0)
            .width_range(196.0..=360.0)
            .frame(crate::theme::side_frame())
            .show(ctx, |ui| {
                // 导入页:左栏换成源选择(参考件的获取面板同样换掉左栏,
                // 右栏抽屉/效果面板也一并收起,见对应开关)。
                if self.page == PageKind::Import {
                    self.import_sources_ui(ui);
                    return;
                }
                crate::theme::section_title(ui, "库");
                let mut navs: Vec<(&str, ViewKind)> = vec![
                    ("最近更新", ViewKind::Timeline),
                    ("相簿", ViewKind::Albums),
                    ("文件夹", ViewKind::Folder),
                ];
                #[cfg(feature = "ai")]
                navs.push(("人物", ViewKind::Faces));
                navs.push(("标签", ViewKind::Tags));
                #[cfg(feature = "ai")]
                navs.push(("语义搜索", ViewKind::Semantic));
                navs.push(("重复照片", ViewKind::Duplicates));
                navs.push(("快速查找", ViewKind::Search));
                for (label, v) in navs {
                    let active = self.view == v;
                    let text = if active {
                        egui::RichText::new(label)
                            .color(crate::theme::p().accent_strong)
                            .strong()
                    } else {
                        egui::RichText::new(label).color(crate::theme::p().text)
                    };
                    if ui.selectable_label(active, text).clicked() && !active {
                        self.set_view(v);
                        // 快速查找:服务连不上就顺势引导启用(意向未定时先弹说明窗)。
                        #[cfg(windows)]
                        if v == ViewKind::Search {
                            self.svc_nudge_on_search_entry();
                        }
                    }
                }
                ui.add_space(6.0);
                ui.separator();
                match self.view {
                    ViewKind::Folder => {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("文件夹")
                                    .size(12.0)
                                    .color(crate::theme::p().muted)
                                    .strong(),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .small_button("全部文件夹")
                                        .on_hover_text("中间栏一次铺开所有文件夹(每组一个相册头)")
                                        .clicked()
                                    {
                                        self.folder_show_all = true;
                                    }
                                },
                            );
                        });
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            if self.folder_tree {
                                // 真递归目录树:每层缩进,三角展开/收起;
                                // 只有装了照片的目录才是可选项(点父级 = 展开)。
                                self.folder_tree_ui(ui, ctx);
                            } else {
                                // 扁平文件夹视图(参考件左栏的平铺列表)。
                                for (i, f) in self.folders.clone().iter().enumerate() {
                                    let on = !self.folder_show_all && self.selected_folder == i;
                                    if ui.selectable_label(on, f).clicked() {
                                        self.selected_folder = i;
                                        self.folder_show_all = false;
                                        // 平铺列表同样是「进目录」:树形那条点完会补派生账,
                                        // 这里漏掉就让两种文件夹视图行为不一致。
                                        self.enter_dir(f);
                                        self.tree_reveal(f);
                                    }
                                }
                            }
                        });
                    }
                    // 相簿(参考件 Album 列表;相簿是照片 id 的命名集合,不动文件)。
                    ViewKind::Albums => {
                        crate::theme::section_title(ui, "相簿");
                        let albums = self.store.albums();
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.album_input)
                                    .hint_text("新相簿名称"),
                            );
                            if ui.button("新建").clicked() && !self.album_input.trim().is_empty()
                            {
                                match self.store.album_create(self.album_input.trim()) {
                                    Ok(aid) => {
                                        self.sel_album = Some(aid);
                                        self.album_input.clear();
                                    }
                                    Err(e) => self.fx_msg = format!("新建相簿失败: {e}"),
                                }
                            }
                        });
                        ui.separator();
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            for (aid, name, n) in albums {
                                if ui
                                    .selectable_label(
                                        self.sel_album == Some(aid),
                                        format!("{name} · {n} 张"),
                                    )
                                    .clicked()
                                {
                                    self.sel_album = Some(aid);
                                }
                            }
                        });
                        if let Some(a) = self.sel_album {
                            ui.separator();
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.album_input)
                                        .hint_text("重命名为"),
                                );
                                if ui.button("重命名").clicked()
                                    && !self.album_input.trim().is_empty()
                                {
                                    if let Err(e) =
                                        self.store.album_rename(a, self.album_input.trim())
                                    {
                                        self.fx_msg = format!("重命名失败: {e}");
                                    } else {
                                        self.album_input.clear();
                                    }
                                }
                            });
                            if ui.button("删除当前相簿").clicked() {
                                let _ = self.store.album_delete(a);
                                self.sel_album = None;
                            }
                        }
                    }
                    ViewKind::Tags => {
                        crate::theme::section_title(ui, "标签");
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
                                                if Some(cval) == tag_color(t.color.as_deref()) {
                                                    "● "
                                                } else {
                                                    "○ "
                                                };
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
                    }
                    #[cfg(feature = "ai")]
                    ViewKind::Faces => {
                        crate::theme::section_title(ui, "人脸簇");
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
                                    ui.label(format!(
                                        "簇 {} → {} ({:.2} · {} 张)",
                                        s.cluster, s.name, s.score, s.count
                                    ));
                                    if ui.small_button("应用").clicked() {
                                        let _ = crate::faces::rename_cluster(
                                            &self.store,
                                            s.cluster,
                                            &s.name,
                                        );
                                        self.refresh_people_and_suggestions();
                                    }
                                });
                            }
                        }
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            for p in self.sorted_people() {
                                let display = p
                                    .person
                                    .clone()
                                    .unwrap_or_else(|| format!("簇 {}", p.cluster));
                                let label = format!(
                                    "{display}  ·  {} 张  · 质量 {:.2}",
                                    p.count, p.quality
                                );
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
                                    if ui
                                        .selectable_label(self.sel_cluster == Some(cid), label)
                                        .clicked()
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
                    }
                    #[cfg(feature = "ai")]
                    ViewKind::Semantic => {
                        crate::theme::section_title(ui, "语义搜索");
                        let resp = ui.text_edit_singleline(&mut self.input_query);
                        let enter =
                            resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if (enter || ui.button("搜索").clicked()) && self.clip_search_rx.is_none()
                        {
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
                            ui.selectable_value(&mut self.clip_lang, crate::clip::Lang::En, "英文");
                            ui.selectable_value(&mut self.clip_lang, crate::clip::Lang::Zh, "中文");
                        });
                        // 模型目录里实际放了哪些 .onnx:一个都没有就直说,
                        // 免得用户点了「语义索引」才在后台线程里撞到"模型缺失"。
                        let models = crate::ai::available_models();
                        if models.is_empty() {
                            ui.small("模型目录还没有 .onnx 模型文件,请先放入后再索引");
                            ui.small(
                                crate::ai::models_dir()
                                    .map(|d| format!("模型目录:{}", d.display()))
                                    .unwrap_or_default(),
                            );
                        } else {
                            ui.small(format!("已装模型:{}", models.join("、")));
                        }
                        let indexed = crate::clip::clip_count_lang(&self.store, self.clip_lang);
                        let total_zh =
                            crate::clip::clip_count_lang(&self.store, crate::clip::Lang::Zh);
                        let total_en =
                            crate::clip::clip_count_lang(&self.store, crate::clip::Lang::En);
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
                    }
                    ViewKind::Timeline | ViewKind::Duplicates | ViewKind::Search => {
                        crate::theme::section_title(ui, "当前视图");
                        ui.label(format!("{} 张", self.view_ids().len()));
                    }
                    #[cfg(not(feature = "ai"))]
                    ViewKind::Faces | ViewKind::Semantic => {}
                }
            });

        // 快速查找:结果形态(任意文件/文件夹列表)与照片网格差异大,自绘面板。
        if self.view == ViewKind::Search {
            self.search_ui(ctx);
            return;
        }

        // 导入页:中央区换成导入面板(清单 + 动作),图库布局不再画。
        if self.page == PageKind::Import {
            self.import_page_ui(ctx);
            return;
        }

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
                            ui.add_space(6.0);
                            // 日期分组头(参考件时间线用弱化的小标题,不与缩略图抢视线)。
                            ui.label(
                                egui::RichText::new(label)
                                    .size(13.0)
                                    .color(crate::theme::p().muted)
                                    .strong(),
                            );
                            ui.add_space(4.0);
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
                                .checkbox(
                                    &mut self.dup_near,
                                    format!("包含近似重复(hamming≤{NEAR_DUP_DIST})"),
                                )
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
                                if !crate::ai::face::Faces::models_present(
                                    crate::faces::current_algo(&self.store),
                                ) {
                                    let (det_f, rec_f) =
                                        crate::faces::current_algo(&self.store).files();
                                    ui.label(format!(
                                        "还没有人脸模型,无法识别:请将 {det_f} 与 {rec_f} \
                                         放入模型目录(获取方式见 README):"
                                    ));
                                    ui.small(
                                        crate::ai::models_dir()
                                            .map(|d| d.display().to_string())
                                            .unwrap_or_default(),
                                    );
                                } else {
                                    ui.label(
                                        "左侧选择一个人脸簇查看照片;识别用 工具 > 人脸识别扫描。",
                                    );
                                }
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
                            let ids =
                                timeline::photos_by_tag(&self.store, &tag).unwrap_or_default();
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
                                ui.heading(format!(
                                    "语义结果 {} 张(查询:{q})",
                                    self.clip_results.len()
                                ));
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
                        if self.folder_show_all {
                            // 平铺图库(参考件):一次铺开所有文件夹,
                            // 每个文件夹一个相册组头 + 一组逐行缩略图。
                            let groups = self.take_folder_groups();
                            for (folder, idxs, date) in &groups {
                                self.album_label(ui, folder, date, idxs.len());
                                ui.add_space(4.0);
                                self.photo_grid(ui, &format!("fb-{folder}"), idxs);
                                ui.add_space(16.0);
                            }
                            if self.rows.is_empty() {
                                ui.label("图库里还没有照片,或当前过滤把照片都滤掉了。");
                            }
                            self.folder_groups.1 = groups;
                            return;
                        }
                        let folder = self
                            .folders
                            .get(self.selected_folder)
                            .cloned()
                            .unwrap_or_default();
                        // 从全部行里按目录筛一遍是 O(行数) 次路径运算,
                        // 只在图库版本变了或换了目录时重算(缓存借出后归还)。
                        let mut idxs = std::mem::take(&mut self.folder_cells.2);
                        let mut date = std::mem::take(&mut self.folder_cells.3);
                        if self.folder_cells.0 != self.rows_gen || self.folder_cells.1 != folder {
                            idxs = self
                                .rows
                                .iter()
                                .enumerate()
                                .filter(|(_, r)| folder_of(&r.path) == folder)
                                .map(|(i, _)| i)
                                .collect();
                            date = timeline::group_date(&self.rows, &idxs);
                            self.folder_cells.0 = self.rows_gen;
                            self.folder_cells.1 = folder.clone();
                        }
                        self.album_label(ui, &folder, &date, idxs.len());
                        ui.add_space(4.0);
                        self.photo_grid(ui, "folder-grid", &idxs);
                        self.folder_cells.2 = idxs;
                        self.folder_cells.3 = date;
                    }
                    // 快速查找视图在进入本面板前已自绘并 return;留空仅为穷尽性。
                    ViewKind::Search => {}
                    ViewKind::Albums => {
                        match self.sel_album {
                            None => {
                                ui.heading("相簿");
                                ui.label(
                                    "在左侧选择或新建一个相簿。相簿是照片的命名集合,不会移动文件。",
                                );
                            }
                            Some(a) => {
                                let name = self.store.album_name(a).unwrap_or_default();
                                ui.heading(format!("相簿:{name}"));
                                // 相簿成员按加入顺序显示(链上 pos 序)。
                                let ids = self.store.album_photo_ids(a);
                                let idxs: Vec<usize> = ids
                                    .iter()
                                    .filter_map(|id| self.id_row.get(id).copied())
                                    .collect();
                                if idxs.is_empty() {
                                    ui.label("(空相簿)用照片右键菜单的「加入相簿」添加。");
                                } else {
                                    self.photo_grid(ui, "album-grid", &idxs);
                                }
                            }
                        }
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
                self.preview_tex = None;
                self.preview_base = None;
                self.preview_missing = None;
                self.preview_failed = None;
            } else {
                let &idx = self.id_row.get(&id).unwrap();
                egui::Window::new("照片预览")
                    .open(&mut preview_open)
                    .resizable(true)
                    .default_size([780.0, 860.0])
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
                            ui.separator();
                            // 90° 直角旋转(参考件编辑视图的两个旋转按钮)。
                            if ui.button("↺ 逆时针").clicked() {
                                self.preview_rotate(id, 3);
                            }
                            if ui.button("↻ 顺时针").clicked() {
                                self.preview_rotate(id, 1);
                            }
                            // 属性(参考件图片菜单含属性项;预览窗里即当前照片)。
                            if ui.button("属性").clicked() {
                                self.show_props = true;
                            }
                            #[cfg(feature = "ai")]
                            {
                                if ui.checkbox(&mut self.show_face_boxes, "人脸框").changed() {
                                    self.preview_boxes = None;
                                }
                                if ui.checkbox(&mut self.show_ocr_boxes, "识别框").changed() {
                                    self.preview_boxes = None;
                                }
                            }
                        });
                        if stepped {
                            return;
                        }
                        // 缩放/平移状态归属检查:换照片自动复位(参考件换图回到适应窗口)。
                        if self.zoom_active_for != Some(id) {
                            self.preview_reset_zoom();
                            self.zoom_active_for = Some(id);
                        }
                        // 收取全图解码结果(放大超过预览档分辨率时按需解码)。
                        if let Some(rx) = &self.zoom_rx {
                            match rx.try_recv() {
                                Ok((rid, Some(img))) => {
                                    if Some(rid) == self.preview {
                                        self.preview_full = Some((rid, img));
                                        self.zoom_tex = None; // 触发全图重合成
                                    }
                                    self.zoom_rx = None;
                                    self.zoom_pending = None;
                                }
                                Ok((_, None)) => {
                                    // 解码失败或超过内存上限:停在预览档。
                                    self.zoom_rx = None;
                                    self.zoom_pending = None;
                                    self.fx_msg =
                                        "原图过大或解码失败,放大显示停留在预览档清晰度".into();
                                }
                                Err(mpsc::TryRecvError::Empty) => {}
                                Err(mpsc::TryRecvError::Disconnected) => {
                                    self.zoom_rx = None;
                                    self.zoom_pending = None;
                                }
                            }
                        }
                        ui.separator();
                        let tex = self.preview_tex(ctx);
                        if let Some(t) = tex {
                            let [tw, th] = t.size();
                            let mut to_100 = false;
                            // 缩放工具行(与参考件一致的手势 + 等价按钮)。
                            ui.horizontal(|ui| {
                                if ui.button("−").clicked() {
                                    self.preview_zoom = (self.preview_zoom / 1.25).max(1.0);
                                }
                                if ui.button("100%").clicked() {
                                    to_100 = true;
                                }
                                if ui.button("+").clicked() {
                                    self.preview_zoom = (self.preview_zoom * 1.25).min(8.0);
                                }
                                ui.label(format!("{:.0}%", self.preview_zoom * 100.0));
                                if ui.button("适应窗口").clicked() {
                                    self.preview_zoom = 1.0;
                                    self.preview_pan = egui::Vec2::ZERO;
                                }
                                ui.separator();
                                // 裁剪工具开关(参考件 Crop)。
                                if ui.selectable_label(self.crop_mode, "裁剪").clicked() {
                                    self.crop_mode = !self.crop_mode;
                                    self.crop_rect = None;
                                    self.crop_drag = None;
                                }
                            });
                            // 裁剪工具行:比例预设(经典图库宽高比清单)+ 应用/取消。
                            if self.crop_mode {
                                ui.horizontal(|ui| {
                                    ui.label("比例:");
                                    let mut presets: Vec<(String, Option<(f32, f32)>)> = vec![
                                        ("自由".into(), None),
                                        ("原比例".into(), Some((tw as f32, th as f32))),
                                        ("4 : 3(屏幕)".into(), Some((4.0, 3.0))),
                                        ("3 : 2(打印)".into(), Some((3.0, 2.0))),
                                        ("16 : 9(电影)".into(), Some((16.0, 9.0))),
                                        ("16 : 10".into(), Some((16.0, 10.0))),
                                        ("1 : 1(方形)".into(), Some((1.0, 1.0))),
                                        ("4 : 6".into(), Some((4.0, 6.0))),
                                        ("5 : 7".into(), Some((5.0, 7.0))),
                                        ("8 : 10".into(), Some((8.0, 10.0))),
                                        ("10 : 15".into(), Some((10.0, 15.0))),
                                        ("9 : 13".into(), Some((9.0, 13.0))),
                                    ];
                                    for (n, w, h) in self.custom_ratios.clone() {
                                        presets.push((format!("{n}({w:.0}×{h:.0})"), Some((w, h))));
                                    }
                                    let cur = match self.crop_ratio {
                                        None => "自由".to_string(),
                                        Some((w, h)) => format!("{w:.0} : {h:.0}"),
                                    };
                                    egui::ComboBox::from_id_salt("crop_ratio")
                                        .selected_text(cur)
                                        .show_ui(ui, |ui| {
                                            for (label, r) in &presets {
                                                if ui
                                                    .selectable_label(self.crop_ratio == *r, label)
                                                    .clicked()
                                                {
                                                    self.crop_ratio = *r;
                                                }
                                            }
                                        });
                                    if let Some((w, h)) = self.crop_ratio {
                                        if ui.button("⇄ 对调").clicked() {
                                            self.crop_ratio = Some((h, w));
                                        }
                                    }
                                    if ui.button("应用裁剪").clicked() {
                                        self.commit_crop(id);
                                    }
                                    if ui.button("取消").clicked() {
                                        self.crop_mode = false;
                                        self.crop_rect = None;
                                        self.crop_drag = None;
                                    }
                                });
                                // 自定义比例录入(参考件 Add Custom Aspect Ratio:宽 × 高 + 名称)。
                                ui.horizontal(|ui| {
                                    ui.label("自定义比例:");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.crop_cw)
                                            .desired_width(40.0),
                                    );
                                    ui.label("×");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.crop_ch)
                                            .desired_width(40.0),
                                    );
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.crop_cname)
                                            .hint_text("名称")
                                            .desired_width(90.0),
                                    );
                                    if ui.button("保存为预设").clicked() {
                                        if let (Ok(w), Ok(h)) = (
                                            self.crop_cw.trim().parse::<f32>(),
                                            self.crop_ch.trim().parse::<f32>(),
                                        ) {
                                            if w > 0.0 && h > 0.0 {
                                                let name = if self.crop_cname.trim().is_empty() {
                                                    format!("{w:.0}:{h:.0}")
                                                } else {
                                                    self.crop_cname.trim().to_string()
                                                };
                                                self.custom_ratios.push((name, w, h));
                                                self.save_custom_ratios();
                                                self.crop_cw.clear();
                                                self.crop_ch.clear();
                                                self.crop_cname.clear();
                                            }
                                        }
                                    }
                                });
                            }
                            // 图区:滚动缩放 / 拖拽平移 / 双击切换。全图放大时
                            // 几何以预览档比例计算(等比,几何与分辨率无关)。
                            let avail = (ui.available_size() - egui::vec2(0.0, 120.0))
                                .max(egui::vec2(240.0, 160.0));
                            let (rect, resp) =
                                ui.allocate_exact_size(avail, egui::Sense::click_and_drag());
                            let painter = ui.painter_at(rect);
                            let fit_scale = (rect.width() / tw.max(1) as f32)
                                .min(rect.height() / th.max(1) as f32)
                                .min(1.0);
                            if to_100 {
                                // 100% 像素:屏幕 1px = 图像 1px。
                                self.preview_zoom = (1.0 / fit_scale.max(1e-4)).min(8.0);
                                self.preview_pan = egui::Vec2::ZERO;
                            }
                            const ZOOM_MIN: f32 = 1.0;
                            const ZOOM_MAX: f32 = 8.0;
                            // 滚轮缩放(以光标为锚点;参考件单图视图滚轮即缩放)。
                            if resp.hovered() {
                                let sd = ui.input(|i| i.raw_scroll_delta.y);
                                if sd != 0.0 {
                                    let z0 = self.preview_zoom;
                                    let z1 = (z0 * f32::exp(sd * 0.0022)).clamp(ZOOM_MIN, ZOOM_MAX);
                                    if (z1 - z0).abs() > f32::EPSILON {
                                        let c0 = egui::vec2(
                                            tw as f32 * fit_scale * z0,
                                            th as f32 * fit_scale * z0,
                                        );
                                        let img0 = egui::Rect::from_center_size(
                                            rect.center() + self.preview_pan,
                                            c0,
                                        );
                                        if let Some(p) = resp.hover_pos() {
                                            let uv = egui::vec2(
                                                ((p.x - img0.min.x) / c0.x).clamp(0.0, 1.0),
                                                ((p.y - img0.min.y) / c0.y).clamp(0.0, 1.0),
                                            );
                                            let c1 = egui::vec2(
                                                tw as f32 * fit_scale * z1,
                                                th as f32 * fit_scale * z1,
                                            );
                                            // 光标下的图像点保持不动。
                                            self.preview_pan =
                                                p - uv * c1 + c1 * 0.5 - rect.center();
                                        }
                                        self.preview_zoom = z1;
                                    }
                                }
                            }
                            // 拖拽平移(裁剪模式让位给框选手势;画框在 img_rect 之后处理)。
                            if !self.crop_mode && resp.dragged_by(egui::PointerButton::Primary) {
                                self.preview_pan += resp.drag_delta();
                            }
                            // 双击:适应窗口 ↔ 100% 像素。
                            if resp.double_clicked() {
                                if self.preview_zoom > 1.01 {
                                    self.preview_zoom = 1.0;
                                    self.preview_pan = egui::Vec2::ZERO;
                                } else {
                                    self.preview_zoom = (1.0 / fit_scale.max(1e-4)).min(ZOOM_MAX);
                                    self.preview_pan = egui::Vec2::ZERO;
                                }
                            }
                            let base = fit_scale * self.preview_zoom;
                            let content = egui::vec2(tw as f32 * base, th as f32 * base);
                            // 平移夹在图像边界内(图像小于图区时居中)。
                            let max_pan = ((content - rect.size()) * 0.5).max(egui::Vec2::ZERO);
                            self.preview_pan = egui::Vec2::new(
                                self.preview_pan.x.clamp(-max_pan.x, max_pan.x),
                                self.preview_pan.y.clamp(-max_pan.y, max_pan.y),
                            );
                            let img_rect = egui::Rect::from_center_size(
                                rect.center() + self.preview_pan,
                                content,
                            );
                            // 裁剪模式:拖拽画裁剪框(比例锁定在像素空间成立)。
                            if self.crop_mode {
                                if resp.drag_started() {
                                    self.crop_drag = resp.interact_pointer_pos();
                                    self.crop_rect = None;
                                }
                                if let Some(start) = self.crop_drag {
                                    if let Some(p) = resp.interact_pointer_pos() {
                                        let to_norm = |q: egui::Pos2| {
                                            let d = q - img_rect.min;
                                            let s = img_rect.size();
                                            egui::vec2(
                                                (d.x / s.x.max(1.0)).clamp(0.0, 1.0),
                                                (d.y / s.y.max(1.0)).clamp(0.0, 1.0),
                                            )
                                        };
                                        let a = to_norm(start);
                                        let mut b = to_norm(p);
                                        if let Some((rw, rh)) = self.crop_ratio {
                                            // 像素空间 pw/ph = rw/rh
                                            // → 归一化空间 dw/dh = (rw/rh)·(th/tw)。
                                            let k = (rw / rh) * (th as f32 / tw as f32);
                                            let mut dw = (b.y - a.y) * k;
                                            dw = dw.clamp(-a.x, 1.0 - a.x);
                                            let dh = (dw / k).clamp(-a.y, 1.0 - a.y);
                                            dw = dh * k;
                                            b = egui::vec2(a.x + dw, a.y + dh);
                                        }
                                        self.crop_rect = Some([a.x, a.y, b.x, b.y]);
                                    }
                                    if !resp.dragged() {
                                        self.crop_drag = None;
                                    }
                                }
                            }
                            // 放大超过预览档约 1.35 倍 → 请求全图解码(一次,后台)。
                            let need_full = self.preview_zoom > 1.35
                                && self.preview_full.as_ref().map(|(k, _)| *k) != Some(id)
                                && self.zoom_rx.is_none()
                                && self.zoom_pending.is_none();
                            if need_full {
                                let (tx, rx) = mpsc::channel();
                                self.zoom_rx = Some(rx);
                                self.zoom_pending = Some(id);
                                let path = self.rows[idx].path.clone();
                                std::thread::spawn(move || {
                                    // 内存上限:约 40MP(160MB RGBA)。超过则放弃,停在预览档。
                                    let ok = image::ImageReader::open(&path)
                                        .ok()
                                        .and_then(|r| r.with_guessed_format().ok())
                                        .and_then(|r| r.decode().ok())
                                        .map(|d| {
                                            crate::thumbs::apply_exif_rotation(
                                                d,
                                                crate::exif::read_orientation(Path::new(&path)),
                                            )
                                        })
                                        .filter(|d| {
                                            (d.width() as u64) * (d.height() as u64) <= 40_000_000
                                        })
                                        .map(|d| d.to_rgba8());
                                    let _ = tx.send((id, ok));
                                });
                                ctx.request_repaint_after(std::time::Duration::from_millis(200));
                            }
                            // 放大态且全图已就绪 → 用全图纹理(效果栈合成,带缓存)。
                            let use_full = self.preview_zoom > 1.35
                                && self.preview_full.as_ref().map(|(k, _)| *k) == Some(id);
                            let paint = if use_full {
                                self.zoom_tex_get(ctx, id).or_else(|| Some(t.clone()))
                            } else {
                                Some(t.clone())
                            };
                            if let Some(pt) = paint {
                                painter.image(
                                    pt.id(),
                                    img_rect,
                                    egui::Rect::from_min_max(
                                        egui::pos2(0.0, 0.0),
                                        egui::pos2(1.0, 1.0),
                                    ),
                                    egui::Color32::WHITE,
                                );
                            }
                            // OCR/人脸框图上标注(坐标相对图像矩形,随缩放平移走)
                            let boxes = self.preview_boxes_for(id);
                            if !boxes.is_empty() {
                                let r = img_rect;
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
                                    painter.rect_stroke(
                                        rect,
                                        0.0,
                                        stroke,
                                        egui::StrokeKind::Inside,
                                    );
                                    if !label.is_empty() {
                                        let galley = painter.layout_no_wrap(
                                            label.clone(),
                                            egui::FontId::proportional(11.0),
                                            egui::Color32::WHITE,
                                        );
                                        let bg = egui::Rect::from_min_size(
                                            rect.min,
                                            galley.size() + egui::vec2(4.0, 2.0),
                                        );
                                        painter.rect_filled(
                                            bg,
                                            0.0,
                                            egui::Color32::from_black_alpha(170),
                                        );
                                        painter.galley(
                                            bg.min + egui::vec2(2.0, 1.0),
                                            galley,
                                            egui::Color32::WHITE,
                                        );
                                    }
                                }
                            }
                            // 裁剪遮罩(参考件 Crop 样式:四周压暗 + 白框 + 三分线)。
                            if self.crop_mode {
                                let dim = egui::Color32::from_black_alpha(110);
                                let inner = self.crop_rect.map(|c| {
                                    let (x0, x1) = (c[0].min(c[2]), c[0].max(c[2]));
                                    let (y0, y1) = (c[1].min(c[3]), c[1].max(c[3]));
                                    egui::Rect::from_min_max(
                                        img_rect.min
                                            + egui::vec2(
                                                x0 * img_rect.width(),
                                                y0 * img_rect.height(),
                                            ),
                                        img_rect.min
                                            + egui::vec2(
                                                x1 * img_rect.width(),
                                                y1 * img_rect.height(),
                                            ),
                                    )
                                });
                                match inner {
                                    Some(r) => {
                                        painter.rect_filled(
                                            egui::Rect::from_min_max(
                                                img_rect.min,
                                                egui::pos2(img_rect.max.x, r.top()),
                                            ),
                                            0.0,
                                            dim,
                                        );
                                        painter.rect_filled(
                                            egui::Rect::from_min_max(
                                                egui::pos2(img_rect.min.x, r.bottom()),
                                                img_rect.max,
                                            ),
                                            0.0,
                                            dim,
                                        );
                                        painter.rect_filled(
                                            egui::Rect::from_min_max(
                                                egui::pos2(img_rect.min.x, r.top()),
                                                egui::pos2(r.left(), r.bottom()),
                                            ),
                                            0.0,
                                            dim,
                                        );
                                        painter.rect_filled(
                                            egui::Rect::from_min_max(
                                                egui::pos2(r.right(), r.top()),
                                                egui::pos2(img_rect.max.x, r.bottom()),
                                            ),
                                            0.0,
                                            dim,
                                        );
                                        painter.rect_stroke(
                                            r,
                                            0.0,
                                            egui::Stroke::new(1.5_f32, egui::Color32::WHITE),
                                            egui::StrokeKind::Inside,
                                        );
                                        // 三分构图线。
                                        let lw = egui::Stroke::new(
                                            0.75_f32,
                                            egui::Color32::from_white_alpha(120),
                                        );
                                        for i in 1..3 {
                                            let t = i as f32 / 3.0;
                                            let x = r.left() + r.width() * t;
                                            let y = r.top() + r.height() * t;
                                            painter.line_segment(
                                                [egui::pos2(x, r.top()), egui::pos2(x, r.bottom())],
                                                lw,
                                            );
                                            painter.line_segment(
                                                [egui::pos2(r.left(), y), egui::pos2(r.right(), y)],
                                                lw,
                                            );
                                        }
                                    }
                                    None => {
                                        painter.rect_filled(img_rect, 0.0, dim);
                                    }
                                }
                            }
                        } else {
                            // P1-10:预览改为后台生成 —— 缓存未就绪时显示"生成中",
                            // 而不是"加载失败"(解码在后台线程,失败才会走这里)。
                            if self.preview_missing == Some(id) {
                                ui.label("原文件已不存在(可能被移动或删除),正在移出图库…");
                            } else if self.preview_failed == Some(id) {
                                ui.label("(预览加载失败:格式不支持或解码出错)");
                            } else if crate::thumbs::preview_path(id).exists() {
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
                            if ui
                                .button(if star {
                                    "★ 取消星标"
                                } else {
                                    "☆ 加星"
                                })
                                .clicked()
                            {
                                self.toggle_star(id);
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
                        self.caption_editor(ui, id);
                        ui.separator();
                        self.tags_editor(ui, id);
                    });
                if !preview_open {
                    self.preview = None;
                    self.preview_tex = None;
                    // 关闭预览即释放缩放状态与全图缓存(几十 MB 级)。
                    self.preview_reset_zoom();
                    self.zoom_active_for = None;
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

        // 工具栏「新建相簿」(参考件 New Album from selected:有选中就同时放入照片)。
        if self.album_new_win {
            let mut open = self.album_new_win;
            let mut close = false;
            egui::Window::new("新建相簿")
                .open(&mut open)
                .default_width(360.0)
                .show(ctx, |ui| {
                    let n = self.sel.len();
                    ui.small(if n == 0 {
                        "相簿是照片 id 的命名集合,不移动文件。"
                    } else {
                        "将把选中的照片一并放入新相簿。"
                    });
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.album_input).hint_text("相簿名称"),
                    );
                    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let name = self.album_input.trim().to_string();
                    let ok = (ui.button("创建").clicked() || enter) && !name.is_empty();
                    ui.horizontal(|ui| {
                        if n > 0 {
                            ui.small(format!("已选中 {n} 张"));
                        }
                        if ui.button("取消").clicked() {
                            close = true;
                        }
                    });
                    if ok {
                        match self.store.album_create(&name) {
                            Ok(aid) => {
                                let ids: Vec<i64> = self.sel.iter().copied().collect();
                                let mut added = 0usize;
                                for id in ids {
                                    if self.store.album_add_photo(aid, id).is_ok() {
                                        added += 1;
                                    }
                                }
                                self.sel_album = Some(aid);
                                self.set_view(ViewKind::Albums);
                                self.album_input.clear();
                                self.status = if added == 0 {
                                    format!("已创建相簿「{name}」")
                                } else {
                                    format!("已创建相簿「{name}」,放入 {added} 张")
                                };
                                close = true;
                            }
                            Err(e) => self.status = format!("创建相簿失败: {e}"),
                        }
                    }
                });
            self.album_new_win = open && !close;
        }

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
                    ui.small("把当前视图的照片写入光盘(走 IMAPI2 引擎;卷标沿用默认命名)。");
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

        // 坏文件台账(方案 P3-22):派生反复失败被立案的文件,不再每轮重试。
        // 这里给整批销案的出口 —— 网格右键只能一张一张来。
        if self.show_bad_media {
            let mut open = self.show_bad_media;
            egui::Window::new("坏文件台账")
                .open(&mut open)
                .default_width(760.0)
                .min_height(320.0)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!("共 {} 条", self.bad_media_rows.len()));
                        if ui.button("刷新").clicked() {
                            self.bad_media_rows = self.store.bad_media();
                        }
                    });
                    ui.separator();
                    if self.bad_media_rows.is_empty() {
                        ui.label("(台账是空的 —— 派生连续失败 3 次的文件才会立案到这里)");
                    } else {
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            for b in &self.bad_media_rows {
                                let t = format_ts(b.since);
                                ui.monospace(t);
                                ui.label(&b.path);
                                ui.small(&b.reason);
                            }
                        });
                    }
                    ui.separator();
                    ui.small("立案的文件不再自动重试缩略图。网格右键单个文件可「不隐藏」,或在这里整批销案。");
                    if ui.button("全部销案(重新排队派生)").clicked() {
                        let n = self.bad_media_rows.len();
                        for b in std::mem::take(&mut self.bad_media_rows) {
                            let _ = self.store.unmark_bad_media(&b.path);
                        }
                        self.rebuild();
                        self.status = format!("已销案 {n} 条,后台会重新尝试派生");
                    }
                });
            self.show_bad_media = open;
        }

        // P3-ui:属性窗口(原版 File > Properties 等价物)。
        if self.show_props {
            self.props_ui(ctx);
        }
        // P3-ui:选项窗口(options 等价物)。
        if self.show_options {
            self.options_ui(ctx);
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
                            .add(
                                egui::TextEdit::singleline(&mut input)
                                    .hint_text("十进制或 0x 十六进制"),
                            )
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
                    ui.label("Ctrl+R        预览中顺时针旋转 90°(自定)");
                    ui.label("Ctrl+Shift+R  预览中逆时针旋转 90°(自定)");
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
                .default_width(560.0)
                .default_height(620.0)
                .show(ctx, |ui| {
                    // 大 Logo + 标题/版本号(版本号取自 Cargo.toml)
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::Image::from_bytes(
                                "bytes://about/icon.png",
                                include_bytes!("../assets/icon.png"),
                            )
                            .fit_to_exact_size(egui::vec2(96.0, 96.0)),
                        );
                        ui.vertical(|ui| {
                            ui.add_space(6.0);
                            ui.heading("Relyto.Maliang");
                            ui.label(format!(
                                "版本 {} · Rust + egui 编写的本地照片管理应用",
                                env!("CARGO_PKG_VERSION")
                            ));
                            ui.label("全本地、无联网;文件夹优先,绝不复制或移动原图。");
                        });
                    });
                    ui.separator();
                    ui.label("功能:全盘扫描索引 / 时间线 / 文件夹 / 彩色标签 / 重复检测 / GPS 地理标记(KML)/ 文件夹管理器 / 索引备份与压缩 / 烘焙保存 / 导出 / WiFi 导入 / OCR / 人脸分类");
                    ui.label("人脸:质量评分、半自动命名建议、忽略簇、算法版本迁移(见 视图 > 人脸)。");
                    ui.label("经典五栏菜单(文件 / 编辑 / 视图 / 工具 / 帮助)。");
                    ui.separator();
                    ui.heading("更新日志");
                    egui::ScrollArea::vertical()
                        .id_salt("about_changelog")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            egui_commonmark::CommonMarkViewer::new()
                                .show(ui, &mut self.about_cache, CHANGELOG_MD);
                        });
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
                                            self.start_scan_roots(
                                                vec![PathBuf::from(&path)],
                                                &path,
                                            );
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
                    ui.small("系统目录、应用缓存、回收站一类不会被收录;可在这里增删规则。");
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
                    // 本程序自己写缩略图的目录:它按完整路径排除,不属于上面那张
                    // 名字表,也没有「移除」—— 用户搬了缓存位置,这里跟着变。
                    let own = excl.own_prefixes().to_vec();
                    if !own.is_empty() {
                        ui.small("以下目录由本程序自己写入,始终不会收录:");
                        for p in own {
                            ui.small(p.display().to_string());
                        }
                    }
                    if let Some(rm) = to_remove {
                        self.remove_exclude_dir(&rm);
                    }
                    ui.horizontal(|ui| {
                        let mut input = self.exclude_input.clone();
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut input).hint_text("目录名,如 Cache"),
                            )
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
                    ui.label(format!(
                        "确定把「{folder}」及其子文件夹下的所有照片从图库索引中移除吗?"
                    ));
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
                            let tag = crate::faces::algo_tag(&self.store);
                            let _ = self.store.set_setting("face_algo_version", &tag);
                            self.face_version_warn = false;
                            self.start_face_scan();
                        }
                        if ui.button("保留旧数据").clicked() {
                            let tag = crate::faces::algo_tag(&self.store);
                            let _ = self.store.set_setting("face_algo_version", &tag);
                            self.face_version_warn = false;
                        }
                    });
                });
            self.face_version_warn = open;
        }

        // 索引服务授权说明窗:动手之前先讲清楚要干什么、要什么权限、能不能撤,
        // 取得同意再发起(UAC 是系统安全桌面,不会有这些上下文,所以在这里说)。
        #[cfg(windows)]
        if self.svc_ask_grant {
            let mut open = self.svc_ask_grant;
            let mut decided: Option<SvcWant> = None;
            // 系统对话框式:屏幕居中、无原生标题条(拖不了就对了);配色自绘,
            // 推荐档绿底,拒绝档深蓝。
            const GRANT_GREEN: egui::Color32 = egui::Color32::from_rgb(0x2E, 0x8B, 0x47);
            const GRANT_BLUE_DARK: egui::Color32 = egui::Color32::from_rgb(0x2C, 0x5F, 0x8A);
            egui::Window::new("启用全盘快速查找")
                .title_bar(false)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .default_width(560.0)
                .show(ctx, |ui| {
                    // 绿色标题条 + 自绘关闭钮:关窗职责归这里。
                    let (hdr, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 36.0),
                        egui::Sense::hover(),
                    );
                    ui.painter().rect_filled(hdr, egui::CornerRadius::same(6), GRANT_GREEN);
                    ui.painter().text(
                        egui::pos2(hdr.center().x, hdr.center().y),
                        egui::Align2::CENTER_CENTER,
                        "启用全盘快速查找",
                        egui::FontId::proportional(17.0),
                        egui::Color32::WHITE,
                    );
                    let x_rect = egui::Rect::from_center_size(
                        egui::pos2(hdr.right() - 20.0, hdr.center().y),
                        egui::vec2(28.0, 28.0),
                    );
                    let x = ui.interact(x_rect, ui.id().with("svc-grant-x"), egui::Sense::click());
                    // ✕ 字符默认字体缺字形,直接画两笔叉更稳。
                    let xc = x_rect.center();
                    let xr = 5.0;
                    let x_stroke = egui::Stroke::new(
                        if x.hovered() { 2.2_f32 } else { 1.6_f32 },
                        egui::Color32::WHITE,
                    );
                    ui.painter().line_segment(
                        [egui::pos2(xc.x - xr, xc.y - xr), egui::pos2(xc.x + xr, xc.y + xr)],
                        x_stroke,
                    );
                    ui.painter().line_segment(
                        [egui::pos2(xc.x - xr, xc.y + xr), egui::pos2(xc.x + xr, xc.y - xr)],
                        x_stroke,
                    );
                    if x.clicked() {
                        open = false;
                    }
                    ui.add_space(14.0);
                    ui.label("「快速查找」要列出全盘所有文件名,数据来自 NTFS 卷的 USN/MFT 日志。");
                    ui.label("读卷日志属于管理员级操作,所以要装一个后台服务来干这件事 —— 这需要你点一次 UAC。");
                    ui.indent("svc_grant_what", |ui| {
                        ui.label("· 服务名 RelytoMaliangIndex,开机自动启动,只在本机跑;");
                        ui.label("· 只读文件名、大小与时间戳,不读文件内容、不改任何文件;");
                        ui.label("· 产物只有一个本地索引文件 nameindex.bin(存在数据目录里);");
                        ui.label("· 界面本身不提权:它通过本地命名管道向服务问结果;");
                        ui.label("· 不联网、不上传,随时可以在 选项 > 常规 卸载。");
                    });
                    ui.add_space(14.0);
                    // 两档:按钮定宽对齐,说明文字跟在右侧,行距拉开。
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("安装并启动服务(推荐)")
                                        .color(egui::Color32::WHITE)
                                        .strong(),
                                )
                                .fill(GRANT_GREEN)
                                .min_size(egui::vec2(200.0, 30.0))
                                .corner_radius(6),
                            )
                            .clicked()
                        {
                            decided = Some(SvcWant::Yes);
                        }
                        ui.small("只有这次弹 UAC。装好后开机自启、持续保鲜索引,以后不再问。");
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("不用全盘快查").color(egui::Color32::WHITE),
                                )
                                .fill(GRANT_BLUE_DARK)
                                .min_size(egui::vec2(200.0, 30.0))
                                .corner_radius(6),
                            )
                            .clicked()
                        {
                            decided = Some(SvcWant::No);
                        }
                        ui.small("跳过:查找只覆盖图库里的照片,发现与保鲜退回逐目录扫描,以后可在 选项 > 常规 改。");
                    });
                    ui.add_space(14.0);
                    ui.checkbox(&mut self.svc_never_ask, "不再提示");
                    ui.small("嫌每次启动都问就勾上:选「不用全盘快查」或直接关窗,以后启动不再询问,老老实实走传统扫描;菜单和页面入口仍可随时启用。");
                });
            let checked = self.svc_never_ask;
            if let Some(g) = decided {
                match g {
                    // 没勾「不再提示」的拒绝不算决定:不落意向,下次启动继续问。
                    SvcWant::No if !checked => {
                        self.svc_ask_grant = false;
                        self.link_declined = false;
                        let _ = self.store.set_setting("svc_declined", "0");
                        self.status =
                            "本次跳过全盘快查,下次启动会再问(勾选「不再提示」可免打扰)。".into();
                    }
                    g => {
                        self.svc_apply_want(g);
                        if matches!(g, SvcWant::No) && checked {
                            let _ = self.store.set_setting("svc_never_ask", "1");
                            self.status =
                                "已选「不再提示」:以后启动不再询问全盘快查(入口仍在,随时可启用)。"
                                    .into();
                        }
                    }
                }
            } else {
                // 直接关窗口 = 还没决定:保持「未问过」,下次启动再问一次;
                // 勾了「不再提示」则尊重,落库后启动不再弹。
                if !open && checked {
                    let _ = self.store.set_setting("svc_never_ask", "1");
                    self.status =
                        "已选「不再提示」:以后启动不再询问全盘快查(入口仍在,随时可启用)。".into();
                }
                self.svc_ask_grant = open;
            }
        }

        // 首启范围对话框(方案 P3-18):两档单选、**无取消**。
        // 作用域必须答一次 —— 空账本下派生门谁都不放行,网格全是占位。
        if self.scope_ask {
            let mut decided: Option<InitialScope> = None;
            const SCOPE_GREEN: egui::Color32 = egui::Color32::from_rgb(0x2E, 0x8B, 0x47);
            egui::Window::new("选择照片范围")
                .title_bar(false)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .default_width(580.0)
                .show(ctx, |ui| {
                    // 标题条不画关闭钮:这一窗不给跳过,答完就落账。
                    let (hdr, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 36.0),
                        egui::Sense::hover(),
                    );
                    ui.painter().rect_filled(hdr, egui::CornerRadius::same(6), SCOPE_GREEN);
                    ui.painter().text(
                        egui::pos2(hdr.center().x, hdr.center().y),
                        egui::Align2::CENTER_CENTER,
                        "选择照片范围",
                        egui::FontId::proportional(17.0),
                        egui::Color32::WHITE,
                    );
                    ui.add_space(14.0);
                    ui.label("整台电脑的文件名都会收进索引 —— 那一步不读文件内容,很快。");
                    ui.label("下面这一档只决定**哪些目录要出缩略图、读拍摄信息**,不影响能不能搜到。");
                    ui.add_space(12.0);
                    ui.radio_value(&mut self.scope_choice, InitialScope::Complete, "全盘");
                    ui.indent("scope_complete_why", |ui| {
                        ui.small("所有磁盘上的图都出缩略图。补齐在后台慢慢跑,眼前的目录优先。");
                    });
                    ui.add_space(6.0);
                    ui.radio_value(&mut self.scope_choice, InitialScope::Limited, "只常用目录");
                    ui.indent("scope_limited_why", |ui| {
                        ui.small("只给「我的文档 / 我的图片 / 桌面」出缩略图。别处的图照样在库里、照样搜得到,点进那个目录时再补。");
                    });
                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("确定").color(egui::Color32::WHITE).strong(),
                                )
                                .fill(SCOPE_GREEN)
                                .min_size(egui::vec2(120.0, 30.0))
                                .corner_radius(6),
                            )
                            .clicked()
                        {
                            decided = Some(self.scope_choice);
                        }
                        ui.small("以后可在 选项 > 常规 改。");
                    });
                });
            if let Some(c) = decided {
                self.apply_initial_scope(c, true);
            }
        }

        // 编辑 > 从图库中移除(二次确认;只删索引,不删文件)
        if self.confirm_remove {
            egui::Window::new("从图库中移除")
                .collapsible(false)
                .default_width(400.0)
                .show(ctx, |ui| {
                    ui.label(format!(
                        "确定把选中的 {} 张照片从图库索引中移除吗?",
                        self.sel.len()
                    ));
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
