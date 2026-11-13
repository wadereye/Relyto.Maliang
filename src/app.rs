use crate::db::Store;
use crate::i18n::{t, t_with};
use crate::timeline::{self, PhotoRow};
use eframe::egui;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

mod editor;
use editor::{EditorTab, Histogram};

/// 视图模式(视图菜单:最近更新/时间线/人物/标签/文件夹)。
/// Faces/Semantic 仅 ai 构建的菜单会构造;无 ai 时变体存在但无人构造,允许死代码。
#[derive(PartialEq, Clone, Copy)]
enum ViewKind {
    /// 首页虚拟相册「最近更新」:成员有上限、只收已有落盘缩略图的那批。
    Recent,
    Timeline,
    Folder,
    Duplicates,
    /// 相簿:照片的命名集合,不动文件。
    Albums,
    #[allow(dead_code)]
    Faces,
    Tags,
    #[allow(dead_code)]
    Semantic,
    /// 快速查找:全盘文件名索引检索(USN/MFT,Windows 提权子进程产出)。
    Search,
}

/// 视图所属的左栏抽屉键(切换视图时顺势展开对应抽屉)。
fn drawer_key(v: ViewKind) -> &'static str {
    match v {
        ViewKind::Recent | ViewKind::Albums => "相册",
        ViewKind::Faces => "人物",
        ViewKind::Folder => "文件夹",
        _ => "其他素材",
    }
}

impl ViewKind {
    /// 状态栏/历史提示用的一词名(左栏导航项的文案另有取舍,见导航列表)。
    fn label(self) -> &'static str {
        match self {
            ViewKind::Recent => t("最近更新"),
            ViewKind::Timeline => t("时间线"),
            ViewKind::Folder => t("文件夹"),
            ViewKind::Duplicates => t("重复照片"),
            ViewKind::Albums => t("相簿"),
            ViewKind::Faces => t("人物"),
            ViewKind::Tags => t("标签"),
            ViewKind::Semantic => t("语义搜索"),
            ViewKind::Search => t("快速查找"),
        }
    }
}

/// 整页("看照片"和"取照片"做成两套整页面板互切:
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
            PageKind::Library => t("图库"),
            PageKind::Import => t("导入"),
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
            PeopleSort::Name => t("按姓名给人物排序"),
            PeopleSort::Amount => t("按数量给人物排序"),
            PeopleSort::Top10 => t("给人物排序并只看前 10"),
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

/// 右栏抽屉的四个面板(属性 / 标签 / 人物 / 地点四页)。
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
            DrawerTab::Props => t("属性"),
            DrawerTab::Tags => t("标签"),
            DrawerTab::People => t("人物"),
            DrawerTab::Places => t("地点"),
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

/// 「检查更新」窗口里的时间戳文案。0 = 从没检查过。
fn update_time_text(secs: i64) -> String {
    if secs <= 0 {
        return t("从未").into();
    }
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| t("从未").into())
}

/// 「检查更新」窗口里的索引服务状态。安装前必须让用户看见它在不在跑。
fn update_service_text() -> &'static str {
    match crate::win::service_state() {
        crate::win::ServiceState::Running => t("运行中(安装时会先停止,完成后恢复)"),
        crate::win::ServiceState::Stopped => t("已停止"),
        crate::win::ServiceState::NotInstalled => t("未安装"),
        crate::win::ServiceState::Transition(_) => t("状态切换中"),
    }
}

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

/// 单图预览里的动图播放状态。只按帧延迟换纹理,不改文件、不套效果栈。
struct AnimState {
    id: i64,
    clip: crate::anim::Clip,
    tex: Option<egui::TextureHandle>,
    playing: bool,
    elapsed_ms: u64,
    tick: std::time::Instant,
    frame: usize,
}

pub struct App {
    store: Arc<Store>,
    rows: Vec<PhotoRow>,
    months: Vec<(String, Vec<usize>)>,
    folders: Vec<String>,
    selected_folder: usize,
    view: ViewKind,
    /// 首页「最近更新」的成员(下标指向 `rows`),重算入口见 `refresh_recent`。
    recent_idxs: Vec<usize>,
    /// 相册头那行日期(组内最早拍摄日,中文写法),空串时只报张数。
    recent_date: String,
    /// 有新缩略图落盘 → 置脏,等节流窗口到了再收进「最近更新」。
    recent_dirty: bool,
    /// 上次重算「最近更新」的时刻。
    recent_at: std::time::Instant,
    texs: TexCache,
    /// 常驻缩略图后台 worker:启动时补存量,扫描后自动补新增。
    thumb_bg: Option<crate::thumbs::ThumbWorker>,
    /// AI 队列调度骨架:三阶段 worker 与暂停闸门。
    #[cfg(feature = "ai")]
    ai_sched: Option<crate::ai_sched::AiSched>,
    /// 预览提级去重:同一张只入队一次。
    #[cfg(feature = "ai")]
    ai_preview_for: Option<i64>,
    /// AI 统计快照(选项页 2 秒内不重复查询)。
    #[cfg(feature = "ai")]
    ai_stats_cache: Option<(std::time::Instant, Vec<crate::ai_sched::StageStats>)>,
    /// 模型下载器(单任务)与最近一次导入/替换的提示。
    #[cfg(feature = "ai")]
    ai_dl: Option<crate::ai_models::SlotDownloader>,
    /// 多文件槽位的待下载队列(前一个文件完成后串行启动)。
    #[cfg(feature = "ai")]
    ai_dl_queue: Vec<crate::ai_models::DownloadPlan>,
    #[cfg(feature = "ai")]
    ai_models_msg: String,
    /// 槽位状态与磁盘占用快照(2 秒缓存)。
    #[cfg(feature = "ai")]
    ai_models_cache: Option<(std::time::Instant, Vec<crate::ai_models::SlotStatus>, u64)>,
    scan_busy: Arc<AtomicBool>,
    scan_rx: Option<mpsc::Receiver<ScanResult>>,
    /// 扫描进度/取消控制(进度回调 + 退出检查)。
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
    /// GIF / 动图 WebP 的逐帧播放状态(没有动图时恒为 None)。
    anim: Option<AnimState>,
    /// 动图后台解码结果通道;帧序列可能较大,不能卡在 UI 线程解。
    anim_rx: Option<mpsc::Receiver<(i64, Option<crate::anim::Clip>)>>,
    /// 已发起过动图探测的照片,避免每帧重复起线程。
    anim_req_for: Option<i64>,
    /// 预览后台 worker(解码 + 落盘缓存,不阻塞 UI)。
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
    /// 最近一次单击/Ctrl+单击命中的照片 id(Shift+单击连选的基准;
    /// 点选习惯:单击只选一张,Ctrl 加减选,Shift 从基准连选)。
    sel_anchor: Option<i64>,
    sort_by: timeline::SortBy,
    sort_rev: bool,
    /// 工具栏过滤开关(星标/人物/视频/地理标记;全 false = 显示全部)。
    filters: timeline::Filters,
    /// 当前缩略图档位(小 96 / 普通 256 / 大 512 / 预览 1024)。
    thumb_tier: crate::thumbs::Tier,
    tags: Vec<timeline::TagInfo>,
    sel_tag: Option<String>,
    /// 标签视图:重命名待编辑项(原名, 输入框)与删除待确认项。
    tag_rename: Option<(String, String)>,
    tag_delete: Option<String>,
    /// 批量加标签弹窗(打开状态、输入、目标照片)。
    tag_batch_open: bool,
    tag_batch_input: String,
    tag_batch_ids: Vec<i64>,
    /// AI 后台索引是否启用(设置页/工具栏开关同步)。
    #[cfg(feature = "ai")]
    ai_enabled: bool,
    /// 标签视图:AI 分类筛选与分类计数缓存。
    #[cfg(feature = "ai")]
    sel_category: Option<String>,
    #[cfg(feature = "ai")]
    ai_category_cache: Option<(std::time::Instant, Vec<(&'static str, &'static str, usize)>)>,
    /// 标签视图:库内标签/图中文字检索词与命中照片。
    #[cfg(feature = "ai")]
    ai_query: String,
    #[cfg(feature = "ai")]
    ai_query_hits: Vec<i64>,
    show_about: bool,
    /// 「关于」窗口里 Markdown 更新日志的渲染缓存(egui_commonmark)。
    about_cache: egui_commonmark::CommonMarkCache,
    show_shortcuts: bool,
    /// 自动更新:后台检查/下载线程与「检查更新」窗口的状态快照。
    updater: crate::update::Updater,
    /// 安装更新的二次确认窗(UAC + 停服务前必须让用户点一次)。
    update_confirm: bool,
    /// 人脸模型:启动检查、后台下载线程与进度快照(下载源 = maliang.ini 的 ModelURL)。
    #[cfg(feature = "ai")]
    models: crate::models::ModelDownloader,
    confirm_remove: bool,
    // —— 文件夹管理器 / 图库维护——
    show_folder_mgr: bool,
    /// 排除目录输入框(自定义排除规则的待添加目录名)。
    exclude_input: String,
    watched: Vec<(String, usize)>,
    confirm_folder_remove: Option<String>,
    maint_rx: Option<mpsc::Receiver<Result<String, String>>>,
    /// 变更历史窗口开关。
    show_changes: bool,
    /// 变更历史条目 (时间戳ms, 路径, 动作)。
    change_log: Vec<(i64, String, String)>,
    /// 坏文件台账窗口开关。
    show_bad_media: bool,
    /// 台账快照(开窗与点「刷新」时重读)。
    bad_media_rows: Vec<crate::db::BadMedia>,
    /// uid64 查找窗口。
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
    /// 滚动限速器(位移阻尼)。
    /// 滚动时按整形后的速度调节"本帧允许解码的缩略图数量",
    /// 滚动越快解码越少 → 大图库滚动不卡顿(限速节点的等价作用)。
    scroll_throttle: crate::scroll::ScrollSmoother,
    /// 本帧允许同步解码的缩略图**像素体积**(RGBA8 字节数)。
    ///
    /// 按字节而非张数设上限:预览档(1024px,4MB/张)张数口径一帧可达
    /// 256MB 的解码 + 纹理上传,一帧几百毫秒;字节口径后各档的每帧耗时拉平
    /// (见 [`crate::scroll::decode_budget_for`])。
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
    /// 键盘翻页/跳转的目标滚动偏移(滚动事件派发后生效)。
    gallery_scroll_target: Option<f32>,
    /// 设备热插拔监听(WM_DEVICECHANGE 链路)。
    device_watch: Option<crate::device::DeviceWatcher>,
    /// 上一次见到的可移动卷列表(用于识别新接入的设备)。
    last_volumes: Vec<PathBuf>,
    // —— 效果系统(效果栈 + 撤销栈)——
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
    /// 邮件发送窗口开关 + 尺寸档。
    email_win: bool,
    email_size: crate::share::ShareSize,
    /// 礼品光盘窗口开关 + 尺寸档。
    cd_win: bool,
    cd_size: crate::share::ShareSize,
    /// 邮件/光盘后台任务结果通道。
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
    /// 字符串池诊断用的探针 id(配合 `pool_maintenance_ui` 的查询按钮)。
    pool_probe_id: i64,
    /// 从设备导入窗口开关 + 统一设备列表(懒枚举,打开窗口时刷新)。
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
    // —— 预览缩放/平移(单图编辑视图:滚轮缩放、拖拽平移、
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
    /// 全图解码已判失败的照片 id(大图 / 解码失败时不再每帧重发请求)。
    zoom_failed_for: Option<i64>,
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
    // —— 幻灯片放映(可调间隔与循环开关)——
    /// 放映中(全屏接管主区)。
    slideshow: bool,
    /// 上次自动切换时刻。
    slide_last: std::time::Instant,
    /// 暂停(空格切换)。
    slide_pause: bool,
    /// 每张停留秒数(默认 3 秒)。
    slide_interval: f32,
    /// 循环放映(设置项 slideshow_loop,默认开)。
    slide_loop: bool,
    // —— 标题(Caption)——
    /// 预览窗标题编辑缓冲(照片 id, 文本)。
    caption_edit: Option<(i64, String)>,
    /// 缩略图下方显示标题(设置项 thumb_captions)。
    thumb_captions: bool,
    // —— 相簿(Album)——
    /// 当前查看的相簿。
    sel_album: Option<i64>,
    /// 新建相簿输入框。
    album_input: String,
    // —— 属性 / 选项窗口 ——
    show_props: bool,
    show_options: bool,
    /// 选项窗口当前页签(0 常规 / 1 电子邮件 / 2 幻灯片)。
    options_tab: usize,
    /// 缓存目录占用(文件数, 字节数)+ 采样时刻。整树遍历不放在每帧里跑,
    /// 选项页打开时按时间片刷新(迁移完立刻重采)。
    cache_usage: Option<(usize, u64, std::time::Instant)>,
    /// 文件夹视图树形形态(左栏可逐级展开的树;设置项 folder_tree,缺省树形)。
    folder_tree: bool,
    /// 中间栏是否一次铺开所有文件夹(每组一个相册组头)。
    /// 左栏点了具体文件夹即收窄为该组;左栏「全部文件夹」再铺回去。
    folder_show_all: bool,
    /// 单个文件夹视图的呈现形态:false = 缩略图网格(缺省),true = 详细列表
    /// (文件名 / 拍摄日期 / 大小,点列头排序;设置项 folder_details)。
    folder_details: bool,
    /// 单个文件夹视图的文件名过滤词(会话内有效,不落库;
    /// 只过滤**当前文件夹**的显示,与顶部全盘搜索无关)。
    folder_filter: String,
    /// 过滤词生效时所在的目录;换目录即清空过滤(资源管理器直觉:
    /// 过滤跟着文件夹走,不跟着用户走)。
    folder_filter_owner: String,
    /// 左栏树里被选中的**子树根**(中间层目录,自身可能没有直挂照片)。
    /// 递归语义:选中父级 = 中间栏聚合显示本目录+全部子孙目录的照片,
    /// 一路点下去逐级收窄;Some 时优先于 selected_folder 槽位。
    subtree_sel: Option<String>,
    /// 左栏每行画该目录的缩略图(设置项 tree_thumbs)。
    tree_thumbs: bool,
    /// 文件夹树的节点缓存(键 = rows_gen;逐帧重建是 O(目录数×层级) 的白活)。
    folder_tree_cache: FolderTree,
    /// 展开中的目录路径(点三角切换;选中项的祖先链自动展开)。
    tree_open: HashSet<String>,
    /// 树行缩略图纹理缓存(按照片 id;随当前档位变化,数量有上限就整体丢弃)。
    tree_texs: HashMap<i64, egui::TextureHandle>,
    /// 视图切换后要展开的左栏抽屉(只作用一帧,随后交回用户收展)。
    nav_open_drawer: Option<&'static str>,
    /// 当前整页(主面板与导入面板两套整页互切,不是弹窗)。
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
    /// 导入页目录树:展开中的路径(点三角切换,换源后祖先链自动展开)。
    import_tree_open: HashSet<String>,
    /// 导入页目录树:子目录缓存(值 = 路径 + 是否导入目录;懒加载,刷新时清空)。
    import_tree_kids: HashMap<String, Vec<(PathBuf, bool)>>,
    /// 人物列表排序方式(按姓名 / 数量 / 前 10)。
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
    /// 人脸扫描取消标志(连续扫描整库可能跑很久,得能中途停)。
    #[cfg(feature = "ai")]
    face_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// 人脸扫描全库进度(累计已处理张数, 开扫时待分析总数)。
    #[cfg(feature = "ai")]
    face_prog: (usize, usize),
    /// 人脸扫描范围单选态(true=只常用目录),重建窗用,确认后落库记住。
    #[cfg(feature = "ai")]
    face_scope_common: bool,
    /// 开机自动续扫只查一次。
    #[cfg(feature = "ai")]
    face_resume_done: bool,
    #[cfg(feature = "ai")]
    face_people: Vec<crate::faces::PeopleRow>,
    #[cfg(feature = "ai")]
    sel_cluster: Option<i64>,
    /// 当前选中的稳定身份(None = 未归属簇视图)。
    #[cfg(feature = "ai")]
    sel_person: Option<i64>,
    #[cfg(feature = "ai")]
    face_hits: Vec<crate::faces::FaceHit>,
    #[cfg(feature = "ai")]
    face_sugs: Vec<crate::faces::Suggestion>,
    /// 按脸检索结果;face_similar_for 记录查询脸,换脸后旧结果自动隐藏。
    #[cfg(feature = "ai")]
    face_similar: Vec<crate::faces::SimilarFace>,
    #[cfg(feature = "ai")]
    face_similar_for: Option<i64>,
    #[cfg(feature = "ai")]
    face_similar_thr: f32,
    /// 相似人脸后台检索通道(查询脸 id, 阈值, 结果);在途时新请求只记重跑。
    #[cfg(feature = "ai")]
    face_sim_rx: Option<mpsc::Receiver<(i64, f32, Vec<crate::faces::SimilarFace>)>>,
    #[cfg(feature = "ai")]
    face_sim_pending: bool,
    /// 全量重聚类后台通道。
    #[cfg(feature = "ai")]
    recluster_rx: Option<mpsc::Receiver<Result<usize, String>>>,
    /// 合并/拆分约束的目标簇(当前簇之外)。
    #[cfg(feature = "ai")]
    face_merge_target: Option<i64>,
    #[cfg(feature = "ai")]
    face_hide_low: bool,
    #[cfg(feature = "ai")]
    face_show_ignored: bool,
    #[cfg(feature = "ai")]
    face_version_warn: bool,
    /// 选项页点了别的方案,等确认窗表态后真正切换。
    #[cfg(feature = "ai")]
    face_algo_pending: Option<crate::ai::face::FaceAlgo>,
    /// 模型下载复选框的勾选状态(已就位的文件每帧强制清掉)。
    #[cfg(feature = "ai")]
    model_pick: [bool; 3],
    /// 「模型诊断」结果缓存:点一次算一次,按方案存,切方案即失效重算。
    #[cfg(feature = "ai")]
    face_diag: Option<(crate::ai::face::FaceAlgo, crate::ai::face::FaceDiag)>,
    #[cfg(feature = "ai")]
    input_rename: String,
    /// 右栏「人物」页正在改名的脸(face_id, 草稿名);失焦/回车即写回所属人物。
    #[cfg(feature = "ai")]
    face_name_draft: Option<(i64, String)>,
    #[cfg(feature = "ai")]
    clip_index_rx: Option<mpsc::Receiver<Result<(usize, usize), String>>>,
    #[cfg(feature = "ai")]
    clip_search_rx: Option<mpsc::Receiver<Result<(Vec<(i64, Option<f32>)>, Option<String>), String>>>,
    #[cfg(feature = "ai")]
    clip_results: Vec<(i64, Option<f32>)>,
    /// 以图搜图的查询照片名(结果标题用;文本搜索时清空)。
    #[cfg(feature = "ai")]
    clip_image_query: Option<String>,
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
    /// 首启范围对话框待示(两档单选,**无取消** —— 必须答一次)。
    scope_ask: bool,
    /// 初次设置向导的当前页(0 = 服务说明,1 = 照片范围;两问合一窗分步呈现)。
    wizard_page: usize,
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
    /// 「屏幕上出现过、已经排过队」的照片 id:同一张只推一次。
    adopt_seen: std::collections::HashSet<i64>,
    /// 待按张放行的照片 id:渲染帧只入队,`photo_grid` 每帧批量消化。
    adopt_queue: std::collections::VecDeque<i64>,
    /// 本帧真正画在屏幕上的目录(原样路径,按出现顺序、去重)。帧首整批发给后台 worker。
    frame_dirs: Vec<String>,
    /// `frame_dirs` 的去重键(大写归一路径)。
    frame_dirs_seen: std::collections::HashSet<String>,
    // —— 单图编辑视图(左工具 / 中预览 / 右属性)——
    /// 左栏工具分类页。
    editor_tab: EditorTab,
    /// 右栏属性面板显隐(标题栏的收展按钮)。
    editor_props: bool,
    /// 底部胶片带显隐。
    editor_film: bool,
    /// RGB 直方图缓存:(照片 id, 效果栈签名, 分桶数据)。
    editor_hist: Option<(i64, String, Histogram)>,
    /// 相机信息缓存(按照片 id;换图才重读一次文件)。
    editor_exif: Option<(i64, Vec<(String, String)>)>,
    /// 邻片缩略图纹理(顶部小图与胶片带共用)。
    editor_thumbs: HashMap<i64, egui::TextureHandle>,
    /// 对比显示:0 = 效果图,1 = 原图,2 = 并排。
    editor_compare: u8,
    /// 原图纹理(未套效果栈;对比显示用)。
    editor_orig_tex: Option<(i64, egui::TextureHandle)>,
    /// 右栏页签(属性 / 标题 / 标签 / 效果)。
    editor_props_tab: u8,
    /// 工具网格里选中的带强度工具(如补光 / 锐化 / 暗角)。
    editor_tool: Option<&'static str>,
    /// 该工具的强度(0..100),实时参与预览合成。
    editor_tool_amount: f32,
    /// 编辑视图草稿归属的照片 id(换图时清掉未提交的工具草稿)。
    editor_for: Option<i64>,
    /// 顶部工具条发来的缩放命令(0 无 / 1 100% / 2 适应 / 3 放大 / 4 缩小),
    /// 中间图区知道自己的实际尺寸,由它在下一帧消费。
    editor_zoom_cmd: u8,
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
    /// 走不了 USN/MFT 的卷 → 原因(这些卷走兼容扫描,无实时快查)。
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
    /// 提权停→同步数据目录标记→起:迁移后让服务立刻用上新目录。
    Restart,
    /// 请求全量重建;空列表 = 全部可扫卷。
    Rebuild(Vec<char>),
    /// 让服务立刻把内存索引落盘。
    Flush,
}

/// 链路心跳间隔:在线时按这个节奏问一次 `Status`。
const LINK_TICK: std::time::Duration = std::time::Duration::from_secs(3);
/// 连不上时的退避上限(服务可能正在被安装,别把重连打成忙等)。
const LINK_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// 用户是否允许使用全盘快查(`svc_wanted=0` 时置 false)。
///
/// 链路线程独立于 App、拿不到它的锁,只能用原子位传递意向;本程序只有一个
/// App 实例,模块级单例足够。置 false 后连管道都不再尝试 —— 服务是机器级
/// 自动启动的,光靠 UI「不弹安装提示」并不能阻止它继续常驻建全盘索引。
/// 消费方有两处:本文件的 `link_connect`(心跳/重连/装卸/重建)与
/// `watch.rs` 的 `l1_gone`(删除对账),两边必须同一口径。
pub(crate) static SVC_LINK_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// 启动索引服务链路线程:维持一根管道连接、周期问 `Status`、掉线退避重连,
/// 并代办装卸服务(要等 UAC)与全量重建。UI 只读回报,自己不碰管道。
fn spawn_link_worker(ctx: egui::Context) -> (mpsc::Sender<LinkCmd>, mpsc::Receiver<LinkMsg>) {
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
                let c = client.as_mut().expect(t("上面刚确保连接在位"));
                match c.call_ok(&crate::ipc::Req::Status) {
                    Ok(crate::ipc::Resp::Status(s)) => {
                        let _ = msg_tx.send(LinkMsg::Up(s, crate::win::service_state()));
                        ctx.request_repaint();
                    }
                    Ok(other) => {
                        client = None;
                        let _ = msg_tx.send(LinkMsg::Down(
                            t_with(
                                "服务对 Status 回了意外答复:{other:?}",
                                &[("other", format!("{other:?}"))],
                            ),
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
        .expect(t("启动索引服务链路线程失败"));
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
        LinkCmd::Restart => {
            let r = crate::win::service_restart_elevated();
            *client = None;
            *backoff = std::time::Duration::from_secs(1);
            let _ = msg_tx.send(LinkMsg::Service("重启", r.err()));
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
    let c = client.as_mut().expect(t("上面刚连上"));
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
    // 用户明确选了「不用全盘快查」:连握手都不做,不向常驻服务索取全盘数据。
    // 所有入口(心跳、Retry、重建、落盘)都经过这里,一道门就能收口。
    if !SVC_LINK_ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(t("已按你的选择关闭全盘快查(在 选项 > 全盘快查 可重新启用)").into());
    }
    let mut c = crate::ipc::Client::connect(500)?;
    match c.call(&crate::ipc::Req::Hello {
        proto: crate::ipc::PROTO_VERSION,
    }) {
        Ok(crate::ipc::Resp::Hello { .. }) => Ok(c),
        Ok(other) => Err(t_with(
            "握手答复异常:{other:?}",
            &[("other", format!("{other:?}"))],
        )),
        Err(e) => Err(e),
    }
}

/// 右键菜单目标:网格行与快速查找命中两条路都能取齐同一组字段,
/// 菜单本身只认这六个量,不再关心行从哪来(快速查找页因此能
/// 直接复用图库网格的那份右键菜单)。
struct CtxPhoto {
    id: i64,
    /// 文件全路径(资源管理器 / 编辑器 / 壁纸 / 隐藏都用它)。
    path: String,
    /// 所在目录(「纳入此目录(覆盖排除名单)」的判定对象)。
    parent: String,
    /// 视频走「播放」,其余走「预览」。
    movie: bool,
    /// 当前星标状态(决定菜单显示加还是取消)。
    star0: bool,
    /// 这张在不在多选里(批量加/去星标文案用;快速查找页恒为单张)。
    selected: bool,
}

/// 快速查找结果行。
///
/// 服务回的命中自带 size/mtime(这几列服务端本来就在内存里握着),
/// 所以 UI 侧不再逐条 `stat` —— 省掉一趟 500 次文件 IO,冷目录上差别很明显。
/// 投影在服务内完成:派生状态也由服务批量查库后随命中一起回。
#[derive(Debug, Clone)]
struct NameRow {
    hit: crate::nameidx::SearchHit,
    /// 字节数;目录为 0。
    size: u64,
    /// 修改时间,UNIX 秒;0 = 这卷走 USN 降级路径,索引里没有权威时间。
    mtime: u64,
    /// 派生状态([`crate::db::RICH_*`]);[`crate::ipc::RICH_UNKNOWN`] = 这条
    /// 不在图库数据库里(非媒体文件、或还没落库),与「缩略图还没轮到生成」不是一回事。
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
    /// 快查是 L1(内存文件名索引)的东西,它看得见全盘;图库只收媒体,
    /// 硬排除目录根本不纳入图库。这一列把两者的差集讲清楚,免得用户以为
    /// 「搜到了但打不开缩略图」是坏了。
    fn rich_label(&self) -> &'static str {
        if self.hit.is_dir {
            return "—";
        }
        // 常量不能带 `as` 当模式用,所以把被匹配的值转成 i32(db 侧的原类型)。
        match i32::from(self.rich_state) {
            crate::db::RICH_OUT => t("待生成缩略图"),
            crate::db::RICH_PENDING => t("排队中"),
            crate::db::RICH_DONE => t("已就绪"),
            crate::db::RICH_BAD => t("读取失败"),
            _ => t("非图库文件"),
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
    Down { gen: u64, why: String },
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
                        if res_tx
                            .send(SearchResp::Down {
                                gen: req.gen,
                                why: e,
                            })
                            .is_err()
                        {
                            return;
                        }
                        continue;
                    }
                }
                let c = client.as_mut().expect(t("上面刚连上"));
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
                        let why = t_with(
                            "服务对 Search 回了意外答复:{other:?}",
                            &[("other", format!("{other:?}"))],
                        );
                        if res_tx.send(SearchResp::Down { gen: req.gen, why }).is_err() {
                            return;
                        }
                        continue;
                    }
                    Err(e) => {
                        client = None;
                        if res_tx
                            .send(SearchResp::Down {
                                gen: req.gen,
                                why: e,
                            })
                            .is_err()
                        {
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
        .expect(t("启动检索线程失败"));
    (req_tx, res_rx, h)
}

/// 明细列表专用的大小格式:统一 MB 口径(用户要求不再 KB/MB 混排)。
/// 不足 0.1 MB 的文件保留两位小数,其余一位小数。
fn fmt_size_mb(bytes: u64) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb < 0.1 {
        format!("{mb:.2} MB")
    } else {
        format!("{mb:.1} MB")
    }
}

/// `photos.mtime` 的 Rust 侧归一(与 SQL 里的 MTIME_SQL 同一阈值口径):
/// 历史/扫描器混着写秒、毫秒、微秒,统一折成 UNIX 秒供 fmt_mtime 用。
fn norm_mtime_secs(raw: i64) -> u64 {
    let v = if raw >= 100_000_000_000_000 {
        raw / 1_000_000
    } else if raw >= 100_000_000_000 {
        raw / 1_000
    } else {
        raw
    };
    v.max(0) as u64
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
        t_with(
            "{d} 天 {h} 小时",
            &[("d", d.to_string()), ("h", h.to_string())],
        )
    } else if h > 0 {
        t_with(
            "{h} 小时 {m} 分",
            &[("h", h.to_string()), ("m", m.to_string())],
        )
    } else if m > 0 {
        t_with("{m} 分", &[("m", m.to_string())])
    } else {
        t_with("{secs} 秒", &[("secs", secs.to_string())])
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
                let img = crate::thumbs::load_for_preview(Path::new(&p), 128).map(|(img, _)| img);
                if out_tx.send((p, img)).is_err() {
                    return;
                }
            }
        })
        .expect(t("启动搜索缩略图线程失败"));
    (tx, out_rx)
}

/// 用户对口全盘索引服务的意向(设置项 `svc_wanted`)。
///
/// 授权分两档:装 or 不装。装服务只过一次 UAC,
/// 之后由 SCM 自动拉起,无需每次单独提权。
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

/// 首启范围对话框的两档(全盘完整扫描 / 仅常见目录)。
///
/// 这一档决定的是**派生**作用域,不是库的作用域:两档都全盘发现、全盘入库,
/// 区别只在"哪些目录的图要出缩略图 / 读 EXIF"。没进作用域的行结构列齐全、
/// 派生列全空,网格里显示成占位,点进去才补。
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

/// 缩略图纹理缓存上限。每张 256×256 RGBA 纹理约 256KB,
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

/// 分类相簿一次展示的上限,网格与选择/导出范围共用同一口径。
const CATEGORY_LIMIT: usize = 2000;

/// 标签视图的照片范围:库内检索 > AI 分类 > 手动标签,与界面展示同一优先级。
fn tags_scope_ids(
    store: &Store,
    query: &str,
    query_hits: &[i64],
    category: Option<&str>,
    tag: Option<&str>,
) -> Vec<i64> {
    #[cfg(feature = "ai")]
    {
        if !query.trim().is_empty() {
            return query_hits.to_vec();
        }
        if let Some(node) = category {
            let ver = crate::ai_queue::stage_version(store, crate::ai_queue::Stage::Tags);
            return crate::ai_tags::category_ids(store, node, &ver, CATEGORY_LIMIT);
        }
    }
    #[cfg(not(feature = "ai"))]
    let _ = (query, query_hits, category);
    tag.and_then(|t| timeline::photos_by_tag(store, t).ok())
        .unwrap_or_default()
}

/// 「隐藏低质量」开关对应的质量下限。
#[cfg(feature = "ai")]
const FACE_LOW_QUALITY: f32 = 0.4;

/// 人脸筛选开关行:返回两个开关的响应,任一变化都要重算右侧列表。
#[cfg(feature = "ai")]
fn face_filter_row(
    ui: &mut egui::Ui,
    hide_low: &mut bool,
    show_ignored: &mut bool,
) -> (egui::Response, egui::Response) {
    let a = ui.checkbox(hide_low, t("隐藏低质量"));
    let b = ui.checkbox(show_ignored, t("显示已忽略"));
    (a, b)
}

/// 清除人脸数据的不可撤销警示:重建与切换方案两处共用,红字点明会丢人工整理。
#[cfg(feature = "ai")]
fn face_wipe_warning(ui: &mut egui::Ui, action: &str) {
    ui.colored_label(
        egui::Color32::LIGHT_RED,
        t_with(
            "「{p1}」不可撤销:人物命名与合并/拆分判定会一并删除,需重新人工整理。",
            &[("p1", action.to_string())],
        ),
    );
}

/// 把标签全路径展开成层级行(深度, 末段名, 全路径, 照片数, 是否真实标签)。
fn tag_tree_rows(tags: &[timeline::TagInfo]) -> Vec<(usize, String, String, usize, bool)> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<String, (usize, usize, bool)> = BTreeMap::new();
    for t in tags {
        let segs: Vec<&str> = t.name.split('/').collect();
        let mut path = String::new();
        for (i, seg) in segs.iter().enumerate() {
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(seg);
            let last = i + 1 == segs.len();
            map.entry(path.clone())
                .and_modify(|e| {
                    if last {
                        e.1 = t.count;
                        e.2 = true;
                    }
                })
                .or_insert((i, if last { t.count } else { 0 }, last));
        }
    }
    map.into_iter()
        .map(|(path, (depth, count, real))| {
            let label = path.rsplit('/').next().unwrap_or(path.as_str()).to_string();
            (depth, label, path, count, real)
        })
        .collect()
}

/// 颜色键对应的显示名(存储仍用英文键)。
fn tag_color_name(key: &str) -> &'static str {
    match key {
        "red" => "红色",
        "orange" => "橙色",
        "yellow" => "黄色",
        "green" => "绿色",
        "blue" => "蓝色",
        "purple" => "紫色",
        "pink" => "粉色",
        "black" => "黑色",
        "white" => "白色",
        "gray" => "灰色",
        _ => "",
    }
}

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

/// path 是否落在 dir 的子树里(含 dir 本身)。与 folder_of 同口径:
/// 两边都是扫描器写库的规范化路径,按字节前缀 + 分隔符比对即可。
/// 用于「点上级目录 = 显示子树全部照片」的递归聚合。
fn path_under_dir(path: &str, dir: &str) -> bool {
    let d = dir.trim_end_matches(['\\', '/']);
    if d.is_empty() {
        return true;
    }
    // 一律按字节切片比对:`&path[..d.len()]` 会在
    // 截断字节恰好落进路径后半段某个多字节字符中间时 panic
    // (目录名字节长度撞上别的路径里的汉字,点文件夹必闪退)。
    let (p, b) = (path.as_bytes(), d.as_bytes());
    if p.len() < b.len() {
        return false;
    }
    let (head, tail) = p.split_at(b.len());
    head == b && (tail.is_empty() || tail[0] == b'\\' || tail[0] == b'/')
}

/// 两个目录路径是否指向同一处(大小写不敏感,容忍尾部斜杠)。
fn same_dir(a: &Path, b: &Path) -> bool {
    let na = a.to_string_lossy();
    let nb = b.to_string_lossy();
    na.trim_end_matches(['\\', '/'])
        .eq_ignore_ascii_case(nb.trim_end_matches(['\\', '/']))
}

/// 导入页左栏分组小标题(我的文件夹 / 设备 / 我的电脑)。
fn muted_title(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(12.0)
            .color(crate::theme::p().muted)
            .strong(),
    );
}

/// 导入源快捷行:图标 + 名称,选中态高亮,返回是否点击。
fn import_source_row(
    ui: &mut egui::Ui,
    icon: &str,
    label: &str,
    path: &Path,
    cur: Option<&Path>,
) -> bool {
    let active = cur.is_some_and(|c| same_dir(c, path));
    ui.horizontal(|ui| {
        ui.add(toolbar_icon(icon));
        ui.selectable_label(active, egui::RichText::new(label).color(crate::theme::p().text))
            .on_hover_text(path.display().to_string())
            .clicked()
    })
    .inner
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
/// 关键:原实现 `image::open(path)` 直接解原图,网格每帧每个可见格子
/// 都重解码一次 JPEG,是帧率杀手。现在只读缓存里的小图。
///
/// 按档位读(`tier`)。不同档位落在不同目录
/// (三张缩略图缓存表)。
fn thumb_tex(
    ctx: &egui::Context,
    store: &crate::db::Store,
    id: i64,
    tier: crate::thumbs::Tier,
) -> Option<egui::TextureHandle> {
    // 自愈装载:缓存文件损坏(解不开)时删坏文件、摘台账,后台回填重生;
    // 文件不存在则返回 None,走「未生成」的正常路径。
    let img = crate::thumbs::load_tier_repair(store, tier, id)?;
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

/// 网格预取的行数(视口上下各这么多行)。
///
/// 预取线程口径:滚到哪就把前后几行的缩略图先备好。
/// 只吃当帧剩余的解码配额,不与真正可见的格子抢时间。
const PREFETCH_ROWS: usize = 2;

/// 一帧最多放行几张「缺缓存」的图:一批一次事务,几十张也只是几十行 UPDATE。
const MAX_ADOPT_PER_FRAME: usize = 64;

/// 按张放行队列的长度上限:快速滚动一屏可能铺出上百格,排太长的
/// 下一帧继续消化;超出的不登记去重,那一格下一帧还会再来敲门。
const MAX_ADOPT_QUEUE: usize = 256;

/// 一帧最多上报几个「屏幕上的目录」给后台 worker(显示优先的清单长度上限)。
const MAX_VISIBLE_DIRS: usize = 32;

/// 把任意长宽比的缩略图裁成正方形(取中间部分)。
/// 图库的网格不再用它(保留宽高比),但放大态的预览格仍需要方图。
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

/// 首页虚拟相册的成员上限(超出淘汰最旧)。
const RECENT_LIMIT: usize = 250;

/// 「最近更新」两次重算的最小间隔:补图洪峰期每批都查库会抢写锁。
const RECENT_REFRESH_GAP: std::time::Duration = std::time::Duration::from_secs(2);

/// 相册头日期:`YYYY-MM-DD` → `YYYY年M月D日`;没有 EXIF 日期时原样返回空串。
fn cn_date(s: &str) -> String {
    let p: Vec<&str> = s.split('-').collect();
    if p.len() == 3 {
        let m = p[1].parse::<u32>().unwrap_or(0);
        let d = p[2].parse::<u32>().unwrap_or(0);
        if m > 0 && d > 0 {
            return t_with(
                "{p1}年{p2}月{p3}日",
                &[
                    ("p1", (p[0]).to_string()),
                    ("p2", (m).to_string()),
                    ("p3", (d).to_string()),
                ],
            );
        }
    }
    s.to_string()
}

/// 行式缩略图布局的一行:每项 (rows 下标, 该格宽) 与本行统一高。
/// 不把照片裁成正方形:同行等高,各自按宽高比取宽,行尾贴近右边界。
struct ThumbRow {
    cells: Vec<(usize, f32)>,
    h: f32,
}

/// 结算一行:按可用宽度等比缩放整行(行高同步缩放,宽高比不变)。
fn finish_thumb_row(cur: &[(usize, f32)], used: f32, avail: f32, base: f32, gap: f32) -> ThumbRow {
    let gaps = gap * cur.len().saturating_sub(1) as f32;
    let f = ((avail - gaps) / (used - gaps).max(1.0)).clamp(0.72, 1.4);
    ThumbRow {
        cells: cur.iter().map(|&(i, w)| (i, w * f)).collect(),
        h: base * f,
    }
}

/// 把一批照片排成「等高、行内不等宽」的行。宽高比取 EXIF 像素,
/// 缺尺寸信息的按正方形;末行不撑满,按基准高度左对齐。
fn thumb_rows(
    rows: &[PhotoRow],
    cells: &[usize],
    avail: f32,
    base: f32,
    gap: f32,
) -> Vec<ThumbRow> {
    let mut out: Vec<ThumbRow> = Vec::new();
    let mut cur: Vec<(usize, f32)> = Vec::new();
    let mut used = 0.0f32;
    let max_w = (avail - gap * 2.0).max(base);
    for &i in cells {
        let ar = match (rows[i].width, rows[i].height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => (w as f32 / h as f32).clamp(0.55, 2.4),
            _ => 1.0,
        };
        let w = (base * ar).min(max_w);
        let need = used + if cur.is_empty() { 0.0 } else { gap } + w;
        if !cur.is_empty() && need > avail {
            out.push(finish_thumb_row(&cur, used, avail, base, gap));
            cur.clear();
            used = 0.0;
        }
        if !cur.is_empty() {
            used += gap;
        }
        used += w;
        cur.push((i, w));
    }
    if !cur.is_empty() {
        out.push(ThumbRow {
            cells: std::mem::take(&mut cur),
            h: base,
        });
    }
    out
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
    resp.on_hover_text(if on {
        crate::i18n::t("取消星标")
    } else {
        crate::i18n::t("加星标")
    });
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
    ("list", include_bytes!("../assets/icons/light/list.svg")),
    ("funnel", include_bytes!("../assets/icons/light/funnel.svg")),
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
    (
        "refresh",
        include_bytes!("../assets/icons/light/refresh.svg"),
    ),
    (
        "square",
        include_bytes!("../assets/icons/light/square.svg"),
    ),
    (
        "square-check",
        include_bytes!("../assets/icons/light/square-check.svg"),
    ),
    (
        "zoom-in",
        include_bytes!("../assets/icons/light/zoom-in.svg"),
    ),
    (
        "zoom-out",
        include_bytes!("../assets/icons/light/zoom-out.svg"),
    ),
    (
        "maximize",
        include_bytes!("../assets/icons/light/maximize.svg"),
    ),
    (
        "rotate-ccw",
        include_bytes!("../assets/icons/light/rotate-ccw.svg"),
    ),
    (
        "rotate-cw",
        include_bytes!("../assets/icons/light/refresh.svg"),
    ),
    ("scan", include_bytes!("../assets/icons/light/scan.svg")),
    ("pause", include_bytes!("../assets/icons/light/pause.svg")),
    ("film", include_bytes!("../assets/icons/light/film.svg")),
    ("undo-2", include_bytes!("../assets/icons/light/undo-2.svg")),
    ("redo-2", include_bytes!("../assets/icons/light/redo-2.svg")),
    ("check", include_bytes!("../assets/icons/light/check.svg")),
    (
        "arrow-left-right",
        include_bytes!("../assets/icons/light/arrow-left-right.svg"),
    ),
    ("save", include_bytes!("../assets/icons/light/save.svg")),
    ("copy", include_bytes!("../assets/icons/light/copy.svg")),
    (
        "clipboard-paste",
        include_bytes!("../assets/icons/light/clipboard-paste.svg"),
    ),
    ("trash-2", include_bytes!("../assets/icons/light/trash-2.svg")),
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
    ("list", include_bytes!("../assets/icons/dark/list.svg")),
    ("funnel", include_bytes!("../assets/icons/dark/funnel.svg")),
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
    (
        "refresh",
        include_bytes!("../assets/icons/dark/refresh.svg"),
    ),
    ("square", include_bytes!("../assets/icons/dark/square.svg")),
    (
        "square-check",
        include_bytes!("../assets/icons/dark/square-check.svg"),
    ),
    ("zoom-in", include_bytes!("../assets/icons/dark/zoom-in.svg")),
    (
        "zoom-out",
        include_bytes!("../assets/icons/dark/zoom-out.svg"),
    ),
    (
        "maximize",
        include_bytes!("../assets/icons/dark/maximize.svg"),
    ),
    (
        "rotate-ccw",
        include_bytes!("../assets/icons/dark/rotate-ccw.svg"),
    ),
    (
        "rotate-cw",
        include_bytes!("../assets/icons/dark/refresh.svg"),
    ),
    ("scan", include_bytes!("../assets/icons/dark/scan.svg")),
    ("pause", include_bytes!("../assets/icons/dark/pause.svg")),
    ("film", include_bytes!("../assets/icons/dark/film.svg")),
    ("undo-2", include_bytes!("../assets/icons/dark/undo-2.svg")),
    ("redo-2", include_bytes!("../assets/icons/dark/redo-2.svg")),
    ("check", include_bytes!("../assets/icons/dark/check.svg")),
    (
        "arrow-left-right",
        include_bytes!("../assets/icons/dark/arrow-left-right.svg"),
    ),
    ("save", include_bytes!("../assets/icons/dark/save.svg")),
    ("copy", include_bytes!("../assets/icons/dark/copy.svg")),
    (
        "clipboard-paste",
        include_bytes!("../assets/icons/dark/clipboard-paste.svg"),
    ),
    ("trash-2", include_bytes!("../assets/icons/dark/trash-2.svg")),
];

/// 内置更新日志全文,「帮助 > 关于」里以 Markdown 渲染。
const CHANGELOG_MD: &str = include_str!("../CHANGELOG.md");

/// 「关于」窗口用的更新日志:去掉文件头的 H1 标题与说明行(窗口里已另有
/// 「更新日志」标题,避免重复显示),直接从第一个版本段开始。
fn about_changelog_md() -> &'static str {
    static MD: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    MD.get_or_init(|| {
        let start = CHANGELOG_MD.find("\n## ").map(|i| i + 1).unwrap_or(0);
        &CHANGELOG_MD[start..]
    })
}

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

/// 指定描边色的文件夹图标(灰 = 默认目录,蓝 = 导入目录)。
fn folder_icon_colored(color: egui::Color32) -> egui::Image<'static> {
    let (_, bytes) = icon_source("folder");
    let svg = std::str::from_utf8(bytes).unwrap_or("");
    let hex = format!("#{:02X}{:02X}{:02X}", color.r(), color.g(), color.b());
    let mut recolored = svg.to_string();
    if let Some(i) = svg.find("stroke=\"") {
        let s = i + 8;
        if let Some(e) = svg[s..].find('"') {
            recolored = format!("{}{}{}", &svg[..s], hex, &svg[s + e..]);
        }
    }
    let half = if crate::theme::p().is_light { "l" } else { "d" };
    egui::Image::from_bytes(
        format!("bytes://folder-{half}-{}.svg", &hex[1..]),
        recolored.into_bytes(),
    )
    .fit_to_exact_size(egui::vec2(18.0, 18.0))
}

/// 组头图标(大一号的目录图标)。
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

/// 统一的图标动作按钮:悬停显示操作说明,返回是否点击。
fn icon_action(ui: &mut egui::Ui, icon: &str, tip: &str, enabled: bool) -> bool {
    ui.add_enabled(enabled, tool_button(icon, false))
        .on_hover_text(tip)
        .on_disabled_hover_text(tip)
        .clicked()
}

/// 统一的图标开关按钮:选中态高亮,悬停显示操作说明,返回是否点击。
fn icon_toggle(ui: &mut egui::Ui, icon: &str, tip: &str, selected: bool) -> bool {
    ui.add(tool_button(icon, selected)).on_hover_text(tip).clicked()
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
/// 用块文件 + 纹理缓存,不会随浏览量无限增长;
/// LRU 超出上限时淘汰最久未用的条目。
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

    /// 清空全部纹理(切换档位时调用)。
    ///
    /// 必须全清 —— 旧档的纹理内容尺寸不同,复用会导致显示错档;
    /// `drop` 掉 `TextureHandle` 会让 egui 真正释放显存。
    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.pending.clear();
    }

    /// 调整 LRU 上限(切大档时按显存预算下调)。
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
    /// 初次设置向导配色:推荐档绿底(服务说明页与照片范围页共用)。
    const GRANT_GREEN: egui::Color32 = egui::Color32::from_rgb(0x2E, 0x8B, 0x47);
    /// 拒绝档深蓝底。
    const GRANT_BLUE_DARK: egui::Color32 = egui::Color32::from_rgb(0x2C, 0x5F, 0x8A);

    pub fn new(cc: &eframe::CreationContext<'_>, store: Arc<Store>) -> Self {
        // 配色:默认灰色,可切蓝黑(设置项 ui_theme,入口在 选项 > 常规)。
        let ui_theme = store
            .get_setting("ui_theme")
            .and_then(|v| crate::theme::Kind::from_key(&v))
            .unwrap_or(crate::theme::Kind::Gray);
        crate::theme::set_kind(ui_theme);
        crate::theme::apply(&cc.egui_ctx);
        // 界面语言:默认中文,可切英文(设置项 ui_lang,入口在 选项 > 常规)。
        let ui_lang = store
            .get_setting("ui_lang")
            .and_then(|v| crate::i18n::Lang::from_key(&v))
            .unwrap_or(crate::i18n::Lang::Zh);
        crate::i18n::set_lang(ui_lang);
        // SVG 图标加载器(工具栏图标等),必须在首帧前装好。
        egui_extras::install_image_loaders(&cc.egui_ctx);
        load_cjk_font(&cc.egui_ctx);
        // 自定义裁剪比例 / 缩略图说明开关在 move store 之前读取。
        let custom_ratios = Self::load_custom_ratios(&store);
        let thumb_captions = store
            .get_setting("thumb_captions")
            .map(|v| v != "0")
            .unwrap_or(false);
        // 缺省树形;显式选过平铺的库存的是 "0",不受影响。
        let folder_tree = store
            .get_setting("folder_tree")
            .map(|v| v != "0")
            .unwrap_or(true);
        // 左栏缩略图:默认在库里显示。
        let tree_thumbs = store
            .get_setting("tree_thumbs")
            .map(|v| v != "0")
            .unwrap_or(true);
        // 单个文件夹视图的呈现形态(缺省缩略图网格,记住上次的详细/网格选择)。
        let folder_details = store
            .get_setting("folder_details")
            .map(|v| v == "1")
            .unwrap_or(false);
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
        // 排序与视图过滤同属"怎么看图库",重启后保持。
        let sort_by = store
            .get_setting("sort_by")
            .and_then(|v| timeline::SortBy::from_key(&v))
            .unwrap_or_default();
        let filters =
            timeline::Filters::from_setting(&store.get_setting("view_filters").unwrap_or_default());
        let sort_rev = store.get_setting("sort_rev").as_deref() == Some("1");
        // 邮件默认尺寸从设置读(选项 > 电子邮件;默认 1024)。
        let email_size = crate::share::ShareSize::from_px(
            store
                .get_setting("email_size_px")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1024),
        );
        #[cfg(feature = "ai")]
        let face_scope_common = crate::faces::scan_scope_common(&store);
        Self {
            store,
            rows: Vec::new(),
            months: Vec::new(),
            folders: Vec::new(),
            selected_folder: 0,
            view: ViewKind::Recent,
            recent_idxs: Vec::new(),
            recent_date: String::new(),
            recent_dirty: false,
            recent_at: std::time::Instant::now(),
            texs: TexCache::new(TEX_CACHE_CAP),
            thumb_bg: None,
            #[cfg(feature = "ai")]
            ai_sched: None,
            #[cfg(feature = "ai")]
            ai_preview_for: None,
            #[cfg(feature = "ai")]
            ai_stats_cache: None,
            #[cfg(feature = "ai")]
            ai_dl: None,
            #[cfg(feature = "ai")]
            ai_dl_queue: Vec::new(),
            #[cfg(feature = "ai")]
            ai_models_msg: String::new(),
            #[cfg(feature = "ai")]
            ai_models_cache: None,
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
            status: t("就绪").into(),
            dups: Vec::new(),
            dup_near: false,
            id_row: HashMap::new(),
            preview: None,
            preview_tex: None,
            anim: None,
            anim_rx: None,
            anim_req_for: None,
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
            sel_anchor: None,
            sort_by,
            sort_rev,
            filters,
            thumb_tier: crate::thumbs::Tier::Normal,
            tags: Vec::new(),
            sel_tag: None,
            tag_rename: None,
            tag_delete: None,
            tag_batch_open: false,
            tag_batch_input: String::new(),
            tag_batch_ids: Vec::new(),
            #[cfg(feature = "ai")]
            ai_enabled: false,
            #[cfg(feature = "ai")]
            sel_category: None,
            #[cfg(feature = "ai")]
            ai_category_cache: None,
            #[cfg(feature = "ai")]
            ai_query: String::new(),
            #[cfg(feature = "ai")]
            ai_query_hits: Vec::new(),
            show_about: false,
            about_cache: Default::default(),
            show_shortcuts: false,
            updater: crate::update::Updater::new(),
            update_confirm: false,
            #[cfg(feature = "ai")]
            models: crate::models::ModelDownloader::new(),
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
            zoom_failed_for: None,
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
            folder_details,
            folder_filter: String::new(),
            folder_filter_owner: String::new(),
            subtree_sel: None,
            tree_thumbs,
            folder_tree_cache: FolderTree::default(),
            tree_open: HashSet::new(),
            tree_texs: HashMap::new(),
            nav_open_drawer: None,
            page: PageKind::Library,
            nav_hist: Vec::new(),
            nav_at: 0,
            import_src: None,
            import_entries: Vec::new(),
            import_sel: BTreeSet::new(),
            last_import_src: String::new(),
            import_tree_open: HashSet::new(),
            import_tree_kids: HashMap::new(),
            scan_verb: t("扫描"),
            people_sort,
            album_new_win: false,
            pending_thumbs: 0,
            wallpaper_rx: None,
            zoom_tex: None,
            zoom_active_for: None,
            #[cfg(feature = "ai")]
            face_rx: None,
            #[cfg(feature = "ai")]
            face_cancel: None,
            #[cfg(feature = "ai")]
            face_prog: (0, 0),
            #[cfg(feature = "ai")]
            face_scope_common,
            #[cfg(feature = "ai")]
            face_resume_done: false,
            #[cfg(feature = "ai")]
            face_people: Vec::new(),
            #[cfg(feature = "ai")]
            sel_cluster: None,
            #[cfg(feature = "ai")]
            sel_person: None,
            #[cfg(feature = "ai")]
            face_hits: Vec::new(),
            #[cfg(feature = "ai")]
            face_similar: Vec::new(),
            #[cfg(feature = "ai")]
            face_similar_for: None,
            #[cfg(feature = "ai")]
            face_similar_thr: 0.35,
            #[cfg(feature = "ai")]
            face_sim_rx: None,
            #[cfg(feature = "ai")]
            face_sim_pending: false,
            #[cfg(feature = "ai")]
            recluster_rx: None,
            #[cfg(feature = "ai")]
            face_sugs: Vec::new(),
            #[cfg(feature = "ai")]
            face_merge_target: None,
            #[cfg(feature = "ai")]
            face_hide_low: false,
            #[cfg(feature = "ai")]
            face_show_ignored: false,
            #[cfg(feature = "ai")]
            face_version_warn: false,
            #[cfg(feature = "ai")]
            face_algo_pending: None,
            // 默认全勾:勾选框每帧会按「文件是否已在位」收敛。
            #[cfg(feature = "ai")]
            model_pick: [true; 3],
            #[cfg(feature = "ai")]
            face_diag: None,
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
            clip_image_query: None,
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
            wizard_page: 0,
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
            editor_tab: EditorTab::default(),
            editor_props: true,
            editor_film: true,
            editor_hist: None,
            editor_exif: None,
            editor_thumbs: HashMap::new(),
            editor_compare: 0,
            editor_orig_tex: None,
            editor_props_tab: 0,
            editor_tool: None,
            editor_tool_amount: 50.0,
            editor_for: None,
            editor_zoom_cmd: 0,
        }
    }

    pub fn start_initial_tasks(&mut self, ctx: egui::Context) {
        // 快速查找后台线程(即输即搜:走管道问索引服务,毫秒级回命中)。
        let (stx, srx, _) = spawn_search_worker();
        self.search_tx = Some(stx);
        self.search_rx = Some(srx);
        // 「不用全盘快查」的意向要先于链路线程落地:线程一起来就会探管道。
        SVC_LINK_ENABLED.store(
            self.svc_want() != SvcWant::No,
            std::sync::atomic::Ordering::Relaxed,
        );
        // 索引服务链路线程(心跳问 Status、掉线重连、代办装卸与重建)。
        let (ltx, lrx) = spawn_link_worker(ctx.clone());
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
        // AI 调度器:默认暂停,由「工具 > 继续 AI 索引」或选项页开启。
        #[cfg(feature = "ai")]
        {
            let enabled = self
                .store
                .get_setting("ai_enabled")
                .map(|v| v != "0")
                .unwrap_or(false);
            self.ai_enabled = enabled;
            self.ai_sched = Some(crate::ai_sched::AiSched::spawn(self.store.clone(), enabled));
        }
        // 首启补图顺序:先把常用目录(我的文档/我的图片/桌面)铺出来,
        // 排空后才按「最新照片优先」兜底其余全库 —— 即使向导答的是
        // 「全盘」,眼前的常用目录也是第一批出图,洪峰在后台慢慢补。
        // 用户切进别的目录时会整体替换这份清单(见 publish_visible)。
        if let Some(w) = &self.thumb_bg {
            w.publish_visible(
                crate::db::default_roots()
                    .into_iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect(),
            );
        }
        // 预览后台 worker(解码 + 落盘 previews/ 缓存,不阻塞 UI)
        self.preview_bg = Some(crate::thumbs::spawn_preview_worker(self.store.clone()));
        // 启动时静默导入新照片的 XMP sidecar(外部工具写的星标/标签,DB 权威不覆盖)
        self.start_xmp_import(false);
        // 监听文件夹(事件驱动 ReadDirectoryChangesW,不可用时自动降级轮询)
        let w = crate::watch::spawn(self.store.clone(), self.watch_on.clone());
        self.watch_rx = Some(w.events);
        self.watch_mode = Some(w.mode);
        // 设备热插拔监听(WM_DEVICECHANGE 链路)。
        // 新接入的相机/手机/可移动盘 → 自动扫描该卷。
        self.device_watch = Some(crate::device::spawn_device_watcher());
        self.last_volumes = crate::device::removable_volumes();
        self.rebuild();
        // 人脸模型就位检查:缺件时提示可下载(不替用户花流量,点了才联网)。
        #[cfg(feature = "ai")]
        self.check_face_models();
        // 上次中断的扫描(持久化队列残留)自动续扫。
        // 正常完成/取消都会出队;本项目改用持久化队列,
        // 进程中途退出后仍可在下次启动时继续。
        let pending = self.store.pending_scans();
        if !pending.is_empty() {
            let roots: Vec<PathBuf> = pending.iter().map(|(r, _)| PathBuf::from(r)).collect();
            let scope = if pending.len() == 1 {
                t_with("续扫 {p1}", &[("p1", (pending[0].0).to_string())])
            } else {
                t_with(
                    "续扫 {p1} 个上次中断的目录",
                    &[("p1", (pending.len()).to_string())],
                )
            };
            self.start_scan_roots(roots, &scope);
        } else if self.store.scope_initialized() {
            // 启动增量对账(删除侧):程序关着的时候被外部删掉的照片,这里补摘。
            // 范围 = 全部监听根(含手动加的文件夹);根离线(盘没挂/目录没了)
            // 整根跳过,不会误清。增量扫描对未变的文件只 stat 不解码,开销可忽略;
            // 摘行后同线程清掉对应缩略图缓存。首扫 / 续扫已占用扫描线程时不重复发。
            let roots = self.store.list_watched();
            if !roots.is_empty() {
                self.start_scan_roots_ext(roots, t("启动对账"), true);
            }
        }
        // 缩略图缓存:数据目录迁走后,旧系统缓存要在后台搬到新落点;
        // 孤儿 GC 与台账对账排在它后面串行跑 —— 否则「文件还没搬到、
        // 台账却说丢了」会把整库缩略图重生成一遍。GC 只在硬排除清理
        // 真摘了行、或从来没跑过时做一次大扫。
        {
            let purged = self
                .store
                .hard_purged
                .load(std::sync::atomic::Ordering::Relaxed);
            let need_gc = purged > 0 || self.store.get_setting("thumb_gc_v1").is_none();
            let store = self.store.clone();
            let gctx = ctx.clone();
            std::thread::spawn(move || {
                crate::win::set_current_thread_priority(crate::win::Priority::BelowNormal);
                crate::db::auto_relocate_system_cache();
                if need_gc {
                    let root = crate::db::cache_dir();
                    // 孤儿文件 GC + 台账缺失对账:前者清「文件在、行没了」的
                    // 垃圾,后者救「台账在、文件没了」的灰格子(缓存目录被
                    // 外部清空 / 搬移后的自动恢复)。
                    let _ = crate::thumbs::gc_orphans(&store, &root);
                    let _ = crate::thumbs::gc_missing_tiers(&store, &root);
                    let _ = store.set_setting("thumb_gc_v1", "1");
                }
                gctx.request_repaint();
            });
        }
        // 首次启动先问派生作用域(两档单选、无取消,必须答一次)。
        // 答完才落账 `dir_intent` 并发起首扫;`skip_initial_scan` 偏好短路 ——
        // 明确说过不要首扫的用户按保守档静默落账,不弹窗。
        if !self.store.scope_initialized() {
            if self.store.get_setting("skip_initial_scan").as_deref() == Some("1") {
                self.apply_initial_scope(InitialScope::Limited, false);
            } else {
                self.scope_ask = true;
            }
        }
        // 上次点了「重建全部缩略图」但进程中途退出 → 续做完。
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
            SvcWant::No => {
                // 启动时只核对状态,不主动弹 UAC:卸载是用户刚表达拒绝时
                // 立刻做的事(见 svc_apply_want),每次开机都提权反而骚扰。
                // 若服务还在(上次卸载被取消 / 用户后来手动装回),如实提示。
                if crate::win::service_installed() {
                    self.status = t(
                        "已按你的选择关闭全盘快查;检测到索引服务仍在,可在 选项 > 全盘快查 卸载。",
                    )
                    .into();
                }
            }
        }
    }

    /// 用户对口索引服务的意向(没存过就是「未问过」)。
    fn svc_want(&self) -> SvcWant {
        SvcWant::from_setting(
            self.store
                .get_setting("svc_wanted")
                .as_deref()
                .unwrap_or(""),
        )
    }

    /// 打开授权说明窗(「不再提示」勾选每次打开时复位)。
    #[cfg(windows)]
    fn svc_open_grant(&mut self) {
        self.svc_ask_grant = true;
        self.svc_never_ask = false;
    }

    /// 首启范围落账:写 `dir_intent` + 打标记,按需发起首扫。
    ///
    /// `scan=false` 用于 `skip_initial_scan` 偏好短路 —— 作用域照样落账
    /// (不落账派生门是空的,谁都不出缩略图),只是不发起扫描。
    /// 授权说明窗正文(向导第 1 页):讲清楚要干什么、要什么权限、能不能撤,
    /// 两档决定写进 `decided`;勾选态留在 `svc_never_ask` 供窗口层落库。
    #[cfg(windows)]
    fn svc_grant_body(&mut self, ui: &mut egui::Ui, decided: &mut Option<SvcWant>) {
        ui.label(t(
            "「快速查找」要列出全盘所有文件名,数据来自 NTFS 卷的 USN/MFT 日志。",
        ));
        ui.label(t(
            "读卷日志属于管理员级操作,所以要装一个后台服务来干这件事 —— 这需要你点一次 UAC。",
        ));
        ui.indent("svc_grant_what", |ui| {
            ui.label(t("· 服务名 RelytoMaliangIndex,开机自动启动,只在本机跑;"));
            ui.label(t("· 只读文件名、大小与时间戳,不读文件内容、不改任何文件;"));
            ui.label(t(
                "· 产物只有一个本地索引文件 nameindex.bin(存在数据目录里);",
            ));
            ui.label(t("· 界面本身不提权:它通过本地命名管道向服务问结果;"));
            ui.label(t("· 不联网、不上传,随时可以在 选项 > 全盘快查 卸载。"));
        });
        ui.add_space(14.0);
        // 两档:按钮定宽对齐,说明文字跟在右侧,行距拉开。
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new(t("安装并启动服务(推荐)"))
                            .color(egui::Color32::WHITE)
                            .strong(),
                    )
                    .fill(Self::GRANT_GREEN)
                    .min_size(egui::vec2(200.0, 30.0))
                    .corner_radius(6),
                )
                .clicked()
            {
                *decided = Some(SvcWant::Yes);
            }
            ui.small(t(
                "只有这次弹 UAC。装好后开机自启、持续保鲜索引,以后不再问。",
            ));
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new(t("不用全盘快查")).color(egui::Color32::WHITE),
                    )
                    .fill(Self::GRANT_BLUE_DARK)
                    .min_size(egui::vec2(200.0, 30.0))
                    .corner_radius(6),
                )
                .clicked()
            {
                *decided = Some(SvcWant::No);
            }
            ui.small(t("跳过:若已装过服务会一并移除(需一次 UAC);查找只覆盖图库里的照片,发现与保鲜退回逐目录扫描,以后可在 选项 > 全盘快查 改。"));
        });
        ui.add_space(14.0);
        ui.checkbox(&mut self.svc_never_ask, t("不再提示"));
        ui.small(t("嫌每次启动都问就勾上:选「不用全盘快查」或直接关窗,以后启动不再询问,老老实实走传统扫描;菜单和页面入口仍可随时启用。"));
    }

    /// 首启范围窗正文(向导第 2 页):两档单选,**无取消** —— 必须答一次;
    /// 空账本下派生门谁都不放行,网格全是占位,所以这一页没有跳过。
    fn scope_ask_body(&mut self, ui: &mut egui::Ui, decided: &mut Option<InitialScope>) {
        ui.label(t(
            "整台电脑的文件名都会收进索引 —— 那一步不读文件内容,很快。",
        ));
        ui.label(t(
            "下面这一档只决定**哪些目录要出缩略图、读拍摄信息**,不影响能不能搜到。",
        ));
        ui.add_space(12.0);
        ui.radio_value(&mut self.scope_choice, InitialScope::Complete, t("全盘"));
        ui.indent("scope_complete_why", |ui| {
            ui.small(t(
                "所有磁盘上的图都出缩略图。补齐在后台慢慢跑,眼前的目录优先。",
            ));
        });
        ui.add_space(6.0);
        ui.radio_value(
            &mut self.scope_choice,
            InitialScope::Limited,
            t("只常用目录"),
        );
        ui.indent("scope_limited_why", |ui| {
            ui.small(t("只给「我的文档 / 我的图片 / 桌面」出缩略图。别处的图照样在库里、照样搜得到,点进那个目录时再补。"));
        });
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new(t("确定"))
                            .color(egui::Color32::WHITE)
                            .strong(),
                    )
                    .fill(Self::GRANT_GREEN)
                    .min_size(egui::vec2(120.0, 30.0))
                    .corner_radius(6),
                )
                .clicked()
            {
                *decided = Some(self.scope_choice);
            }
            ui.small(t("以后可在 选项 > 全盘快查 改。"));
        });
    }

    fn apply_initial_scope(&mut self, choice: InitialScope, scan: bool) {
        let roots = self
            .store
            .apply_initial_scope(choice == InitialScope::Complete);
        self.scope_ask = false;
        self.scope_choice = choice;
        if roots.is_empty() {
            return;
        }
        // 落账改变了派生门:已入库的行按新作用域重算一遍 rich_state。
        let gate = crate::db::ScopeGate::load(&self.store);
        let _ = self.store.reconcile_rich_state(&gate);
        if !scan {
            self.status = t("已按偏好跳过首扫;派生作用域已落账,可在文件夹管理器里手动扫。").into();
            return;
        }
        let label = match choice {
            InitialScope::Complete => t("全盘"),
            InitialScope::Limited => t("常用目录"),
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

    /// 停→同步数据目录标记→起:数据目录迁移后让服务立刻搬进新目录。
    #[cfg(windows)]
    fn svc_restart(&mut self) {
        if self.svc_busy {
            return;
        }
        self.svc_busy = true;
        if let Some(tx) = &self.link_tx {
            let _ = tx.send(LinkCmd::Restart);
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
        // 意向一变立刻同步给链路线程,别等到下次启动才生效。
        SVC_LINK_ENABLED.store(want != SvcWant::No, std::sync::atomic::Ordering::Relaxed);
        match want {
            SvcWant::Yes => self.svc_install(),
            SvcWant::No => {
                // 「不用全盘快查」是撤回授权,不只是"这次不弹窗":服务是机器级
                // 自动启动的,留着它就等于继续常驻提权、继续建全盘索引。
                // 所以已经装上的要一并卸载(一次 UAC),并立刻断开现有连接。
                if crate::win::service_installed() {
                    self.status = t("正在移除索引服务…(需要一次 UAC)").into();
                    self.svc_uninstall();
                } else {
                    self.status = t("已关闭全盘快查;发现与保鲜走逐目录扫描。").into();
                }
                if let Some(tx) = &self.link_tx {
                    let _ = tx.send(LinkCmd::Retry);
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
            self.status = t("全盘快查已关闭(在 选项 > 全盘快查 可改)").into();
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
                self.status = t("索引服务在运行,正在重连…").into();
            }
        }
    }

    /// 非 Windows:没有索引服务这回事,入口只负责说明。
    #[cfg(not(windows))]
    fn svc_try_start(&mut self) {
        self.status = t("全盘快查仅 Windows 可用(NTFS USN/MFT 索引服务)").into();
    }

    /// 进入快速查找页时的引导判定。
    ///
    /// 连得上就不打扰;连不上、且用户没明确拒绝过也没勾「不再提示」,才走
    /// [`Self::svc_try_start`] —— 意向未定时它先弹说明窗,不是直接弹 UAC。
    #[cfg(windows)]
    fn svc_nudge_on_search_entry(&mut self) {
        // 明确说过不用的人,进快查页也不该被反复劝装。
        if self.svc_want() == SvcWant::No {
            return;
        }
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
        self.status = t("已请求索引服务全量重建(进度见下方状态)").into();
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
                        // (「2 卷 0 条」就是这么来的)。
                        self.status = t_with(
                            "全盘快查已接上索引服务:{p1} 卷(条数见快查页)",
                            &[("p1", (self.link.vols).to_string())],
                        );
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
                        self.status = t_with(
                            "索引服务断开,已降级为逐目录扫描:{why}",
                            &[("why", why.to_string())],
                        );
                    }
                }
                Ok(LinkMsg::Rebuild(err)) => {
                    self.status = match err {
                        None => t("索引服务已受理全量重建").into(),
                        Some(e) => t_with("请求重建失败:{e}", &[("e", e.to_string())]),
                    };
                }
                Ok(LinkMsg::Service(op, err)) => {
                    self.svc_busy = false;
                    // 「停止/启动/重启」只是运维动作(换二进制、让磁盘歇口、搬目录),
                    // 不代表用户不想要全盘快查 —— 别把它记成拒绝授权。
                    let state_only = matches!(op, "停止" | "启动" | "重启");
                    if state_only {
                        self.status = match (&err, op) {
                            (None, "停止") => {
                                t("索引服务已停止(注册项保留):全盘快查暂不可用,同页可再启动").into()
                            }
                            (None, "重启") => t("索引服务已重启,正在重连…").into(),
                            (None, _) => t("索引服务已启动,正在重连…").into(),
                            (Some(e), _) => t_with(
                                "{op}索引服务未完成:{e}(可在 选项 > 全盘快查 重试)",
                                &[("op", t(op).to_string()), ("e", e.to_string())],
                            ),
                        };
                    } else if op == "卸载" {
                        // 卸载是「不同意」的落地动作。失败只是移除没完成,
                        // 绝不能照安装失败那样把 svc_wanted 清成「未问过」——
                        // 那会让下次启动又弹授权窗、甚至把服务装回来。
                        self.status = match err {
                            None => {
                                t("已移除索引服务:全盘快查关闭,发现与保鲜走逐目录扫描。").into()
                            }
                            Some(e) => t_with("移除索引服务未完成:{e}(可在 选项 > 全盘快查 点「卸载索引服务」重试)", &[("e", e.to_string())]),
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
                                self.status = t_with(
                                    "索引服务未完成:{e}(可在 选项 > 全盘快查 重试)",
                                    &[("e", e.to_string())],
                                );
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
                    self.status = t_with(
                        "快速查找 \"{p1}\"({p2}):{p3} 条命中,耗时 {p4} ms",
                        &[
                            ("p1", (self.name_query.trim()).to_string()),
                            ("p2", t(self.name_class.label()).to_string()),
                            ("p3", (self.name_results.len()).to_string()),
                            ("p4", format!("{p4:.1}", p4 = (ms))),
                        ],
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
    /// 连接失败必须明示降级 —— 静默退化成"搜不到东西"最坑人:
    /// 用户会以为盘上真没有这个文件。图库侧的发现/保鲜另有 walkdir + watch
    /// 兜底,这里丢的只是"全盘按名字快查"这一项能力。
    fn svc_degraded_ui(&mut self, ui: &mut egui::Ui) {
        use crate::win::ServiceState as S;
        let svc = self.link.svc;
        let why = self.link.why.clone();
        let busy = self.svc_busy;
        let want_no = self.svc_want() == SvcWant::No;
        let head = if want_no {
            t("全盘快查已按你的选择关闭")
        } else {
            match svc {
                S::NotInstalled => t("全盘快查未启用(索引服务未安装)"),
                S::Stopped => t("索引服务已停止"),
                S::Transition(_) => t("索引服务正在启动…"),
                S::Running => t("索引服务在运行,但连不上它的管道"),
            }
        };
        ui.horizontal(|ui| {
            // ⚠ 字形默认字体里没有,用橙色加粗文字代替图标(项目里同口径)。
            ui.label(
                egui::RichText::new(head)
                    .color(egui::Color32::from_rgb(230, 145, 56))
                    .strong(),
            );
        });
        ui.small(t("索引服务不可用时,全盘文件与文件夹的查找、以及未导入目录的实时发现都不可用;标签和图中文字请到「标签」页检索。"));
        if !why.is_empty() {
            ui.small(egui::RichText::new(t_with("原因:{why}", &[("why", why.to_string())])).weak());
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            #[cfg(windows)]
            {
                let label = if want_no {
                    t("重新启用全盘快查…")
                } else {
                    match svc {
                        S::NotInstalled => t("安装索引服务…"),
                        S::Stopped | S::Transition(_) => t("重新安装并启动服务…"),
                        S::Running => t("重试连接"),
                    }
                };
                if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                    if want_no {
                        // 主动点回来才算重新授权,直接走 Yes 的装配流程。
                        self.svc_apply_want(SvcWant::Yes);
                    } else {
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
                }
                if busy {
                    ui.spinner();
                    ui.small(t("等待你在 UAC 授权窗口点「是」…"));
                }
            }
            #[cfg(not(windows))]
            ui.small(t("全盘快查仅支持 Windows(NTFS USN/MFT)。"));
        });
        if matches!(svc, S::Stopped | S::Transition(_)) {
            ui.small(t(
                "服务是「自动启动」的:停在这里通常是被手动停过或上次启动失败,重装一次即可恢复。",
            ));
        }
    }

    /// 快速查找视图(自绘 CentralPanel:任意文件/文件夹结果列表)。
    fn search_ui(&mut self, ctx: &egui::Context) {
        // 索引服务链路事件已在 update 顶层统一泵(见 pump_link_events),这里只画。
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading(t("快速查找"));
            // 三层数据必须标来源(仅源码注释,不进 UI)。名字/路径/大小/日期这一路走
            // L1(服务内存里的文件名索引,不经 SQLite);图库条件查询走 L2;派生列只可能
            // 来自 L3。只有「图库」这一列是 L2 的投影,由服务批量查库后随命中一起回。
            ui.separator();

            // 服务连不上 = 全盘快查没有数据源:明示降级,本页只剩说明。
            // 图库内照片仍可从顶栏的图库搜索查(那条路走 SQL,与这里无关)。
            if !self.link.online {
                self.svc_degraded_ui(ui);
                return;
            }
            if self.link.building > 0 {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(t_with("索引服务正在建库({p1} 卷进行中),已建好 {p2} 条 —— 可以先搜,结果会越查越全", &[("p1", (self.link.building).to_string()), ("p2", (self.link.entries).to_string())]));
                });
            }
            for (letter, why) in self.link.compat.clone() {
                ui.small(t_with("{letter}: 盘走兼容扫描,无实时快查({why})", &[("letter", letter.to_string()), ("why", why.to_string())]));
            }
            if self.link.entries == 0 {
                ui.label(t("索引还是空的:服务正在扫第一轮全盘,建好后这里自动可用。"));
                return;
            }

            // 输入框在顶部整条快速查找栏(gsearch),这里只出结果面板;
            // 即输即搜由 gsearch 的 changed() 触发,本页被动刷新。
            if self.name_query.trim().is_empty() {
                ui.label(t("在上方搜索栏输入关键字开始查找(文件与文件夹都搜,大小写不敏感)"));
                return;
            }
            if self.name_results.is_empty() {
                if self.search_inflight {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(t("正在检索…"));
                    });
                } else {
                    ui.label(t("没有命中的文件"));
                }
                return;
            }

            let results = std::mem::take(&mut self.name_results);
            let mut opened: Option<String> = None;
            let mut preview_req: Option<String> = None;
            let mut revealed: Option<String> = None;
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
                    for title in [t("文件名称"), t("路径"), t("大小"), t("修改时间"), t("图库"), t("预览")] {
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
                                // 快查看得见全盘,硬排除目录不进图库 —— 这一列把
                                // 「搜到了为什么网格里没有」讲明白。
                                let label = r.rich_label();
                                let why = match i32::from(r.rich_state) {
                                    crate::db::RICH_OUT => {
                                        t("图库里有这张照片,缩略图还没轮到生成。打开它所在的文件夹就会开始补,不影响查看原图。想让这个文件夹以后优先生成,可以在「工具 → 文件夹管理器」里导入它。")
                                    }
                                    crate::db::RICH_PENDING => t("缩略图正在排队生成。"),
                                    crate::db::RICH_DONE => t("缩略图已就绪,网格里能看到。"),
                                    crate::db::RICH_BAD => {
                                        t("文件读取失败(损坏或无权限),已隔离,不再重试。")
                                    }
                                    _ if r.hit.is_dir => t("文件夹不进图库。"),
                                    _ => t("不是图库收录的媒体类型,或还没被扫描入库。"),
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
                            // 表格行的 response 只是行内控件 response 的并集,
                            // 而单元格里全是 Label 这类不感知点击的控件,
                            // clicked/double_clicked/context_menu 因此永不触发。
                            // 先对整行补一次点击感知,选中/双击/右键才活。
                            let resp = row.response().interact(egui::Sense::click());
                            if resp.clicked() {
                                self.name_sel = Some(h.path.clone());
                            }
                            // 右键出应用内菜单。已入库的媒体命中(photos 表只收
                            // 媒体)直接复用图库网格那份右键菜单 —— 预览/星标/
                            // 相簿/壁纸一应俱全,末尾补两条文件级动作;未入库的
                            // 文件与目录仍是文件菜单。系统原生那份(Shell
                            // IContextMenu)两条路都收在末项里,不丢功能。
                            let mut open_clicked = false;
                            let mut star_clicked = false;
                            let mut preview_clicked = false;
                            let mut ctx_id: Option<i64> = None;
                            resp.context_menu(|ui| {
                                // photo_id_at 是唯一索引点查,只在右键那一帧
                                // 对那一行执行,平时零开销。
                                let lib_id = if h.is_dir {
                                    None
                                } else {
                                    self.store.photo_id_at(&h.path)
                                };
                                if let Some(pid) = lib_id {
                                    let (star, kind) = self
                                        .store
                                        .photo_brief_by_id(pid)
                                        .unwrap_or((0, 0));
                                    let t = CtxPhoto {
                                        id: pid,
                                        path: h.path.clone(),
                                        parent: Path::new(&h.path)
                                            .parent()
                                            .map(|d| d.to_string_lossy().into_owned())
                                            .unwrap_or_default(),
                                        movie: kind == i64::from(crate::video::kind::MOVIE),
                                        star0: star != 0,
                                        selected: false,
                                    };
                                    self.photo_ctx_menu(
                                        ui,
                                        &t,
                                        &mut star_clicked,
                                        &mut preview_clicked,
                                    );
                                    ctx_id = Some(pid);
                                    ui.separator();
                                    if ui.button(crate::i18n::t("复制完整路径")).clicked() {
                                        ui.ctx().copy_text(h.path.clone());
                                        ui.close();
                                    }
                                    if ui.button(crate::i18n::t("系统右键菜单…")).clicked() {
                                        menu_for = Some(h.path.clone());
                                        ui.close();
                                    }
                                } else {
                                    if ui.button(t("打开")).clicked() {
                                        open_clicked = true;
                                        ui.close();
                                    }
                                    if ui
                                        .button(if h.is_dir {
                                            t("打开文件夹")
                                        } else {
                                            t("打开所在文件夹")
                                        })
                                        .clicked()
                                    {
                                        revealed = Some(h.path.clone());
                                        ui.close();
                                    }
                                    if ui.button(t("复制完整路径")).clicked() {
                                        ui.ctx().copy_text(h.path.clone());
                                        ui.close();
                                    }
                                    ui.separator();
                                    if ui.button(t("系统右键菜单…")).clicked() {
                                        menu_for = Some(h.path.clone());
                                        ui.close();
                                    }
                                }
                            });
                            if star_clicked {
                                if let Some(pid) = ctx_id {
                                    self.toggle_star(pid);
                                }
                            }
                            if preview_clicked {
                                // 与图库菜单同一语义:视频交给系统播放器,
                                // 图片走下方的预览路由(库里那行已在手)。
                                if let Some(pid) = ctx_id {
                                    let movie = self
                                        .store
                                        .photo_brief_by_id(pid)
                                        .map(|(_, k)| {
                                            k == i64::from(crate::video::kind::MOVIE)
                                        })
                                        .unwrap_or(false);
                                    if movie {
                                        opened = Some(h.path.clone());
                                    } else {
                                        preview_req = Some(h.path.clone());
                                    }
                                }
                            }
                            // 双击与菜单「打开」同一路由:目录直接进资源管理器;
                            // 图片请求本系统预览(库里有没有这行,表外再查);
                            // 视频与其余类型交给系统关联程序。
                            if resp.double_clicked() || open_clicked {
                                let p = Path::new(&h.path);
                                if h.is_dir || !crate::scan::is_image(p) {
                                    opened = Some(h.path.clone());
                                } else {
                                    preview_req = Some(h.path.clone());
                                }
                            }
                        });
                    }
                });
            self.name_results = results;
            // 动作在表外执行:打开类即时生效;系统右键菜单是模态的,不在
            // 遍历中途弹。文件夹条目「打开所在文件夹」= 直接进该目录。
            if let Some(p) = opened {
                crate::win::shell_open(std::path::Path::new(&p));
            }
            if let Some(p) = preview_req {
                // 图片双击:库里有这行就进编辑视图(与网格双击同一套状态复位);
                // 还没入库的退回系统关联打开,并讲明白为什么没进预览。
                let path = Path::new(&p).to_string_lossy().into_owned();
                match self.store.photo_id_at(&path) {
                    Some(id) => {
                        self.preview = Some(id);
                        self.preview_tex = None;
                        self.preview_failed = None;
                        self.anim_reset();
                        self.preview_reset_zoom();
                    }
                    None => {
                        if !crate::win::shell_open(Path::new(&path)) {
                            self.status = t_with("无法打开:{path}(无关联程序)", &[("path", path.to_string())]);
                        } else {
                            self.status = t_with("{p} 还没入库,已用系统关联程序打开;要在本系统里预览编辑,请先把所在目录纳入图库。", &[("p", p.to_string())]);
                        }
                    }
                }
            }
            if let Some(p) = revealed {
                let p = Path::new(&p);
                if p.is_dir() {
                    crate::win::shell_open(p);
                } else {
                    show_in_explorer(&p.to_string_lossy());
                }
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

    /// 平铺 / 树形一对互斥档(同一开关的两态:平铺列表或多级树)。
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
            // 检查/下载/安装更新也在后台跑,让工具栏的指示灯带上这一段。
            || self.updater.phase.working()
            || self.models_busy()
    }

    /// 人脸模型下载同样算一段后台活(精简构建里没有这个模块,恒为不忙)。
    #[cfg(feature = "ai")]
    fn models_busy(&self) -> bool {
        self.models.phase.working()
    }

    #[cfg(not(feature = "ai"))]
    fn models_busy(&self) -> bool {
        false
    }

    /// 帮助 > 检查更新 窗口。
    ///
    /// 按钮动作先收成局部标记,出了 egui 闭包再动 `self.updater`:闭包里只持有
    /// 状态快照,不会被窗口关闭后的可变借用绊住。
    fn update_window(&mut self, ctx: &egui::Context) {
        if !self.updater.win_open {
            return;
        }
        use crate::update::Phase;
        let phase = self.updater.phase.clone();
        let cfg = self.updater.cfg.clone();
        let last_check = self.updater.last_check;
        let notice = self.updater.notice.clone();
        let working = phase.working();
        // 显示「生效版本」:安装后未重启时 applied_version 比运行中的映像新,
        // 口径与 --check-update 一致,免得用户以为升级没成功。
        let st = crate::update::load_state();
        let cur_ver = crate::update::effective_version(&st);
        let pending_restart = cur_ver != crate::update::CURRENT_VERSION.to_string();
        let (mut check, mut download, mut install, mut open_cfg, mut clear_notice) =
            (false, false, false, false, false);
        let mut skip_ver: Option<String> = None;
        let mut open = true;

        egui::Window::new(t("检查更新"))
            .open(&mut open)
            .default_width(560.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(t("检查更新"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(if pending_restart {
                            t_with("当前版本 v{cur_ver}(重启后生效)", &[("cur_ver", cur_ver.to_string())])
                        } else {
                            t_with("当前版本 v{cur_ver}", &[("cur_ver", cur_ver.to_string())])
                        });
                    });
                });
                ui.separator();
                egui::Grid::new("update_info")
                    .num_columns(2)
                    .spacing([10.0, 5.0])
                    .show(ui, |ui| {
                        ui.label(t("状态"));
                        ui.horizontal(|ui| {
                            if working {
                                ui.spinner();
                            }
                            ui.label(phase.label());
                        });
                        ui.end_row();
                        ui.label(t("更新地址"));
                        ui.label(if cfg.url.is_empty() {
                            t("已停用(配置里留空)").to_string()
                        } else {
                            cfg.url.clone()
                        });
                        ui.end_row();
                        ui.label(t("配置文件"));
                        ui.monospace(cfg.path.display().to_string());
                        ui.end_row();
                        ui.label(t("上次检查"));
                        ui.label(update_time_text(last_check));
                        ui.end_row();
                        ui.label(t("索引服务"));
                        ui.label(update_service_text());
                        ui.end_row();
                    });

                if let Some(n) = &notice {
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(n).color(crate::theme::p().accent));
                        if ui.small_button(t("知道了")).clicked() {
                            clear_notice = true;
                        }
                    });
                }
                let notes = match &phase {
                    Phase::Found(m) => m.notes.clone(),
                    Phase::Ready(p) => p.notes.clone(),
                    _ => String::new(),
                };
                if !notes.is_empty() {
                    ui.add_space(4.0);
                    ui.label(t("更新说明:"));
                    egui::ScrollArea::vertical()
                        .id_salt("update_notes")
                        .max_height(110.0)
                        .show(ui, |ui| ui.label(&notes));
                }

                ui.separator();
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(!working, egui::Button::new(t("立即检查")))
                        .clicked()
                    {
                        check = true;
                    }
                    // 自动下载开着时已经在跑了,别让用户又排一次相同的下载。
                    let can_download = matches!(phase, Phase::Found(_)) && !cfg.auto_download;
                    if ui
                        .add_enabled(can_download && !working, egui::Button::new(t("下载更新")))
                        .clicked()
                    {
                        download = true;
                    }
                    let can_install = matches!(phase, Phase::Ready(_)) && !working;
                    if ui
                        .add_enabled(can_install, egui::Button::new(t("安装更新…")))
                        .clicked()
                    {
                        install = true;
                    }
                    if let Some(v) = match &phase {
                        Phase::Found(m) => Some(m.version.clone()),
                        Phase::Ready(p) => Some(p.version.clone()),
                        _ => None,
                    } {
                        if ui.button(t("跳过此版本")).clicked() {
                            skip_ver = Some(v);
                        }
                    }
                    if ui.button(t("打开配置文件")).clicked() {
                        open_cfg = true;
                    }
                });
                ui.add_space(6.0);
                ui.small(t("启动后在空闲时才检查(没有输入,且扫描 / 导入 / 服务装卸都不在跑)。"));
                ui.small(t("发现新版本先提示一次;后台分段下载,断线会保留进度,下载完成再提示一次。"));
                ui.small(t("安装会等索引服务状态落定后停它(它锁着程序文件),替换完按原状态恢复;需要一次管理员授权。"));
                ui.small(t("若安装进程意外中断,下次启动会自动把索引服务拉回来。"));
                ui.small(t("当前窗口继续运行,请尽快重新启动程序以使用新版本(服务与界面版本一致更稳)。"));
                if cfg.url.is_empty() {
                    ui.add_space(4.0);
                    ui.small(t("配置里把 UpdateURL 留空,自动更新已停用;填回地址即可启用。"));
                }
            });

        self.updater.win_open = open;
        if clear_notice {
            self.updater.notice = None;
        }
        if check {
            self.updater.check_now();
        }
        if download {
            if let Phase::Found(m) = &phase {
                self.updater.download(m);
            }
        }
        if install {
            self.update_confirm = true;
        }
        if let Some(v) = skip_ver {
            self.updater.skip(&v);
        }
        if open_cfg {
            crate::update::open_config_file();
        }
    }

    /// 安装前的二次确认:这一步会弹 UAC 并短暂停止索引服务,必须让用户明说一次。
    fn update_confirm_window(&mut self, ctx: &egui::Context) {
        if !self.update_confirm {
            return;
        }
        let mut open = true;
        let (mut yes, mut no) = (false, false);
        let ver = match &self.updater.phase {
            crate::update::Phase::Ready(p) => p.version.clone(),
            _ => t("新版本").into(),
        };
        egui::Window::new(t("确认安装更新"))
            .open(&mut open)
            .default_width(440.0)
            .show(ctx, |ui| {
                ui.label(t_with("即将安装 v{ver}。", &[("ver", ver.to_string())]));
                ui.add_space(4.0);
                ui.label(t(
                    "· 索引服务正锁着程序文件,安装时会先等它状态稳定再停止,替换完成后自动恢复。",
                ));
                ui.label(t(
                    "· 停止 / 启动服务需要管理员权限,系统会弹出一次授权确认。",
                ));
                ui.label(t("· 若安装进程被中断,下次启动程序会自动把索引服务起回来。"));
                ui.label(t(
                    "· 本次会话不会中断;请尽快重新启动程序,让界面和服务都跑在新版本上。",
                ));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button(t("开始安装")).clicked() {
                        yes = true;
                    }
                    if ui.button(t("取消")).clicked() {
                        no = true;
                    }
                });
            });
        self.update_confirm = open && !yes && !no;
        if yes {
            self.updater.apply();
        }
    }

    /// 「查看选项」里的壳目录快捷键(桌面 / 我的文档 / 我的图片 / 我的电脑):
    /// 目录已在图库里 → 跳到该文件夹页;没导入 → 只给提示,不擅自扩扫描范围。
    /// 进入一个目录(点击懒加载 + 可见优先)。
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

    /// 磁盘上确实没有这张图的缓存 → 把它的 id 排进「按张放行」队列。
    ///
    /// 只看「有没有缓存文件」,不看派生状态、也不看目录在不在批量计划里:
    /// 库里有的图就该显示,缺的那一张就补那一张,不牵动整个目录。
    /// 渲染帧不直接写库,由 `photo_grid` 每帧批量消化;同一 id 只推一次。
    /// 队列排满时**不**登记去重,下一帧还会再来敲门。
    fn note_missing_photo(&mut self, id: i64) -> bool {
        if self.adopt_queue.len() >= MAX_ADOPT_QUEUE {
            return false;
        }
        if !self.adopt_seen.insert(id) {
            return false;
        }
        self.adopt_queue.push_back(id);
        true
    }

    /// 消化按张放行队列:一批 id 一次批量抬进后台派生队列,不写目录账本。
    fn drain_adopts(&mut self, max: usize) {
        let ids: Vec<i64> = self
            .adopt_queue
            .drain(..max.min(self.adopt_queue.len()))
            .collect();
        if ids.is_empty() {
            return;
        }
        let _ = self.store.promote_photos(&ids);
        // 不回读库:网格下一帧照常读缓存,缓存生成完由 worker 的完成信号
        // 刷新纹理。剩余队列靠 note_missing_photo 那边的重绘请求继续消化。
    }

    /// 用户明确点进 / 导入一个文件夹 → 整个目录排进派生队列。
    ///
    /// 「明确表态」包括导入和点进文件夹树两种:用户已经指到这个目录了,
    /// 软排除名单(应用缓存、临时目录一类)该让路,否则点了也看不到图。
    /// 只有硬排除名单(回收站、系统目录、自家缓存)任何来源都不收 ——
    /// 那些目录根本不纳入图库,是唯一真正的排除。
    ///
    /// 已纳过的目录不重写账本 —— 免得把用户显式设过的来源(首启对话框 /
    /// 手动导入)冲成 click;但 `promote_dir` 照样跑一遍:并发扫描可能刚
    /// 插进一批 `rich_state=0` 的新行。
    fn include_dir(&mut self, dir: &str, state: &str, source: &str) -> bool {
        let deliberate = source == crate::db::SRC_IMPORT || source == crate::db::SRC_CLICK;
        if !self.store.adoptable_dir(dir, deliberate) {
            return false;
        }
        if !self.store.dir_intent(dir).is_some_and(|d| d.included()) {
            let _ = self
                .store
                .set_dir_intent(&crate::db::DirIntent::new(dir, state, source));
            // 导入树的灰/蓝着色缓存在父目录的 kids 里,落账后清掉重读。
            self.import_tree_kids.clear();
        }
        if self.store.promote_dir(dir).unwrap_or(0) > 0 {
            self.sync_dir_promoted(dir);
        }
        true
    }

    /// `promote_dir` 落库后,把库里的真相搬进内存视图。
    ///
    /// 网格每帧只读 `self.rows`,而这里的 `rich_state` 要等 `rebuild` 才跟库对齐 ——
    /// 不搬这一趟,刚点进目录时内存行还停在旧状态。**真相一律回读库**
    /// (`Store::promoted_states`),界面不按排除名单自己再判一遍门:那么做
    /// 会让库里早已派生完的目录在界面上停在旧状态。
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
            self.status = t_with(
                "{label}:图库树只覆盖已导入的目录",
                &[("label", label.to_string())],
            );
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
                self.subtree_sel = None;
                self.folder_show_all = false;
                self.set_view(ViewKind::Folder);
                let p = self.folders[i].clone();
                self.enter_dir(&p);
                self.tree_reveal(&p);
                self.status = t_with("已定位到 {label}", &[("label", label.to_string())]);
            }
            None => {
                self.status = t_with(
                    "{label} 未导入图库:文件夹管理器 > 添加文件夹",
                    &[("label", label.to_string())],
                )
            }
        }
    }

    /// 顶部工具条,左→右:
    /// 导入 · 新建相簿 · 放映 · 平铺/树形 · 查看选项 ▾ …… 过滤四开关 · 尺寸滑块 · 搜索框 · 活动灯。
    fn toolbar_ui(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(2.0);
            // 前后导航(浏览器式历史;Alt+←/→ 同一条路)。
            if ui
                .add_enabled(self.nav_at > 0, tool_button("arrow-left", false))
                .on_hover_text(t("后退(Alt+←)"))
                .clicked()
            {
                self.nav_back();
            }
            if ui
                .add_enabled(
                    self.nav_at + 1 < self.nav_hist.len(),
                    tool_button("arrow-right", false),
                )
                .on_hover_text(t("前进(Alt+→)"))
                .clicked()
            {
                self.nav_forward();
            }
            ui.separator();
            // 主「导入」按钮开的是整页导入面板(选源 → 看清单 → 入库),
            // 直接挑文件夹的一次性入口仍在 文件 菜单里。
            let on_import = self.page == PageKind::Import;
            if ui
                .add(tool_button("folder-plus", on_import))
                .on_hover_text(t("导入(打开导入页:选源 → 看清单 → 入库)"))
                .clicked()
            {
                self.go_page(PageKind::Import);
            }
            if ui
                .add(tool_button("smartphone", false))
                .on_hover_text(t("从设备导入…"))
                .clicked()
            {
                self.open_import_devices();
            }
            if ui
                .add(tool_button("image-plus", false))
                .on_hover_text(t("新建相簿(有选中照片则一并放入)"))
                .clicked()
            {
                self.album_input.clear();
                self.album_new_win = true;
            }
            ui.separator();
            if ui
                .add(tool_button("images", false))
                .on_hover_text(t("幻灯片放映(选中照片;未选中则放映当前视图)"))
                .clicked()
            {
                self.start_slideshow();
            }
            ui.separator();
            if ui
                .add(tool_button("layout-grid", !self.folder_tree))
                .on_hover_text(t("平铺文件夹视图"))
                .clicked()
            {
                self.set_folder_tree(false);
            }
            if ui
                .add(tool_button("list-tree", self.folder_tree))
                .on_hover_text(t("树形视图(只显示有照片的目录)"))
                .clicked()
            {
                self.set_folder_tree(true);
            }
            ui.menu_button(toolbar_icon("sliders-horizontal"), |ui| {
                self.view_options_menu(ui)
            })
            .response
            .on_hover_text(t("查看选项"));
            ui.separator();
            if ui
                .add(tool_button("wand-sparkles", self.fx_panel))
                .on_hover_text(t("效果面板"))
                .clicked()
            {
                self.fx_panel = !self.fx_panel;
            }
            // —— 右端成组控件:从右往左依次是 活动灯 → 搜索框 → 滑块 → 过滤开关 ——
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // 二选一的配色开关:浅色显月亮(点了变暗),暗色显太阳(点了变亮)。
                let dark = !crate::theme::p().is_light;
                let tip = if dark {
                    t("切回灰色配色")
                } else {
                    t("切到蓝黑配色")
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
                // 右栏抽屉开关(工具栏的属性面板按钮;抽屉内四页签见右栏)。
                if ui
                    .add(tool_button("panel-right", self.drawer_open))
                    .on_hover_text(t("属性面板"))
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
                        .on_hover_text(t("后台正在扫描 / 生成缩略图 / 建立索引 / 检查或下载更新"));
                } else {
                    // 不忙也占住同样的宽度,免得指示灯出现时整行左右抖。
                    ui.add_space(23.0);
                }
                // 库内搜索框已取消(与全盘快速查找重复,统一走顶部整条搜索)。
                ui.separator();
                // 缩放滑块;档位即已缓存的缩略图尺寸。
                let mut idx = self.thumb_tier as i32;
                let slider = egui::Slider::new(&mut idx, 0..=3)
                    .show_value(false)
                    .text(t("图"));
                // egui 的滑块宽度取自 spacing.slider_width,工具栏里要收窄。
                let old_w = ui.spacing().slider_width;
                ui.spacing_mut().slider_width = 96.0;
                let moved = ui
                    .add(slider)
                    .on_hover_text(t("缩略图大小:小 · 普通 · 大 · 预览"))
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
                for i in (0..timeline::filter_btns().len()).rev() {
                    let (name, tip) = timeline::filter_btns()[i];
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
            .selectable_label(!self.folder_tree, t("平铺文件夹视图"))
            .clicked()
        {
            self.set_folder_tree(false);
            ui.close();
        }
        if ui
            .selectable_label(self.folder_tree, t("树形视图"))
            .clicked()
        {
            self.set_folder_tree(true);
            ui.close();
        }
        ui.separator();
        for s in timeline::SortBy::ALL {
            if ui
                .selectable_label(self.sort_by == s, t(s.label()))
                .clicked()
            {
                self.sort_by = s;
                self.apply_view_state();
                ui.close();
            }
        }
        if ui.selectable_label(self.sort_rev, t("反向排序")).clicked() {
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
                t("跳到该目录(需已导入图库)")
            } else {
                t("图库树只覆盖已导入的目录")
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
            .selectable_label(self.tree_thumbs, t("在图库中显示缩略图"))
            .clicked()
        {
            self.tree_thumbs = !self.tree_thumbs;
            let _ = self
                .store
                .set_setting("tree_thumbs", if self.tree_thumbs { "1" } else { "0" });
            ui.close();
        }
        if ui
            .selectable_label(self.thumb_captions, t("缩略图说明"))
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
        self.refresh_recent();
        self.dups = if self.dup_near {
            timeline::dup_groups_near(&self.store, NEAR_DUP_DIST).unwrap_or_default()
        } else {
            timeline::dup_groups(&self.store).unwrap_or_default()
        };
        self.tags = timeline::all_tags(&self.store).unwrap_or_default();
        if self.selected_folder >= self.folders.len() {
            self.selected_folder = 0;
        }
        self.rows_gen = self.rows_gen.wrapping_add(1);
        // 工具栏 spinner:本视图还有多少张缩略图没落地(后台 worker 在补就是"有活")。
        self.pending_thumbs = self.rows.iter().filter(|r| r.thumb_ok == 0).count();
        self.refresh_tier_count();
    }

    /// 重算首页「最近更新」成员:只查库收人,不重载 `rows`。
    fn refresh_recent(&mut self) {
        let watched = self.store.watched_stats();
        let roots: Vec<String> = watched.iter().map(|(p, _)| p.clone()).collect();
        let ids = timeline::load_recent(&self.store, &roots, RECENT_LIMIT * 3).unwrap_or_default();
        let excl = crate::scan::Exclusions::from_store(&self.store);
        let mut recent = Vec::with_capacity(RECENT_LIMIT);
        for id in ids {
            let Some(&i) = self.id_row.get(&id) else {
                continue;
            };
            let dir = self.rows[i].parent.as_str();
            if dir.is_empty()
                || matches!(
                    excl.exclude_layer_dir(Path::new(dir)),
                    Some(crate::scan::ExcludeLayer::Hard)
                )
            {
                continue;
            }
            recent.push(i);
            if recent.len() >= RECENT_LIMIT {
                break;
            }
        }
        self.recent_date = cn_date(&timeline::group_date(&self.rows, &recent));
        self.recent_idxs = recent;
        self.watched = watched;
        self.recent_dirty = false;
        self.recent_at = std::time::Instant::now();
    }

    /// 重扫当前全部监听目录。
    ///
    /// 范围只经文件夹管理器扩大,
    /// 这里提供对已登记范围的一键重扫入口(不新增范围)。
    fn start_scan_full(&mut self) {
        let roots: Vec<PathBuf> = self
            .store
            .watched_stats()
            .into_iter()
            .map(|(p, _)| PathBuf::from(p))
            .collect();
        self.start_scan_roots(roots, t("全部监听目录"));
    }

    /// 启动扫描后台线程;scope 仅用于状态文案。
    ///
    /// 扫描根先登记进持久化队列,线程完成后移除 ——
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
        // 进度与取消控制(进度回调 + 退出检查)。
        let ctrl = Arc::new(crate::scan::ScanControl::new());
        self.scan_ctrl = Some(ctrl.clone());
        self.scan_scope = scope_owned.clone();
        self.scan_verb = t("扫描");
        self.status = t_with(
            "正在{p1}{scope}…",
            &[
                ("p1", (self.scan_verb).to_string()),
                ("scope", scope.to_string()),
            ],
        );
        std::thread::spawn(move || {
            // L1 索引住在服务进程里,UI 侧不再持有 `IndexFile`,
            // 所以监听根的发现走 walkdir —— 这正是的口径:全盘发现与
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
            // 无论成功/失败/取消都出队(取消是用户意志,不续扫);
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

    /// 整页切换(看图与取图做成两套整页面板互切,不是弹窗)。
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
        self.status = t_with("切到{p1}页", &[("p1", (page.label()).to_string())]);
    }

    /// 选视图:分类只在图库页有,所以顺带把页面切回去并记站。
    fn set_view(&mut self, v: ViewKind) {
        if self.view == v && self.page == PageKind::Library {
            return;
        }
        self.view = v;
        self.nav_open_drawer = Some(drawer_key(v));
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
        self.nav_open_drawer = Some(drawer_key(s.view));
        self.folder_show_all = s.show_all;
        self.subtree_sel = None;
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
        self.status = t_with(
            "回到 {p1} · {p2}",
            &[
                ("p1", (s.page.label()).to_string()),
                ("p2", (s.view.label()).to_string()),
            ],
        );
    }

    // ===== 导入页 =====

    /// 换导入源目录:清勾选 + 现读一次清单(只有这里碰磁盘)。
    fn set_import_source(&mut self, dir: PathBuf) {
        let indexed = timeline::indexed_paths_in(&self.store, &dir.to_string_lossy());
        self.import_entries = list_import_dir(&dir, &indexed);
        let n = self.import_entries.iter().filter(|e| !e.is_dir).count();
        self.last_import_src = dir.to_string_lossy().into_owned();
        self.import_tree_reveal(&dir);
        self.import_src = Some(dir);
        self.import_sel.clear();
        self.status = t_with("导入源:{n} 个可入库文件", &[("n", n.to_string())]);
    }

    /// 展开导入源在目录树上的祖先链(换源后树上看得见选中的那一条)。
    fn import_tree_reveal(&mut self, dir: &Path) {
        for p in dir.ancestors().take(64) {
            self.import_tree_open.insert(p.to_string_lossy().into_owned());
        }
    }

    /// 重读当前源目录(清单按钮「刷新」;新增/删除文件后不必重开页面)。
    fn refresh_import_entries(&mut self) {
        self.import_tree_kids.clear();
        let Some(dir) = self.import_src.clone() else {
            self.import_entries.clear();
            return;
        };
        self.set_import_source(dir);
    }

    /// 导入页左栏:常用位置/设备快捷入口 + 全盘目录树(点目录 = 设为导入源)。
    fn import_tree_ui(&mut self, ui: &mut egui::Ui) {
        crate::theme::section_title(ui, t("选择源"));
        let roots = crate::win::drive_roots();
        let shell = crate::win::known_dirs();
        let devs = crate::device::removable_volumes();
        egui::ScrollArea::vertical()
            .id_salt("import-tree")
            .auto_shrink([false, true])
            .show(ui, |ui| {
                // 我的文件夹:壳目录快捷入口,点一下直接设为导入源。
                if !shell.is_empty() {
                    muted_title(ui, t("我的文件夹"));
                    for p in &shell {
                        let name = p
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| p.display().to_string());
                        if import_source_row(ui, "folder", &name, p, self.import_src.as_deref()) {
                            self.set_import_source(p.clone());
                        }
                    }
                }
                // 设备:可移动卷(U 盘 / 相机卡)。
                if !devs.is_empty() {
                    muted_title(ui, t("设备"));
                    for v in &devs {
                        let label = crate::win::volume_label(v)
                            .map(|l| format!("{l} ({})", v.display()))
                            .unwrap_or_else(|| {
                                t_with("可移动磁盘 {p1}", &[("p1", (v.display()).to_string())])
                            });
                        if import_source_row(ui, "smartphone", &label, v, self.import_src.as_deref())
                        {
                            self.set_import_source(v.clone());
                        }
                    }
                }
                // 我的电脑:全盘目录树(盘根带硬盘图标,可逐级下探)。
                muted_title(ui, t("我的电脑"));
                for r in roots {
                    self.walk_import_tree(ui, &r, 0, false);
                }
            });
    }

    /// 递归画一个磁盘目录节点:子目录懒加载并缓存,硬排除目录不列。
    fn walk_import_tree(&mut self, ui: &mut egui::Ui, dir: &Path, depth: usize, blue: bool) {
        if depth > 24 {
            return;
        }
        let path = dir.to_string_lossy().into_owned();
        let open = self.import_tree_open.contains(&path);
        if open && !self.import_tree_kids.contains_key(&path) {
            if self.import_tree_kids.len() > 4096 {
                self.import_tree_kids.clear();
            }
            let excl = crate::scan::Exclusions::from_store(&self.store);
            let mut kids: Vec<(PathBuf, bool)> = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.is_dir()
                        && excl.exclude_layer_dir(p) != Some(crate::scan::ExcludeLayer::Hard)
                })
                .map(|p| {
                    let blue = self
                        .store
                        .dir_intent(&p.to_string_lossy())
                        .is_some_and(|d| d.state == crate::db::INTENT_IMPORTED);
                    (p, blue)
                })
                .collect();
            kids.sort_by(|a, b| a.0.cmp(&b.0));
            self.import_tree_kids.insert(path.clone(), kids);
        }
        let kids = self.import_tree_kids.get(&path).cloned().unwrap_or_default();
        let is_root = depth == 0;
        let name = if is_root {
            let letter = path.trim_end_matches(['\\', '/']).to_string();
            match crate::win::volume_label(dir) {
                Some(l) => format!("{l} ({letter})"),
                None => {
                    if matches!(
                        crate::win::drive_type(dir),
                        Some(crate::win::DriveType::Removable)
                    ) {
                        t_with("可移动磁盘 {p1}", &[("p1", letter)])
                    } else {
                        t_with("本地磁盘 {p1}", &[("p1", letter)])
                    }
                }
            }
        } else {
            dir.file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone())
        };
        let active = self.import_src.as_deref().is_some_and(|s| same_dir(s, dir));
        ui.horizontal(|ui| {
            // 引导虚线与图库树同款:祖先层竖虚线 + 本层横虚线。
            let line = egui::Stroke::new(1.0_f32, crate::theme::p().faint);
            let left = ui.cursor().left();
            let top = ui.cursor().top();
            let cy = top + 10.0;
            let mut segs: Vec<egui::Shape> = Vec::new();
            for i in 0..depth {
                let x = left + (i as f32) * 10.0 + 5.0;
                segs.extend(egui::Shape::dashed_line(
                    &[egui::pos2(x, top), egui::pos2(x, top + 20.0)],
                    line,
                    3.0,
                    3.0,
                ));
            }
            if depth > 0 {
                let x0 = left + (depth as f32 - 1.0) * 10.0 + 5.0;
                let x1 = left + (depth as f32) * 10.0 + 12.0;
                segs.extend(egui::Shape::dashed_line(
                    &[egui::pos2(x0, cy), egui::pos2(x1, cy)],
                    line,
                    3.0,
                    3.0,
                ));
            }
            ui.painter().extend(segs);
            ui.add_space((depth as f32) * 10.0);
            if open && kids.is_empty() {
                ui.add_space(26.0);
            } else if ui
                .small_button(if open { "▾" } else { "▸" })
                .on_hover_text(t("展开 / 收起"))
                .clicked()
            {
                if open {
                    self.import_tree_open.remove(&path);
                } else {
                    self.import_tree_open.insert(path.clone());
                }
            }
            if is_root {
                ui.add(toolbar_icon("hard-drive"));
            } else {
                let color = if blue {
                    crate::theme::p().accent
                } else {
                    crate::theme::p().muted
                };
                ui.add(folder_icon_colored(color));
            }
            let text = egui::RichText::new(name.as_str()).color(crate::theme::p().text);
            if ui
                .selectable_label(active, text)
                .on_hover_text(&path)
                .clicked()
            {
                self.set_import_source(dir.to_path_buf());
            }
        });
        if open {
            for (k, b) in kids {
                self.walk_import_tree(ui, &k, depth + 1, b);
            }
        }
    }

    /// 导入页中央区:提示行 + 顶部一排图标动作 + 文件清单。
    fn import_page_ui(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let pics = self.import_entries.iter().filter(|e| !e.is_dir).count();
            let fresh = self
                .import_entries
                .iter()
                .filter(|e| !e.is_dir && !e.in_library)
                .count();
            let sel: Vec<PathBuf> = self
                .import_sel
                .iter()
                .filter_map(|&i| self.import_entries.get(i))
                .filter(|e| !e.is_dir && !e.in_library)
                .map(|e| e.path.clone())
                .collect();
            let has_src = self.import_src.is_some();

            // 提示行(弱化小字):导入含义 + 当前源与计数。
            ui.small(t("导入 = 登记该目录进图库并建索引,不移动、不改写原文件。"));
            if let Some(src) = &self.import_src {
                ui.horizontal(|ui| {
                    ui.small(
                        egui::RichText::new(src.display().to_string())
                            .color(crate::theme::p().accent),
                    )
                    .on_hover_text(t("当前导入源目录"));
                    ui.small(t_with(
                        "{pics} 个可入库文件(未入库 {fresh} 个),已勾选 {p1} 个",
                        &[
                            ("pics", pics.to_string()),
                            ("fresh", fresh.to_string()),
                            ("p1", (self.import_sel.len()).to_string()),
                        ],
                    ));
                });
            } else {
                ui.small(t(
                    "左侧还没有可浏览的源:选一个文件夹,或点「浏览其他文件夹…」。",
                ));
            }
            ui.add_space(6.0);

            // 动作行:纯图标,悬停显示操作说明。
            let mut back = false;
            let mut browse = false;
            let mut refresh = false;
            let mut select_all = false;
            let mut select_none = false;
            let mut import_sel_go = false;
            let mut import_all_go = false;
            ui.horizontal(|ui| {
                back = icon_action(ui, "arrow-left", t("返回图库"), true);
                browse = icon_action(ui, "folder-open", t("浏览其他文件夹…"), true);
                refresh = icon_action(ui, "refresh", t("刷新清单"), has_src);
                ui.separator();
                select_all = icon_action(ui, "square-check", t("全选照片"), pics > 0);
                select_none =
                    icon_action(ui, "square", t("全不选"), !self.import_sel.is_empty());
                ui.separator();
                import_sel_go = icon_action(
                    ui,
                    "download",
                    &t_with("导入所选({p1})", &[("p1", (sel.len()).to_string())]),
                    !sel.is_empty(),
                );
                import_all_go = icon_action(ui, "folder-plus", t("导入整个文件夹"), has_src);
            });
            ui.separator();
            if back {
                self.go_page(PageKind::Library);
            }
            if browse {
                if let Some(dir) = rfd::FileDialog::new()
                    .set_title(t("选择导入源文件夹"))
                    .pick_folder()
                {
                    self.set_import_source(dir);
                }
            }
            if refresh {
                self.refresh_import_entries();
            }
            if select_all {
                self.import_sel = (0..self.import_entries.len())
                    .filter(|&i| !self.import_entries[i].is_dir)
                    .collect();
            }
            if select_none {
                self.import_sel.clear();
            }
            if import_sel_go {
                self.import_selected(sel.clone());
            }
            if import_all_go {
                self.import_whole_folder();
            }

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
                                    .on_hover_text(t("进入该子目录"))
                                    .clicked()
                                {
                                    descend = Some(e.path.clone());
                                }
                            } else {
                                ui.add(toolbar_icon("images"));
                                let mut on = self.import_sel.contains(&i);
                                let name = if e.in_library {
                                    t_with("{p1}  (已入库)", &[("p1", (e.name).to_string())])
                                } else {
                                    e.name.clone()
                                };
                                let chk =
                                    ui.checkbox(&mut on, &name).on_hover_text(if e.in_library {
                                        t("已在图库里,导入时会跳过")
                                    } else {
                                        t("勾选后「导入所选」只入这几张")
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
                    if entries.is_empty() && has_src {
                        ui.small(t("这个目录里没有可入库的图片(或子目录)。"));
                    }
                });
            self.import_entries = entries;
            if let Some(d) = descend {
                self.set_import_source(d);
            }
        });
    }

    /// 「导入整个文件夹」:登记目录 + 常规扫描(进度/取消/断点续扫都复用同一套)。
    fn import_whole_folder(&mut self) {
        let Some(dir) = self.import_src.clone() else {
            return;
        };
        if let Err(e) = self.store.upsert_watched(&dir) {
            self.status = t_with("记录文件夹失败(本次仍会扫描): {e}", &[("e", e.to_string())]);
        }
        let scope = dir.display().to_string();
        // 用户明确要这个目录 = 它该出缩略图:落 `imported` 账。
        // 必须在起扫之前落 —— 扫描线程入库时读这份账本算 `rich_state`。
        self.include_dir(&scope, crate::db::INTENT_IMPORTED, crate::db::SRC_IMPORT);
        self.start_scan_roots(vec![dir], &scope);
    }

    /// 「导入所选」:只入勾选的那几张(目录仍登记进监听列表,
    /// 否则左栏看不到刚落库的图)。
    fn import_selected(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            self.status = t("没有待导入的勾选(已入库的会跳过)").into();
            return;
        }
        if self.scan_busy.swap(true, Ordering::SeqCst) {
            self.status = t("已有扫描在跑,请等它结束后再导入所选").into();
            return;
        }
        let Some(dir) = self.import_src.clone() else {
            self.scan_busy.store(false, Ordering::SeqCst);
            return;
        };
        let _ = self.store.upsert_watched(&dir);
        // 用户挑中的这几张 = 它所在的目录该出缩略图。
        self.include_dir(
            &dir.display().to_string(),
            crate::db::INTENT_IMPORTED,
            crate::db::SRC_IMPORT,
        );
        let store = self.store.clone();
        let busy = self.scan_busy.clone();
        let scope = t_with("所选 {p1} 张", &[("p1", (paths.len()).to_string())]);
        self.scan_scope = scope.clone();
        self.scan_verb = t("导入");
        let scope_owned = scope.clone();
        let (tx, rx) = mpsc::channel();
        self.scan_rx = Some(rx);
        let ctrl = Arc::new(crate::scan::ScanControl::new());
        ctrl.set_total(paths.len());
        self.scan_ctrl = Some(ctrl.clone());
        self.status = t_with("正在{scope}…", &[("scope", scope.to_string())]);
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
            .set_title(t("添加文件夹到图库"))
            .pick_folder()
        else {
            return;
        };
        if let Err(e) = self.store.upsert_watched(&dir) {
            self.status = t_with("记录文件夹失败(本次仍会扫描): {e}", &[("e", e.to_string())]);
        }
        let scope = dir.display().to_string();
        // 用户明确要这个目录 = 它该出缩略图:落 `imported` 账。
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
            Ok(()) => {
                self.status = t_with(
                    "已从图库移除 {n} 张(未删除磁盘文件)",
                    &[("n", n.to_string())],
                )
            }
            Err(e) => self.status = t_with("移除失败: {e}", &[("e", e.to_string())]),
        }
        self.sel.clear();
        self.preview = None;
        self.preview_tex = None;
        self.rebuild();
    }

    /// 启动保存烘培后台线程(备份+重编码+重缩略图)。
    fn start_bake(&mut self, id: i64) {
        if self.bake_rx.is_some() {
            self.status = t("保存进行中,请稍候…").into();
            return;
        }
        // 只读速看格式:即便 UI 上误触保存,也不启动会覆盖原文件的后台任务。
        if self.readonly_media(id) {
            self.status = t("只读速看预览格式不参与保存烘培,原文件保持不变").into();
            return;
        }
        let store = self.store.clone();
        let (tx, rx) = mpsc::channel();
        self.bake_rx = Some(rx);
        self.status = t("正在保存(备份并重写原图)…").into();
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
        self.status = t("正在 OCR 识别(首次需加载模型)…").into();
        let store = self.store.clone();
        std::thread::spawn(move || {
            let out = (|| -> Result<(i64, Vec<crate::ai::ocr::Txt>), String> {
                let img = image::open(&path).map_err(|e| e.to_string())?;
                let mut o = crate::ai::ocr::Ocr::load(&store)?;
                Ok((id, o.extract(&img)?))
            })();
            let _ = tx.send(out);
        });
    }

    /// 启动检查:人脸模型缺不缺件。缺件 → 弹一次提示窗(内置官方直链或
    /// ModelURL 皆可下载),由用户点「开始下载」才联网(识别模型上百 MB,
    /// 不替用户花流量)。
    #[cfg(feature = "ai")]
    fn check_face_models(&mut self) {
        let algo = crate::faces::current_algo(&self.store);
        if !self.models.check_on_startup(algo) {
            return;
        }
        // 「不再提示」只压掉开机弹窗,下载入口仍留在 选项 > AI 模型 与人脸视图。
        if self.model_never_ask() {
            self.models.prompt_open = false;
            self.status = t("人脸模型未就位:可在 选项 > AI 模型 或人脸视图里下载").into();
        }
    }

    /// 开机是否不再提示下载人脸模型。
    #[cfg(feature = "ai")]
    fn model_never_ask(&self) -> bool {
        self.store.get_setting("model_never_ask").as_deref() == Some("1")
    }

    /// 后台开始下载指定方案的缺件(提示窗与人脸视图按钮走这里)。
    #[cfg(feature = "ai")]
    fn start_model_download(&mut self, algo: crate::ai::face::FaceAlgo) {
        if !crate::models::downloadable(algo) {
            let (det_f, rec_f) = algo.files();
            self.status = t_with("「{det_f} / {rec_f}」的权重许可不可再分发,程序不代下:请按 选项 > AI 模型 页给出的官方地址手动获取", &[("det_f", det_f.to_string()), ("rec_f", rec_f.to_string())]);
            return;
        }
        let dir = crate::ai::models_dir();
        let files = dir
            .map(|d| crate::models::missing_files(algo, &d))
            .unwrap_or_default();
        if files.is_empty() {
            self.status = t("人脸模型已就位").into();
            return;
        }
        self.start_model_files(files);
    }

    /// 后台开始下载用户勾选的模型文件(源偏好随设置项走)。
    #[cfg(feature = "ai")]
    fn start_model_files(&mut self, files: Vec<String>) {
        let pref = crate::models::SourcePref::from_setting(
            &self
                .store
                .get_setting(crate::models::SourcePref::SETTING)
                .unwrap_or_default(),
        );
        self.models.start_files(files, pref);
        if self.models.phase.working() {
            self.status = t("正在后台下载人脸模型…").into();
        } else if let crate::models::Phase::Failed(e) = &self.models.phase {
            self.status = t_with("人脸模型下载失败:{e}", &[("e", e.to_string())]);
        }
    }

    /// 下载进度只画一份(下载区/状态行),不再按方案各画一份,
    /// 免得别的方案行也跟着转圈让人误读。
    #[cfg(feature = "ai")]
    fn face_download_progress_ui(&mut self, ui: &mut egui::Ui) {
        use crate::models::Phase;
        let phase = self.models.phase.clone();
        match &phase {
            Phase::Downloading { file, .. } => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(phase.label());
                    if ui.button(t("停止下载")).clicked() {
                        self.models.cancel();
                        self.status =
                            t("已停止下载:断点留在 .part,稍后点「下载选中的模型」接着下").into();
                    }
                });
                if let Some(d) = crate::ai::models_dir() {
                    ui.small(t_with(
                        "断点留在 {p1}/{p2}.part,重开程序后再点下载可接着下",
                        &[
                            ("p1", (d.display()).to_string()),
                            ("p2", (file).to_string()),
                        ],
                    ));
                }
            }
            Phase::Failed(e) => {
                ui.label(
                    egui::RichText::new(t_with("下载失败:{e}", &[("e", e.to_string())]))
                        .color(crate::theme::p().accent),
                );
            }
            Phase::Ready => {
                ui.small(phase.label());
            }
            Phase::Idle => {}
        }
    }

    /// 选项 > AI 模型:方案单选(切换需确认重建)、模型复选下载、
    /// 下载源选择与测速。
    #[cfg(feature = "ai")]
    fn face_options_ui(&mut self, ui: &mut egui::Ui) {
        // —— 识别方案(单选) ——
        ui.heading(egui::RichText::new(t("识别方案")).size(14.0));
        let cur = crate::faces::current_algo(&self.store);
        for (algo, note) in [
            (
                crate::ai::face::FaceAlgo::SFace,
                t("YuNet + SFace,均 OpenCV Zoo,Apache-2.0 含权重,可商用(默认)"),
            ),
            (
                crate::ai::face::FaceAlgo::AdaFace,
                t("YuNet + AdaFace;权重训练数据非商用,选择即自担原厂许可"),
            ),
            (
                crate::ai::face::FaceAlgo::AntelopeV2,
                t("YuNet + antelopev2;权重非商用 research-only,不代下,需手动放置"),
            ),
        ] {
            ui.horizontal(|ui| {
                if ui.radio(cur == algo, algo.label()).clicked() && cur != algo {
                    self.face_algo_pending = Some(algo);
                }
                ui.small(note);
            });
        }
        ui.small(t("三个方案的模型文件可以都下好备着,但方案之间特征向量不通用:切换需确认清除现有人脸数据并按新方案重扫。"));
        ui.separator();

        // —— 模型文件(复选下载) ——
        let working =
            self.models.phase.working() || self.ai_dl.as_ref().is_some_and(|d| d.phase.working());
        ui.heading(egui::RichText::new(t("模型文件")).size(14.0));
        let catalog: [(&str, &str); 3] = [
            (
                "face_det.onnx",
                t("YuNet 人脸检测 · Apache-2.0(所有方案共用)"),
            ),
            ("face_rec_sface.onnx", t("SFace 人脸识别 · Apache-2.0")),
            (
                "face_rec.onnx",
                t("AdaFace 人脸识别 · 社区 ONNX 导出(条款自决)"),
            ),
        ];
        let mut any_pick = false;
        for (i, (name, desc)) in catalog.iter().enumerate() {
            let present = crate::ai::models_dir()
                .map(|d| d.join(name).is_file())
                .unwrap_or(false);
            if present {
                self.model_pick[i] = false;
                ui.small(t_with(
                    "✓ {name} 已就位 —— {desc}",
                    &[("name", name.to_string()), ("desc", desc.to_string())],
                ));
            } else {
                ui.checkbox(&mut self.model_pick[i], format!("{name} —— {desc}"));
                any_pick |= self.model_pick[i];
            }
        }
        if let Some(url) = crate::models::manual_hint(crate::ai::face::FaceAlgo::AntelopeV2) {
            ui.small(t_with("antelopev2(glint360k/antelopev2 两个 onnx,非商用)不代下:自行从 {p1} 下载,解包后按 README 改名放入模型目录。", &[("p1", (url).to_string())]));
        }
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(any_pick && !working, egui::Button::new(t("下载选中的模型")))
                .clicked()
            {
                let files: Vec<String> = catalog
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| self.model_pick[*i])
                    .map(|(_, (name, _))| name.to_string())
                    .collect();
                self.start_model_files(files);
            }
            if ui.button(t("打开模型目录")).clicked() {
                if let Some(d) = crate::ai::models_dir() {
                    show_in_explorer(&d.display().to_string());
                }
            }
            if ui.button(t("打开配置文件")).clicked() {
                crate::update::open_config_file();
            }
        });
        ui.separator();

        // —— 模型诊断(只读:核对生效路径/哈希/契约,不改写也不替换错放文件) ——
        ui.heading(egui::RichText::new(t("模型诊断")).size(14.0));
        ui.small(t(
            "按当前方案核对生效的模型文件:路径、内容哈希与导出契约。只读,不会改写或替换任何文件。",
        ));
        // 方案切换后旧诊断作废,免得展示的是上一个方案的结果。
        if self.face_diag.as_ref().is_some_and(|(a, _)| *a != cur) {
            self.face_diag = None;
        }
        if ui
            .add_enabled(!working, egui::Button::new(t("诊断当前方案的模型")))
            .clicked()
        {
            self.face_diag = Some((cur, crate::ai::face::diagnose(cur)));
        }
        if let Some((_, diag)) = &self.face_diag {
            ui.small(t_with(
                "方案:{p1} · 契约:{p2}",
                &[("p1", cur.label().to_string()), ("p2", diag.contract.to_string())],
            ));
            for m in [&diag.det, &diag.rec] {
                let head = if m.mismatch { "✗" } else { "✓" };
                ui.small(format!("{head} {} · {}", m.role, m.file));
                ui.small(format!("    {}:{}", t("路径"), m.path.display()));
                ui.small(format!("    SHA256:{}", m.sha16));
                if !m.note.is_empty() {
                    ui.small(format!("    {}", m.note));
                }
                if !m.inputs.is_empty() {
                    ui.small(format!("    {}:{}", t("输入"), m.inputs.join(", ")));
                }
                if !m.outputs.is_empty() {
                    ui.small(format!("    {}:{}", t("输出"), m.outputs.join(", ")));
                }
            }
        }
        ui.separator();

        // —— 下载源(全局单选 + 测速) ——
        ui.heading(egui::RichText::new(t("下载源")).size(14.0));
        let pref = crate::models::SourcePref::from_setting(
            &self
                .store
                .get_setting(crate::models::SourcePref::SETTING)
                .unwrap_or_default(),
        );
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("model_src_pref")
                .selected_text(pref.label())
                .show_ui(ui, |ui| {
                    for p in [
                        crate::models::SourcePref::Auto,
                        crate::models::SourcePref::Official,
                        crate::models::SourcePref::Mirror,
                    ] {
                        // 镜像源没配 ModelURL 时禁选,免得选完下不动。
                        let enabled = p != crate::models::SourcePref::Mirror
                            || !self.models.model_base().is_empty();
                        if ui
                            .add_enabled(enabled, egui::Button::new(p.label()).selected(pref == p))
                            .clicked()
                            && pref != p
                        {
                            let _ = self
                                .store
                                .set_setting(crate::models::SourcePref::SETTING, p.key());
                            // 下载进行中切源:立刻打断当前任务并用新源重启人脸剩余缺件,
                            // 断点(.part)保留;AI 槽位保留断点,下次点「下载」续传。
                            if self.models.phase.working() {
                                self.models.cancel();
                                let files: Vec<String> = catalog
                                    .iter()
                                    .map(|(name, _)| name.to_string())
                                    .filter(|name| {
                                        crate::ai::models_dir()
                                            .map(|d| !d.join(name).is_file())
                                            .unwrap_or(false)
                                    })
                                    .collect();
                                if files.is_empty() {
                                    self.status =
                                        t("下载源已切换;当前任务已停,所需文件其实都已就位").into();
                                } else {
                                    self.start_model_files(files);
                                    self.status = t_with(
                                        "已切换为「{p1}」并用新源重启下载,断点保留接着下",
                                        &[("p1", (p.label()).to_string())],
                                    );
                                }
                            } else {
                                self.status = t_with(
                                    "模型下载源已切换为「{p1}」,下次下载生效",
                                    &[("p1", (p.label()).to_string())],
                                );
                            }
                            if self.ai_dl.as_ref().is_some_and(|d| d.phase.working()) {
                                if let Some(d) = self.ai_dl.as_mut() {
                                    d.cancel();
                                }
                                self.ai_dl_queue.clear();
                                self.ai_models_msg =
                                    t("已停止当前模型下载:断点保留,点「下载」即用新源接着下")
                                        .into();
                            }
                        }
                    }
                });
            let probing = matches!(self.models.probe, crate::models::ProbeState::Running);
            if ui
                .add_enabled(!probing, egui::Button::new(t("测速")))
                .clicked()
            {
                self.models.start_probe();
                self.status = t("正在对各下载源测速(各取 256 KB)…").into();
            }
            if probing {
                ui.spinner();
                ui.small(t("测速中…"));
            }
        });
        if self.models.model_base().is_empty() {
            ui.small(t("想用镜像下载:在 maliang.ini 里配好 ModelURL(「打开配置文件」可直达),这里才会放开「仅镜像」。"));
        }
        if let crate::models::ProbeState::Done(res) = &self.models.probe {
            for r in res {
                if r.ok {
                    let speed = r
                        .kbps
                        .map(|k| format!(" · {:.0} KB/s", k))
                        .unwrap_or_default();
                    ui.small(format!("{}:{} ms{}", r.label, r.ms, speed));
                } else {
                    ui.small(t_with(
                        "{p1}:不可用({p2})",
                        &[
                            ("p1", (r.label).to_string()),
                            (
                                "p2",
                                (r.err.as_deref().unwrap_or(t("未知错误"))).to_string(),
                            ),
                        ],
                    ));
                }
            }
        }
        ui.separator();

        // 开机提醒(勾过「不再提示」后的回勾入口)。
        let mut ask = !self.model_never_ask();
        if ui.checkbox(&mut ask, t("开机提醒下载模型")).changed() {
            let _ = self
                .store
                .set_setting("model_never_ask", if ask { "0" } else { "1" });
            self.status = if ask {
                t("已恢复开机提醒:缺人脸模型时会弹窗问一次").into()
            } else {
                t("已设为不再提示:开机不再弹人脸模型窗,下载入口留在 选项 > AI 模型").into()
            };
        }
    }

    /// 人脸视图里的精简下载入口:按钮 + 指路(完整选项在 选项 > AI 模型)。
    #[cfg(feature = "ai")]
    fn face_model_compact_ui(&mut self, ui: &mut egui::Ui, algo: crate::ai::face::FaceAlgo) {
        let working = self.models.phase.working();
        if working {
            self.face_download_progress_ui(ui);
        } else if crate::models::downloadable(algo) {
            if ui.button(t("下载缺失模型")).clicked() {
                self.start_model_download(algo);
            }
        } else if let Some(url) = crate::models::manual_hint(algo) {
            ui.small(t_with(
                "权重许可不可再分发,请自行从 {url} 获取后放入模型目录。",
                &[("url", url.to_string())],
            ));
        }
        if ui.button(t("打开模型目录")).clicked() {
            if let Some(d) = crate::ai::models_dir() {
                show_in_explorer(&d.display().to_string());
            }
        }
        ui.small(t("更多选项(复选文件、选源、测速)在 选项 > AI 模型。"));
    }

    /// 首启提示窗:缺人脸模型时问一次「现在下载吗」。
    /// 关掉后不再生扰,下载入口仍留在 选项 > AI 模型 与人脸视图里。
    #[cfg(feature = "ai")]
    fn model_prompt_window(&mut self, ctx: &egui::Context) {
        if !self.models.prompt_open {
            return;
        }
        let mut open = self.models.prompt_open;
        let mut dismissed = false;
        let mut start = false;
        // 勾选态以库里的设置为初值,否则每帧重置,复选框永远显示未勾。
        let stored_never = self.model_never_ask();
        let mut never = stored_never;
        let dir_text = crate::ai::models_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_default();
        egui::Window::new(t("缺少人脸模型"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .default_width(520.0)
            .show(ctx, |ui| {
                ui.label(t(
                    "「人物」视图要靠人脸模型工作,本机模型目录里还没有模型文件。",
                ));
                ui.small(t_with(
                    "模型目录:{dir_text}",
                    &[("dir_text", dir_text.to_string())],
                ));
                let base = self.models.model_base();
                if base.is_empty() {
                    ui.small(t(
                        "下载源:程序内置官方地址(可在 maliang.ini 配 ModelURL 作镜像兜底)",
                    ));
                } else {
                    ui.small(t_with(
                        "下载源:内置官方地址,镜像兜底 {base}(ModelURL)",
                        &[("base", base.to_string())],
                    ));
                }
                ui.label(t(
                    "识别模型体积较大,确认后会在后台分段下载,可随时关掉本窗口继续用其它功能。",
                ));
                ui.add_space(6.0);
                ui.checkbox(&mut never, t("不再提示(以后开机不再弹这个窗)"));
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.button(t("开始下载")).clicked() {
                        start = true;
                        dismissed = true;
                    }
                    if ui.button(t("以后再说")).clicked() {
                        dismissed = true;
                    }
                });
            });
        if dismissed {
            self.models.prompt_open = false;
            if start {
                // 愿意下 = 撤回「不再提示」:没下成下次开机还得再问。
                if !never {
                    let _ = self.store.set_setting("model_never_ask", "0");
                }
                let algo = crate::faces::current_algo(&self.store);
                self.start_model_download(algo);
            }
        } else if !open {
            self.models.prompt_open = false;
        }
        // 勾选状态一变就落库,不依赖点按钮那一帧;取消勾选也能正确写回。
        if never != stored_never {
            let _ = self
                .store
                .set_setting("model_never_ask", if never { "1" } else { "0" });
            if never {
                self.status =
                    t("已设为不再提示:开机不再弹人脸模型窗,下载入口在 选项 > AI 模型").into();
            }
        }
    }

    /// 后台相似人脸检索:全库余弦不放 UI 线程;在途时只记一次重跑。
    #[cfg(feature = "ai")]
    fn start_face_similar(&mut self, qid: i64) {
        self.face_similar_for = Some(qid);
        if self.face_sim_rx.is_some() {
            self.face_sim_pending = true;
            return;
        }
        let store = self.store.clone();
        let thr = self.face_similar_thr;
        let (tx, rx) = mpsc::channel();
        self.face_sim_rx = Some(rx);
        self.status = t("正在全库检索相似人脸…").into();
        std::thread::spawn(move || {
            let hits = crate::faces::similar_faces(&store, qid, thr, 60, false);
            let _ = tx.send((qid, thr, hits));
        });
    }

    /// 后台全量重聚类:与扫描共用互斥,忙时按钮禁用。
    #[cfg(feature = "ai")]
    fn start_recluster(&mut self) {
        if self.recluster_rx.is_some() {
            return;
        }
        let store = self.store.clone();
        let thr = crate::faces::cluster_thr(&store);
        let (tx, rx) = mpsc::channel();
        self.recluster_rx = Some(rx);
        self.status = t("正在重新聚类…").into();
        std::thread::spawn(move || {
            let res = crate::faces::recluster_all(&store, thr).map_err(|e| e.to_string());
            let _ = tx.send(res);
        });
    }

    /// 后台人脸识别扫描(M2c):加载模型分析一批未分析照片并入库存簇。
    #[cfg(feature = "ai")]
    fn start_face_scan(&mut self) {
        if self.face_rx.is_some() {
            self.status = t("人脸扫描进行中…").into();
            return;
        }
        // 模型缺失时先问要不要下载(不可代下方案直说怎么手动放),不进后台线程撞裸错误。
        let algo = crate::faces::current_algo(&self.store);
        if !crate::ai::face::Faces::models_present(algo) {
            let (det_f, rec_f) = algo.files();
            if crate::models::downloadable(algo) {
                self.status = t_with(
                    "缺少人脸模型 {det_f} / {rec_f}:确认后可在后台下载",
                    &[("det_f", det_f.to_string()), ("rec_f", rec_f.to_string())],
                );
                self.models.prompt_open = true;
            } else {
                let dir = crate::ai::models_dir()
                    .map(|d| d.display().to_string())
                    .unwrap_or_default();
                self.status = t_with(
                    "缺少人脸模型:请将 {det_f} 与 {rec_f} 放入模型目录 {dir},获取方式见 README",
                    &[
                        ("det_f", det_f.to_string()),
                        ("rec_f", rec_f.to_string()),
                        ("dir", dir.to_string()),
                    ],
                );
            }
            return;
        }
        let store = self.store.clone();
        let (tx, rx) = mpsc::channel();
        self.face_rx = Some(rx);
        self.face_prog = (0, 0);
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.face_cancel = Some(cancel.clone());
        let remaining = crate::faces::remaining_count(&store);
        self.status = t_with(
            "正在人脸识别(全库连续扫描,剩余 {remaining} 张)…",
            &[("remaining", remaining.to_string())],
        );
        std::thread::spawn(move || {
            crate::win::set_current_thread_priority(crate::win::Priority::BelowNormal);
            // 连续扫描直到队列空:此前单批 200 张,无人脸照片永远卡住队列,
            // 覆盖整个库要点几百次;face_done 标记修复后一次即可扫完,可取消。
            let res = crate::faces::scan_all(&store, 200, &cancel, &|done, total| {
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

    /// 刷新人物列表 + 命名建议(建议阈值随所选方案)。
    #[cfg(feature = "ai")]
    fn refresh_people_and_suggestions(&mut self) {
        self.refresh_people();
        let thr = crate::faces::suggestion_thr(&self.store);
        self.face_sugs = crate::faces::face_suggestions(&self.store, thr);
        self.refresh_face_hits();
    }

    /// 按当前选中分组重算右侧人脸列表,并跟随身份的新代表簇。
    #[cfg(feature = "ai")]
    fn refresh_face_hits(&mut self) {
        if let Some(pid) = self.sel_person {
            let representative = self
                .face_people
                .iter()
                .find(|p| p.person_id == Some(pid))
                .map(|p| p.cluster);
            match representative {
                Some(c) => self.sel_cluster = Some(c),
                None => {
                    self.sel_person = None;
                    self.sel_cluster = None;
                }
            }
        } else if let Some(c) = self.sel_cluster {
            let visible = self
                .face_people
                .iter()
                .any(|p| p.person_id.is_none() && p.cluster == c)
                || crate::faces::ignored_clusters(&self.store).contains(&c);
            if !visible {
                self.sel_cluster = None;
            }
        }
        let include_ignored = self.face_show_ignored;
        let min_q = if self.face_hide_low {
            FACE_LOW_QUALITY
        } else {
            0.0
        };
        self.face_hits = match (self.sel_person, self.sel_cluster) {
            (Some(pid), _) => {
                crate::faces::faces_for_person(&self.store, pid, include_ignored, min_q)
            }
            (None, Some(c)) => {
                crate::faces::faces_for_cluster(&self.store, c, include_ignored, min_q)
            }
            _ => Vec::new(),
        };
    }

    /// 人脸管线签名检查:存在非当前签名模板时请用户确认重建,不自动覆盖人工数据。
    #[cfg(feature = "ai")]
    fn check_face_algo_version(&mut self) {
        if crate::faces::pending_rebuild(&self.store) {
            crate::faces::drop_people_tags(&self.store);
            self.face_version_warn = true;
            return;
        }
        let tag = crate::faces::algo_tag(&self.store);
        let _ = self.store.set_setting("face_algo_version", &tag);
    }

    /// 后台语义索引(M4b):CLIP 图像编码器嵌入一批未处理照片(较慢)。
    /// lang = 索引语言(英文/中文模型各自独立索引)。
    #[cfg(feature = "ai")]
    fn start_clip_index(&mut self) {
        if self.clip_index_rx.is_some() {
            self.status = t("语义索引进行中…").into();
            return;
        }
        let store = self.store.clone();
        if crate::clip::natural_ready(&store) {
            self.status = t("已把全库排入语义索引队列(后台分批完成)").into();
            std::thread::spawn(move || {
                let _ = crate::ai_queue::enqueue_all_for(&store, crate::ai_queue::Stage::Clip);
            });
            return;
        }
        let lang = self.clip_lang;
        let (tx, rx) = mpsc::channel();
        self.clip_index_rx = Some(rx);
        self.status = t_with(
            "正在建立语义索引({p1}模型,较慢)…",
            &[("p1", (lang.label()).to_string())],
        );
        std::thread::spawn(move || {
            let res = crate::clip::scan_batch_lang(&store, lang, 200).map_err(|e| e.to_string());
            let _ = tx.send(res);
        });
    }

    /// 以图搜图:用一张照片查全库相似照片,复用语义搜索的后台通道与结果网格。
    #[cfg(feature = "ai")]
    fn start_image_search(&mut self, photo_id: i64) {
        if self.clip_search_rx.is_some() {
            self.status = t("语义搜索进行中…").into();
            return;
        }
        let path = self
            .photo_path(photo_id)
            .or_else(|| self.store.photo_path_by_id(photo_id));
        let Some(path) = path else {
            self.status = t("照片不存在").into();
            return;
        };
        let name = Path::new(&path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or(path);
        let store = self.store.clone();
        let (tx, rx) = mpsc::channel();
        self.clip_search_rx = Some(rx);
        self.clip_results.clear();
        self.clip_image_query = Some(name.clone());
        self.set_view(ViewKind::Semantic);
        self.status = t_with("正在搜相似照片:{p1}", &[("p1", name)]);
        std::thread::spawn(move || {
            let res = crate::clip::search_by_photo(&store, photo_id, 200);
            let _ = tx.send(res);
        });
    }

    /// 后台语义搜索(M4b):编码查询文本,与入库嵌入算余弦排序。
    /// 查询语言自动探测(含 CJK → 中文模型);对应语言无索引时给出提示。
    #[cfg(feature = "ai")]
    fn start_clip_search(&mut self, query: String) {
        if self.clip_search_rx.is_some() {
            self.status = t("语义搜索进行中…").into();
            return;
        }
        self.clip_image_query = None;
        let store = self.store.clone();
        let natural = crate::clip::natural_ready(&store);
        let lang = crate::clip::Lang::detect(&query);
        if !natural && crate::clip::clip_count_lang(&store, lang) == 0 {
            self.status = t_with(
                "{p1}模型还没有语义索引 — 请先在下方选择「{p2}」模型并点「语义索引」",
                &[
                    ("p1", (lang.label()).to_string()),
                    ("p2", (lang.label()).to_string()),
                ],
            );
            return;
        }
        // 地点意图要有一张带定位的基准照片才能算距离,否则只提示不过滤。
        let base_id = self.preview.or_else(|| {
            if self.sel.len() == 1 {
                self.sel.iter().next().copied()
            } else {
                None
            }
        });
        let near_base = base_id.and_then(|id| crate::ai_search::photo_gps(&store, id));
        let (tx, rx) = mpsc::channel();
        self.clip_search_rx = Some(rx);
        self.status = if natural {
            t_with("正在综合检索:{p1}", &[("p1", (query).to_string())])
        } else {
            t_with(
                "正在语义搜索({p1}模型):{query}",
                &[
                    ("p1", (lang.label()).to_string()),
                    ("query", query.to_string()),
                ],
            )
        };
        std::thread::spawn(move || {
            let parsed = crate::ai_parse::parse_query(&query, None);
            let window = crate::ai_parse::date_window(&parsed, chrono::Local::now().date_naive());
            let text = if parsed.text.is_empty() {
                query.clone()
            } else {
                parsed.text.clone()
            };
            let res: Result<(Vec<(i64, Option<f32>)>, Option<String>), String> =
                match crate::clip::search_best(&store, &text, 200) {
                    Ok((mut hits, used_natural)) => {
                        // 分类与时间过滤只在新链路上生效,旧模型没有对应数据。
                        if used_natural {
                            if let Some(cat) = parsed.category.as_deref() {
                                let tag_ver = crate::ai_queue::stage_version(
                                    &store,
                                    crate::ai_queue::Stage::Tags,
                                );
                                let allowed: std::collections::HashSet<i64> =
                                    crate::ai_tags::category_ids(
                                        &store,
                                        cat,
                                        &tag_ver,
                                        usize::MAX,
                                    )
                                    .into_iter()
                                    .collect();
                                hits.retain(|(id, _)| allowed.contains(id));
                            }
                            if let Some((start, end)) = window {
                                let ids: Vec<i64> = hits.iter().map(|(id, _)| *id).collect();
                                let allowed = crate::ai_search::ids_in_date_window(
                                    &store, &ids, &start, &end,
                                );
                                hits.retain(|(id, _)| allowed.contains(id));
                            }
                        }
                        let mut note: Option<String> = None;
                        if parsed.near {
                            let km = parsed
                                .near_km
                                .unwrap_or(crate::ai_parse::DEFAULT_NEAR_KM);
                            if let Some((la, lo)) = near_base {
                                let ids: Vec<i64> = hits.iter().map(|(id, _)| *id).collect();
                                let allowed = crate::ai_search::ids_near_point(
                                    &store, &ids, la, lo, km,
                                );
                                hits.retain(|(id, _)| allowed.contains(id));
                                note = Some(t_with(
                                    "已按基准照片周边 {p1} 公里筛选(仅含带定位的照片)",
                                    &[("p1", km.to_string())],
                                ));
                            } else {
                                note = Some(t("未指定基准照片,地点条件暂不参与筛选").to_string());
                            }
                        }
                        Ok((hits, note))
                    }
                    Err(e) => Err(e),
                };
            let _ = tx.send(res.map_err(|e| e.to_string()));
        });
    }

    /// 当前视图范围内的照片 id(选择/导出的作用范围与视图一致)。
    fn view_ids(&self) -> Vec<i64> {
        match self.view {
            ViewKind::Recent => self.recent_idxs.iter().map(|&i| self.rows[i].id).collect(),
            ViewKind::Timeline => self.rows.iter().map(|r| r.id).collect(),
            ViewKind::Folder => {
                // 铺开全部文件夹时,选择/导出/放映的范围就是整个图库
                // (与中间栏看到的组头一致,不能只算 selected_folder 那一组)。
                if self.folder_show_all {
                    return self.rows.iter().map(|r| r.id).collect();
                }
                // 与文件夹视图同一套选中口径:子树根优先(递归聚合),
                // 否则回退槽位选中。范围口径必须与中间栏完全一致。
                let folder = self
                    .subtree_sel
                    .clone()
                    .or_else(|| self.folders.get(self.selected_folder).cloned())
                    .unwrap_or_default();
                self.rows
                    .iter()
                    .filter(|r| path_under_dir(&r.path, &folder))
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
            ViewKind::Tags => self.tags_scope(),
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

    /// 分享条目 = 当前视图 + 可选效果(与导出同一作用域语义)。
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

    /// 后台线程暂存邮件附件并调 Simple MAPI(附件暂存目录链路)。
    fn start_email(&mut self) {
        if self.share_rx.is_some() {
            self.status = t("已有分享任务进行中…").into();
            return;
        }
        let items = self.share_items();
        if items.is_empty() {
            self.status = t("当前视图没有可发送的照片").into();
            return;
        }
        let size = self.email_size;
        let dir = std::env::temp_dir().join("maliang_email");
        let (tx, rx) = mpsc::channel();
        self.share_rx = Some(rx);
        self.status = t_with(
            "正在准备 {p1} 张照片的邮件附件…",
            &[("p1", (items.len()).to_string())],
        );
        std::thread::spawn(move || {
            let res = (|| -> anyhow::Result<String> {
                let files = crate::share::stage_jpegs(&items, size.px(), &dir)?;
                crate::share::send_email(t("Maliang 照片分享"), &files)?;
                Ok(t_with(
                    "邮件已交给系统邮件客户端(附件 {p1} 张)",
                    &[("p1", (files.len()).to_string())],
                ))
            })();
            let _ = tx.send(res.map_err(|e| e.to_string()));
        });
    }

    /// 后台线程暂存礼品光盘内容并走 IMAPI2 刻录。
    fn start_gift_cd(&mut self) {
        if self.share_rx.is_some() {
            self.status = t("已有分享任务进行中…").into();
            return;
        }
        let items = self.share_items();
        if items.is_empty() {
            self.status = t("当前视图没有可刻录的照片").into();
            return;
        }
        let size = self.cd_size;
        let dir = std::env::temp_dir().join("maliang_giftcd");
        let (tx, rx) = mpsc::channel();
        self.share_rx = Some(rx);
        self.status = t_with(
            "正在准备礼品光盘内容({p1} 张)…",
            &[("p1", (items.len()).to_string())],
        );
        std::thread::spawn(move || {
            let res = (|| -> anyhow::Result<String> {
                let files = crate::share::stage_gift_cd(&items, size.px(), &dir)?;
                crate::share::burn_folder(&dir, "Maliang CD", "Maliang")?;
                Ok(t_with(
                    "刻录完成(共 {p1} 张)",
                    &[("p1", (files.len()).to_string())],
                ))
            })();
            let _ = tx.send(res.map_err(|e| e.to_string()));
        });
    }

    /// 弹出目录选择,后台线程把当前视图的原图复制过去。
    fn start_export(&mut self) {
        if self.export_rx.is_some() {
            self.status = t("导出进行中…").into();
            return;
        }
        let paths = self.view_paths();
        if paths.is_empty() {
            self.status = t("当前视图没有可导出的照片").into();
            return;
        }
        let Some(dir) = rfd::FileDialog::new()
            .set_title(t("选择导出目录"))
            .pick_folder()
        else {
            return;
        };
        // 导出可选择"套用已保存的效果"(不修改原图)。
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
        self.status = t_with("正在导出 {p1} 张…", &[("p1", (items.len()).to_string())]);
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
            self.status = t("WiFi 导入已停止").into();
        } else {
            match crate::net::start(self.store.clone()) {
                Ok(h) => {
                    self.status = t_with("WiFi 导入已启动:{p1}/", &[("p1", (h.addr).to_string())]);
                    let _ = self.store.upsert_watched(&crate::net::incoming_dir());
                    self.wifi = Some(h);
                    self.wifi_panel = true;
                    self.watched = self.store.watched_stats();
                }
                Err(e) => {
                    self.status = t_with("WiFi 导入启动失败: {e}", &[("e", e.to_string())]);
                }
            }
        }
    }

    /// 文件 > 导入 > 从设备(相机/手机):打开统一设备列表窗口。
    ///
    /// WPD 设备 + 可移动卷合成一个统一列表。
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

    /// 重新读取变更历史(最近 500 条)。
    fn reload_changes(&mut self) {
        self.change_log = self.store.recent_changes(500);
    }

    /// 按输入的 uid64 查照片路径。
    fn lookup_uid(&mut self) -> Result<String, String> {
        let t = self.uid_input.trim();
        if t.is_empty() {
            return Err(crate::i18n::t("请输入 uid64").into());
        }
        let raw = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X"));
        let uid = match raw {
            Some(hex) => u64::from_str_radix(hex, 16)
                .map_err(|e| t_with("十六进制解析失败: {e}", &[("e", e.to_string())]))?,
            None => t
                .parse::<u64>()
                .map_err(|e| t_with("数字解析失败: {e}", &[("e", e.to_string())]))?,
        };
        // uid64 存的是 i64(高位为 1 时是负数)→ 还原成 u64。
        let id = self
            .store
            .photo_by_uid64(uid)
            .ok_or_else(|| t_with("未找到 uid64 = {uid} 的照片", &[("uid", uid.to_string())]))?;
        let path: String = self
            .store
            .read(|conn| conn.query_row("SELECT path FROM photos WHERE id=?1", [id], |r| r.get(0)))
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

    /// 切换缩略图档位。    ///
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
        self.status = t_with("已切换到{p1}", &[("p1", (tier.label()).to_string())]);
    }

    // ===== 效果系统 =====

    /// 读某张照片已保存的效果栈(`photos.filters` 列)。
    fn fx_stack_for(&self, id: i64) -> crate::effects::FilterStack {
        match self.store.get_filters(id) {
            Some(s) => crate::effects::FilterStack::parse(&s),
            None => crate::effects::FilterStack::new(),
        }
    }

    /// 预览实际渲染用的效果栈 = 已存效果 + 当前草稿(草稿覆盖同名项)。
    ///
    /// "参数 + op 树实时渲染":滑块一动 op 树立刻重建 → 等价实现:
    /// 预览纹理签名随草稿变化而重算。
    fn fx_preview_stack(&self, id: i64) -> crate::effects::FilterStack {
        // 只读速看格式不套效果:动画帧与合成图都不参与编辑链。
        if self.readonly_media(id) {
            return crate::effects::FilterStack::new();
        }
        let mut st = self.fx_stack_for(id);
        if Some(id) == self.preview {
            if !self.fx_draft.is_identity() {
                st.push(crate::effects::Effect::enhance(&self.fx_draft));
            }
            if !self.fx_curve_draft.is_identity() {
                st.push(crate::effects::Effect::curves(&self.fx_curve_draft));
            }
            // 草稿一律带上当前滑杆参数,预览与「应用」写库的形态完全一致。
            if let Some(eff) = self.fx_oneclick_draft_effect() {
                st.push(eff);
            }
            if let Some(name) = &self.fx_recipe {
                let mut params = std::collections::BTreeMap::new();
                params.insert("amount".into(), format!("{:.4}", self.fx_recipe_amount));
                st.push(crate::effects::Effect::oneclick(name, params));
            }
            // 编辑视图工具格子里选中的带滑杆工具:先只进预览栈,
            // 点「应用」才落库(与调整页草稿同一套语义)。
            if let Some(name) = self.editor_tool {
                st.push(crate::effects::Effect::oneclick(
                    name,
                    editor::editor_params(name, self.editor_tool_amount),
                ));
            }
        }
        st
    }

    /// 一键效果草稿按当前滑杆值构造(预览与落库共用一份参数)。
    fn fx_oneclick_draft_effect(&self) -> Option<crate::effects::Effect> {
        let name = self.fx_oneclick.clone()?;
        Some(crate::effects::Effect::oneclick(
            &name,
            self.fx_oneclick_params(&name),
        ))
    }

    /// 一键效果的参数表:颗粒与锐化各有独立滑杆,调色类共用一个。
    fn fx_oneclick_params(&self, name: &str) -> std::collections::BTreeMap<String, String> {
        let mut p = std::collections::BTreeMap::new();
        match name {
            "grain" => {
                let preset = crate::oneclick::grain_preset(0);
                p.insert("amount".into(), format!("{:.4}", self.fx_grain_amount));
                p.insert("seed".into(), preset.seed.to_string());
                p.insert("channeloptions".into(), preset.channel_options.to_string());
                p.insert("grayscale".into(), preset.grayscale.to_string());
            }
            "sepia" => {
                let (mix, shadow, highlight) = crate::oneclick::sepia_default();
                p.insert("mix".into(), format!("{mix:.4}"));
                p.insert(
                    "shadow".into(),
                    format!("{:02x}{:02x}{:02x}", shadow[0], shadow[1], shadow[2]),
                );
                p.insert(
                    "highlight".into(),
                    format!(
                        "{:02x}{:02x}{:02x}",
                        highlight[0], highlight[1], highlight[2]
                    ),
                );
            }
            "unsharp" => {
                p.insert("amount".into(), format!("{:.4}", self.fx_sharpen_amount));
            }
            "warm" | "cool" | "vignette" | "faded" | "tilt" | "shadow" => {
                p.insert("amount".into(), format!("{:.4}", self.fx_tone_amount));
            }
            _ => {}
        }
        p
    }

    /// 把草稿调整写入选中(或当前预览)的照片,并记入撤销栈。
    ///
    /// 提交一个命名编辑项 → 入撤销栈。
    fn fx_apply_draft(&mut self, targets: &[i64]) {
        if targets.is_empty() {
            self.fx_msg = t("没有可应用的照片").into();
            return;
        }
        // 批量应用时把只读格式摘掉:效果写进库记录也只会让预览语义含混。
        let targets: Vec<i64> = targets
            .iter()
            .copied()
            .filter(|id| !self.readonly_media(*id))
            .collect();
        if targets.is_empty() {
            self.fx_msg = t("选中的都是只读速看预览格式,未应用").into();
            return;
        }
        let adj = self.fx_draft.clamped();
        // 恒等调整不入栈(空效果不建编辑项)。
        if crate::effects::Effect::enhance(&adj).is_identity() {
            self.fx_msg = t("当前调整全为默认值,未应用").into();
            return;
        }
        // 先记下提交前的栈(撤销用),再统一写入。
        let befores: Vec<(i64, crate::effects::FilterStack)> = targets
            .iter()
            .map(|id| (*id, crate::effects::read_stack(&self.store, *id)))
            .collect();
        let n = match crate::effects::commit_adjust(&self.store, &targets, &adj) {
            Ok(n) => n,
            Err(e) => {
                self.fx_msg = t_with("写入失败: {e}", &[("e", e.to_string())]);
                return;
            }
        };
        for (id, before) in befores {
            let after = crate::effects::read_stack(&self.store, id);
            let stack = self
                .fx_undo
                .entry(id)
                .or_insert_with(crate::effects::EditStack::new);
            stack.commit(t("增强"), before, after);
        }
        // 预览纹理需要重算(签名变化即触发)。
        self.fx_applied_sig = None;
        self.fx_draft_id = None;
        self.fx_curve_draft_id = None;
        self.fx_msg = t_with("已应用到 {n} 张照片", &[("n", n.to_string())]);
    }

    /// 撤销一步。
    fn fx_undo_step(&mut self, id: i64) {
        let Some(stack) = self.fx_undo.get_mut(&id) else {
            self.fx_msg = t("无可撤销的操作").into();
            return;
        };
        match crate::effects::undo_commit(&self.store, stack, id) {
            Some((label, _)) => {
                self.fx_applied_sig = None;
                // 栈被换回旧状态,两条草稿必须按新栈重新载入,否则预览会叠旧值。
                self.fx_draft_id = None;
                self.fx_curve_draft_id = None;
                self.fx_msg = t_with("已撤销: {label}", &[("label", label.to_string())]);
            }
            None => self.fx_msg = t("无可撤销的操作").into(),
        }
    }

    /// 重做一步。
    fn fx_redo_step(&mut self, id: i64) {
        let Some(stack) = self.fx_undo.get_mut(&id) else {
            self.fx_msg = t("无可重做的操作").into();
            return;
        };
        match crate::effects::redo_commit(&self.store, stack, id) {
            Some((label, _)) => {
                self.fx_applied_sig = None;
                self.fx_draft_id = None;
                self.fx_curve_draft_id = None;
                self.fx_msg = t_with("已重做: {label}", &[("label", label.to_string())]);
            }
            None => self.fx_msg = t("无可重做的操作").into(),
        }
    }

    /// 复制所有效果:从**单张**照片取效果栈到内存暂存。
    ///
    /// 选中数量校验要求"恰好选中一张"(提示语
    /// "Please select a single picture to copy the effects from.")。
    fn fx_copy_all(&mut self, source: i64) {
        let st = crate::effects::copy_from(&self.store, source);
        self.fx_clipboard = Some(st.clone());
        self.fx_msg = t_with(
            "已复制效果({p1} 项)",
            &[("p1", (st.items.len()).to_string())],
        );
    }

    /// 粘贴所有效果:把暂存的效果栈写给选中的多张照片。
    ///
    /// 效果批量应用对电影类型跳过(库内只有照片,天然满足)。
    fn fx_paste_all(&mut self, targets: &[i64]) {
        let Some(st) = self.fx_clipboard.clone() else {
            self.fx_msg = t("剪贴板里没有效果").into();
            return;
        };
        if targets.is_empty() {
            self.fx_msg = t("没有选中的照片").into();
            return;
        }
        match crate::effects::paste_to(&self.store, targets, &st) {
            Ok(n) => {
                self.fx_applied_sig = None;
                self.fx_draft_id = None;
                self.fx_curve_draft_id = None;
                self.fx_msg = t_with("已粘贴效果到 {n} 张照片", &[("n", n.to_string())]);
            }
            Err(e) => self.fx_msg = t_with("粘贴失败: {e}", &[("e", e.to_string())]),
        }
    }

    /// 效果面板 UI。
    fn fx_panel_ui(&mut self, ui: &mut egui::Ui) {
        let cur = self.preview;
        let Some(id) = cur else {
            ui.label(t("先打开一张照片"));
            return;
        };
        // 打开新照片时,把已存的效果载入草稿(打开照片即显示其效果)。
        // 只在**换照片**时载入 —— 否则会把用户正在拖的滑块冲掉。
        if self.fx_draft_id != Some(id) {
            self.fx_draft_id = Some(id);
            let stored = self.fx_stack_for(id);
            self.fx_draft = stored.merged_adjust();
        }
        ui.heading(t("效果"));
        // 标签页(编辑面板按功能分页,不堆长列表)。
        ui.horizontal(|ui| {
            for (i, name) in [t("调整"), t("曲线"), t("一键"), t("裁剪"), t("维护")]
                .iter()
                .enumerate()
            {
                if ui.selectable_label(self.fx_tab == i, *name).clicked() {
                    self.fx_tab = i;
                }
            }
        });
        ui.separator();
        match self.fx_tab {
            0 => {
                ui.small(t("亮度 / 对比度 / 饱和度 / 曝光 / 暗部 / 锐化 / 模糊"));
                let mut d = self.fx_draft;
                let mut changed = false;
                changed |= ui
                    .add(egui::Slider::new(&mut d.brightness, -100.0..=100.0).text(t("亮度")))
                    .changed();
                changed |= ui
                    .add(egui::Slider::new(&mut d.contrast, -100.0..=100.0).text(t("对比度")))
                    .changed();
                changed |= ui
                    .add(egui::Slider::new(&mut d.saturation, -100.0..=100.0).text(t("饱和度")))
                    .changed();
                changed |= ui
                    .add(egui::Slider::new(&mut d.exposure, -3.0..=3.0).text(t("曝光(档)")))
                    .changed();
                changed |= ui
                    .add(egui::Slider::new(&mut d.blacks, -100.0..=100.0).text(t("暗部")))
                    .changed();
                changed |= ui
                    .add(egui::Slider::new(&mut d.sharpen, 0.0..=100.0).text(t("锐化")))
                    .changed();
                changed |= ui
                    .add(egui::Slider::new(&mut d.blur, 0.0..=20.0).text(t("模糊")))
                    .changed();
                if changed {
                    self.fx_draft = d;
                    // 草稿变化 → 预览实时重算(不落库,属"未提交"状态)。
                    self.fx_applied_sig = None;
                }
                ui.horizontal(|ui| {
                    if ui.button(t("应用到选中")).clicked() {
                        let targets: Vec<i64> = if self.sel.is_empty() {
                            vec![id]
                        } else {
                            self.sel.iter().copied().collect()
                        };
                        self.fx_apply_draft(&targets);
                    }
                    if ui.button(t("应用到这张")).clicked() {
                        self.fx_apply_draft(&[id]);
                    }
                    if ui.button(t("重置")).clicked() {
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
                .add_enabled(can_undo, egui::Button::new(t("撤销")))
                .clicked()
            {
                self.fx_undo_step(id);
            }
            if ui
                .add_enabled(can_redo, egui::Button::new(t("重做")))
                .clicked()
            {
                self.fx_redo_step(id);
            }
            if ui.button(t("复制所有效果")).clicked() {
                self.fx_copy_all(id);
            }
            let paste_targets: Vec<i64> = if self.sel.is_empty() {
                vec![id]
            } else {
                self.sel.iter().copied().collect()
            };
            if ui.button(t("粘贴所有效果")).clicked() {
                self.fx_paste_all(&paste_targets);
            }
        });
        // 清除效果(移除栈中所有命名项 —— 逐项撤销到空栈)。
        if ui.button(t("清除这张的所有效果")).clicked() {
            let before = self.fx_stack_for(id);
            if before.is_empty() {
                self.fx_msg = t("这张照片没有效果").into();
            } else {
                let mut after = before.clone();
                while after.remove("enhance").is_some() {}
                let _ = self.store.set_filters(id, &after.serialize());
                let stack = self
                    .fx_undo
                    .entry(id)
                    .or_insert_with(crate::effects::EditStack::new);
                stack.commit(t("清除效果"), before, after);
                self.fx_applied_sig = None;
                self.fx_draft = crate::effects::Adjust::default();
                self.fx_msg = t("已清除效果").into();
            }
        }
        // 统计:整个视图里有多少张带效果(效果是记录级属性)。
        let view = self.view_ids();
        let with_fx = self.store.get_filters_many(&view).len();
        ui.small(t_with(
            "当前视图 {with_fx} 张带效果",
            &[("with_fx", with_fx.to_string())],
        ));
        // 已应用的效果项清单(效果栈的每一项都是命名编辑项)。
        let st = self.fx_stack_for(id);
        if !st.is_empty() {
            ui.small(t("本张已应用:"));
            for it in &st.items {
                // 未实现的项(如需笔刷交互)要显式标出,别让用户以为已生效
                let tag = match crate::effects::support_of(&it.name) {
                    crate::effects::Support::ReferenceOnly => t("暂不支持"),
                    crate::effects::Support::Unknown => t("未知名"),
                    crate::effects::Support::Pixel => {
                        if it.is_oneclick() {
                            t("一键")
                        } else {
                            t("调整")
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

    /// 字符串池维护区。
    ///
    /// 字符串池:按列隔离、
    /// append-only、**无引用计数**,回收靠「写时重建」。
    /// 这个面板把内部状态暴露出来,并提供手动触发重建的入口。
    fn pool_maintenance_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading(t("字符串池"));
        ui.small(t(
            "每列一个去重池,行记录只存 32 位 id;池按旧 id 升序重建(写时重建,无引用计数)",
        ));
        let stats = self.store.pool_stats().unwrap_or_default();
        if stats.is_empty() {
            ui.small(t("尚未池化任何列(扫描带 EXIF 相机型号的照片后会填充)"));
            return;
        }
        let mut total_entries = 0i64;
        let mut total_refs = 0i64;
        for (pool, n, refs) in &stats {
            total_entries += *n;
            total_refs += *refs;
            ui.small(t_with(
                "  · {pool}:池内 {n} 条/ 引用 {refs} 行  (去重率 {p1}%)",
                &[
                    ("pool", pool.to_string()),
                    ("n", n.to_string()),
                    ("refs", refs.to_string()),
                    (
                        "p1",
                        format!(
                            "{p1:.1}",
                            p1 = if *refs > 0 {
                                *n as f64 * 100.0 / *refs as f64
                            } else {
                                0.0
                            }
                        ),
                    ),
                ],
            ));
        }
        if total_refs > 0 {
            ui.small(t_with(
                "合计:{p1} 条池条目承载 {total_refs} 行引用(平均压缩 {p2}×)",
                &[
                    ("p1", (total_entries).to_string()),
                    ("total_refs", total_refs.to_string()),
                    (
                        "p2",
                        format!(
                            "{p2:.1}",
                            p2 = (total_refs as f64 / total_entries.max(1) as f64)
                        ),
                    ),
                ],
            ));
        }
        // 重建前的预估(纯函数,不改状态):能省下多少条孤儿
        if let (Ok(set), Ok(live)) = (self.store.load_pool_set(), self.store.live_pool_ids()) {
            for (pool, cur, alive) in crate::pool::vacuum_plan(&set, &live) {
                if cur > alive {
                    ui.small(t_with(
                        "  · {pool}:可回收 {p1} 条孤儿({cur} → {alive})",
                        &[
                            ("pool", pool.to_string()),
                            ("p1", (cur - alive).to_string()),
                            ("cur", cur.to_string()),
                            ("alive", alive.to_string()),
                        ],
                    ));
                }
            }
        }
        ui.horizontal(|ui| {
            if ui.button(t("重建池(回收孤儿)")).clicked() {
                match self.store.vacuum_all_pools() {
                    Ok(report) => {
                        let mut removed = 0usize;
                        let mut remapped = 0usize;
                        for (_pool, (o, m)) in &report {
                            removed += o;
                            remapped += m;
                        }
                        self.fx_msg = t_with(
                            "池重建完成:回收孤儿 {removed} 条,重映射 {remapped} 行",
                            &[
                                ("removed", removed.to_string()),
                                ("remapped", remapped.to_string()),
                            ],
                        );
                    }
                    Err(e) => self.fx_msg = t_with("池重建失败:{e}", &[("e", e.to_string())]),
                }
            }
            if ui.button(t("查一个池 id")).clicked() {
                // 诊断入口:按 id 查询池内字符串
                let id = self.pool_probe_id;
                match self.store.pooled_value(crate::timeline::CAM_POOL, id) {
                    Some(s) => self.fx_msg = format!("photos_cam[{id}] = {s}"),
                    None => {
                        self.fx_msg = t_with("photos_cam[{id}] 不存在", &[("id", id.to_string())])
                    }
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label(t("池 id:"));
            ui.add(egui::DragValue::new(&mut self.pool_probe_id).range(0..=9999));
        });
        ui.small(t_with(
            "查重哈希(seed 0x12345678):hash(\"Nikon D850\") = 0x{p1}",
            &[(
                "p1",
                format!("{p1:08x}", p1 = (crate::pool::pool_hash("Nikon D850"))),
            )],
        ));
        // 内存池快照:展示池的当前内容
        if let Ok(set) = self.store.load_pool_set() {
            let total = set.total_entries();
            ui.small(t_with(
                "内存池快照:{p1} 个池,共 {total} 条(去重后)",
                &[
                    ("p1", (set.names().len()).to_string()),
                    ("total", total.to_string()),
                ],
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
            ui.small(t_with(
                "相机型号(池列解析,共 {p1} 种):",
                &[("p1", (cams.len()).to_string())],
            ));
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
    /// 预设曲线数据为内置常量。
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
        ui.heading(t("色调曲线"));
        ui.small(t(
            "自然三次样条:Master 先算出公共值,RGB 三条曲线各自以同一值求值",
        ));
        // 内置滤镜预设
        let mut picked = 0usize;
        ui.horizontal_wrapped(|ui| {
            if ui.button(t("电影感")).clicked() {
                picked = 1;
            }
            if ui.button(t("交叉冲洗")).clicked() {
                picked = 2;
            }
            if ui.button(t("六十年代")).clicked() {
                picked = 3;
            }
            if ui.button(t("反相")).clicked() {
                picked = 4;
            }
            if ui.button(t("奥顿")).clicked() {
                picked = 5;
            }
            if ui.button(t("素描")).clicked() {
                picked = 6;
            }
            if ui.button(t("清空曲线")).clicked() {
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
            .add(egui::Slider::new(&mut exp, -3.0..=3.0).text(t("曝光(档)")))
            .changed()
        {
            self.fx_curve_draft.exposure = exp;
            self.fx_applied_sig = None;
        }
        ui.horizontal(|ui| {
            if ui.button(t("应用曲线")).clicked() {
                self.fx_commit_effect(
                    id,
                    t("曲线"),
                    crate::effects::Effect::curves(&self.fx_curve_draft),
                );
            }
            if ui.button(t("移除曲线")).clicked() {
                self.fx_remove_effect(id, t("曲线"), "curves");
            }
        });
    }

    /// 一键效果区 UI。
    ///
    /// - `bw`/`sepia`/`grain`/`unsharp` 的算法已实现 → 可用。
    /// - `warm`/`cool`/`vignette`/`faded` 是本项目自主设计的调色 → 可用,
    ///   强度共用一个滑块(量程 0~100)。
    fn fx_oneclick_ui(&mut self, ui: &mut egui::Ui, id: i64) {
        ui.heading(t("一键效果"));
        ui.horizontal_wrapped(|ui| {
            for (label, key) in [
                (t("黑白"), "bw"),
                (t("棕褐色"), "sepia"),
                (t("颗粒"), "grain"),
                (t("锐化"), "unsharp"),
                (t("暖调"), "warm"),
                (t("冷调"), "cool"),
                (t("暗角"), "vignette"),
                (t("褪色"), "faded"),
                (t("扶正"), "tilt"),
                (t("暗部/高光"), "shadow"),
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
                .add(egui::Slider::new(&mut self.fx_grain_amount, 0.0..=1.0).text(t("颗粒强度")))
                .changed()
            {
                self.fx_applied_sig = None;
            }
        }
        if self.fx_oneclick.as_deref() == Some("unsharp") {
            if ui
                .add(
                    egui::Slider::new(&mut self.fx_sharpen_amount, 0.0..=100.0).text(t("锐化强度")),
                )
                .changed()
            {
                self.fx_applied_sig = None;
            }
        }
        // 自主设计调色四件套共用一个强度滑块(语义都是"0 = 不动,100 = 最重")。
        if let Some(text) = match self.fx_oneclick.as_deref() {
            Some("warm") => Some(t("暖调强度")),
            Some("cool") => Some(t("冷调强度")),
            Some("vignette") => Some(t("暗角强度")),
            Some("faded") => Some(t("褪色强度")),
            // 扶正:0~100 折算到 ±15°,50 = 不旋转。
            Some("tilt") => Some(t("扶正角度")),
            // 暗部/高光:一根滑杆同时驱动两端。
            Some("shadow") => Some(t("修复强度")),
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
            if ui.button(t("应用一键效果")).clicked() {
                let Some(name) = self.fx_oneclick.clone() else {
                    self.fx_msg = t("先选一个一键效果").into();
                    return;
                };
                let params = self.fx_oneclick_params(&name);
                self.fx_commit_effect(
                    id,
                    t("一键效果"),
                    crate::effects::Effect::oneclick(&name, params),
                );
            }
            if ui.button(t("移除一键效果")).clicked() {
                for n in [
                    "bw", "sepia", "grain", "unsharp", "warm", "cool", "vignette", "faded", "tilt",
                    "shadow",
                ] {
                    self.fx_remove_effect(id, t("一键效果"), n);
                }
                self.fx_oneclick = None;
            }
        });

        // ===== 风格配方:一个名字对应一串固定顺序的操作 =====
        ui.separator();
        ui.label(t("风格配方(整组效果,强度控制与原图的混合比例)"));
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
            let text = t_with(
                "{p1} 强度",
                &[(
                    "p1",
                    (crate::recipes::label(self.fx_recipe.as_deref().unwrap_or(""))).to_string(),
                )],
            );
            if ui
                .add(egui::Slider::new(&mut self.fx_recipe_amount, 0.0..=100.0).text(text))
                .changed()
            {
                self.fx_applied_sig = None;
            }
        }
        ui.horizontal(|ui| {
            if ui.button(t("应用风格配方")).clicked() {
                let Some(name) = self.fx_recipe.clone() else {
                    self.fx_msg = t("先选一个风格配方").into();
                    return;
                };
                let mut params = std::collections::BTreeMap::new();
                params.insert("amount".into(), format!("{:.4}", self.fx_recipe_amount));
                self.fx_commit_effect(
                    id,
                    t("风格配方"),
                    crate::effects::Effect::oneclick(&name, params),
                );
            }
            if ui.button(t("移除风格配方")).clicked() {
                for n in crate::recipes::RECIPES {
                    self.fx_remove_effect(id, t("风格配方"), n);
                }
                self.fx_recipe = None;
            }
        });
    }

    /// 视频裁剪面板(trim UI)。
    ///
    /// 单位说明:`moviestart` / `movieend` 以 **100 纳秒为计数单位**
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
        ui.heading(t("视频裁剪"));
        ui.small(t("起点 / 终点(秒)。内部以 100 纳秒为单位,此处按秒操作。"));
        let mut changed = false;
        changed |= ui
            .add(
                egui::DragValue::new(&mut self.fx_trim_start)
                    .range(0.0..=f64::MAX)
                    .speed(0.1)
                    .prefix(t("起点 ")),
            )
            .changed();
        changed |= ui
            .add(
                egui::DragValue::new(&mut self.fx_trim_end)
                    .range(0.0..=f64::MAX)
                    .speed(0.1)
                    .prefix(t("终点 ")),
            )
            .changed();
        changed |= ui
            .add(
                egui::DragValue::new(&mut self.fx_trim_total)
                    .range(0.0..=f64::MAX)
                    .speed(0.5)
                    .prefix(t("总时长(0=不限) ")),
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
        ui.small(t_with(
            "区间 {p1} 秒({p2})",
            &[
                ("p1", (format!("{:.1}", t.duration_secs())).to_string()),
                ("p2", (t.label()).to_string()),
            ],
        ));
        ui.small(t_with(
            "落库片段:{p1}",
            &[("p1", (t.serialize()).to_string())],
        ));
        ui.horizontal(|ui| {
            if ui.button(crate::i18n::t("应用裁剪")).clicked() {
                if t.is_empty() {
                    self.fx_msg = crate::i18n::t("终点需大于起点").into();
                    return;
                }
                self.fx_commit_effect(
                    id,
                    crate::i18n::t("视频裁剪"),
                    crate::effects::Effect::oneclick("movie", t.to_params()),
                );
                self.fx_msg = crate::i18n::t("已应用裁剪").into();
            }
            if ui.button(crate::i18n::t("清除裁剪")).clicked() {
                self.fx_remove_effect(id, crate::i18n::t("视频裁剪"), "movie");
                self.fx_trim_start = 0.0;
                self.fx_trim_end = 0.0;
            }
        });
    }

    /// 把一个效果项写进目标照片的效果栈并记入撤销栈。
    fn fx_commit_effect(&mut self, id: i64, label: &str, eff: crate::effects::Effect) {
        if self.readonly_media(id) {
            self.fx_msg = t("只读速看预览格式不支持编辑效果").into();
            return;
        }
        let before = self.fx_stack_for(id);
        let mut after = before.clone();
        after.push(eff);
        if let Err(e) = self.store.set_filters(id, &after.serialize()) {
            self.fx_msg = t_with("写入失败: {e}", &[("e", e.to_string())]);
            return;
        }
        let stack = self
            .fx_undo
            .entry(id)
            .or_insert_with(crate::effects::EditStack::new);
        stack.commit(label, before, after);
        self.fx_applied_sig = None;
        self.fx_msg = t_with("已应用: {label}", &[("label", label.to_string())]);
    }

    /// 从目标照片的效果栈移除指定名字的效果项(并记入撤销栈)。
    fn fx_remove_effect(&mut self, id: i64, label: &str, name: &str) {
        let before = self.fx_stack_for(id);
        let mut after = before.clone();
        if after.remove(name).is_none() {
            return;
        }
        if let Err(e) = self.store.set_filters(id, &after.serialize()) {
            self.fx_msg = t_with("写入失败: {e}", &[("e", e.to_string())]);
            return;
        }
        let stack = self
            .fx_undo
            .entry(id)
            .or_insert_with(crate::effects::EditStack::new);
        stack.commit(label, before, after);
        self.fx_applied_sig = None;
    }

    /// 90° 直角旋转(顺时针 / 逆时针)。
    ///
    /// 效果栈的同名项会被替换,所以先把既有 `rot` 的 quarter 累加再提交 ——
    /// 连点两次顺时针 = 180°,四次回到原位(quarter 归零时直接移除该项)。
    fn preview_rotate(&mut self, id: i64, quarter_delta: u32) {
        if self.readonly_media(id) {
            self.fx_msg = t("只读速看预览格式不支持旋转(不写原文件)").into();
            return;
        }
        let cur = self
            .fx_preview_stack(id)
            .items
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case("rot"))
            .and_then(|e| e.params.get("quarter").and_then(|v| v.parse::<u32>().ok()))
            .unwrap_or(0);
        let q = (cur + quarter_delta) % 4;
        if q == 0 {
            self.fx_remove_effect(id, t("旋转复位"), "rot");
            self.fx_msg = t("已复位旋转").into();
        } else {
            let mut params = std::collections::BTreeMap::new();
            params.insert("quarter".into(), q.to_string());
            self.fx_commit_effect(
                id,
                if quarter_delta % 2 == 1 {
                    t("旋转90°")
                } else {
                    t("旋转180°")
                },
                crate::effects::Effect {
                    name: "rot".into(),
                    params,
                },
            );
        }
    }

    /// 读取自定义裁剪比例(设置项「custom_crop_ratios」,格式 name|w|h 分号分隔)。
    /// 对应「添加自定义比例」对话框的持久化。
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

    /// 进入幻灯片放映(当前视图逐张全屏放映)。
    /// 间隔 / 循环来自设置项(选项对话框可改)。
    fn start_slideshow(&mut self) {
        if self.view_ids().is_empty() {
            self.fx_msg = t("当前视图没有照片").into();
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
    /// 按路径设壁纸(网格右键与快速查找右键共用;后台线程解码转 JPEG)。
    fn set_wallpaper(&mut self, path: &str) {
        if self.wallpaper_rx.is_some() {
            self.status = t("上一个壁纸设置还在进行中…").into();
            return;
        }
        let path = PathBuf::from(path);
        self.status = t_with(
            "正在设为桌面壁纸:{p1}…",
            &[("p1", (path.display()).to_string())],
        );
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
                    Ok(t("已设为桌面壁纸").into())
                } else {
                    Err(t("SystemParametersInfoW 调用失败").into())
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
    /// 浮动属性窗与右栏抽屉「属性」页共用(属性详情部分)。
    fn props_detail_ui(&mut self, ui: &mut egui::Ui, id: i64) {
        let Some(&idx) = self.id_row.get(&id) else {
            ui.small(t("该照片不在当前视图。"));
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
        // 大小 + 修改日期。
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
            ui.small(t_with(
                "{size_s}    修改:{mtime}",
                &[("size_s", size_s.to_string()), ("mtime", mtime.to_string())],
            ));
        }
        ui.separator();
        // Property / Value 两列清单。
        egui::Grid::new("props-grid").num_columns(2).show(ui, |ui| {
            ui.label(t("类型"));
            ui.label(if kind == crate::video::kind::MOVIE {
                t("视频").into()
            } else {
                Path::new(&path)
                    .extension()
                    .map(|e| {
                        t_with(
                            "图片({p1})",
                            &[("p1", (e.to_string_lossy().to_uppercase()).to_string())],
                        )
                    })
                    .unwrap_or_else(|| t("图片").into())
            });
            ui.end_row();
            ui.label(t("像素"));
            ui.label(match dims {
                (Some(w), Some(h)) => format!("{w} × {h}"),
                _ => "—".into(),
            });
            ui.end_row();
            ui.label(t("拍摄时间"));
            ui.label(taken.unwrap_or_default().replace('T', " "));
            ui.end_row();
            ui.label(t("相机"));
            ui.label(if cam.is_empty() { "—".into() } else { cam });
            ui.end_row();
            ui.label("GPS");
            ui.label(match (lat, lon) {
                (Some(a), Some(b)) => format!("{a:.5}, {b:.5}"),
                _ => "—".into(),
            });
            ui.end_row();
            ui.label(t("星标"));
            ui.label(if star {
                t("★ 已加星")
            } else {
                t("☆ 未加星")
            });
            ui.end_row();
            ui.label(t("标题"));
            ui.label(if caption.is_empty() {
                "—".into()
            } else {
                caption
            });
            ui.end_row();
            ui.label(t("效果项"));
            ui.label(t_with(
                "{fx_count} 项",
                &[("fx_count", fx_count.to_string())],
            ));
            ui.end_row();
        });
    }

    /// 属性窗口(文件名 / 位置 / 大小+日期 /
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
        egui::Window::new(t("属性"))
            .open(&mut props_open)
            .default_width(460.0)
            .show(ctx, |ui| {
                self.props_detail_ui(ui, id);
                ui.separator();
                // 上一张 / 下一张(序号行)。
                ui.horizontal(|ui| {
                    if ui.button(t("◀ 上一张")).clicked() {
                        self.step_preview(-1);
                    }
                    ui.label(t_with(
                        "{pos} / {total} 张",
                        &[("pos", pos.to_string()), ("total", total.to_string())],
                    ));
                    if ui.button(t("下一张 ▶")).clicked() {
                        self.step_preview(1);
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button(t("关闭")).clicked() {
                        self.show_props = false;
                    }
                    if ui.button(t("在资源管理器中显示")).clicked() {
                        if let Some(path) = self.photo_path(id) {
                            show_in_explorer(&path);
                        }
                    }
                });
            });
        // 标题栏 X(props_open=false)或关闭按钮都会关掉窗口。
        self.show_props = props_open && self.show_props;
    }

    /// 批量加标签弹窗(右键菜单触发,预览视图与图库视图都要画)。
    fn tag_batch_window(&mut self, ctx: &egui::Context) {
        if !self.tag_batch_open {
            return;
        }
        let mut open = true;
        let mut apply = false;
        let mut cancel = false;
        let n = self.tag_batch_ids.len();
        egui::Window::new(t("批量加标签"))
            .open(&mut open)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.label(t_with(
                    "将给选中的 {p1} 张照片加标签",
                    &[("p1", n.to_string())],
                ));
                ui.add(
                    egui::TextEdit::singleline(&mut self.tag_batch_input).hint_text(t("标签名")),
                );
                let can = n > 0 && !self.tag_batch_input.trim().is_empty();
                ui.horizontal(|ui| {
                    apply = ui
                        .add_enabled(can, egui::Button::new(t("添加")))
                        .clicked();
                    cancel = ui.button(t("取消")).clicked();
                });
            });
        if cancel || !open {
            self.tag_batch_open = false;
            return;
        }
        if !apply {
            return;
        }
        let name = self.tag_batch_input.trim().to_string();
        if name.is_empty() {
            return;
        }
        let ids = self.tag_batch_ids.clone();
        let mut ok = 0usize;
        for id in ids {
            if timeline::add_tag(&self.store, id, &name).is_ok() {
                let star = self.star_of(id);
                self.sync_sidecars(id, star);
                ok += 1;
            }
        }
        self.tags = timeline::all_tags(&self.store).unwrap_or_default();
        self.status = t_with("已给 {p1} 张照片加标签", &[("p1", ok.to_string())]).into();
        self.tag_batch_open = false;
        self.tag_batch_input.clear();
        self.tag_batch_ids.clear();
    }

    /// 标题编辑(失焦或回车即保存)。
    /// 单图预览与右栏抽屉「属性」页共用同一份 caption_edit 草稿。
    fn caption_editor(&mut self, ui: &mut egui::Ui, id: i64) {
        if self.caption_edit.as_ref().map(|(k, _)| *k) != Some(id) {
            self.caption_edit = Some((id, self.store.get_caption(id)));
        }
        if let Some((_, cap)) = &mut self.caption_edit {
            let resp = ui
                .horizontal(|ui| {
                    ui.label(t("标题:"));
                    ui.add(
                        egui::TextEdit::singleline(cap).desired_width(ui.available_width() - 60.0),
                    )
                })
                .inner;
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter)) && resp.has_focus();
            if resp.lost_focus() || enter {
                if *cap != self.store.get_caption(id) {
                    let _ = self.store.set_caption(id, cap);
                    self.fx_msg = t("标题已保存").into();
                }
            }
        }
        #[cfg(feature = "ai")]
        self.caption_ai_button(ui, id);
    }

    /// 用已有识别结果重建标题(不覆盖手填标题的自动刷新)。
    #[cfg(feature = "ai")]
    fn caption_ai_button(&mut self, ui: &mut egui::Ui, id: i64) {
        if ui.button(t("用 AI 数据生成描述")).clicked() {
            let ver = crate::ai_queue::stage_version(
                &self.store,
                crate::ai_queue::Stage::Tags,
            );
            if crate::ai_caption::generate(&self.store, id, &ver, true) {
                self.caption_edit = Some((id, self.store.get_caption(id)));
                self.fx_msg = t("描述已生成").into();
            } else {
                self.fx_msg = t("暂无可生成描述的数据").into();
            }
        }
    }

    /// 标签编辑(输入框 + 色标 + 点 ✕ 移除)。
    fn tags_editor(&mut self, ui: &mut egui::Ui, id: i64) {
        ui.horizontal(|ui| {
            ui.label(t("标签:"));
            if ui.text_edit_singleline(&mut self.input_tag).lost_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter))
            {
                let t = std::mem::take(&mut self.input_tag);
                if !t.is_empty() {
                    let _ = timeline::add_tag(&self.store, id, &t);
                    self.sync_sidecars(id, self.star_of(id));
                }
            }
            if ui.button(t("添加")).clicked() {
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
        // 不在当前视图的行(快速查找页右键的命中)退回查库,拿真实星标值,
        // 否则第一次点击会按「未加星」算,把已加星的又设成加星。
        let cur = match self.id_row.get(&id) {
            Some(&idx) => self.rows[idx].star,
            None => self
                .store
                .photo_brief_by_id(id)
                .map(|(s, _)| s as i32)
                .unwrap_or(0),
        };
        let new = if cur != 0 { 0 } else { 1 };
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
        ui.small(t("先在图库点选一张照片(或双击打开单图)。"));
        ui.small(t("这里显示它的属性、标签、人物与拍摄地点。"));
    }

    /// 右栏抽屉(属性 / 标签 / 人物 / 地点 四页签,
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
                        // 抽屉里这几页的字段只可能来自 L3(派生层)—— EXIF、
                        // 缩略图、标签、人物、地理都是逐条打开文件抽出来的,L1/L2 里
                        // 没有。标注挂在页签的悬停上,不新增可见控件。
                        if ui
                            .selectable_label(cur == t, t.label())
                            .on_hover_text(crate::i18n::t("来源 L3:派生数据(EXIF / 缩略图 / 标签 / 人物 / 地理),由图库逐条抽取,后台按常用目录、最近变更、当前可见的顺序依次补齐。还没轮到的照片这里暂时没有内容,打开文件夹即开始生成。"))
                            .clicked()
                        {
                            self.drawer_tab = t;
                            let _ = self.store.set_setting("drawer_tab", t.key());
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("»").on_hover_text(t("收起属性面板")).clicked() {
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
                            ui.small(t("此构建未开启人脸功能(ai 特性)。"));
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
                    t("★ 取消星标")
                } else {
                    t("☆ 加星")
                })
                .clicked()
            {
                self.toggle_star(id);
            }
            if ui.small_button(t("在资源管理器中显示")).clicked() {
                if let Some(path) = self.photo_path(id) {
                    show_in_explorer(&path);
                }
            }
        });
    }

    /// 抽屉「标签」页:本张标签增删 + 库内已有标签一键套用。
    fn drawer_tags_ui(&mut self, ui: &mut egui::Ui) {
        let Some(id) = self.drawer_target() else {
            Self::drawer_need_photo(ui);
            return;
        };
        self.tags_editor(ui, id);
        ui.add_space(6.0);
        ui.separator();
        ui.small(t("库内标签(点一下即加到本张):"));
        let names: Vec<String> = self.tags.iter().map(|t| t.name.clone()).collect();
        if names.is_empty() {
            ui.small(t("(还没有标签)"));
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
        // 大脸在前(最大的人脸当主脸显示)。
        faces.sort_by(|a, b| {
            let area = |r: &(f32, f32, f32, f32)| (r.2 - r.0).max(0.0) * (r.3 - r.1).max(0.0);
            area(&b.rect)
                .partial_cmp(&area(&a.rect))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        if faces.is_empty() {
            ui.small(t("本张照片还没有人脸数据。"));
            ui.small(t("左栏「人物」里可以扫描图库并自动聚类。"));
            return;
        }
        for f in &faces {
            ui.separator();
            let named = f.person.clone().unwrap_or_default();
            ui.horizontal(|ui| {
                if f.person_id.is_some() {
                    if named.is_empty() {
                        match f.cluster {
                            Some(cluster) => ui.strong(t_with(
                                "簇 {cluster}",
                                &[("cluster", cluster.to_string())],
                            )),
                            None => ui.strong(t("未归属")),
                        };
                    } else {
                        ui.strong(named.as_str());
                    }
                } else {
                    ui.strong(t("未归入人物的脸"));
                }
                ui.small(t_with(
                    "质量 {p1}",
                    &[("p1", format!("{p1:.2}", p1 = (f.quality)))],
                ));
                if f.ignored {
                    ui.small(t("(已忽略)"));
                }
            });
            ui.horizontal(|ui| {
                if f.cluster.is_some() && ui.small_button(t("命名")).clicked() {
                    self.face_name_draft = Some((f.face_id, named.clone()));
                }
                if f.person_id.is_some() {
                    if ui.small_button(t("移出人物")).clicked() {
                        match crate::faces::remove_faces(&self.store, &[f.face_id]) {
                            Ok(_) => {
                                self.status = t("已移出,待下次聚类重新归入").into();
                                self.refresh_people_and_suggestions();
                            }
                            Err(e) => {
                                self.status =
                                    t_with("操作失败: {e}", &[("e", e.to_string())]);
                            }
                        }
                    }
                }
                if let Some(cluster) = f.cluster {
                    if ui.small_button(t("查看该人物")).clicked() {
                        self.set_view(ViewKind::Faces);
                        self.sel_cluster = Some(cluster);
                        self.sel_person = f.person_id;
                        self.refresh_people_and_suggestions();
                    }
                }
                if ui.small_button(t("找相似人脸")).clicked() {
                    self.start_face_similar(f.face_id);
                }
                if ui
                    .small_button(if f.ignored { t("恢复") } else { t("忽略") })
                    .clicked()
                {
                    let _ =
                        crate::faces::set_ignored(&self.store, &[f.face_id], !f.ignored);
                    self.face_show_ignored = true;
                    self.refresh_people_and_suggestions();
                }
                if let Some(cluster) = f.cluster {
                    if !f.ignored && ui.small_button(t("忽略此组")).clicked() {
                        let _ = crate::faces::ignore_cluster(&self.store, cluster, true);
                        self.face_show_ignored = true;
                        self.refresh_people_and_suggestions();
                    }
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
                        let res: anyhow::Result<()> = match (f.person_id, f.cluster) {
                            (Some(pid), _) => {
                                crate::faces::rename_person(&self.store, pid, &name)
                            }
                            (None, Some(c)) => {
                                crate::faces::rename_cluster(&self.store, c, &name)
                            }
                            _ => Ok(()),
                        };
                        match res {
                            Ok(()) => {
                                self.refresh_people_and_suggestions();
                                self.status =
                                    t_with("已命名:{name}", &[("name", name.to_string())]);
                            }
                            Err(e) => {
                                self.status = t_with("命名失败: {e}", &[("e", e.to_string())]);
                            }
                        }
                    }
                    self.face_name_draft = None;
                }
            }
        }
        self.drawer_similar_ui(ui, &faces);
    }

    /// 抽屉:按脸检索结果与归档、纠错操作。
    #[cfg(feature = "ai")]
    fn drawer_similar_ui(&mut self, ui: &mut egui::Ui, faces: &[crate::faces::PhotoFace]) {
        let Some(qid) = self.face_similar_for else {
            return;
        };
        let Some(qface) = faces.iter().find(|f| f.face_id == qid) else {
            self.face_similar.clear();
            self.face_similar_for = None;
            return;
        };
        ui.separator();
        ui.horizontal(|ui| {
            ui.strong(t_with(
                "相似人脸({n})",
                &[("n", self.face_similar.len().to_string())],
            ));
            if ui.small_button(t("清除")).clicked() {
                self.face_similar.clear();
                self.face_similar_for = None;
            }
        });
        let mut thr = self.face_similar_thr;
        if ui
            .add(egui::Slider::new(&mut thr, 0.2..=0.8).text(t("相似度下限")))
            .changed()
        {
            self.face_similar_thr = thr;
            self.start_face_similar(qid);
        }
        if qface.person_id.is_none() {
            ui.small(t("当前脸还未归入人物;先命名再点「加入此人物」。"));
        }
        if self.face_similar.is_empty() {
            ui.small(t("该阈值下没有相近人脸;可调低下限再试。"));
            return;
        }
        let target = qface.person_id;
        let mut act: Option<(usize, u8)> = None;
        for (i, hit) in self.face_similar.iter().enumerate() {
            ui.separator();
            ui.horizontal(|ui| {
                let file = std::path::Path::new(&hit.photo_path)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| hit.photo_path.clone());
                ui.strong(file);
                match &hit.person {
                    Some(name) => {
                        ui.small(name.as_str());
                    }
                    None if hit.person_id.is_none() => {
                        ui.small(t("未归属"));
                    }
                    None => match hit.cluster {
                        Some(c) => {
                            ui.small(t_with("簇 {cluster}", &[("cluster", c.to_string())]));
                        }
                        None => {
                            ui.small(t("未归属"));
                        }
                    },
                }
            });
            ui.small(t_with(
                "相似度 {s} · 质量 {q}",
                &[
                    ("s", format!("{:.3}", hit.score)),
                    ("q", format!("{:.2}", hit.quality)),
                ],
            ));
            ui.horizontal(|ui| {
                if target.is_some() && ui.small_button(t("加入此人物")).clicked() {
                    act = Some((i, 0));
                }
                if ui.small_button(t("单独建人物")).clicked() {
                    act = Some((i, 3));
                }
                if ui.small_button(t("不是同一人")).clicked() {
                    act = Some((i, 1));
                }
                if ui.small_button(t("忽略")).clicked() {
                    act = Some((i, 2));
                }
            });
        }
        if self.face_similar.len() >= 60 {
            ui.small(t("只显示最相近的 60 个结果。"));
        }
        let Some((i, code)) = act else {
            return;
        };
        let hit_id = self.face_similar[i].face_id;
        let res = match code {
            0 => target.map(|c| {
                crate::faces::assign_faces(&self.store, &[hit_id], c)
                    .map(|_| t("已加入此人物").to_string())
            }),
            1 => Some(
                crate::ai_review::add(&self.store, qid, hit_id, crate::ai_review::Kind::Never)
                    .map(|_| t("已标记为不同人").to_string()),
            ),
            3 => Some(
                crate::faces::create_person(&self.store, &[hit_id], None)
                    .map(|_| t("已新建人物(未命名)").to_string()),
            ),
            _ => Some(
                crate::faces::set_ignored(&self.store, &[hit_id], true)
                    .map(|_| t("已忽略该脸").to_string()),
            ),
        };
        match res {
            Some(Ok(msg)) => {
                self.status = msg;
                self.refresh_people_and_suggestions();
                self.start_face_similar(qid);
            }
            Some(Err(e)) => {
                self.status = t_with("操作失败: {e}", &[("e", e.to_string())]);
            }
            None => {}
        }
    }

    /// 抽屉「地点」页:GPS 坐标、复制、地图入口与全库 KML 导出。
    fn drawer_places_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(id) = self.drawer_target() else {
            Self::drawer_need_photo(ui);
            return;
        };
        let Some(&idx) = self.id_row.get(&id) else {
            ui.small(t("该照片不在当前视图。"));
            return;
        };
        let (lat, lon) = (self.rows[idx].lat, self.rows[idx].lon);
        let taken = self.rows[idx].taken.clone();
        match (lat, lon) {
            (Some(a), Some(b)) => {
                ui.label(format!("GPS:{a:.6}, {b:.6}"));
                if let Some(t) = taken {
                    ui.small(t_with(
                        "拍摄时间:{p1}",
                        &[("p1", (t.replace('T', " ")).to_string())],
                    ));
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.small_button(t("复制坐标")).clicked() {
                        ctx.copy_text(format!("{a:.6}, {b:.6}"));
                        self.status = t("已复制 GPS 坐标").into();
                    }
                    if ui.small_button(t("在地图中打开")).clicked() {
                        // 交给系统默认浏览器(OpenStreetMap,无需第三方密钥)。
                        let url = format!(
                            "https://www.openstreetmap.org/?mlat={a}&mlon={b}#map=17/{a}/{b}"
                        );
                        if !crate::win::shell_open(Path::new(&url)) {
                            self.status = t("无法打开地图(没有默认浏览器?)").into();
                        }
                    }
                });
            }
            (Some(a), None) => {
                ui.small(t("只有纬度,坐标不完整(原图 EXIF 可能损坏)。"));
                ui.small(t_with("纬度:{a:.6}", &[("a", format!("{a:.6}"))]));
            }
            (None, Some(b)) => {
                ui.small(t("只有经度,坐标不完整(原图 EXIF 可能损坏)。"));
                ui.small(t_with("经度:{b:.6}", &[("b", format!("{b:.6}"))]));
            }
            (None, None) => {
                ui.small(t("本张照片的 EXIF 里没有 GPS 坐标。"));
                ui.small(t("相机未定位或导出时丢掉了定位信息;可在原图属性里补写。"));
            }
        }
        ui.add_space(6.0);
        ui.separator();
        ui.small(t("整个图库的定位数据:"));
        if ui.small_button(t("导出地理标记(KML)…")).clicked() {
            self.export_kml();
        }
    }

    /// 选项 > 全盘快查:索引服务状态一览 + 启用意向 + 手动重建/卸载入口。
    fn options_svc_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading(egui::RichText::new(t("全盘快查(索引服务)")).size(14.0));
        let link = self.link.clone();
        let svc_text = match link.svc {
            crate::win::ServiceState::NotInstalled => t("未安装").to_string(),
            crate::win::ServiceState::Stopped => t("已停止").to_string(),
            // 过渡态的原始码只给排障看,别当成确定结论。
            crate::win::ServiceState::Transition(c) => {
                t_with("过渡态(码 {c})", &[("c", c.to_string())])
            }
            crate::win::ServiceState::Running => t("运行中").to_string(),
        };
        let status = if link.online {
            let mut s = t_with(
                "服务{svc_text} · {p1} 卷 {p2} 条",
                &[
                    ("svc_text", svc_text.to_string()),
                    ("p1", (link.vols).to_string()),
                    ("p2", (link.entries).to_string()),
                ],
            );
            if link.building > 0 {
                s.push_str(&t_with(
                    " · {p1} 卷正在建库",
                    &[("p1", (link.building).to_string())],
                ));
            }
            if link.dirty {
                s.push_str(t(" · 有未落盘的变更"));
            }
            s.push_str(&t_with(
                " · 已运行 {p1}",
                &[("p1", (fmt_uptime(link.uptime)).to_string())],
            ));
            if let Some(t) = link.seen {
                s.push_str(&t_with(
                    " · {p1} 秒前刷新",
                    &[("p1", (t.elapsed().as_secs()).to_string())],
                ));
            }
            s
        } else if link.svc == crate::win::ServiceState::Running {
            t_with(
                "服务{svc_text},但连不上它的管道(全盘快查暂不可用)",
                &[("svc_text", svc_text.to_string())],
            )
        } else {
            t_with(
                "服务{svc_text}:全盘快查不可用,发现与保鲜走逐目录扫描兜底",
                &[("svc_text", svc_text.to_string())],
            )
        };
        ui.horizontal(|ui| {
            ui.label(t("状态:"));
            ui.label(&status);
        });
        if link.building > 0 {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.small(t_with(
                    "{p1} 卷正在全量建库,期间快查结果可能不全",
                    &[("p1", (link.building).to_string())],
                ));
            });
        }
        for (letter, why) in &link.compat {
            ui.small(t_with(
                "{letter}: 盘走兼容扫描(walkdir + 目录监听),无实时快查 —— {why}",
                &[("letter", letter.to_string()), ("why", why.to_string())],
            ));
        }
        #[cfg(windows)]
        {
            let cur = self.svc_want();
            ui.horizontal(|ui| {
                ui.label(t("全盘快查:"));
                if ui.radio(cur != SvcWant::No, t("启用(推荐)")).clicked() {
                    self.svc_apply_want(SvcWant::Yes);
                }
                if ui.radio(cur == SvcWant::No, t("不用")).clicked() {
                    self.svc_apply_want(SvcWant::No);
                }
            });
            ui.small(t("启用 = 装一个自动启动的索引服务:安装与卸载各过一次 UAC,之后开机自启、全程静默;选「不用」会断开连接并移除已装的服务。"));
            if self.store.get_setting("svc_never_ask").as_deref() == Some("1") {
                ui.small(t(
                    "已勾「不再提示」:启动不再自动询问,点上面任一档即可恢复。",
                ));
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(link.online, egui::Button::new(t("立即重建全盘索引")))
                    .clicked()
                {
                    self.svc_rebuild();
                }
                if ui
                    .add_enabled(link.online, egui::Button::new(t("立刻保存索引")))
                    .clicked()
                {
                    if let Some(tx) = &self.link_tx {
                        let _ = tx.send(LinkCmd::Flush);
                    }
                    self.status = t("已请求索引服务把内存索引落盘").into();
                }
                if ui
                    .add_enabled(!self.svc_busy, egui::Button::new(t("重试连接")))
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
                            .add_enabled(!self.svc_busy, egui::Button::new(t("停止索引服务…")))
                            .clicked()
                    {
                        self.svc_stop();
                    }
                    if startable
                        && ui
                            .add_enabled(!self.svc_busy, egui::Button::new(t("启动索引服务…")))
                            .clicked()
                    {
                        self.svc_start();
                    }
                });
                ui.small(if stoppable {
                    t("停一下服务能让程序自己腾出手来:升级、搬家或想把硬盘歇口气的时候用,索引记录都保留,在同一页点「启动」就接着跑。")
                } else {
                    t("服务停着时全盘快查不可用,发现与保鲜退回逐目录扫描;索引文件都留在数据目录,启动后接着用。")
                });
            }
            if link.svc != crate::win::ServiceState::NotInstalled
                && ui
                    .add_enabled(!self.svc_busy, egui::Button::new(t("卸载索引服务…")))
                    .clicked()
            {
                self.svc_uninstall();
            }
            if link.svc != crate::win::ServiceState::NotInstalled {
                ui.small(t(
                    "卸载会停掉服务并删除注册项(需要一次 UAC);索引文件本身留在数据目录。",
                ));
            }
            if self.svc_busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.small(t("等待你在 UAC 授权窗口点「是」…"));
                });
            }
        }
        #[cfg(not(windows))]
        ui.small(t("全盘快查仅支持 Windows(NTFS USN journal)。"));
        ui.small(t("服务装卸失败后不再自动打扰;可在此或快速查找页重新发起。"));
    }

    /// 后台全库入队,不阻塞界面。
    #[cfg(feature = "ai")]
    fn enqueue_all_ai(&mut self) {
        let store = self.store.clone();
        self.status = t("已在后台把全库排入 AI 队列").into();
        std::thread::spawn(move || {
            crate::win::set_current_thread_priority(crate::win::Priority::BelowNormal);
            let _ = crate::ai_queue::enqueue_all(&store);
        });
    }

    /// 标签视图当前筛选出的照片(展示与选择/导出共用同一范围)。
    fn tags_scope(&self) -> Vec<i64> {
        #[cfg(feature = "ai")]
        return tags_scope_ids(
            &self.store,
            &self.ai_query,
            &self.ai_query_hits,
            self.sel_category.as_deref(),
            self.sel_tag.as_deref(),
        );
        #[cfg(not(feature = "ai"))]
        return tags_scope_ids(&self.store, "", &[], None, self.sel_tag.as_deref());
    }

    /// AI 分类与计数(5 秒缓存):侧栏与设置页共用,不每帧查库。
    #[cfg(feature = "ai")]
    fn ai_categories(&mut self) -> Vec<(&'static str, &'static str, usize)> {
        let fresh = self
            .ai_category_cache
            .as_ref()
            .map(|(t, _)| t.elapsed() < std::time::Duration::from_secs(5))
            .unwrap_or(false);
        if !fresh {
            let ver =
                crate::ai_queue::stage_version(&self.store, crate::ai_queue::Stage::Tags);
            let counts = crate::ai_tags::category_counts(&self.store, &ver);
            let list = crate::ai_tags::CATEGORIES
                .iter()
                .zip(counts)
                .map(|((node, label), (_, n))| (*node, *label, n))
                .collect();
            self.ai_category_cache = Some((std::time::Instant::now(), list));
        }
        self.ai_category_cache
            .as_ref()
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    /// 标签视图:库内检索 / AI 分类 / 手动标签三种筛选。
    fn tags_view(&mut self, ui: &mut egui::Ui) {
        let ids = self.tags_scope();
        let cells: Vec<usize> = ids
            .iter()
            .filter_map(|id| self.id_row.get(id).copied())
            .collect();
        #[cfg(feature = "ai")]
        {
            let q = self.ai_query.trim().to_string();
            if !q.is_empty() {
                ui.heading(t_with(
                    "库内检索 «{p1}» · {p2} 张",
                    &[("p1", q), ("p2", (cells.len()).to_string())],
                ));
                if cells.is_empty() {
                    ui.label(t("没有命中的照片;先启用 AI 索引可生成标签、文字与描述索引。"));
                }
                self.photo_grid(ui, "tag-search-grid", &cells);
                return;
            }
            if let Some(node) = self.sel_category.clone() {
                let name = crate::ai_tags::CATEGORIES
                    .iter()
                    .find(|(n, _)| *n == node.as_str())
                    .map(|(_, l)| *l)
                    .unwrap_or("");
                let ver =
                    crate::ai_queue::stage_version(&self.store, crate::ai_queue::Stage::Tags);
                let total = crate::ai_tags::category_count(&self.store, &node, &ver);
                ui.heading(t_with(
                    "分类 «{p1}» · {p2} 张",
                    &[("p1", t(name).to_string()), ("p2", total.to_string())],
                ));
                if total > ids.len() {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        t_with(
                            "仅显示前 {p1} 张,用库内检索可缩小范围。",
                            &[("p1", ids.len().to_string())],
                        ),
                    );
                }
                if cells.is_empty() {
                    ui.label(t("该分类下还没有照片。"));
                }
                self.photo_grid(ui, "category-grid", &cells);
                return;
            }
        }
        match self.sel_tag.clone() {
            None => {
                ui.heading(t("标签"));
                ui.label(t("左侧选择一个标签,查看标有该标签的照片。"));
            }
            Some(tag) => {
                ui.heading(t_with("标签 «{tag}» · {p1} 张", &[("tag", tag.to_string()), ("p1", (cells.len()).to_string())]));
                self.photo_grid(ui, "tags-grid", &cells);
            }
        }
    }

    /// 只把某一阶段全库入队(重建).
    #[cfg(feature = "ai")]
    fn enqueue_stage_ai(&mut self, stage: crate::ai_queue::Stage) {
        let store = self.store.clone();
        self.status = t_with(
            "已在后台把全库排入{p1}队列",
            &[("p1", (stage.label()).to_string())],
        );
        std::thread::spawn(move || {
            crate::win::set_current_thread_priority(crate::win::Priority::BelowNormal);
            let _ = crate::ai_queue::enqueue_all_for(&store, stage);
        });
    }

    /// 队列统计行的三个动作:继续处理 / 重试失败 / 强制重建。
    #[cfg(feature = "ai")]
    fn stage_action_ai(&mut self, stage: crate::ai_queue::Stage, kind: u8) {
        if kind == 0 {
            self.enqueue_stage_ai(stage);
            return;
        }
        let store = self.store.clone();
        self.status = if kind == 1 {
            t_with(
                "已重新安排 {p1} 的失败项",
                &[("p1", (stage.label()).to_string())],
            )
        } else {
            t_with(
                "正在重建 {p1}:清除本阶段完成状态与派生产物后重新入队",
                &[("p1", (stage.label()).to_string())],
            )
        };
        std::thread::spawn(move || {
            crate::win::set_current_thread_priority(crate::win::Priority::BelowNormal);
            let _ = if kind == 1 {
                crate::ai_queue::retry_failed(&store, stage)
            } else {
                crate::ai_queue::rebuild_stage(&store, stage)
            };
        });
    }

    /// 选项页「AI 行为」:基础开关、暂停恢复与队列统计。
    #[cfg(feature = "ai")]
    fn ai_options_ui(&mut self, ui: &mut egui::Ui) {
        let mut enabled = self.ai_enabled;
        if ui
            .checkbox(&mut enabled, t("启用后台 AI 索引"))
            .changed()
        {
            let _ = self
                .store
                .set_setting("ai_enabled", if enabled { "1" } else { "0" });
            self.ai_enabled = enabled;
            if let Some(s) = &self.ai_sched {
                if enabled {
                    s.resume();
                } else {
                    s.pause();
                }
            }
            if enabled {
                self.enqueue_all_ai();
            }
        }
        let paused = self
            .ai_sched
            .as_ref()
            .map(|s| s.is_paused())
            .unwrap_or(true);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!paused, egui::Button::new(t("暂停")))
                .clicked()
            {
                let _ = self.store.set_setting("ai_enabled", "0");
                self.ai_enabled = false;
                if let Some(s) = &self.ai_sched {
                    s.pause();
                }
            }
            if ui
                .add_enabled(paused, egui::Button::new(t("继续")))
                .clicked()
            {
                let _ = self.store.set_setting("ai_enabled", "1");
                self.ai_enabled = true;
                if let Some(s) = &self.ai_sched {
                    s.resume();
                }
                self.enqueue_all_ai();
            }
        });
        if ui.button(t("立即入队全库")).clicked() {
            self.enqueue_all_ai();
        }
        ui.separator();
        let mut sens = crate::ai_tags::sensitivity(&self.store);
        egui::ComboBox::from_id_salt("ai_tag_sens")
            .selected_text([t("宽松"), t("适中"), t("严格")][sens as usize])
            .show_ui(ui, |ui| {
                for (i, name) in [t("宽松"), t("适中"), t("严格")].iter().enumerate() {
                    if ui.selectable_value(&mut sens, i as i32, *name).clicked() {
                        let _ = self.store.set_setting("ai_tag_sensitivity", &i.to_string());
                        self.enqueue_stage_ai(crate::ai_queue::Stage::Tags);
                    }
                }
            });
        ui.horizontal(|ui| {
            ui.label(t("OCR 触发"));
            let mut mode = crate::ai_ocr::mode(&self.store);
            egui::ComboBox::from_id_salt("ai_ocr_mode")
                .selected_text(mode.label())
                .show_ui(ui, |ui| {
                    for m in [
                        crate::ai_ocr::Mode::Off,
                        crate::ai_ocr::Mode::Auto,
                        crate::ai_ocr::Mode::Selected,
                    ] {
                        if ui.selectable_value(&mut mode, m, m.label()).clicked() {
                            let _ = self.store.set_setting(crate::ai_ocr::Mode::KEY, m.key());
                        }
                    }
                });
        });
        ui.small(t_with(
            "词表 {p1} 条 · 语义向量 {p2} 条 · OCR 文档 {p3} 篇 · 人脸约束 {p4} 条",
            &[
                ("p1", (crate::ai_tags::label_count(&self.store)).to_string()),
                (
                    "p2",
                    (crate::ai_search::count(
                        &self.store,
                        &crate::ai_queue::stage_version(&self.store, crate::ai_queue::Stage::Clip),
                    ))
                    .to_string(),
                ),
                ("p3", (crate::ai_ocr::count(&self.store)).to_string()),
                ("p4", (crate::ai_review::all(&self.store).len()).to_string()),
            ],
        ));
        let counts = self.ai_categories();
        ui.horizontal_wrapped(|ui| {
            for (_, name, n) in counts {
                if n > 0 {
                    ui.small(format!("{}:{n}", t(name)));
                }
            }
        });
        ui.separator();
        let fresh = self
            .ai_stats_cache
            .as_ref()
            .map(|(t, _)| t.elapsed() < std::time::Duration::from_secs(2))
            .unwrap_or(false);
        if !fresh {
            if let Some(s) = &self.ai_sched {
                let v = s.stats(&self.store);
                self.ai_stats_cache = Some((std::time::Instant::now(), v));
            }
        }
        if let Some((_, rows)) = &self.ai_stats_cache {
            let rows = rows.clone();
            let mut action: Option<(crate::ai_queue::Stage, u8)> = None;
            for row in &rows {
                ui.horizontal_wrapped(|ui| {
                    ui.small(t_with(
                        "{p1}:待办 {p2} · 完成 {p3} · 失败 {p4}",
                        &[
                            ("p1", (row.stage.label()).to_string()),
                            ("p2", (row.stats.pending).to_string()),
                            ("p3", (row.stats.done).to_string()),
                            ("p4", (row.stats.failed).to_string()),
                        ],
                    ));
                    if ui.small_button(t("继续处理")).clicked() {
                        action = Some((row.stage, 0));
                    }
                    if ui.small_button(t("重试失败")).clicked() {
                        action = Some((row.stage, 1));
                    }
                    if ui.small_button(t("强制重建")).clicked() {
                        action = Some((row.stage, 2));
                    }
                });
            }
            if let Some((stage, kind)) = action {
                self.stage_action_ai(stage, kind);
            }
        }
        ui.separator();
        ui.small(t("暂停/恢复在批边界生效;任务持久化在索引库,重启后继续。"));
        ui.small(t("继续处理只补缺失/过期项;重试失败只重排隔离项;强制重建会清除本阶段完成状态与派生产物,人工标注保留。"));
    }

    /// 启用某槽位版本(带调度器的顺序化替换)。
    #[cfg(feature = "ai")]
    fn ai_activate_slot(&mut self, slot: &str, ver: &str) {
        let out = match &self.ai_sched {
            Some(s) => crate::ai_models::replace_model(&self.store, s, slot, ver),
            None => crate::ai_models::activate(&self.store, slot, ver),
        };
        self.ai_models_msg = match out {
            Ok(()) => t_with(
                "已启用 {slot} · {ver}",
                &[("slot", slot.to_string()), ("ver", ver.to_string())],
            ),
            Err(e) => t_with("启用失败:{e}", &[("e", e.to_string())]),
        };
        self.ai_models_cache = None;
    }

    /// 选项页「AI 模型」:槽位状态、导入、下载与版本切换。
    #[cfg(feature = "ai")]
    fn ai_models_ui(&mut self, ui: &mut egui::Ui) {
        use crate::ai_models::DlPhase;
        use crate::models::SourcePref;
        if self.ai_dl.is_none() {
            self.ai_dl = Some(crate::ai_models::SlotDownloader::new(self.store.clone()));
        }
        let phase = if let Some(d) = self.ai_dl.as_mut() {
            d.tick();
            d.phase.clone()
        } else {
            DlPhase::Idle
        };
        // 多文件槽位:前一个文件完成后串行启动下一个。
        if matches!(phase, DlPhase::Done) && !self.ai_dl_queue.is_empty() {
            let next = self.ai_dl_queue.remove(0);
            if let Some(d) = self.ai_dl.as_mut() {
                if let Err(e) = d.start(next) {
                    self.ai_dl_queue.clear();
                    self.ai_models_msg = t_with("下载未能开始:{e}", &[("e", e.to_string())]);
                }
            }
        }
        if matches!(&phase, DlPhase::Failed(_)) {
            self.ai_dl_queue.clear();
        }

        // —— 进度:人脸与 AI 槽位都画在顶部,不散落在各自分区 ——
        let face = self.models.phase.clone();
        if face.working() || matches!(face, crate::models::Phase::Failed(_)) {
            self.face_download_progress_ui(ui);
        }
        if phase.working() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(phase.label());
                if ui.button(t("停止下载")).clicked() {
                    if let Some(d) = self.ai_dl.as_mut() {
                        d.cancel();
                    }
                    self.ai_dl_queue.clear();
                    self.ai_models_msg = t("已停止下载:断点保留,点「下载」即接着下").into();
                }
            });
            if let Some(root) = crate::ai_models::root() {
                ui.small(t_with(
                    "断点留在 {p1},重开程序后再点「下载」可接着下",
                    &[(
                        "p1",
                        (crate::ai_models::staging(&root).display()).to_string(),
                    )],
                ));
            }
        } else if let DlPhase::Failed(e) = &phase {
            ui.colored_label(egui::Color32::LIGHT_RED, e);
        }
        if !self.ai_models_msg.is_empty() {
            ui.small(self.ai_models_msg.clone());
        }
        ui.separator();

        // 识别方案、下载源、人脸模型文件(原「人脸识别」页内容)。
        self.face_options_ui(ui);

        // —— AI 槽位(逐行折叠,默认收起,避免整页过高) ——
        ui.separator();
        let fresh = self
            .ai_models_cache
            .as_ref()
            .map(|(t, _, _)| t.elapsed() < std::time::Duration::from_secs(2))
            .unwrap_or(false);
        if !fresh {
            let rows = crate::ai_models::status(&self.store);
            let disk = crate::ai_models::disk_usage(&self.store);
            self.ai_models_cache = Some((std::time::Instant::now(), rows, disk));
        }
        let (rows, disk) = match &self.ai_models_cache {
            Some((_, rows, disk)) => (rows.clone(), *disk),
            None => (Vec::new(), 0),
        };
        ui.horizontal(|ui| {
            ui.heading(egui::RichText::new(t("AI 模型槽位")).size(14.0));
            ui.small(t_with(
                "占用 {p1} MB(含旧版本与暂存)",
                &[("p1", format!("{:.1}", disk as f64 / 1_048_576.0))],
            ));
        });
        let now = crate::ai_queue::now_ms();
        let pref = SourcePref::from_setting(
            &self
                .store
                .get_setting(SourcePref::SETTING)
                .unwrap_or_default(),
        );
        for row in rows {
            let Some(spec) = crate::ai_models::spec(&row.key) else {
                continue;
            };
            let head = if row.installed {
                format!(
                    "{} · {} · {:.1} MB{}",
                    row.label,
                    row.version,
                    row.bytes as f64 / 1_048_576.0,
                    if row.legacy {
                        format!(" · {}", t("旧平铺路径"))
                    } else {
                        String::new()
                    }
                )
            } else {
                format!("{} · {}", row.label, t("未安装"))
            };
            egui::CollapsingHeader::new(head)
                .id_salt(format!("ai_slot_{}", row.key))
                .default_open(false)
                .show(ui, |ui| {
                    ui.small(t_with("许可:{p1}", &[("p1", (row.license).to_string())]));
                    let links = crate::ai_models::official_weight_links(&row.key);
                    if !links.is_empty() {
                        ui.horizontal_wrapped(|ui| {
                            ui.small(t("官方权重(仅 .pt):"));
                            for (name, url) in links {
                                ui.hyperlink_to(*name, *url);
                            }
                        });
                        ui.small(t("官方未发布 ONNX;这里的 ONNX 由本项目转换后经镜像分发。"));
                    }
                    if row.manual {
                        ui.weak(t("需自行获取"));
                        ui.small(t("本槽位没有可验证的官方直链,请用「导入文件」或「导入目录」放置模型。"));
                        if row.key == "llm_parse" {
                            ui.small(t("当前使用规则解析查询,该槽位模型暂不参与推理。"));
                        }
                        if let Some(spec) = crate::ai_models::spec(&row.key) {
                            ui.small(t_with(
                                "需要文件: {p1}",
                                &[("p1", spec.files.join(", "))],
                            ));
                            for f in spec.files {
                                if let Some(a) = crate::ai_models::asset(&row.key, f) {
                                    if !a.sha256.is_empty() {
                                        ui.small(format!("{} · {}", f, &a.sha256[..16]));
                                    }
                                }
                            }
                        }
                    }
                    // 自定义主文件直链;留空则按上面的全局下载源自动选择。
                    let url_key = format!("ai_url:{}", row.key);
                    let mut url = self.store.get_setting(&url_key).unwrap_or_default();
                    ui.horizontal(|ui| {
                        ui.label(t("自定义直链"));
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut url)
                                    .desired_width(220.0)
                                    .hint_text(t("留空按下载源自动选择")),
                            )
                            .changed()
                        {
                            let _ = self.store.set_setting(&url_key, &url);
                        }
                        if ui
                            .add_enabled(
                                !phase.working() && (!row.manual || !url.trim().is_empty()),
                                egui::Button::new(t("下载")),
                            )
                            .clicked()
                        {
                            self.start_ai_slot_download(&row.key, &row.label, &url, pref);
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        let ver = format!("import-{now}");
                        if ui.button(t("导入文件")).clicked() {
                            if let Some(p) = rfd::FileDialog::new()
                                .add_filter("ONNX", &["onnx"])
                                .pick_file()
                            {
                                let out = crate::ai_models::import_file(
                                    &self.store,
                                    &row.key,
                                    &p,
                                    &ver,
                                    None,
                                    None,
                                );
                                match out {
                                    Ok(_) => self.ai_activate_slot(&row.key, &ver),
                                    Err(e) => {
                                        self.ai_models_msg =
                                            t_with("导入失败:{e}", &[("e", e.to_string())])
                                    }
                                }
                            }
                        }
                        if spec.files.len() > 1 && ui.button(t("导入目录")).clicked() {
                            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                                let out =
                                    crate::ai_models::import_dir(&self.store, &row.key, &p, &ver);
                                match out {
                                    Ok(_) => self.ai_activate_slot(&row.key, &ver),
                                    Err(e) => {
                                        self.ai_models_msg =
                                            t_with("导入失败:{e}", &[("e", e.to_string())])
                                    }
                                }
                            }
                        }
                        if !row.versions.is_empty() {
                            let mut pick = row.active.clone();
                            egui::ComboBox::from_id_salt(format!("ai_ver_{}", row.key))
                                .selected_text(if pick.is_empty() {
                                    t("选择版本").to_string()
                                } else {
                                    pick.clone()
                                })
                                .show_ui(ui, |ui| {
                                    for v in &row.versions {
                                        ui.selectable_value(&mut pick, v.clone(), v);
                                    }
                                });
                            if ui.button(t("启用")).clicked() && !pick.is_empty() {
                                self.ai_activate_slot(&row.key, &pick);
                            }
                        }
                    });
                });
        }
        ui.separator();
        ui.small(t("更换模型严格按 暂停 → 等当前批结束 → 释放会话 → 切版本 → 重建 → 继续 的顺序执行;旧版本目录保留可滚回。"));
    }

    /// 按全局下载源偏好启动某个槽位的下载(多文件自动串行,断点续传由下载器负责)。
    #[cfg(feature = "ai")]
    fn start_ai_slot_download(
        &mut self,
        key: &str,
        label: &str,
        custom_url: &str,
        pref: crate::models::SourcePref,
    ) {
        let now = crate::ai_queue::now_ms();
        let mut plans =
            crate::ai_models::plans_for_pref(key, &format!("dl-{now}"), custom_url, pref);
        if plans.is_empty() {
            self.ai_models_msg =
                t("该槽位在此下载源下没有可用地址,请换「自动」或「仅镜像」后再试").to_string();
            return;
        }
        let count = plans.len();
        let first = plans.remove(0);
        self.ai_dl_queue = plans;
        if let Some(d) = self.ai_dl.as_mut() {
            if let Err(e) = d.start(first) {
                self.ai_dl_queue.clear();
                self.ai_models_msg = t_with("下载未能开始:{e}", &[("e", e.to_string())]);
            } else if count > 1 {
                self.ai_models_msg = t_with(
                    "开始下载 {p1}(共 {p2} 个文件)",
                    &[("p1", label.to_string()), ("p2", count.to_string())],
                );
            } else {
                self.ai_models_msg = t_with("开始下载 {p1}", &[("p1", label.to_string())]);
            }
        }
    }

    /// 选项窗口(常规 / 全盘快查 / 电子邮件 / 幻灯片 / AI 行为 / AI 模型 六页签,
    /// 人脸方案与各 AI 槽位的下载合并在「AI 模型」页)。
    fn options_ui(&mut self, ctx: &egui::Context) {
        let mut opts_open = self.show_options;
        egui::Window::new(t("选项"))
            .open(&mut opts_open)
            .default_width(640.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (i, name) in [t("常规"), t("全盘快查"), t("电子邮件"), t("幻灯片"), t("AI 行为"), t("AI 模型")]
                        .iter()
                        .enumerate()
                    {
                        if ui.selectable_label(self.options_tab == i, *name).clicked() {
                            self.options_tab = i;
                        }
                    }
                });
                ui.separator();
                match self.options_tab {
                    0 => {
                        // 常规页(本地适用项)。
                        if ui
                            .checkbox(&mut self.export_with_effects, t("导出时套用已保存的效果"))
                            .changed()
                        {
                            self.fx_msg = if self.export_with_effects {
                                t("导出将带上已保存的效果").into()
                            } else {
                                t("导出 = 原图副本").into()
                            };
                        }
                        if ui
                            .checkbox(&mut self.thumb_captions, t("缩略图下方显示标题"))
                            .changed()
                        {
                            let _ = self.store.set_setting(
                                "thumb_captions",
                                if self.thumb_captions { "1" } else { "0" },
                            );
                        }
                        if ui
                            .checkbox(&mut self.dup_near, t("重复照片检测包含近似重复"))
                            .changed()
                        {
                            self.rebuild();
                        }
                        ui.separator();
                        // 配色:默认灰色;暗色方案为浅蓝点缀。切换即存。
                        ui.heading(egui::RichText::new(t("配色")).size(14.0));
                        ui.horizontal(|ui| {
                            for k in [crate::theme::Kind::Gray, crate::theme::Kind::Blue] {
                                if ui
                                    .selectable_label(crate::theme::kind() == k, t(k.label()))
                                    .clicked()
                                {
                                    crate::theme::switch(ctx, k);
                                    let _ = self.store.set_setting("ui_theme", k.key());
                                }
                            }
                        });
                        ui.separator();
                        // 界面语言:默认中文,可切英文。切换即存、立即生效。
                        ui.heading(egui::RichText::new(t("界面语言")).size(14.0));
                        ui.horizontal(|ui| {
                            for l in crate::i18n::Lang::ALL {
                                if ui
                                    .selectable_label(crate::i18n::lang() == l, l.label())
                                    .clicked()
                                {
                                    crate::i18n::set_lang(l);
                                    let _ = self.store.set_setting("ui_lang", l.key());
                                }
                            }
                        });
                        ui.separator();
                        // 数据目录:防 C 盘写满 —— 库/索引/模型整体迁走。
                        ui.heading(egui::RichText::new(t("数据目录")).size(14.0));
                        ui.small(crate::db::data_dir().to_string_lossy().to_string());
                        ui.horizontal(|ui| {
                            if ui.button(t("更改数据目录…")).clicked() {
                                if let Some(d) = rfd::FileDialog::new().pick_folder() {
                                    if d != crate::db::data_dir() {
                                        match crate::db::set_data_dir(Some(&d)) {
                                            Ok(()) => {
                                                self.status = t_with("数据目录已指向 {p1},下次启动时自动迁移文件", &[("p1", (d.display()).to_string())]);
                                            }
                                            Err(e) => {
                                                self.status =
                                                    t_with("写入数据目录配置失败: {e}", &[("e", e.to_string())]);
                                            }
                                        }
                                    }
                                }
                            }
                            if ui.button(t("恢复默认")).clicked() {
                                match crate::db::set_data_dir(None) {
                                    Ok(()) => {
                                        self.status =
                                            t("数据目录已恢复默认(旧目录文件保留,不自动删)")
                                                .into();
                                    }
                                    Err(e) => {
                                        self.status = t_with("清除数据目录配置失败: {e}", &[("e", e.to_string())]);
                                    }
                                }
                            }
                        });
                        ui.small(
                            t("更改后,数据库/全盘索引/模型在下次启动时复制到新目录(原目录文件保留);本会话继续用原目录。"),
                        );
                        ui.small(
                            t("索引服务会跟随新数据目录:迁移完成后自动更新机器级标记;服务正在运行时点「重启索引服务」立刻生效。"),
                        );
                        #[cfg(windows)]
                        if crate::db::data_migrated_this_run()
                            && matches!(
                                crate::win::service_state(),
                                crate::win::ServiceState::Running
                            )
                        {
                            if ui.button(t("重启索引服务(应用新数据目录)")).clicked() {
                                self.svc_restart();
                            }
                        }
                        ui.separator();
                        // 缩略图缓存:库搬到大盘时缓存跟着走,不再默默写系统盘。
                        ui.heading(egui::RichText::new(t("缩略图缓存")).size(14.0));
                        let cdir = crate::db::cache_dir();
                        let sdir = crate::db::system_cache_dir();
                        ui.small(cdir.to_string_lossy().to_string());
                        ui.small(if cdir == sdir {
                            t("在系统缓存目录;把数据目录迁走后会跟随到新数据目录的 cache\\ 子目录(重启后生效)。")
                        } else {
                            t("跟随数据目录(可用 MALIANG_CACHE_DIR 单独指定);旧缓存会在启动后自动搬到此处。")
                        });
                        let (mig_state, mig_moved) = crate::db::cache_migration_status();
                        match mig_state {
                            1 => {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.small(t("正在把旧缓存搬到新目录…"));
                                });
                            }
                            2 if mig_moved > 0 => {
                                ui.small(t_with("已从系统缓存目录搬来 {mig_moved} 个文件。", &[("mig_moved", mig_moved.to_string())]));
                            }
                            3 => {
                                ui.small(t("上次自动搬运有文件被占用,旧位置留有残留,可用下方按钮重试。"));
                            }
                            _ => {}
                        }
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
                            ui.small(t_with("{files} 个文件 · {p1}", &[("files", files.to_string()), ("p1", (fmt_size(bytes)).to_string())]));
                        }
                        // 自动搬迁只处理四档缩略图;旧位置可能还剩历史备份
                        // (*.reset-*),留一个手动入口由用户决定是否一起搬。
                        if cdir != sdir && crate::db::system_cache_has_content() {
                            let busy = mig_state == 1;
                            if ui
                                .add_enabled(
                                    !busy,
                                    egui::Button::new(t("把旧位置剩余内容搬过来")),
                                )
                                .clicked()
                            {
                                match crate::db::migrate_cache_from(&sdir) {
                                    Ok((0, _)) => {
                                        self.status =
                                            t("旧位置无新内容可搬(目标目录已是最新)").into();
                                    }
                                    Ok((n, cleared)) => {
                                        self.status = t_with("已搬动 {n} 个缓存文件{p1}", &[("n", n.to_string()), ("p1", (if cleared { "" } else { t("(旧目录尚有占用中的文件)") }).to_string())]);
                                    }
                                    Err(e) => {
                                        self.status = t_with("缓存迁移失败: {e}", &[("e", e.to_string())]);
                                    }
                                }
                                // 搬完立刻重采,别让面板显示旧数字。
                                self.cache_usage = None;
                            }
                            ui.small(
                                t("旧位置若有 *.reset-* 历史备份,后台不会自动搬;点此会整棵搬到新缓存目录并清掉来源。"),
                            );
                        }
                        ui.separator();
                        ui.small(t("排除规则、文件夹监听在「工具 > 文件夹管理器」里配置。"));
                        ui.small(t("索引备份 / 压缩在「工具 > 图库维护」里。"));
                    }
                    // 全盘快查;人脸方案与 AI 模型已合并到「AI 模型」页。
                    1 => self.options_svc_ui(ui),
                    2 => {
                        // 电子邮件(8 档尺寸列表,默认 1024)。
                        ui.label(t("邮件附件默认尺寸(发送窗口仍可临时改):"));
                        let cur_px = self.email_size.px();
                        for s in crate::share::ShareSize::ALL {
                            if ui
                                .selectable_label(cur_px == s.px(), t(s.label()))
                                .clicked()
                            {
                                self.email_size = s;
                                let _ = self
                                    .store
                                    .set_setting("email_size_px", &s.px().to_string());
                            }
                        }
                    }
                    3 => {
                        let mut itv = self
                            .store
                            .get_setting("slideshow_interval")
                            .and_then(|v| v.parse::<f32>().ok())
                            .unwrap_or(3.0);
                        if ui
                            .add(egui::Slider::new(&mut itv, 1.0..=10.0).text(t("每张停留(秒)")))
                            .changed()
                        {
                            let _ = self.store.set_setting("slideshow_interval", &format!("{itv:.1}"));
                        }
                        let mut lp = self
                            .store
                            .get_setting("slideshow_loop")
                            .map(|v| v != "0")
                            .unwrap_or(true);
                        if ui.checkbox(&mut lp, t("循环放映")).changed() {
                            let _ =
                                self.store.set_setting("slideshow_loop", if lp { "1" } else { "0" });
                        }
                    }
                    #[cfg(feature = "ai")]
                    4 => self.ai_options_ui(ui),
                    #[cfg(feature = "ai")]
                    5 => self.ai_models_ui(ui),
                    #[cfg(not(feature = "ai"))]
                    4 | 5 => {
                        ui.label(t("本版本未启用 AI 管线(编译时未带 ai 特性)。"));
                    }
                    _ => {}
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
                // 不循环时放到尾张即退出。
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
                // 点击前进。
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
                let mut line = t_with(
                    "{p1} / {p2}    空格 暂停 · Esc 退出",
                    &[("p1", (pos).to_string()), ("p2", (n).to_string())],
                );
                if self.slide_pause {
                    line = t_with("[已暂停]  {line}", &[("line", line.to_string())]);
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

    /// 应用裁剪(把归一化矩形写进效果链)。
    fn commit_crop(&mut self, id: i64) {
        let Some([ax, ay, bx, by]) = self.crop_rect.take() else {
            return;
        };
        let (x0, x1) = (ax.min(bx), ax.max(bx));
        let (y0, y1) = (ay.min(by), ay.max(by));
        let (w, h) = (x1 - x0, y1 - y0);
        if w < 0.02 || h < 0.02 {
            self.fx_msg = t("裁剪区域太小").into();
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
            t("裁剪"),
            crate::effects::Effect {
                name: "crop".into(),
                params,
            },
        );
    }

    /// 派发滚动事件(键盘翻页 / 跳到顶底)。
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
        self.status = t_with("已派发 {event}", &[("event", event.to_string())]);
    }

    /// 用位移阻尼曲线整形本帧滚动速度,
    /// 并据此决定"本帧允许同步解码多少张缩略图"。
    ///
    /// 设计要点:
    /// - 滚动时把"这一步该滚多少"整形成**平滑速度**,
    ///   保证缩略图网格快速滚动时不掉帧;
    /// - 其余量:`0.5` 是包围盒中点系数、`2.7` 是调用方增益、
    ///   `-2.4` 是阻尼系数、`0.3`/`4.25` 是两个 `powf` 指数、`100` 是分支阈值。
    ///
    /// 本项目用 egui `ScrollArea`(自带虚拟化与惯性),把"限速"作用
    /// 等价实现为**解码预算**:滚动越快 → 每帧同步解码越少。
    fn update_scroll_throttle(&mut self, offset_y: f32, viewport_h: f32, content_h: f32) {
        self.last_viewport_h = viewport_h;
        self.last_content_h = content_h;
        self.scroll_throttle.set_viewport(viewport_h);
        // 逐帧的质心位移 × 2.7 → 阻尼曲线 → 平滑速度。
        // 这条路径对应缩略图滚动事件
        // (唯一走缩略图质心位移算法质心算法的事件,滚动事件处理)。
        let vel = self.scroll_throttle.feed_offset(offset_y, content_h);
        // 张数口径 → 字节口径:大档一张就是 4MB,只按张数限流会让 UI 卡死。
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

    /// 添加一个自定义排除目录名(写回 `settings.exclude_dirs`)。
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
            self.status = t_with("排除规则「{name}」已存在", &[("name", name.to_string())]);
            return;
        }
        if !cur.is_empty() && !cur.ends_with('\n') {
            cur.push('\n');
        }
        cur.push_str(name);
        match self.store.set_setting("exclude_dirs", &cur) {
            Ok(()) => {
                self.status = t_with("已添加排除目录「{name}」", &[("name", name.to_string())])
            }
            Err(e) => self.status = t_with("添加失败: {e}", &[("e", e.to_string())]),
        }
    }

    /// 移除一个自定义排除目录名。
    fn remove_exclude_dir(&mut self, name: &str) {
        let cur = self.store.get_setting("exclude_dirs").unwrap_or_default();
        let kept: Vec<&str> = cur
            .lines()
            .filter(|l| !l.trim().eq_ignore_ascii_case(name))
            .collect();
        let newv = kept.join("\n");
        match self.store.set_setting("exclude_dirs", &newv) {
            Ok(()) => {
                self.status = t_with("已移除排除目录「{name}」", &[("name", name.to_string())])
            }
            Err(e) => self.status = t_with("移除失败: {e}", &[("e", e.to_string())]),
        }
    }

    /// 文件夹管理器 > 从图库移除:删该文件夹(含子文件夹)索引 + 取消监听。
    fn remove_folder_from_library(&mut self, folder: &str) {
        match timeline::remove_photos_under(&self.store, folder) {
            Ok(n) => {
                let _ = self.store.remove_watched(Path::new(folder));
                self.status = t_with(
                    "已移除 {folder} 下 {n} 张索引(未删除磁盘文件)",
                    &[("folder", folder.to_string()), ("n", n.to_string())],
                );
                self.sel.clear();
                self.preview = None;
                self.preview_tex = None;
                self.rebuild();
                self.watched = self.store.watched_stats();
            }
            Err(e) => self.status = t_with("移除失败: {e}", &[("e", e.to_string())]),
        }
    }

    /// 强制刷新缩略图(的 `force_thumb_update` 偏好)。
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
        self.status = t_with(
            "{dirs} 个目录 / {rows} 行已退回待派生,后台正在重出缩略图",
            &[("dirs", dirs.to_string()), ("rows", rows.to_string())],
        );
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
            self.status = t_with(
                "上次未完成的强制刷新已续做:{rows} 行退回待派生",
                &[("rows", rows.to_string())],
            );
        }
    }

    /// 工具 > 图库维护 > 备份索引到…(VACUUM INTO,后台线程)。
    fn start_backup(&mut self) {
        if self.maint_rx.is_some() {
            self.status = t("维护操作进行中…").into();
            return;
        }
        let Some(dest) = rfd::FileDialog::new()
            .set_title(t("备份索引数据库"))
            .add_filter(t("SQLite 数据库"), &["db"])
            .set_file_name("maliang-index-backup.db")
            .save_file()
        else {
            return;
        };
        let store = self.store.clone();
        let (tx, rx) = mpsc::channel();
        self.maint_rx = Some(rx);
        self.status = t("正在备份索引…").into();
        std::thread::spawn(move || {
            let r = store
                .backup_to(&dest)
                .map(|_| t_with("索引已备份到 {p1}", &[("p1", (dest.display()).to_string())]))
                .map_err(|e| e.to_string());
            let _ = tx.send(r);
        });
    }

    /// 工具 > 图库维护 > 压缩索引(VACUUM,后台线程)。
    fn start_compact(&mut self) {
        if self.maint_rx.is_some() {
            self.status = t("维护操作进行中…").into();
            return;
        }
        let store = self.store.clone();
        let (tx, rx) = mpsc::channel();
        self.maint_rx = Some(rx);
        self.status = t("正在压缩索引…").into();
        std::thread::spawn(move || {
            let r = store
                .compact()
                .map(|_| t("索引已压缩(VACUUM)").to_string())
                .map_err(|e| e.to_string());
            let _ = tx.send(r);
        });
    }

    /// 工具 > 图库维护 > 从 XMP 导入标签…(后台线程)。
    /// reset=true 全量重导(菜单手动触发);false 只处理新扫描照片(启动静默增量)。
    /// 语义见 timeline::import_xmp_batch:一次性导入,DB 有值不复活。
    fn start_xmp_import(&mut self, reset: bool) {
        if self.maint_rx.is_some() {
            self.status = t("维护操作进行中…").into();
            return;
        }
        let store = self.store.clone();
        let (tx, rx) = mpsc::channel();
        self.maint_rx = Some(rx);
        self.status = t("正在从 XMP 导入标签/星标…").into();
        std::thread::spawn(move || {
            let r = (|| -> anyhow::Result<String> {
                if reset {
                    store.write(|conn| -> anyhow::Result<()> {
                        conn.execute("UPDATE photos SET xmp_imported=0", [])?;
                        Ok(())
                    })?;
                }
                let (stars, tagged) = timeline::import_xmp_batch(&store, 1_000_000)?;
                Ok(t_with(
                    "XMP 导入完成:{stars} 张星标,{tagged} 张标签",
                    &[("stars", stars.to_string()), ("tagged", tagged.to_string())],
                ))
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
                self.status = t_with("清理缺失文件失败: {e}", &[("e", e.to_string())]);
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
        self.status = t_with(
            "{p1} 张照片的原文件已不存在,已移出图库",
            &[("p1", (gone.len()).to_string())],
        );
    }

    /// 工具 > 图库维护 > 清理已缺失文件的索引(后台线程)。
    fn clean_missing(&mut self) {
        if self.maint_rx.is_some() {
            self.status = t("维护操作进行中…").into();
            return;
        }
        let store = self.store.clone();
        let (tx, rx) = mpsc::channel();
        self.maint_rx = Some(rx);
        self.status = t("正在检查缺失文件…").into();
        std::thread::spawn(move || {
            let r = store
                .mark_missing()
                .map(|v| {
                    t_with(
                        "已清理 {p1} 条缺失文件的索引",
                        &[("p1", (v.len()).to_string())],
                    )
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(r);
        });
    }

    /// 工具 > 导出地理标记(KML):带 GPS 的照片 -> Google Earth 可打开的 KML。
    fn export_kml(&mut self) {
        let pts = timeline::gps_points(&self.store).unwrap_or_default();
        if pts.is_empty() {
            self.status = t("没有带 GPS 坐标的照片(需原图 EXIF 含定位信息)").into();
            return;
        }
        let Some(dest) = rfd::FileDialog::new()
            .set_title(t("导出地理标记 KML"))
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
            Ok(()) => {
                self.status = t_with(
                    "已导出 {p1} 个地理标记到 {p2}",
                    &[
                        ("p1", (pts.len()).to_string()),
                        ("p2", (dest.display()).to_string()),
                    ],
                )
            }
            Err(e) => self.status = t_with("KML 导出失败: {e}", &[("e", e.to_string())]),
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
            None => self.status = t("没有可定位的照片(先预览或选中一张)").into(),
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
            self.anim_reset();
            self.preview_reset_zoom();
            debug_assert!(row < self.rows.len());
        }
    }

    /// 预览缩放/平移复位(换照片、关闭预览时调用;换图回到适应窗口)。
    fn preview_reset_zoom(&mut self) {
        self.preview_zoom = 1.0;
        self.preview_pan = egui::Vec2::ZERO;
        self.crop_mode = false;
        // 全图缓存内存可观(几十 MB),换图即释放;在途解码结果按 id 校验丢弃。
        self.preview_full = None;
        self.zoom_tex = None;
        self.zoom_rx = None;
        self.zoom_pending = None;
        self.zoom_failed_for = None;
        // 裁剪框是相对当前照片的草稿,换图 / 关预览时必须一起丢掉。
        self.crop_rect = None;
        self.crop_drag = None;
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
        // 选中态:叶子 = 槽位命中且没有子树根覆盖;中间层 = 自己就是当前子树根。
        let active = !self.folder_show_all
            && match folder_index {
                Some(fi) => self.subtree_sel.is_none() && self.selected_folder == fi,
                None => self.subtree_sel.as_deref() == Some(path.as_str()),
            };
        ui.horizontal(|ui| {
            // 引导虚线(先画,压在内容底下):祖先层各一条竖虚线,
            // 本层再一条横虚线接到三角钮 —— 父子层级一眼可见。
            let line = egui::Stroke::new(1.0_f32, crate::theme::p().faint);
            let left = ui.cursor().left();
            let top = ui.cursor().top();
            let cy = top + 10.0;
            let mut segs: Vec<egui::Shape> = Vec::new();
            for i in 0..depth {
                let x = left + (i as f32) * 10.0 + 5.0;
                segs.extend(egui::Shape::dashed_line(
                    &[egui::pos2(x, top), egui::pos2(x, top + 20.0)],
                    line,
                    3.0,
                    3.0,
                ));
            }
            if depth > 0 {
                let x0 = left + (depth as f32 - 1.0) * 10.0 + 5.0;
                let x1 = left + (depth as f32) * 10.0 + 12.0;
                segs.extend(egui::Shape::dashed_line(
                    &[egui::pos2(x0, cy), egui::pos2(x1, cy)],
                    line,
                    3.0,
                    3.0,
                ));
            }
            ui.painter().extend(segs);
            ui.add_space((depth as f32) * 10.0);
            if !kids.is_empty() {
                if ui
                    .small_button(if open { "▾" } else { "▸" })
                    .on_hover_text(t("展开 / 收起"))
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
            // 树里每个节点都装着照片(至少子树里有),名字一律可选色;
            // 中间层与叶子的差别只在点击后的聚合范围。
            let text = egui::RichText::new(name.as_str()).color(crate::theme::p().text);
            if ui.selectable_label(active, text).clicked() {
                match folder_index {
                    Some(fi) => {
                        self.selected_folder = fi;
                        self.subtree_sel = None;
                        self.folder_show_all = false;
                        self.enter_dir(&path);
                        self.tree_reveal(&path);
                    }
                    // 中间层目录:点名字 = 选中该子树,中间栏聚合显示
                    // 本目录 + 全部子孙目录的照片;已选中时再点 = 展开/收起。
                    None => {
                        if self.subtree_sel.as_deref() == Some(path.as_str()) {
                            if open {
                                self.tree_open.remove(&path);
                            } else {
                                self.tree_open.insert(path.clone());
                            }
                        } else {
                            self.subtree_sel = Some(path.clone());
                            self.folder_show_all = false;
                            self.enter_dir(&path);
                            self.tree_reveal(&path);
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
                let t = thumb_tex(ctx, &self.store, id, tier);
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

    /// 左栏「文件夹」抽屉明细:目录树 / 平铺列表。
    fn nav_folder_detail(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(t("文件夹"))
                    .size(12.0)
                    .color(crate::theme::p().muted)
                    .strong(),
            );
            ui.with_layout(
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| {
                    let tip = format!(
                        "{} — {}",
                        t("全部文件夹"),
                        t("中间栏一次铺开所有文件夹(每组一个相册头)")
                    );
                    if icon_toggle(ui, "images", &tip, self.folder_show_all) {
                        self.folder_show_all = true;
                        self.subtree_sel = None;
                    }
                },
            );
        });
        if self.folder_tree {
            // 真递归目录树:每层缩进,三角展开/收起;
            // 只有装了照片的目录才是可选项(点父级 = 展开)。
            self.folder_tree_ui(ui, ctx);
        } else {
            // 扁平文件夹视图(平铺列表)。
            for (i, f) in self.folders.clone().iter().enumerate() {
                let on = !self.folder_show_all && self.selected_folder == i;
                if ui.selectable_label(on, f).clicked() {
                    self.selected_folder = i;
                    self.subtree_sel = None;
                    self.folder_show_all = false;
                    // 平铺列表同样是「进目录」:树形那条点完会补派生账,
                    // 这里漏掉就让两种文件夹视图行为不一致。
                    self.enter_dir(f);
                    self.tree_reveal(f);
                }
            }
        }
    }

    /// 左栏「相簿」抽屉明细:新建 / 列表 / 重命名 / 删除。
    fn nav_albums_detail(&mut self, ui: &mut egui::Ui) {
        crate::theme::section_title(ui, t("相簿"));
        let albums = self.store.albums();
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.album_input)
                    .hint_text(t("新相簿名称")),
            );
            if ui.button(t("新建")).clicked() && !self.album_input.trim().is_empty()
            {
                match self.store.album_create(self.album_input.trim()) {
                    Ok(aid) => {
                        self.sel_album = Some(aid);
                        self.album_input.clear();
                    }
                    Err(e) => {
                        self.fx_msg =
                            t_with("新建相簿失败: {e}", &[("e", e.to_string())])
                    }
                }
            }
        });
        ui.separator();
        for (aid, name, n) in albums {
            if ui
                .selectable_label(
                    self.sel_album == Some(aid),
                    t_with(
                        "{name} · {n} 张",
                        &[("name", name.to_string()), ("n", n.to_string())],
                    ),
                )
                .clicked()
            {
                self.sel_album = Some(aid);
            }
        }
        if let Some(a) = self.sel_album {
            ui.separator();
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.album_input)
                        .hint_text(t("重命名为")),
                );
                if ui.button(t("重命名")).clicked()
                    && !self.album_input.trim().is_empty()
                {
                    if let Err(e) =
                        self.store.album_rename(a, self.album_input.trim())
                    {
                        self.fx_msg =
                            t_with("重命名失败: {e}", &[("e", e.to_string())]);
                    } else {
                        self.album_input.clear();
                    }
                }
            });
            if ui.button(t("删除当前相簿")).clicked() {
                let _ = self.store.album_delete(a);
                self.sel_album = None;
            }
        }
    }

    /// 左栏「标签」抽屉明细:检索、AI 分类与标签树。
    fn nav_tags_detail(&mut self, ui: &mut egui::Ui) {
        crate::theme::section_title(ui, t("标签"));
        #[cfg(feature = "ai")]
        {
            ui.label(t("库内检索:标签、图中文字或描述"));
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.ai_query)
                    .hint_text(t("输入标签名 / 图中文字 / 描述,即输即搜…"))
                    .desired_width(f32::INFINITY),
            );
            if resp.changed() {
                self.sel_tag = None;
                self.sel_category = None;
                self.ai_query_hits.clear();
                let q = self.ai_query.trim().to_string();
                if !q.is_empty() {
                    let tag_ver = crate::ai_queue::stage_version(
                        &self.store,
                        crate::ai_queue::Stage::Tags,
                    );
                    let tags = crate::ai_search::tag_search(
                        &self.store, &tag_ver, &q, 200,
                    );
                    let ocr =
                        crate::ai_ocr::search(&self.store, &q, 200);
                    let cap = crate::ai_search::caption_search(
                        &self.store, &q, 200,
                    );
                    self.ai_query_hits = crate::ai_search::fuse(
                        &[tags, ocr, cap],
                        60.0,
                        200,
                    )
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect();
                }
            }
            ui.separator();
            ui.label(t("AI 分类"));
            let cats = self.ai_categories();
            for (node, label, n) in cats {
                let resp = ui.selectable_label(
                    self.sel_category.as_deref() == Some(node),
                    t_with(
                        "{p1}  ·  {p2} 张",
                        &[
                            ("p1", t(label).to_string()),
                            ("p2", (n).to_string()),
                        ],
                    ),
                );
                if resp.clicked() {
                    if self.sel_category.as_deref() == Some(node) {
                        self.sel_category = None;
                    } else {
                        self.sel_category = Some(node.to_string());
                        self.sel_tag = None;
                        self.ai_query.clear();
                        self.ai_query_hits.clear();
                    }
                }
            }
            ui.separator();
        }
        ui.label(t_with(
            "共 {p1} 个(右键设颜色/重命名/删除)",
            &[("p1", (self.tags.len()).to_string())],
        ));
        if self.tag_rename.is_some() {
            let old = self.tag_rename.as_ref().unwrap().0.clone();
            let mut confirm = false;
            let mut cancel = false;
            ui.horizontal(|ui| {
                ui.label(t_with(
                    "重命名 «{p1}»",
                    &[("p1", old.clone())],
                ));
                if let Some((_, input)) = self.tag_rename.as_mut() {
                    ui.add(
                        egui::TextEdit::singleline(input)
                            .hint_text(t("新标签名")),
                    );
                }
                confirm = ui.button(t("重命名")).clicked();
                cancel = ui.button(t("取消")).clicked();
            });
            if cancel {
                self.tag_rename = None;
            } else if confirm {
                let new = self
                    .tag_rename
                    .as_ref()
                    .map(|(_, s)| s.trim().to_string())
                    .unwrap_or_default();
                match timeline::rename_tag(&self.store, &old, &new) {
                    Ok(ids) => {
                        if self.sel_tag.as_deref() == Some(old.as_str()) {
                            self.sel_tag = Some(new.clone());
                        }
                        for id in ids {
                            let star = self.star_of(id);
                            self.sync_sidecars(id, star);
                        }
                        self.tags = timeline::all_tags(&self.store)
                            .unwrap_or_default();
                        self.tag_rename = None;
                        self.status = t("标签已重命名").into();
                    }
                    Err(_) => {
                        self.status =
                            t("重命名失败:名称为空或已存在").into();
                    }
                }
            }
        }
        if let Some(name) = self.tag_delete.clone() {
            let n = self
                .tags
                .iter()
                .find(|x| x.name == name)
                .map(|x| x.count)
                .unwrap_or(0);
            let mut confirm = false;
            let mut cancel = false;
            ui.horizontal(|ui| {
                ui.label(t_with(
                    "确定删除标签 «{p1}»(含子标签)? 将同时从 {p2} 张照片上移除",
                    &[("p1", name.clone()), ("p2", n.to_string())],
                ));
                confirm = ui.button(t("删除")).clicked();
                cancel = ui.button(t("取消")).clicked();
            });
            if cancel {
                self.tag_delete = None;
            } else if confirm {
                match timeline::delete_tag(&self.store, &name) {
                    Ok(ids) => {
                        if self.sel_tag.as_deref() == Some(name.as_str()) {
                            self.sel_tag = None;
                        }
                        for id in ids {
                            let star = self.star_of(id);
                            self.sync_sidecars(id, star);
                        }
                        self.tags = timeline::all_tags(&self.store)
                            .unwrap_or_default();
                        self.tag_delete = None;
                        self.status = t("标签已删除").into();
                    }
                    Err(_) => {
                        self.status = t("删除标签失败").into();
                    }
                }
            }
        }
        // 标签只渲染一份:层级缩进 + 色标 + 右键菜单都在同一行上。
        for (depth, label, path, count, real) in tag_tree_rows(&self.tags) {
            let color = self
                .tags
                .iter()
                .find(|x| x.name == path)
                .and_then(|x| x.color.clone());
            ui.horizontal(|ui| {
                ui.add_space(depth as f32 * 12.0);
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(10.0, 10.0),
                    egui::Sense::hover(),
                );
                if real {
                    ui.painter().rect_filled(
                        rect,
                        2.0,
                        tag_color(color.as_deref())
                            .unwrap_or_else(|| egui::Color32::from_gray(90)),
                    );
                }
                let text = if real {
                    t_with(
                        "{p1}  ·  {p2} 张",
                        &[("p1", label), ("p2", count.to_string())],
                    )
                } else {
                    label
                };
                let resp = ui.selectable_label(
                    self.sel_tag.as_deref() == Some(path.as_str()),
                    text,
                );
                if resp.clicked() {
                    // 再次点击已选中的标签 = 取消筛选(回到全部)
                    if self.sel_tag.as_deref() == Some(path.as_str()) {
                        self.sel_tag = None;
                    } else {
                        self.sel_tag = Some(path.clone());
                    }
                    #[cfg(feature = "ai")]
                    {
                        self.sel_category = None;
                        self.ai_query.clear();
                        self.ai_query_hits.clear();
                    }
                }
                if !real {
                    return;
                }
                resp.context_menu(|ui| {
                    ui.label(crate::i18n::t("标签颜色"));
                    for (cname, cval) in TAG_COLORS {
                        let mark = if Some(cval) == tag_color(color.as_deref()) {
                            "● "
                        } else {
                            "○ "
                        };
                        if ui
                            .button(format!(
                                "{mark}{}",
                                crate::i18n::t(tag_color_name(cname))
                            ))
                            .clicked()
                        {
                            let _ = timeline::set_tag_color(
                                &self.store,
                                &path,
                                Some(cname),
                            );
                            self.tags = timeline::all_tags(&self.store)
                                .unwrap_or_default();
                            ui.close();
                        }
                    }
                    if ui.button(crate::i18n::t("无颜色")).clicked() {
                        let _ = timeline::set_tag_color(&self.store, &path, None);
                        self.tags = timeline::all_tags(&self.store)
                            .unwrap_or_default();
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .button(crate::i18n::t("重命名标签"))
                        .clicked()
                    {
                        self.tag_rename = Some((path.clone(), path.clone()));
                        self.tag_delete = None;
                        ui.close();
                    }
                    if ui.button(crate::i18n::t("删除标签")).clicked() {
                        self.tag_delete = Some(path.clone());
                        self.tag_rename = None;
                        ui.close();
                    }
                });
            });
        }
    }

    #[cfg(feature = "ai")]
    /// 左栏「人物」抽屉明细:聚类列表与命名建议。
    fn nav_faces_detail(&mut self, ui: &mut egui::Ui) {
        crate::theme::section_title(ui, t("人物"));
        ui.horizontal(|ui| {
            let (hide, ignored) =
                face_filter_row(ui, &mut self.face_hide_low, &mut self.face_show_ignored);
            if hide.changed() || ignored.changed() {
                self.refresh_face_hits();
            }
        });
        ui.label(t_with(
            "共 {p1} 组 · 未分析 {p2} 张",
            &[
                ("p1", (self.face_people.len()).to_string()),
                (
                    "p2",
                    (crate::faces::remaining_count(&self.store)).to_string(),
                ),
            ],
        ));
        // 全量重聚类:向量重排簇号,身份按成员多数票继承。
        let failed = crate::faces::failed_count(&self.store);
        let unassigned = crate::faces::unassigned_count(&self.store);
        if unassigned > 0 {
            ui.colored_label(
                crate::theme::p().accent,
                t_with(
                    "未归簇 {p1} 脸 · 点「重新聚类」收齐",
                    &[("p1", unassigned.to_string())],
                ),
            );
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    self.recluster_rx.is_none(),
                    egui::Button::new(t("重新聚类")),
                )
                .clicked()
            {
                self.start_recluster();
            }
            if failed > 0
                && ui
                    .button(t_with(
                        "重试失败 ({p1})",
                        &[("p1", failed.to_string())],
                    ))
                    .clicked()
            {
                match crate::faces::retry_failed(&self.store) {
                    Ok(n) => {
                        self.status = t_with(
                            "已重新排队 {n} 张失败照片",
                            &[("n", n.to_string())],
                        );
                        self.refresh_people_and_suggestions();
                        if n > 0 {
                            self.start_face_scan();
                        }
                    }
                    Err(e) => {
                        self.status = t_with(
                            "重试失败: {e}",
                            &[("e", e.to_string())],
                        );
                    }
                }
            }
        });
        // 待命名组按相似度建议并入已有档案,不新建同名身份。
        if !self.face_sugs.is_empty() {
            ui.separator();
            ui.label(t("命名建议:"));
            for s in self.face_sugs.clone() {
                ui.horizontal(|ui| {
                    ui.label(t_with(
                        "簇 {p1} → {p2} ({p3} · {p4} 张)",
                        &[
                            ("p1", (s.cluster).to_string()),
                            ("p2", (s.name).to_string()),
                            ("p3", format!("{p3:.2}", p3 = (s.score))),
                            ("p4", (s.count).to_string()),
                        ],
                    ));
                    if ui.small_button(t("并入人物")).clicked() {
                        match crate::faces::merge_into_person(
                            &self.store,
                            s.cluster,
                            s.person_id,
                            s.target_person_id,
                        ) {
                            Ok(_) => {
                                self.status = t_with(
                                    "已并入:{name}",
                                    &[("name", s.name.clone())],
                                );
                            }
                            Err(e) => {
                                self.status = t_with(
                                    "并入失败: {e}",
                                    &[("e", e.to_string())],
                                );
                            }
                        }
                        self.refresh_people_and_suggestions();
                    }
                });
            }
        }
        for p in self.sorted_people() {
            let display = p.person.clone().unwrap_or_else(|| {
                match p.person_id {
                    None => t_with(
                        "未归属 · 簇 {p1}",
                        &[("p1", (p.cluster).to_string())],
                    )
                    .to_string(),
                    Some(pid) => t_with(
                        "未命名人物 {p1}",
                        &[("p1", pid.to_string())],
                    )
                    .to_string(),
                }
            });
            let label = t_with(
                "{display}  ·  {p1} 张  · 质量 {p2}",
                &[
                    ("display", display.to_string()),
                    ("p1", (p.count).to_string()),
                    ("p2", format!("{p2:.2}", p2 = (p.quality))),
                ],
            );
            let selected = self.sel_person == p.person_id
                && self.sel_cluster == Some(p.cluster);
            if ui.selectable_label(selected, label).clicked() {
                self.sel_person = p.person_id;
                self.sel_cluster = Some(p.cluster);
                self.refresh_face_hits();
            }
        }
        // 已忽略簇(可选中后恢复)
        if self.face_show_ignored {
            for cid in crate::faces::ignored_clusters(&self.store) {
                let label = t_with(
                    "簇 {cid}  · 已忽略",
                    &[("cid", cid.to_string())],
                );
                let selected = self.sel_person.is_none()
                    && self.sel_cluster == Some(cid);
                if ui.selectable_label(selected, label).clicked() {
                    self.sel_person = None;
                    self.sel_cluster = Some(cid);
                    self.refresh_face_hits();
                }
            }
        }
    }

    #[cfg(feature = "ai")]
    /// 左栏「语义搜索」抽屉明细:查询、模型状态与结果。
    fn nav_semantic_detail(&mut self, ui: &mut egui::Ui) {
        crate::theme::section_title(ui, t("语义搜索"));
        let resp = ui.text_edit_singleline(&mut self.input_query);
        let enter =
            resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (enter || icon_action(ui, "search", t("搜索"), self.clip_search_rx.is_none()))
            && self.clip_search_rx.is_none()
        {
            let q = self.input_query.trim().to_string();
            if !q.is_empty() {
                self.start_clip_search(q);
            }
        }
        if self.clip_search_rx.is_some() {
            ui.spinner();
            ui.label(t("搜索中…"));
        }
        ui.separator();
        // 索引语言单选(中英双模型;搜索按查询含 CJK 自动匹配)
        ui.horizontal(|ui| {
            ui.label(t("模型:"));
            ui.selectable_value(
                &mut self.clip_lang,
                crate::clip::Lang::En,
                t("英文"),
            );
            ui.selectable_value(
                &mut self.clip_lang,
                crate::clip::Lang::Zh,
                t("中文"),
            );
        });
        // 模型目录里实际放了哪些 .onnx:一个都没有就直说,
        // 免得用户点了「语义索引」才在后台线程里撞到"模型缺失"。
        let models = crate::ai::available_models();
        if models.is_empty() {
            ui.small(t("模型目录还没有 .onnx 模型文件,请先放入后再索引"));
            ui.small(
                crate::ai::models_dir()
                    .map(|d| {
                        t_with(
                            "模型目录:{p1}",
                            &[("p1", (d.display()).to_string())],
                        )
                    })
                    .unwrap_or_default(),
            );
        } else {
            ui.small(t_with(
                "已装模型:{p1}",
                &[("p1", (models.join("、")).to_string())],
            ));
        }
        let natural = crate::clip::natural_ready(&self.store);
        let indexed = crate::clip::progress(&self.store, self.clip_lang);
        let total_zh =
            crate::clip::clip_count_lang(&self.store, crate::clip::Lang::Zh);
        let total_en =
            crate::clip::clip_count_lang(&self.store, crate::clip::Lang::En);
        ui.label(t_with(
            "已索引({p1}){indexed} / {p2} 张",
            &[
                (
                    "p1",
                    if natural {
                        t("语义向量").to_string()
                    } else {
                        self.clip_lang.label().to_string()
                    },
                ),
                ("indexed", indexed.to_string()),
                ("p2", (self.rows.len()).to_string()),
            ],
        ));
        if indexed < self.rows.len() {
            if natural {
                ui.label(t("语义索引正在后台补建,稍后再试。"));
            } else {
                ui.label(t_with(
                    "点 工具 > 语义索引 补嵌(当前{p1}模型,较慢)",
                    &[("p1", (self.clip_lang.label()).to_string())],
                ));
            }
        }
        if total_en + total_zh > 0 {
            ui.label(t_with(
                "中/英索引量:{total_zh} / {total_en}",
                &[
                    ("total_zh", total_zh.to_string()),
                    ("total_en", total_en.to_string()),
                ],
            ));
        }
        ui.separator();
        ui.label(t_with(
            "结果 {p1} 张",
            &[("p1", (self.clip_results.len()).to_string())],
        ));
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
            let label = match score {
                Some(s) => format!("{s:.3}  {fname}"),
                None => format!("{}  {fname}", t("文本命中")),
            };
            if ui.selectable_label(false, label).clicked() {
                if self.id_row.contains_key(&pid) {
                    self.preview = Some(pid);
                }
            }
        }
    }

    /// 左栏一条导航项(条目后带 `(n)` 计数)。
    fn nav_item(&mut self, ui: &mut egui::Ui, label: &str, v: ViewKind, count: Option<usize>) {
        let active = self.view == v;
        let text = match count {
            Some(n) => format!("{label} ({n})"),
            None => label.to_string(),
        };
        let rt = if active {
            egui::RichText::new(text)
                .color(crate::theme::p().accent_strong)
                .strong()
        } else {
            egui::RichText::new(text).color(crate::theme::p().text)
        };
        if ui.selectable_label(active, rt).clicked() && !active {
            self.set_view(v);
            // 快速查找:服务连不上就顺势引导启用。
            #[cfg(windows)]
            if v == ViewKind::Search {
                self.svc_nudge_on_search_entry();
            }
        }
    }

    /// 首页「最近更新」的相册头:标题 + 日期 + 张数,右侧挂放映 / 邮件 / 重算。
    /// 这一组控件固定放在相册缩略图上方,文件夹视图的组头与之同构。
    fn recent_header(&mut self, ui: &mut egui::Ui) {
        let n = self.recent_idxs.len();
        let sub = if self.recent_date.is_empty() {
            t_with("{n} 张照片", &[("n", n.to_string())])
        } else {
            t_with(
                "{p1} · {n} 张照片",
                &[("p1", (self.recent_date).to_string()), ("n", n.to_string())],
            )
        };
        let mut act_play = false;
        let mut act_mail = false;
        let mut act_refresh = false;
        ui.horizontal(|ui| {
            ui.add(album_icon("images", 26.0));
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.add(
                    egui::Label::new(egui::RichText::new(t("最近更新")).size(17.0).strong())
                        .selectable(false),
                );
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&sub)
                            .size(12.0)
                            .color(crate::theme::p().muted),
                    )
                    .selectable(false),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(tool_button("refresh", false))
                    .on_hover_text(t("刷新最近更新"))
                    .clicked()
                {
                    act_refresh = true;
                }
                if ui
                    .add(tool_button("mail", false))
                    .on_hover_text(t("以邮件发送本相册照片"))
                    .clicked()
                {
                    act_mail = true;
                }
                if ui
                    .add(tool_button("play", false))
                    .on_hover_text(t("放映本相册幻灯片"))
                    .clicked()
                {
                    act_play = true;
                }
            });
        });
        if act_refresh {
            self.rebuild();
            self.status = t_with(
                "最近更新已刷新:{p1} 张",
                &[("p1", (self.recent_idxs.len()).to_string())],
            );
        }
        if act_play {
            let ids = self.view_ids();
            if self.preview.is_some_and(|p| !ids.contains(&p)) {
                self.preview = None;
                self.preview_tex = None;
            }
            self.start_slideshow();
        }
        if act_mail {
            self.email_win = true;
        }
    }

    /// 组头(每个相册上方那行:大图标 + 名称 + 日期):
    /// 大号目录图标 + 文件夹名 + 组内最早拍摄日期与张数,右侧挂本组动作按钮。
    ///
    /// `date` 由调用方缓存好传进来(见 `take_folder_groups`):逐帧扫全组找最早日期
    /// 在大图库里是 O(行数) 的白活。
    ///
    /// 放映与邮件复用视图级入口:先把视图范围收窄到本组,再调现成的
    /// `start_slideshow` / 邮件窗口 —— 不必为幻灯片另开一条 id 队列。
    #[allow(clippy::too_many_arguments)]
    fn album_label(&mut self, ui: &mut egui::Ui, folder: &str, date: &str, count: usize) {
        let mut name = Path::new(folder.trim_end_matches(['\\', '/']))
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| folder.to_string());
        if name.is_empty() {
            name = t("(未知位置)").into();
        }
        // 组内全无 EXIF 拍摄时间时 date 是空串:只报张数,不编日期。
        let sub = if date.is_empty() {
            t_with("{count} 张", &[("count", count.to_string())])
        } else {
            t_with(
                "{date} · {count} 张",
                &[("date", date.to_string()), ("count", count.to_string())],
            )
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
            // 动作按钮钉在组头右侧(从右往左排列)。
            // 文件名过滤框与详细/缩略图切换是视图级的,已提到滚动区之外常驻。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(tool_button("mail", false))
                    .on_hover_text(t("以邮件发送本组照片"))
                    .clicked()
                {
                    act_mail = true;
                }
                if ui
                    .add(tool_button("folder-open", false))
                    .on_hover_text(t("在资源管理器中打开该文件夹"))
                    .clicked()
                {
                    act_open = true;
                }
                if ui
                    .add(tool_button("play", false))
                    .on_hover_text(t("放映本组幻灯片"))
                    .clicked()
                {
                    act_play = true;
                }
            });
        });
        if act_open && !crate::win::shell_open(Path::new(folder)) {
            self.status = t_with(
                "无法在资源管理器中打开:{folder}",
                &[("folder", folder.to_string())],
            );
        }
        if act_play || act_mail {
            // 只在点击时反查一次左栏槽位(逐帧扫 folders 列表没必要)。
            if let Some(slot) = self.folders.iter().position(|f| f == folder) {
                self.selected_folder = slot;
                self.subtree_sel = None;
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

    /// 文件夹视图的常驻工具条(视图级,画在滚动区之外):
    /// 文件名过滤框 + 详细/缩略图切换。滚动再远也钉在中央面板顶部,
    /// 不属于某一个文件夹组 —— 铺开所有文件夹时明细切换不适用,置灰。
    fn folder_toolbar_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let show_all = self.folder_show_all;
            if ui
                .add_enabled(!show_all, tool_button("list", self.folder_details))
                .on_hover_text(t("详细列表视图(点列头排序)"))
                .clicked()
            {
                self.set_folder_details(true);
            }
            if ui
                .add_enabled(!show_all, tool_button("layout-grid", !self.folder_details))
                .on_hover_text(t("缩略图视图"))
                .clicked()
            {
                self.set_folder_details(false);
            }
            ui.separator();
            ui.add_enabled(!show_all, toolbar_icon("funnel"))
                .on_hover_text(t("只显示文件名含关键词的照片(仅当前文件夹)"));
            ui.add_enabled(
                !show_all,
                egui::TextEdit::singleline(&mut self.folder_filter)
                    .desired_width(240.0)
                    .hint_text(t("在此文件夹内过滤文件名…")),
            );
            if !show_all
                && !self.folder_filter.is_empty()
                && ui.add(tool_button("x", false)).clicked()
            {
                self.folder_filter.clear();
            }
            if show_all {
                ui.label(
                    egui::RichText::new(t("铺开所有文件夹:选中左侧某个目录后可用过滤与明细视图。"))
                        .size(11.0)
                        .color(crate::theme::p().muted),
                );
            }
        });
        ui.separator();
    }

    /// 切换单文件夹视图的详细/网格形态(设置项 folder_details,记住上次选择)。
    fn set_folder_details(&mut self, details: bool) {
        if self.folder_details == details {
            return;
        }
        self.folder_details = details;
        let _ = self
            .store
            .set_setting("folder_details", if details { "1" } else { "0" });
    }

    /// 重复照片视图的顶部工具条:近似开关(漏斗图标)+ 组数提示。
    fn dups_toolbar_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if icon_toggle(
                ui,
                "funnel",
                &t_with(
                    "包含近似重复(hamming≤{NEAR_DUP_DIST})",
                    &[("NEAR_DUP_DIST", NEAR_DUP_DIST.to_string())],
                ),
                self.dup_near,
            ) {
                self.dup_near = !self.dup_near;
                self.dups = if self.dup_near {
                    timeline::dup_groups_near(&self.store, NEAR_DUP_DIST).unwrap_or_default()
                } else {
                    timeline::dup_groups(&self.store).unwrap_or_default()
                };
            }
            ui.separator();
            let label = if self.dup_near {
                t_with(
                    "重复照片 {p1} 组(含近似)",
                    &[("p1", (self.dups.len()).to_string())],
                )
            } else {
                t_with("重复照片 {p1} 组", &[("p1", (self.dups.len()).to_string())])
            };
            ui.label(
                egui::RichText::new(label)
                    .size(11.0)
                    .color(crate::theme::p().muted),
            );
        });
        ui.separator();
    }

    /// 点选语义(网格格子与详细列表行共用):
    /// 单击 = 只选这一张(替换选区);Ctrl+单击 = 加入/移出;
    /// Shift+单击 = 从上次单击处到当前张连选(在**当前视图范围内**取,
    /// 基准不在本视图里就退化为只选当前张;基准本身不移动)。
    fn select_by_click(&mut self, id: i64, ctrl: bool, shift: bool) {
        if ctrl {
            if !self.sel.remove(&id) {
                self.sel.insert(id);
            }
            self.sel_anchor = Some(id);
            return;
        }
        if shift {
            let view = self.view_ids();
            if view.is_empty() {
                return;
            }
            let pos = |x: i64| view.iter().position(|&p| p == x);
            let clicked = pos(id).unwrap_or(0);
            let anchor = self.sel_anchor.and_then(pos).unwrap_or(clicked);
            let (lo, hi) = if anchor <= clicked {
                (anchor, clicked)
            } else {
                (clicked, anchor)
            };
            self.sel = view[lo..=hi].iter().copied().collect();
            return;
        }
        self.sel.clear();
        self.sel.insert(id);
        self.sel_anchor = Some(id);
    }

    /// 双击打开(双击 = 进单图编辑视图;视频不进库内渲染,
    /// 交给系统关联播放器)。与快速查找页双击同一套状态复位。
    fn open_photo_editor(&mut self, id: i64, movie: bool) {
        if movie {
            if let Some(p) = self.photo_path(id) {
                if !crate::win::shell_open(Path::new(&p)) {
                    self.status = t_with("无法打开视频(无关联播放器):{p}", &[("p", p.to_string())]);
                }
            }
        } else {
            self.preview = Some(id);
            self.preview_tex = None;
            self.preview_failed = None;
            self.anim_reset();
            self.preview_reset_zoom();
        }
    }

    /// 详细列表视图(单文件夹视图的「详细」形态):
    /// 文件名 / 创建日期 / 大小 三列 + 星标,点列头排序(再点同一列反向);
    /// 交互与网格一致:单击选中(双击进单图视图)、Ctrl+单击加减选、右键同一套上下文菜单。
    fn photo_details(&mut self, ui: &mut egui::Ui, salt: &str, cells: &[usize]) {
        // 与网格同一套「显示优先」:屏幕上出现的目录照常消化纳入队列。
        self.drain_adopts(MAX_ADOPT_PER_FRAME);
        // 逐行先取数据再谈交互(行内要用 &mut self,不能同时攥着 &self.rows)。
        struct Row {
            idx: usize,
            id: i64,
            star: bool,
            movie: bool,
            name: String,
            dir: String,
            created: String,
            size: String,
        }
        let items: Vec<Row> = cells
            .iter()
            .map(|&i| {
                let r = &self.rows[i];
                Row {
                    idx: i,
                    id: r.id,
                    star: r.star != 0,
                    movie: r.kind == crate::video::kind::MOVIE,
                    name: r
                        .path
                        .rsplit(['\\', '/'])
                        .next()
                        .unwrap_or(&r.path)
                        .to_string(),
                    dir: r.parent.clone(),
                    // 明细列显示「创建日期」,但 photos 表没有 ctime,只有
                    // mtime(最后修改时间);照片文件的修改时间通常≈首次落盘,
                    // 拿它当创建日期用。不用 EXIF 拍摄时间:截图/转存图大多
                    // 没有拍摄时间,那一列会全空。
                    created: fmt_mtime(norm_mtime_secs(r.mtime)),
                    // 大小统一 MB 口径(不再 KB/MB 混排)。
                    size: fmt_size_mb(r.size.max(0) as u64),
                }
            })
            .collect();
        let date_w = 150.0;
        let size_w = 80.0;
        let star_w = 26.0;
        let name_w = (ui.available_width() - date_w - size_w - star_w - 24.0).max(120.0);
        let row_h = 22.0;
        let accent = crate::theme::p().accent;
        let muted = crate::theme::p().muted;
        let text = crate::theme::p().text;
        let faint = crate::theme::p().faint;
        ui.push_id(salt, |ui| {
            // —— 列头(扁平文字,与数据行同轴对齐,高度统一;
            // 点列名即按该列排序,再点同一列反向。名称/日期左对齐,大小右对齐)——
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let cols = [
                    (t("名称"), name_w, timeline::SortBy::Name, false),
                    (t("创建日期"), date_w, timeline::SortBy::Recent, false),
                    (t("大小"), size_w, timeline::SortBy::Size, true),
                ];
                for (col_label, w, col, right) in cols {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(w, 20.0), egui::Sense::hover());
                    let hit = ui.interact(
                        rect,
                        egui::Id::new(("col-head", col.key())),
                        egui::Sense::click(),
                    );
                    let active = self.sort_by == col;
                    let mut disp = col_label.to_string();
                    if active {
                        let arrow_up = if col == timeline::SortBy::Name {
                            !self.sort_rev
                        } else {
                            self.sort_rev
                        };
                        disp.push_str(if arrow_up { " ▲" } else { " ▼" });
                    }
                    if ui.rect_contains_pointer(rect) {
                        ui.painter().rect_filled(rect, 3.0, crate::theme::p().card);
                    }
                    let (pos, align) = if right {
                        (
                            egui::pos2(rect.right(), rect.center().y),
                            egui::Align2::RIGHT_CENTER,
                        )
                    } else {
                        (
                            egui::pos2(rect.left() + 8.0, rect.center().y),
                            egui::Align2::LEFT_CENTER,
                        )
                    };
                    ui.painter().text(
                        pos,
                        align,
                        &disp,
                        egui::FontId::proportional(12.0),
                        if active { accent } else { muted },
                    );
                    if hit.clicked() {
                        if active {
                            self.sort_rev = !self.sort_rev;
                        } else {
                            self.sort_by = col;
                            self.sort_rev = false;
                        }
                        self.apply_view_state();
                    }
                    hit.on_hover_text(t_with(
                        "按{col_label}排序(再点一次反向)",
                        &[("col_label", col_label.to_string())],
                    ));
                }
            });
            ui.separator();
            // —— 行(文本为主,不建缩略图;选中高亮 + 悬停变色)——
            for it in &items {
                let selected = self.sel.contains(&it.id);
                let total_w = name_w + date_w + size_w + star_w + 16.0;
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(total_w, row_h), egui::Sense::hover());
                self.note_frame_dir(&it.dir);
                let hovered = ui.rect_contains_pointer(rect);
                let fill = if selected || hovered {
                    crate::theme::p().card_hover
                } else {
                    egui::Color32::TRANSPARENT
                };
                if fill != egui::Color32::TRANSPARENT {
                    ui.painter().rect_filled(rect, 4.0, fill);
                }
                if selected {
                    ui.painter().rect_stroke(
                        rect,
                        4.0,
                        egui::Stroke::new(1.5_f32, accent),
                        egui::StrokeKind::Inside,
                    );
                }
                let name_text = if it.movie {
                    format!("▶ {}", it.name)
                } else {
                    it.name.clone()
                };
                let name_rect =
                    egui::Rect::from_min_size(rect.left_top(), egui::vec2(name_w, row_h))
                        .translate(egui::vec2(8.0, 0.0));
                // 名称手排 galley:ui.put 套的是 centered_and_justified 布局,
                // 短名字会在列内居中,Label::halign 管不到那一层。
                let galley = ui.fonts_mut(|f| {
                    f.layout_job(egui::text::LayoutJob {
                        wrap: egui::text::TextWrapping {
                            max_width: name_w,
                            max_rows: 1,
                            break_anywhere: true,
                            ..Default::default()
                        },
                        ..egui::text::LayoutJob::single_section(
                            name_text.clone(),
                            egui::TextFormat {
                                font_id: egui::FontId::proportional(12.5),
                                color: text,
                                ..Default::default()
                            },
                        )
                    })
                });
                ui.painter().galley(
                    egui::pos2(name_rect.left(), rect.center().y - galley.size().y / 2.0),
                    galley,
                    text,
                );
                ui.interact(
                    name_rect,
                    egui::Id::new(("detail-name", it.id)),
                    egui::Sense::hover(),
                )
                .on_hover_text(&it.dir);
                let date_c = egui::pos2(rect.left() + 8.0 + name_w + 8.0, rect.center().y);
                ui.painter().text(
                    date_c,
                    egui::Align2::LEFT_CENTER,
                    if it.created.is_empty() {
                        "—"
                    } else {
                        &it.created
                    },
                    egui::FontId::proportional(12.0),
                    if it.created.is_empty() { faint } else { muted },
                );
                let size_r = egui::pos2(
                    rect.left() + 8.0 + name_w + 8.0 + date_w + size_w,
                    rect.center().y,
                );
                ui.painter().text(
                    size_r,
                    egui::Align2::RIGHT_CENTER,
                    &it.size,
                    egui::FontId::proportional(12.0),
                    muted,
                );
                if it.star {
                    ui.painter().text(
                        egui::pos2(rect.right() - star_w / 2.0, rect.center().y),
                        egui::Align2::CENTER_CENTER,
                        "★",
                        egui::FontId::proportional(13.0),
                        accent,
                    );
                }
                // —— 交互(与 photo_cell 同一套语义)——
                let hit = ui.interact(
                    rect,
                    egui::Id::new(("detail-hit", it.id)),
                    egui::Sense::click(),
                );
                let mut star_clicked = false;
                let mut preview_clicked = false;
                let t = CtxPhoto {
                    id: it.id,
                    path: self.rows[it.idx].path.clone(),
                    parent: it.dir.clone(),
                    movie: it.movie,
                    star0: it.star,
                    selected,
                };
                hit.context_menu(|ui| {
                    self.photo_ctx_menu(ui, &t, &mut star_clicked, &mut preview_clicked)
                });
                if hit.clicked() {
                    // 单击只是选中,与网格一致;缺缓存的行由渲染帧按张排队,
                    // 不再因为派生状态去做「纳入目录」这一步。
                    // 点选语义与网格一致:单击只选、Ctrl 加减选、Shift 连选。
                    let (ctrl, shift) =
                        ui.input(|i| (i.modifiers.ctrl || i.modifiers.command, i.modifiers.shift));
                    self.select_by_click(it.id, ctrl, shift);
                }
                if hit.double_clicked() {
                    self.open_photo_editor(it.id, it.movie);
                }
                if star_clicked {
                    self.toggle_star(it.id);
                }
                if preview_clicked {
                    self.open_photo_editor(it.id, it.movie);
                }
            }
        });
    }

    /// 照片网格(行式排布:同行等高、各格按宽高比取宽、行尾铺满)。
    ///
    /// 缩略图档位(4 档常用值)决定基准行高。
    /// 行式布局的行高会随铺满系数浮动,所以先把整批的行算出来再按行虚拟化。
    fn photo_grid(&mut self, ui: &mut egui::Ui, salt: &str, cells: &[usize]) {
        // 显示优先:先把屏幕上排队等纳入的目录消化掉几个(写账 + 放行 + 交给后台优先派生),
        // 再画格子。渲染帧不一次写几十个目录的账,所以有上限。
        self.drain_adopts(MAX_ADOPT_PER_FRAME);
        let base = self.thumb_tier.size() as f32;
        let gap = 14.0;
        let avail = ui.available_width();
        let layout = thumb_rows(&self.rows, cells, avail, base, gap);
        // 行级虚拟化:视口外的行不建 widget,只按该行确定的高度占位。
        // 占位记法:每行连它后面的那个间距一起记(行高 + gap),这样后续行
        // 依旧落在原来的位置;若占位段结尾就是整个网格,得多退一次间距,
        // 否则内容总高会比真实值多出一个 gap。
        let clip = ui.clip_rect();
        // 预取带 —— 视口上下各留这么高的一段,里面的行不建 widget,
        // 但会用当帧**剩余**的解码配额把纹理先暖进缓存(滚动时少闪一帧占位块)。
        let band = (base + 8.0 + gap) * PREFETCH_ROWS as f32;
        let ctx = ui.ctx().clone();
        ui.push_id(salt, |ui| {
            ui.spacing_mut().item_spacing.y = gap;
            // 逐行维护虚拟坐标:跳过的行不建 widget,光标不会前移,
            // 若继续用 `ui.cursor()` 判视口,滚动后光标始终停在本网格顶部,
            // 整片网格都会被判成"在视口上方"而不画(滚着滚着就全空)。
            let mut y = ui.cursor().top();
            let mut hidden = 0.0f32;
            for row in &layout {
                let cap = if self.thumb_captions
                    && row
                        .cells
                        .iter()
                        .any(|&(i, _)| !self.rows[i].caption.trim().is_empty())
                {
                    20.0
                } else {
                    0.0
                };
                let row_h = row.h + 8.0 + cap;
                if y + row_h < clip.min.y || y > clip.max.y {
                    // 整行在视口外:记进占位高度,这一帧不建行。
                    let near = y + row_h + band >= clip.min.y && y <= clip.max.y + band;
                    hidden += row_h + gap;
                    y += row_h + gap;
                    if near {
                        let ids: Vec<usize> = row.cells.iter().map(|&(i, _)| i).collect();
                        self.prefetch_row(&ctx, &ids);
                    }
                    continue;
                }
                if hidden > 0.0 {
                    ui.add_space(hidden);
                    hidden = 0.0;
                }
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for &(i, w) in &row.cells {
                        self.photo_cell(ui, i, w, row.h);
                    }
                });
                y += row_h + gap;
            }
            if hidden > 0.0 {
                ui.add_space(hidden - gap);
            }
        });
    }

    /// 预取一行的缩略图纹理。
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
            // 已立案的坏文件不吃配额;其余行一律照常读缓存。
            if self.rows[i].rich_state == crate::db::RICH_BAD {
                continue;
            }
            let id = self.rows[i].id;
            if self.texs.has(id) || self.texs.is_pending(id) {
                continue;
            }
            self.thumb_decode_count += self.thumb_tier.bytes();
            match thumb_tex(ctx, &self.store, id, self.thumb_tier) {
                Some(t) => self.texs.put(id, t),
                // 磁盘上还没有 → 也按张放行,交给后台生成(与可见格子同一套语义)。
                None => {
                    if self.note_missing_photo(id) {
                        ctx.request_repaint();
                    }
                    self.texs.mark_pending(id);
                }
            }
        }
    }

    /// 照片右键上下文菜单(常用项)。
    fn photo_ctx_menu(
        &mut self,
        ui: &mut egui::Ui,
        t: &CtxPhoto,
        star_clicked: &mut bool,
        preview_clicked: &mut bool,
    ) {
        if ui
            .button(if t.movie {
                crate::i18n::t("播放")
            } else {
                crate::i18n::t("预览")
            })
            .clicked()
        {
            *preview_clicked = true;
            ui.close();
        }
        let target = if t.star0 { 0 } else { 1 };
        let batch = t.selected && self.sel.len() > 1;
        let label = if batch {
            if target == 1 {
                t_with(
                    "给选中的 {p1} 张加星标",
                    &[("p1", (self.sel.len()).to_string())],
                )
            } else {
                t_with(
                    "给选中的 {p1} 张取消星标",
                    &[("p1", (self.sel.len()).to_string())],
                )
            }
        } else if target == 1 {
            crate::i18n::t("加星标").to_string()
        } else {
            crate::i18n::t("取消星标").to_string()
        };
        if ui.button(label).clicked() {
            if batch {
                self.star_selection(target);
            } else {
                *star_clicked = true;
            }
            ui.close();
        }
        if ui.button(crate::i18n::t("在资源管理器中显示")).clicked() {
            show_in_explorer(&t.path);
            ui.close();
        }
        if ui.button(crate::i18n::t("批量加标签…")).clicked() {
            self.tag_batch_ids = if t.selected && !self.sel.is_empty() {
                self.sel.iter().copied().collect()
            } else {
                vec![t.id]
            };
            self.tag_batch_input.clear();
            self.tag_batch_open = true;
            ui.close();
        }
        // 以图搜图(视频没有语义向量,不给入口)。
        #[cfg(feature = "ai")]
        if !t.movie && ui.button(crate::i18n::t("搜相似照片")).clicked() {
            self.start_image_search(t.id);
            ui.close();
        }
        // 加入相簿(相簿不移动文件;已加入的打勾,点击移除)。
        ui.menu_button(crate::i18n::t("加入相簿…"), |ui| {
            let albums = self.store.albums();
            if albums.is_empty() {
                ui.label(crate::i18n::t("(还没有相簿:视图 > 相簿 里新建)"));
            }
            let member: Vec<i64> = self
                .store
                .albums_of_photo(t.id)
                .into_iter()
                .map(|(a, _)| a)
                .collect();
            for (aid, name, _) in albums {
                let in_album = member.contains(&aid);
                let label = if in_album {
                    t_with("✓ 「{name}」(点击移出)", &[("name", name.to_string())])
                } else {
                    format!("「{name}」")
                };
                if ui.button(label).clicked() {
                    if in_album {
                        let _ = self.store.album_remove_photo(aid, t.id);
                        self.fx_msg = t_with("已移出相簿「{name}」", &[("name", name.to_string())]);
                    } else {
                        let _ = self.store.album_add_photo(aid, t.id);
                        self.fx_msg = t_with("已加入相簿「{name}」", &[("name", name.to_string())]);
                    }
                    ui.close();
                }
            }
        });
        if self.view == ViewKind::Albums {
            if let Some(a) = self.sel_album {
                if ui.button(crate::i18n::t("从相簿移除")).clicked() {
                    let _ = self.store.album_remove_photo(a, t.id);
                    ui.close();
                }
            }
        }
        ui.separator();
        // 用编辑器打开。
        if ui.button(crate::i18n::t("用编辑器打开")).clicked() {
            let p = PathBuf::from(&t.path);
            if !crate::win::shell_edit(&p) {
                self.status = t_with(
                    "无法打开(未关联编辑器):{p1}",
                    &[("p1", (p.display()).to_string())],
                );
            }
            ui.close();
        }
        // 设为桌面壁纸。
        if ui.button(crate::i18n::t("设为桌面壁纸")).clicked() {
            self.set_wallpaper(&t.path);
            ui.close();
        }
        ui.separator();
        if ui.button(crate::i18n::t("从图库中移除…")).clicked() {
            let id = t.id;
            self.sel.clear();
            self.sel.insert(id);
            self.confirm_remove = true;
            ui.close();
        }
        // 软排除目录的显式出口(AppData 也要能访问)。
        //
        // 「被动滚到过 / 点过一张图」不写账本,但用户亲手右键选这项 = 表态,
        // 与导入页同一档(`imported` + `import`)。硬层(回收站、自家缓存)
        // 这里不给按钮 —— 给了也是点不开的死键。
        let parent = t.parent.clone();
        if !self.store.adoptable_dir(&parent, false) && self.store.adoptable_dir(&parent, true) {
            if ui
                .button(crate::i18n::t("纳入此目录(覆盖排除名单)"))
                .clicked()
            {
                let ok =
                    self.include_dir(&parent, crate::db::INTENT_IMPORTED, crate::db::SRC_IMPORT);
                if ok {
                    self.publish_visible(&parent);
                }
                self.status = if ok {
                    t_with(
                        "已纳入 {parent}:该目录的照片正在生成缩略图",
                        &[("parent", parent.to_string())],
                    )
                } else {
                    t_with(
                        "{parent} 仍不能纳入(硬排除名单)",
                        &[("parent", parent.to_string())],
                    )
                };
                ui.close();
            }
        }
        // 坏文件出口(对齐经典图库坏图清单的
        // Hide / Don't Hide):立过案的文件不再每轮重试,这里给两个方向的开关。
        let p = t.path.clone();
        if self.store.is_bad_media(&p) {
            if ui
                .button(crate::i18n::t("不隐藏此文件(重试缩略图)"))
                .clicked()
            {
                self.status = match self.store.unmark_bad_media(&p) {
                    Ok(()) => t_with("已销案,重新排队派生:{p}", &[("p", p.to_string())]),
                    Err(e) => t_with("销案失败: {e}", &[("e", e.to_string())]),
                };
                ui.close();
            }
        } else if ui
            .button(crate::i18n::t("隐藏此文件(不再重试缩略图)"))
            .clicked()
        {
            self.status = match self
                .store
                .mark_bad_media(&p, crate::i18n::t("用户手动隐藏"))
            {
                Ok(()) => t_with("已隐藏,不再重试:{p}", &[("p", p.to_string())]),
                Err(e) => t_with("隐藏失败: {e}", &[("e", e.to_string())]),
            };
            ui.close();
        }
        // 属性(置于末项;未开预览时属性窗取选中项)。
        if ui.button(crate::i18n::t("属性…")).clicked() {
            let id = t.id;
            self.sel.clear();
            self.sel.insert(id);
            self.show_props = true;
            ui.close();
        }
    }

    fn photo_cell(&mut self, ui: &mut egui::Ui, idx: usize, w: f32, h: f32) {
        let id = self.rows[idx].id;
        let star0 = self.rows[idx].star != 0;
        let selected = self.sel.contains(&id);
        // 显示与派生解耦:派生状态只是后台排队进度,不是「能不能看图」的开关。
        // 真排除的目录根本不纳入图库,库里有的行一律照常读缓存;只有已立案的坏文件
        // 不读(它确实解不开)。缓存不在磁盘上时才把这一张推去后台生成。
        let rich = self.rows[idx].rich_state;
        let bad = rich == crate::db::RICH_BAD;
        // 屏幕上出现过的目录整批发给后台当第一优先,用户看哪儿先补哪儿。
        let dir = self.rows[idx].parent.clone();
        self.note_frame_dir(&dir);
        let tex = if bad {
            None
        } else {
            match self.texs.get(id) {
                Some(t) => Some(t),
                None if self.texs.is_pending(id) => None, // 后台正在生成,先占位
                None => {
                    // 滚动限速 —— 本帧解码配额已用尽时,跳过同步解码,
                    // 只画灰色占位块(下一帧再补)。滚动越快配额越小 → 滚动不掉帧。
                    // 注意:此处**不能** mark_pending(那是"磁盘上还没有、交给后台生成"
                    // 的语义),否则已落盘的缩略图会被误判为待生成而不再加载。
                    if self.thumb_decode_count >= self.thumb_decode_budget {
                        None
                    } else {
                        self.thumb_decode_count += self.thumb_tier.bytes();
                        // 只从落盘的该档缩略图读(绝不重解码原图)。
                        match thumb_tex(ui.ctx(), &self.store, id, self.thumb_tier) {
                            Some(t) => {
                                self.texs.put(id, t.clone());
                                Some(t)
                            }
                            None => {
                                // 磁盘上确实没有这一档缓存 → 只放行这一张,
                                // 由后台生成;本帧先显示占位块。
                                if self.note_missing_photo(id) {
                                    ui.ctx().request_repaint();
                                }
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
        // 缩略图说明(视图开关)。
        let caption: Option<String> = if self.thumb_captions {
            let cap = self.rows[idx].caption.trim();
            if cap.is_empty() {
                None
            } else {
                let max_chars = ((w / 7.0) as usize).max(6);
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
        let cap_h = if caption.is_some() { 20.0 } else { 0.0 };
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(w + pad * 2.0, h + pad * 2.0 + cap_h),
            egui::Sense::hover(),
        );
        // 先取悬停态再登记图片控件,否则图片会盖住整卡的命中区域。
        let hovered = ui.rect_contains_pointer(rect);
        let card_fill = if hovered {
            crate::theme::p().card_hover
        } else {
            crate::theme::p().card
        };
        ui.painter()
            .rect_filled(rect, crate::theme::CARD_RADIUS, card_fill);
        let img_rect = egui::Rect::from_min_size(rect.min + egui::vec2(pad, pad), egui::vec2(w, h));
        // 保留照片本身的宽高比:等比缩进到格子里,不居中裁切。
        // 缺档时 fit_rect 就是整格,占位块仍铺满格子。
        let fit_rect = match &tex {
            Some(t) => {
                let (tw, th) = (t.size()[0].max(1) as f32, t.size()[1].max(1) as f32);
                let ar = tw / th;
                let size = if ar > w / h {
                    egui::vec2(w, w / ar)
                } else {
                    egui::vec2(h * ar, h)
                };
                let c = img_rect.center();
                let half = size * 0.5;
                egui::Rect::from_min_max(c - half, c + half)
            }
            None => img_rect,
        };
        if let Some(ref t) = tex {
            ui.put(
                fit_rect,
                egui::Image::new((t.id(), fit_rect.size()))
                    .corner_radius(crate::theme::IMAGE_RADIUS),
            );
        } else {
            ui.painter().rect_filled(
                img_rect,
                crate::theme::IMAGE_RADIUS,
                crate::theme::p().placeholder,
            );
            // 占位只有两种:真的读不出来,以及还在准备。派生进度是内部实现,
            // 不印在格子上给用户看。
            let ph = if rich == crate::db::RICH_BAD {
                t("读取失败")
            } else {
                t("载入中…")
            };
            ui.painter().text(
                img_rect.center(),
                egui::Align2::CENTER_CENTER,
                ph,
                egui::FontId::proportional(11.0),
                crate::theme::p().faint,
            );
        }
        // 视频条目左上角 ▶ 徽标(贴着实际图片边界,不贴卡片留白)。
        if self.rows[idx].kind == crate::video::kind::MOVIE {
            ui.painter().text(
                fit_rect.left_top() + egui::vec2(6.0, 5.0),
                egui::Align2::LEFT_TOP,
                "▶",
                egui::FontId::proportional(15.0),
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 235),
            );
        }
        // 星标悬浮在缩略图右上角(常驻显示已加星,悬停才露出空心星)。
        let hit = ui.interact(rect, egui::Id::new(("thumb-hit", id)), egui::Sense::click());
        // 坏文件格给诊断与销案出口;其余占位只是「还在准备」,不必解释内部状态。
        let hit = if bad {
            hit.on_hover_text(t("这张照片读取失败(文件损坏或无权限),已记入坏文件台账并隔离,不再重试。在「工具 → 图库维护 → 坏文件台账…」里可以看明细或整批销案。"))
        } else {
            hit
        };
        thumb_star(ui, id, star0, fit_rect, hovered, &mut star_clicked);
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
            // 单击只是选中。缺缓存的那一格渲染时已按张排进后台队列,
            // 不需要再靠点击去「纳入目录」,也没有点不动的情况。
            // 点选语义:单击只选一张,Ctrl 加减选,Shift 连选;
            // 双击才进单图视图(见下方 double_clicked)。
            let (ctrl, shift) =
                ui.input(|i| (i.modifiers.ctrl || i.modifiers.command, i.modifiers.shift));
            self.select_by_click(id, ctrl, shift);
        }
        if hit.double_clicked() {
            let movie = self.rows[idx].kind == crate::video::kind::MOVIE;
            self.open_photo_editor(id, movie);
        }
        hit.context_menu(|ui| {
            let t = CtxPhoto {
                id,
                path: self.rows[idx].path.clone(),
                parent: self.rows[idx].parent.clone(),
                movie: self.rows[idx].kind == crate::video::kind::MOVIE,
                star0,
                selected,
            };
            self.photo_ctx_menu(ui, &t, &mut star_clicked, &mut preview_clicked)
        });
        if star_clicked {
            self.toggle_star(id);
        }
        if preview_clicked {
            let movie = self.rows[idx].kind == crate::video::kind::MOVIE;
            self.open_photo_editor(id, movie);
        }
    }

    /// 只读速看预览格式(GIF / 动图 WebP / PSD / PSB)的公共判定。
    /// 编辑、旋转、烘培、效果面板都以此为准,避免各处口径不一致。
    pub(super) fn readonly_media(&self, id: i64) -> bool {
        self.photo_path(id)
            .map(|p| crate::scan::is_readonly_media(Path::new(&p)))
            .unwrap_or(false)
    }

    /// 只读格式在界面上给的说明(没有则 None)。
    pub(super) fn readonly_hint(&self, id: i64) -> Option<&'static str> {
        let p = self.photo_path(id)?;
        let ext = Path::new(&p)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)?;
        match ext.as_str() {
            "gif" => Some(t("GIF 只读速看:逐帧播放,不保存、不旋转、不编辑原文件")),
            "webp" if self.readonly_media(id) => Some(t(
                "动图 WebP 只读速看:逐帧播放,不保存、不旋转、不编辑原文件",
            )),
            "psd" | "psb" => Some(t(
                "PSD/PSB 只读速看:显示合并合成图,可能与图层结果不同;不保存、不旋转、不编辑",
            )),
            _ => None,
        }
    }

    /// 发起动图帧序列的后台解码;非动图路径直接跳过。
    fn anim_request(&mut self, id: i64) {
        let Some(path) = self.photo_path(id) else {
            return;
        };
        let p = PathBuf::from(path);
        if !crate::anim::maybe_animated(&p) || !p.exists() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.anim_rx = Some(rx);
        std::thread::spawn(move || {
            let clip = crate::anim::decode_clip(&p);
            let _ = tx.send((id, clip));
        });
    }

    /// 换图 / 关预览时释放帧序列(一张动图可能占几十 MB)。
    fn anim_reset(&mut self) {
        self.anim = None;
        self.anim_rx = None;
        self.anim_req_for = None;
    }

    /// 收后台解码结果;换图后的滞后结果直接丢弃。
    fn anim_pump(&mut self) {
        let got = match &self.anim_rx {
            Some(rx) => match rx.try_recv() {
                Ok(v) => Some(v),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some((0, None)),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
            },
            None => None,
        };
        if let Some((rid, clip)) = got {
            self.anim_rx = None;
            if Some(rid) == self.preview {
                if let Some(clip) = clip {
                    self.anim = Some(AnimState {
                        id: rid,
                        clip,
                        tex: None,
                        playing: true,
                        elapsed_ms: 0,
                        tick: std::time::Instant::now(),
                        frame: usize::MAX,
                    });
                    // 静态预览纹理不再需要,避免同时占两份显存。
                    self.preview_tex = None;
                }
            }
        }
    }

    /// 按帧延迟推进时间轴并就地更新同一张 GPU 纹理(不每帧新建句柄)。
    fn anim_tick(&mut self, ctx: &egui::Context) -> Option<egui::TextureHandle> {
        let now = std::time::Instant::now();
        let st = self.anim.as_mut()?;
        if st.playing {
            st.elapsed_ms = st
                .elapsed_ms
                .saturating_add(u64::try_from(st.tick.elapsed().as_millis()).unwrap_or(u64::MAX));
        }
        st.tick = now;
        let last = st.clip.len().saturating_sub(1);
        let (idx, remain) = match st.clip.frame_index_at(st.elapsed_ms) {
            Some(v) => v,
            None => {
                st.playing = false;
                (last, 0)
            }
        };
        if idx != st.frame || st.tex.is_none() {
            st.frame = idx;
            let f = &st.clip.frames[idx];
            let size = [f.rgba.width() as usize, f.rgba.height() as usize];
            let ci = egui::ColorImage::from_rgba_unmultiplied(size, f.rgba.as_raw());
            match &mut st.tex {
                Some(t) => t.set(ci, egui::TextureOptions::default()),
                None => {
                    st.tex = Some(ctx.load_texture(
                        format!("anim-{}", st.id),
                        ci,
                        egui::TextureOptions::default(),
                    ));
                }
            }
        }
        if st.playing {
            ctx.request_repaint_after(std::time::Duration::from_millis(remain.max(16)));
        }
        st.tex.clone()
    }

    /// 顶栏播放/暂停按钮:只在有帧序列时才有意义。
    pub(super) fn anim_toggle(&mut self) {
        if let Some(st) = &mut self.anim {
            st.playing = !st.playing;
            st.tick = std::time::Instant::now();
        }
    }

    /// 顶栏的帧进度文案(没有动图时 None)。
    pub(super) fn anim_status(&self, id: i64) -> Option<String> {
        let st = self.anim.as_ref()?;
        if st.id != id {
            return None;
        }
        Some(t_with(
            "动图 {p1}/{p2} 帧 · {p3}",
            &[
                ("p1", (st.frame.min(st.clip.len() - 1) + 1).to_string()),
                ("p2", (st.clip.len()).to_string()),
                (
                    "p3",
                    (if st.playing {
                        t("播放中")
                    } else {
                        t("已暂停")
                    })
                    .to_string(),
                ),
            ],
        ))
    }

    pub(super) fn anim_playing(&self, id: i64) -> bool {
        self.anim
            .as_ref()
            .map(|a| a.id == id && a.playing)
            .unwrap_or(false)
    }

    /// 预览大图纹理(最长边 ≤ 1024;首次打开同步加载一次,按 EXIF 方向摆正)。
    ///
    /// 相机 RAW 由 `thumbs::load_for_preview` 借 Windows Shell 缩略图扩展解码,
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
        // 动图:帧序列解出来后按延迟轮播;解码期间先沿用常规静态首帧。
        self.anim_pump();
        if self.anim.as_ref().map(|a| a.id) == Some(id) {
            return self.anim_tick(ctx);
        }
        if self.anim_req_for != Some(id) {
            self.anim = None;
            self.anim_rx = None;
            self.anim_req_for = Some(id);
            self.anim_request(id);
        }
        // 预览渲染要叠加该照片的效果栈(op 树实时渲染)。
        // 签名 = 照片 id + 效果栈序列化;变了才重算纹理。
        let stack = self.fx_preview_stack(id);
        let sig = format!("{id}|{}", stack.serialize());
        if self.preview_tex.as_ref().map(|(k, _)| *k) != Some(id)
            || self.fx_applied_sig.as_deref() != Some(sig.as_str())
        {
            // 预览**不阻塞 UI**。
            // 1) 先查落盘缓存;
            // 2) 没有 → 发请求给后台 worker(后台解码 + 落盘),本帧先不显示;
            // 3) worker 完成后回报,UI 再加载纹理。
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
                // 在解码后的像素上套用效果栈(op 树在此位置执行)。
                let mut rgba = img.to_rgba8();
                crate::effects::apply_stack(&mut rgba, &stack);
                // 直方图跟着效果栈走:只在纹理重算时统计一次(编辑视图左栏画)。
                self.editor_hist = Some((id, sig.clone(), Histogram::of(&rgba)));
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
                        out.push((r, t("人脸").to_string()));
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

    /// 「点上级 = 显示子树全部照片」的路径前缀判定。
    #[test]
    fn path_under_dir_matches_subtree_only() {
        assert!(path_under_dir(r"D:\pics", r"D:\pics"));
        assert!(path_under_dir(r"D:\pics\a.jpg", r"D:\pics"));
        assert!(path_under_dir(r"D:\pics\2024\jan\x.jpg", r"D:\pics"));
        assert!(path_under_dir(r"D:\pics\", r"D:\pics"));
        // 前缀撞车不算子树:photos2 不是 photos 的孩子。
        assert!(!path_under_dir(r"D:\photos2\a.jpg", r"D:\photos"));
        assert!(!path_under_dir(r"D:\pic\a.jpg", r"D:\pics"));
        assert!(!path_under_dir(r"D:\other\a.jpg", r"D:\pics"));
        // 盘根本身也是合法子树根(D:\ 下的全部)。
        assert!(path_under_dir(r"D:\anything.jpg", r"D:\"));
        // 修字节边界闪退:目录名长度撞上别的路径里的汉字。
        // 「D:\我的资料库」15 字节,历史照片路径里某汉字被第 15 字节切开。
        assert!(path_under_dir(r"D:\我的资料库\a.jpg", r"D:\我的资料库"));
        assert!(!path_under_dir(r"D:\我的资料库2\a.jpg", r"D:\我的资料库"));
        assert!(!path_under_dir(r"D:\照片库\a.jpg", r"D:\我的资料库"));
        assert!(!path_under_dir(r"D:\我的资料\库\a.jpg", r"D:\我的资料库"));
        assert!(!path_under_dir(r"D:\我", r"D:\我的资料库"));
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

    /// 标签视图的选择/导出范围必须跟着当前筛选走:库内检索 > AI 分类 > 手动标签。
    #[test]
    #[cfg(feature = "ai")]
    fn tags_scope_matches_visible_filter() {
        let store = Store::open_mem().unwrap();
        crate::ai_tags::seed_labels(&store).unwrap();
        for id in 1..=3i64 {
            store
                .write(|c| {
                    c.execute(
                        "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,'.',0,0)",
                        rusqlite::params![id, format!("p{id}.jpg")],
                    )
                })
                .unwrap();
        }
        let ver = crate::ai_queue::stage_version(&store, crate::ai_queue::Stage::Tags);
        let cat = store.read(|c| {
            c.query_row(
                "SELECT id FROM tag_labels WHERE node='动物/猫'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        });
        crate::ai_tags::save(&store, 2, &ver, &[(cat, 0.9)]).unwrap();
        timeline::add_tag(&store, 3, "旅行").unwrap();

        assert!(tags_scope_ids(&store, "", &[], None, None).is_empty());
        assert_eq!(tags_scope_ids(&store, "", &[], None, Some("旅行")), vec![3]);
        // 选了分类就按分类取,残留的手动标签选择不参与。
        assert_eq!(
            tags_scope_ids(&store, "", &[], Some("动物"), Some("旅行")),
            vec![2]
        );
        // 检索词非空时优先级最高,直接用命中集。
        assert_eq!(
            tags_scope_ids(&store, "猫", &[1, 2], Some("动物"), Some("旅行")),
            vec![1, 2]
        );
        // 纯空白不算检索,回落到分类。
        assert_eq!(
            tags_scope_ids(&store, "   ", &[1], Some("动物"), None),
            vec![2]
        );
    }

    /// 标签列表只渲染一份:每个真实标签恰好一行,层级组节点不占标签名额。
    #[test]
    fn tag_tree_rows_lists_each_tag_once() {
        let tags = vec![
            timeline::TagInfo {
                name: "旅行/2024".into(),
                color: None,
                count: 2,
            },
            timeline::TagInfo {
                name: "旅行/2025".into(),
                color: None,
                count: 3,
            },
            timeline::TagInfo {
                name: "猫".into(),
                color: None,
                count: 5,
            },
        ];
        let rows = tag_tree_rows(&tags);
        let mut paths: Vec<&str> = rows.iter().map(|r| r.2.as_str()).collect();
        let n = paths.len();
        paths.sort_unstable();
        paths.dedup();
        assert_eq!(paths.len(), n, "同一路径多行就会在界面上重复: {rows:?}");
        let real: Vec<(&str, usize)> = rows
            .iter()
            .filter(|r| r.4)
            .map(|r| (r.2.as_str(), r.3))
            .collect();
        assert_eq!(real.len(), tags.len(), "真实标签行数应等于标签数");
        for t in &tags {
            let hit = real.iter().find(|(p, _)| *p == t.name.as_str());
            assert_eq!(hit.map(|(_, c)| *c), Some(t.count), "标签 {} 的行不对", t.name);
        }
        let group = rows.iter().find(|r| r.2 == "旅行").expect("应有层级组节点");
        assert!(!group.4, "组节点不是真实标签");
        assert_eq!(group.0, 0);
        assert_eq!(
            rows.iter().find(|r| r.2 == "旅行/2024").unwrap().0,
            1,
            "子标签要缩进一层"
        );
    }

    /// 无窗口跑一帧:同一 Context 连续多帧,才能模拟真实的按下与抬起。
    #[cfg(feature = "ai")]
    fn ui_frame(
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        mut f: impl FnMut(&mut egui::Ui),
    ) -> egui::FullOutput {
        let input = egui::RawInput {
            events,
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 200.0),
            )),
            ..Default::default()
        };
        ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| f(ui));
        })
    }

    #[cfg(feature = "ai")]
    fn pointer_events(pos: egui::Pos2, pressed: bool) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            },
        ]
    }

    /// 两个筛选开关都必须把点击报成「变了」,否则右侧人脸列表不会重算。
    #[test]
    #[cfg(feature = "ai")]
    fn face_filter_row_reports_both_toggles() {
        let ctx = egui::Context::default();
        // (隐藏低质量, 显示已忽略, 本帧是否有变化, 两个开关的点击区)
        let st = std::cell::RefCell::new((false, false, false, Vec::<egui::Rect>::new()));
        let mut draw = |ui: &mut egui::Ui| {
            let mut s = st.borrow_mut();
            let (hide, ignored, changed, rects) = &mut *s;
            ui.horizontal(|ui| {
                let (a, b) = face_filter_row(ui, hide, ignored);
                *changed = a.changed() || b.changed();
                *rects = vec![a.rect, b.rect];
            });
        };
        ui_frame(&ctx, vec![], &mut draw);
        let rects = st.borrow().3.clone();
        assert!(!st.borrow().2, "没点就不该报变化");
        assert_eq!(rects.len(), 2);
        assert!(
            rects[0].width() > 0.0 && rects[1].width() > 0.0,
            "开关应有可点区域: {rects:?}"
        );
        assert!(
            rects[1].left() >= rects[0].right(),
            "两个开关不应重叠: {rects:?}"
        );

        for (i, want) in [(0usize, true), (1usize, false)] {
            let pos = rects[i].center();
            ui_frame(&ctx, pointer_events(pos, true), &mut draw);
            ui_frame(&ctx, pointer_events(pos, false), &mut draw);
            let s = st.borrow();
            assert!(s.2, "点第 {} 个开关必须报变化", i + 1);
            assert_eq!(s.0, want, "「隐藏低质量」状态不对");
            assert_eq!(s.1, !want, "「显示已忽略」状态不对");
            drop(s);
            // 再点一次应翻回去,证明不是只生效一次。
            ui_frame(&ctx, pointer_events(pos, true), &mut draw);
            ui_frame(&ctx, pointer_events(pos, false), &mut draw);
            let s = st.borrow();
            assert!(s.2, "回点也要报变化");
            assert_eq!((s.0, s.1), (false, false), "回点后应复位");
        }
        // 空帧不该报变化。
        ui_frame(&ctx, vec![], &mut draw);
        assert!(!st.borrow().2, "无输入的帧不该报变化");
    }

    /// 「隐藏低质量」的门槛要真能挡掉低质量脸,且不误伤高质量脸。
    #[test]
    #[cfg(feature = "ai")]
    fn hide_low_quality_filters_face_list() {
        let store = Store::open_mem().unwrap();
        store.write(|c| {
            c.execute(
                "INSERT INTO photos(id,path,parent,mtime,size) VALUES(1,'a.jpg','.',0,0)",
                [],
            )
            .unwrap();
            for (id, q) in [(1i64, 0.9f64), (2, 0.2)] {
                c.execute(
                    "INSERT INTO faces(id,photo_id,x1,y1,x2,y2,cluster,score,quality,ignored)
                     VALUES(?1,1,0,0,1,1,7,0.9,?2,0)",
                    rusqlite::params![id, q],
                )
                .unwrap();
            }
        });
        assert_eq!(crate::faces::faces_for_cluster(&store, 7, false, 0.0).len(), 2);
        let hi = crate::faces::faces_for_cluster(&store, 7, false, FACE_LOW_QUALITY);
        assert_eq!(hi.len(), 1, "低质量脸应被挡掉");
        assert_eq!(hi[0].face_id, 1);
        assert!(hi[0].quality >= FACE_LOW_QUALITY);
    }

    /// 两个破坏性入口都要红字警示,并把具体动作名写进警示里。
    #[test]
    #[cfg(feature = "ai")]
    fn face_wipe_warning_paints_red_with_action() {
        let ctx = egui::Context::default();
        for action in ["清除并重新扫描", "切换并重建"] {
            let out = ui_frame(&ctx, vec![], |ui| {
                face_wipe_warning(ui, action);
            });
            let mut found = false;
            for clipped in &out.shapes {
                if let egui::Shape::Text(ts) = &clipped.shape {
                    let text = ts.galley.text();
                    if !text.contains("不可撤销") {
                        continue;
                    }
                    assert!(text.contains(action), "警示应点明动作名: {text}");
                    assert!(
                        ts.galley
                            .job
                            .sections
                            .iter()
                            .any(|s| s.format.color == egui::Color32::LIGHT_RED),
                        "警示必须是红字: {text}"
                    );
                    found = true;
                }
            }
            assert!(found, "「{action}」的不可撤销警示没有画出来");
        }
    }
}

impl eframe::App for App {
    /// 退出时停掉后台缩略图 worker(不留游离线程)。
    fn on_exit(&mut self) {
        // 自动更新后台线程:让它自己收尾,别留一条游离线程被进程硬砍。
        self.updater.stop();
        if let Some(w) = self.thumb_bg.take() {
            w.stop();
        }
        // 预览 worker 也要停(否则后台线程会随进程一起被砍,留下半张图)。
        if let Some(w) = self.preview_bg.take() {
            w.stop();
        }
        // 设备监听线程(隐藏窗口 + 消息循环)也要停。
        if let Some(w) = self.device_watch.take() {
            w.stop();
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 先给整帧铺一层底色:面板之间万一留出空隙,也不会露出窗口的黑色清屏色。
        // 这一层画在最底层,所有面板照常盖在上面。
        ctx.layer_painter(egui::LayerId::background()).rect_filled(
            ctx.viewport_rect(),
            0.0,
            crate::theme::p().bg,
        );
        // 每帧重置缩略图解码配额计数(预算由滚动速度在 CentralPanel 里定)。
        self.thumb_decode_count = 0;
        // 自动更新:空闲门控与后台事件回流放在帧首 —— 幻灯片/编辑视图会提前
        // return,放在后面就收不到「发现新版本」的提示了。
        let app_busy = self.busy();
        self.updater.tick(ctx, app_busy);
        // 人脸模型:后台下载的事件回流 + 缺件提示窗。同放帧首,免得幻灯片/编辑
        // 视图提前 return 时收不到进度。
        #[cfg(feature = "ai")]
        {
            let was_working = self.models.phase.working();
            self.models.tick(ctx);
            if was_working && !self.models.phase.working() {
                if let Some(h) = self.models.phase.status_hint() {
                    self.status = h;
                }
            }
            self.model_prompt_window(ctx);
        }
        // 开机自动续扫:待办>0 且模型就位、非待重建、无在跑扫描时后台接着扫
        // (范围沿用已记住的选择);只查一次,模型缺失不弹下载窗。
        #[cfg(feature = "ai")]
        if !self.face_resume_done {
            self.face_resume_done = true;
            if self.face_rx.is_none() && crate::faces::should_resume(&self.store) {
                self.start_face_scan();
            }
        }
        // 显示优先:上一帧真正画在屏幕上的目录整批发给后台 worker
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
        // 打开预览即把这张提到 AI 队列最前(每张只做一次)。
        #[cfg(feature = "ai")]
        if let Some(id) = self.preview {
            if self.ai_preview_for != Some(id) {
                self.ai_preview_for = Some(id);
                if self.ai_enabled {
                    crate::ai_queue::enqueue_current(&self.store, id);
                }
            }
        }
        // 轮询扫描结果
        if let Some(rx) = &self.scan_rx {
            if let Ok(r) = rx.try_recv() {
                self.scan_rx = None;
                self.scan_ctrl = None;
                #[cfg(feature = "ai")]
                let ai_enqueue =
                    r.err.is_none() && !r.cancelled && r.indexed > 0 && self.ai_enabled;
                self.status = match r.err {
                    Some(e) => t_with(
                        "{p1}{p2}失败: {e}",
                        &[
                            ("p1", (self.scan_verb).to_string()),
                            ("p2", (r.scope).to_string()),
                            ("e", e.to_string()),
                        ],
                    ),
                    None if r.cancelled => t_with(
                        "{p1}{p2}已取消:已索引 {p3} 张(已处理 {p4} 个文件)",
                        &[
                            ("p1", (self.scan_verb).to_string()),
                            ("p2", (r.scope).to_string()),
                            ("p3", (r.indexed).to_string()),
                            ("p4", (r.scanned).to_string()),
                        ],
                    ),
                    None => {
                        let mut msg = t_with("{p1}{p2}完成:发现 {p3} 个文件,索引 {p4} 张(未变跳过 {p5} 张,排除目录 {p6} 个)", &[("p1", (self.scan_verb).to_string()), ("p2", (r.scope).to_string()), ("p3", (r.scanned).to_string()), ("p4", (r.indexed).to_string()), ("p5", (r.unchanged).to_string()), ("p6", (r.excluded_dirs).to_string())]);
                        if r.removed > 0 {
                            msg.push_str(&t_with(
                                ";{p1} 个已消失文件移出图库",
                                &[("p1", (r.removed).to_string())],
                            ));
                        }
                        msg
                    }
                };
                self.rebuild();
                #[cfg(feature = "ai")]
                if ai_enqueue {
                    self.enqueue_all_ai();
                }
            }
        }
        // 设备热插拔事件("置刷新标志 + 触发刷新")。
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
                    self.status = t_with(
                        "检测到新设备({p1}),正在扫描 {p2}",
                        &[
                            ("p1", (ev.label()).to_string()),
                            (
                                "p2",
                                (added
                                    .iter()
                                    .map(|p| p.to_string_lossy().to_string())
                                    .collect::<Vec<_>>()
                                    .join("、"))
                                .to_string(),
                            ),
                        ],
                    );
                    // 设备列表缓存失效(WM_DEVICECHANGE 链路会重建设备列表);
                    // 若导入窗口开着,下次重绘即看到新设备。
                    self.refresh_import_devices();
                    self.start_scan_roots(roots, t("新接入的设备"));
                } else {
                    self.status =
                        t_with("检测到设备变化({p1})", &[("p1", (ev.label()).to_string())]);
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
                // 这批 id 已经生成过一次。把按张去重的记录也清掉:日后
                // 缓存文件被外部清掉时,那一格还能再排一次队补回来。
                for id in &got {
                    self.adopt_seen.remove(id);
                }
                // 库里刚多了一批已生成记录,顺手刷新;不写 status ——
                // 逐批覆盖会让状态栏文字一直跳,进度看「N缓存 x/y」就够了。
                self.refresh_tier_count();
                // 只有 Normal 档回写 thumb_ok,「最近更新」的入组门槛才跟着动。
                if self.thumb_tier == crate::thumbs::Tier::Normal {
                    self.recent_dirty = true;
                }
                // 缩略图就位且索引已启用才排入语义向量阶段。
                #[cfg(feature = "ai")]
                if self.ai_enabled {
                    crate::ai_queue::enqueue_for_ids(&self.store, crate::ai_queue::Stage::Clip, &got);
                }
                ctx.request_repaint();
            }
        }
        // 新缩略图落盘 → 「最近更新」按节流收人(尾批由后台定时重绘兜住)。
        if self.recent_dirty && self.recent_at.elapsed() >= RECENT_REFRESH_GAP {
            self.refresh_recent();
        }
        // 预览后台完成信号 → 清掉预览纹理缓存,让下一帧重新加载。
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
                self.status = t_with(
                    "{failed} 张预览生成失败(格式不支持或文件已移动)",
                    &[("failed", failed.to_string())],
                );
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
                    Ok(()) => t("保存完成:已备份原稿并重写原图").into(),
                    Err(e) => t_with("保存失败: {e}", &[("e", e.to_string())]),
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
                    t_with("导出完成 {ok} 张", &[("ok", ok.to_string())])
                } else {
                    let first: Vec<String> = errs.iter().take(2).cloned().collect();
                    t_with(
                        "导出完成 {ok} 张,失败 {p1} 个: {p2}",
                        &[
                            ("ok", ok.to_string()),
                            ("p1", (errs.len()).to_string()),
                            ("p2", (first.join("; ")).to_string()),
                        ],
                    )
                };
                self.status = self.status.replace('\n', " ");
            }
        }
        if self.export_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        // 邮件/光盘后台任务结果
        if let Some(rx) = &self.share_rx {
            if let Ok(res) = rx.try_recv() {
                self.share_rx = None;
                self.status = match res {
                    Ok(msg) => msg,
                    Err(e) => t_with("分享失败: {e}", &[("e", e.to_string())]),
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
                    Err(e) => t_with("设为壁纸失败: {e}", &[("e", e.to_string())]),
                };
            }
        }
        // 图库维护(备份/压缩/清理缺失)结果轮询
        if let Some(rx) = &self.maint_rx {
            if let Ok(res) = rx.try_recv() {
                self.maint_rx = None;
                self.status = match res {
                    Ok(msg) => msg,
                    Err(e) => t_with("维护失败: {e}", &[("e", e.to_string())]),
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
                            t("OCR 完成:未识别到文本").into()
                        } else {
                            t_with(
                                "OCR 完成:识别到 {p1} 处文本",
                                &[("p1", (self.ocr_results.len()).to_string())],
                            )
                        };
                    }
                    Err(e) => self.status = t_with("OCR 失败: {e}", &[("e", e.to_string())]),
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
                    FaceScan::Progress(done, total) => {
                        self.face_prog = (done, total);
                        self.status = t_with(
                            "正在人脸识别(全库连续扫描,剩余 {remaining} 张)…",
                            &[("remaining", total.saturating_sub(done).to_string())],
                        );
                    }
                    FaceScan::Done(res) => finished = Some(res),
                }
            }
            if let Some(res) = finished {
                self.face_rx = None;
                self.face_cancel = None;
                self.face_prog = (0, 0);
                match res {
                    Ok((np, nf)) => {
                        self.status = t_with(
                            "人脸扫描完成:{np} 张照片,检出 {nf} 张人脸",
                            &[("np", np.to_string()), ("nf", nf.to_string())],
                        )
                    }
                    Err(e) => self.status = t_with("人脸扫描失败: {e}", &[("e", e.to_string())]),
                }
                self.refresh_people_and_suggestions();
            }
        }
        #[cfg(feature = "ai")]
        if self.face_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        // 相似人脸后台检索轮询:查询脸或阈值已变的结果直接丢弃。
        #[cfg(feature = "ai")]
        if let Some(rx) = &self.face_sim_rx {
            if let Ok((qid, thr, hits)) = rx.try_recv() {
                self.face_sim_rx = None;
                if self.face_similar_for == Some(qid)
                    && (thr - self.face_similar_thr).abs() < 1e-6
                {
                    let n = hits.len();
                    self.face_similar = hits;
                    self.status = t_with(
                        "已在全库找到 {n} 个相近人脸",
                        &[("n", n.to_string())],
                    );
                }
                if self.face_sim_pending {
                    self.face_sim_pending = false;
                    if let Some(q) = self.face_similar_for {
                        self.start_face_similar(q);
                    }
                }
            }
        }
        #[cfg(feature = "ai")]
        if self.face_sim_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        // 全量重聚类轮询。
        #[cfg(feature = "ai")]
        if let Some(rx) = &self.recluster_rx {
            if let Ok(res) = rx.try_recv() {
                self.recluster_rx = None;
                match res {
                    Ok(n) => {
                        self.status = t_with(
                            "重新聚类完成:共 {n} 簇",
                            &[("n", n.to_string())],
                        );
                    }
                    Err(e) => {
                        self.status = t_with(
                            "重新聚类失败: {e}",
                            &[("e", e.to_string())],
                        )
                    }
                }
                self.sel_cluster = None;
                self.sel_person = None;
                self.refresh_people_and_suggestions();
            }
        }
        #[cfg(feature = "ai")]
        if self.recluster_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        // CLIP(M4b)语义索引轮询
        #[cfg(feature = "ai")]
        if let Some(rx) = &self.clip_index_rx {
            if let Ok(res) = rx.try_recv() {
                self.clip_index_rx = None;
                match res {
                    Ok((n, ok)) => {
                        self.status = t_with(
                            "语义索引完成:{n} 张照片,嵌入 {ok} 张",
                            &[("n", n.to_string()), ("ok", ok.to_string())],
                        )
                    }
                    Err(e) => self.status = t_with("语义索引失败: {e}", &[("e", e.to_string())]),
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
                    Ok((hits, note)) => {
                        let n = hits.len();
                        self.clip_results = hits;
                        self.status = t_with("语义搜索完成:{n} 个结果", &[("n", n.to_string())]);
                        if let Some(msg) = note {
                            self.status = format!("{} · {}", self.status, msg);
                        }
                    }
                    Err(e) => self.status = t_with("语义搜索失败: {e}", &[("e", e.to_string())]),
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
        // 键盘快捷键(有文本输入焦点时不拦截)
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
                // ESC 优先取消正在进行的扫描
                // (ESC 取消绑定);
                // 没有扫描时才关闭预览窗(同时丢弃编辑视图里未提交的工具草稿)。
                if let Some(ctrl) = &self.scan_ctrl {
                    ctrl.cancel();
                    self.status = t_with("正在取消{p1}…", &[("p1", (self.scan_verb).to_string())]);
                } else {
                    // 与左栏「返回到图片库」走同一套清理,避免两处状态不一致。
                    self.editor_back_to_library();
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
                        // 旋转命令按 Windows 图库惯例补
                        // Ctrl+R / Ctrl+Shift+R,只在预览窗生效。
                        self.preview_rotate(id, if rot_cw { 1 } else { 3 });
                    }
                }
            }
            // 7 个滚动事件在这里派发
            // (按事件名匹配 + 派发;
            //  缩略图滚动事件走缩略图质心位移算法)。
            // 本项目把前 6 个接成键盘翻页/跳转。
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

        // 预览已关闭:释放帧序列与在途解码(所有关闭路径的统一兜底)。
        if self.preview.is_none()
            && (self.anim.is_some() || self.anim_rx.is_some() || self.anim_req_for.is_some())
        {
            self.anim_reset();
        }

        // 单图编辑视图:打开预览后整页接管,返回图库即恢复常规面板。
        // 版式:左工具 / 中预览 / 右属性,
        // 顶部导航与底部邻片条常驻,工具不必再从菜单里翻出来。
        if self.preview.is_some() {
            self.editor_ui(ctx);
            if self.show_options {
                self.options_ui(ctx);
            }
            self.tag_batch_window(ctx);
            // 编辑视图提前返回,通用提示窗仍要画(否则后台消息会一直憋到回图库)。
            self.update_window(ctx);
            self.update_confirm_window(ctx);
            return;
        }

        // ===== 菜单栏(文件/编辑/视图/工具/帮助) =====
        egui::TopBottomPanel::top("menu")
            .frame(crate::theme::bar_frame())
            .show(ctx, |ui| {
                egui::MenuBar::new().ui(ui, |ui| {
                    ui.label(
                        egui::RichText::new(t("马良"))
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
                    ui.menu_button(t("文件(F)"), |ui| {
                        // 属性窗口。
                        if ui
                            .add_enabled(
                                self.preview.is_some() || !self.sel.is_empty(),
                                egui::Button::new(t("属性…")),
                            )
                            .clicked()
                        {
                            self.show_props = true;
                            ui.close();
                        }
                        ui.separator();
                        if ui.button(t("添加文件夹到图库…")).clicked() {
                            self.add_folder_to_library();
                            ui.close();
                        }
                        let scanning = self.scan_busy.load(Ordering::Relaxed);
                        if ui
                            .add_enabled(!scanning, egui::Button::new(t("重新扫描监听目录…")))
                            .clicked()
                        {
                            self.start_scan_full();
                            ui.close();
                        }
                        if scanning {
                            ui.spinner();
                        }
                        // 范围只经文件夹管理器扩大,
                        // 这里只重扫已登记的监听目录,不新增范围。
                        ui.small(t_with(
                            "监听 {p1} 个目录;扩大范围请用「添加文件夹到图库」",
                            &[("p1", (self.watched.len()).to_string())],
                        ));
                        ui.separator();
                        ui.menu_button(t("导入(I)"), |ui| {
                            // 导入页:整页面板(选源 → 看清单 → 入库 → 返回)。
                            if ui.button(t("导入页(选源入库)…")).clicked() {
                                self.go_page(PageKind::Import);
                                ui.close();
                            }
                            // 直接挑文件夹的一次性入口(不开页面)。
                            if ui.button(t("添加文件夹到图库…")).clicked() {
                                self.add_folder_to_library();
                                ui.close();
                            }
                            // 从设备导入(设备选择窗口)。
                            if ui.button(t("从设备(相机/手机)…")).clicked() {
                                self.open_import_devices();
                                ui.close();
                            }
                            let label = if self.wifi.is_some() {
                                t("从手机(WiFi)导入…(运行中,点击停止)")
                            } else {
                                t("从手机(WiFi)导入…")
                            };
                            if ui.button(label).clicked() {
                                self.toggle_wifi();
                                ui.close();
                            }
                        });
                        ui.menu_button(t("导出(E)"), |ui| {
                            if ui
                                .add_enabled(
                                    self.export_rx.is_none(),
                                    egui::Button::new(t("导出到文件夹…")),
                                )
                                .clicked()
                            {
                                self.start_export();
                                ui.close();
                            }
                            // 导出时套用已保存的效果(不修改原图)。
                            ui.checkbox(&mut self.export_with_effects, t("套用已保存的效果"));
                        });
                        // 分享(邮件 / 礼品光盘;上传目标服务已关停,不实现)。
                        ui.menu_button(t("分享(S)"), |ui| {
                            let busy = self.share_rx.is_some();
                            if ui
                                .add_enabled(!busy, egui::Button::new(t("以邮件发送…")))
                                .clicked()
                            {
                                self.email_win = true;
                                ui.close();
                            }
                            if ui
                                .add_enabled(!busy, egui::Button::new(t("制作礼品光盘…")))
                                .clicked()
                            {
                                self.cd_win = true;
                                ui.close();
                            }
                            ui.separator();
                            ui.small(t("网络上传:目前无可用实现。"));
                        });
                        ui.separator();
                        if ui
                            .add_enabled(
                                self.preview.is_some() && self.bake_rx.is_none(),
                                egui::Button::new(t("保存  F2")),
                            )
                            .clicked()
                        {
                            if let Some(id) = self.preview {
                                self.start_bake(id);
                            }
                            ui.close();
                        }
                        ui.separator();
                        if ui.button(t("退出(X)")).clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            ui.close();
                        }
                    });
                    // —— 编辑 ——
                    ui.menu_button(t("编辑(E)"), |ui| {
                        let ids = self.view_ids();
                        let empty_view = ids.is_empty();
                        let starred: Vec<i64> = self
                            .rows
                            .iter()
                            .filter(|r| r.star != 0)
                            .map(|r| r.id)
                            .collect();
                        if ui
                            .add_enabled(!empty_view, egui::Button::new(t("全选  Ctrl+A")))
                            .clicked()
                        {
                            self.sel = ids.iter().copied().collect();
                            ui.close();
                        }
                        if ui
                            .add_enabled(!empty_view, egui::Button::new(t("选择加星标的照片")))
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
                            .add_enabled(!empty_view, egui::Button::new(t("反向选择")))
                            .clicked()
                        {
                            let all: BTreeSet<i64> = ids.iter().copied().collect();
                            self.sel = all.difference(&self.sel).copied().collect();
                            ui.close();
                        }
                        if ui
                            .add_enabled(!self.sel.is_empty(), egui::Button::new(t("清除选择")))
                            .clicked()
                        {
                            self.sel.clear();
                            ui.close();
                        }
                        ui.separator();
                        // 效果面板。
                        if ui.selectable_label(self.fx_panel, t("效果面板")).clicked() {
                            self.fx_panel = !self.fx_panel;
                            ui.close();
                        }
                        if ui
                            .add_enabled(
                                !self.sel.is_empty(),
                                egui::Button::new(t("从图库中移除…")),
                            )
                            .clicked()
                        {
                            self.confirm_remove = true;
                            ui.close();
                        }
                    });
                    // —— 视图 ——
                    ui.menu_button(t("视图(V)"), |ui| {
                        if ui
                            .selectable_label(self.view == ViewKind::Recent, t("最近更新"))
                            .clicked()
                        {
                            self.set_view(ViewKind::Recent);
                            ui.close();
                        }
                        if ui
                            .selectable_label(self.view == ViewKind::Timeline, t("时间线"))
                            .clicked()
                        {
                            self.set_view(ViewKind::Timeline);
                            ui.close();
                        }
                        // 相簿列表。
                        if ui
                            .selectable_label(self.view == ViewKind::Albums, t("相簿"))
                            .clicked()
                        {
                            self.set_view(ViewKind::Albums);
                            ui.close();
                        }
                        #[cfg(feature = "ai")]
                        if ui
                            .selectable_label(self.view == ViewKind::Faces, t("人物"))
                            .clicked()
                        {
                            self.set_view(ViewKind::Faces);
                            ui.close();
                        }
                        #[cfg(feature = "ai")]
                        if ui
                            .selectable_label(self.view == ViewKind::Semantic, t("语义搜索"))
                            .clicked()
                        {
                            self.set_view(ViewKind::Semantic);
                            ui.close();
                        }
                        if ui
                            .selectable_label(self.view == ViewKind::Search, t("快速查找"))
                            .clicked()
                        {
                            self.set_view(ViewKind::Search);
                            #[cfg(windows)]
                            self.svc_nudge_on_search_entry();
                            ui.close();
                        }
                        if ui
                            .selectable_label(self.view == ViewKind::Tags, t("标签"))
                            .clicked()
                        {
                            self.set_view(ViewKind::Tags);
                            ui.close();
                        }
                        if ui
                            .selectable_label(self.view == ViewKind::Folder, t("文件夹"))
                            .clicked()
                        {
                            self.set_view(ViewKind::Folder);
                            ui.close();
                        }
                        ui.separator();
                        // 幻灯片放映(当前视图逐张全屏)。
                        if ui.button(t("幻灯片放映")).clicked() {
                            self.start_slideshow();
                            ui.close();
                        }
                        // 缩略图说明。
                        if ui
                            .checkbox(&mut self.thumb_captions, t("缩略图说明"))
                            .changed()
                        {
                            let _ = self.store.set_setting(
                                "thumb_captions",
                                if self.thumb_captions { "1" } else { "0" },
                            );
                        }
                        // 文件夹视图形态(树形 / 平铺)。
                        ui.menu_button(t("文件夹视图"), |ui| {
                            if ui
                                .selectable_label(!self.folder_tree, t("平铺文件夹视图"))
                                .clicked()
                            {
                                self.set_folder_tree(false);
                                ui.close();
                            }
                            if ui
                                .selectable_label(self.folder_tree, t("树形视图"))
                                .clicked()
                            {
                                self.set_folder_tree(true);
                                ui.close();
                            }
                        });
                        ui.separator();
                        // 四种排序是同一层菜单项(不套子菜单),顺序沿用经典布局。
                        for s in timeline::SortBy::ALL {
                            if ui
                                .selectable_label(self.sort_by == s, t(s.label()))
                                .clicked()
                            {
                                self.sort_by = s;
                                self.apply_view_state();
                                ui.close();
                            }
                        }
                        if ui.selectable_label(self.sort_rev, t("反向排序")).clicked() {
                            self.sort_rev = !self.sort_rev;
                            self.apply_view_state();
                            ui.close();
                        }
                        ui.menu_button(t("缩略图"), |ui| {
                            // 多档缩略图(四个视图尺寸菜单项)。
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
                                egui::Button::new(t("在资源管理器中显示")),
                            )
                            .clicked()
                        {
                            self.show_in_explorer_current();
                            ui.close();
                        }
                    });
                    // —— 工具 ——
                    ui.menu_button(t("工具(T)"), |ui| {
                        // 选项(分页设置窗)。
                        if ui.button(t("选项…")).clicked() {
                            self.show_options = true;
                            ui.close();
                        }
                        ui.separator();
                        #[cfg(feature = "ai")]
                        {
                            let ai_paused = self
                                .ai_sched
                                .as_ref()
                                .map(|s| s.is_paused())
                                .unwrap_or(true);
                            if ui
                                .button(if ai_paused {
                                    t("继续 AI 索引")
                                } else {
                                    t("暂停 AI 索引")
                                })
                                .clicked()
                            {
                                let _ = self
                                    .store
                                    .set_setting("ai_enabled", if ai_paused { "1" } else { "0" });
                                if let Some(s) = &self.ai_sched {
                                    if ai_paused {
                                        s.resume();
                                    } else {
                                        s.pause();
                                    }
                                }
                                self.ai_enabled = ai_paused;
                                if ai_paused {
                                    self.enqueue_all_ai();
                                }
                                self.status = if ai_paused {
                                    t("AI 索引已继续")
                                } else {
                                    t("AI 索引已暂停")
                                }
                                .into();
                                ui.close();
                            }
                            ui.separator();
                            if ui
                                .add_enabled(
                                    self.face_rx.is_none(),
                                    egui::Button::new(t("人脸识别扫描…")),
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
                                    egui::Button::new(t("文字识别(OCR)…")),
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
                                    egui::Button::new(t("语义索引…")),
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
                        if ui.button(t("文件夹管理器…")).clicked() {
                            self.open_folder_mgr();
                            ui.close();
                        }
                        let mut watching = self.watch_on.load(Ordering::SeqCst);
                        if ui.checkbox(&mut watching, t("监听文件夹变化")).clicked() {
                            self.watch_on.store(watching, Ordering::SeqCst);
                            self.status = if watching {
                                t("已开启文件夹监听(事件驱动,不可用时自动降级轮询)").into()
                            } else {
                                t("已暂停文件夹监听").into()
                            };
                            ui.close();
                        }
                        if ui
                            .selectable_label(self.view == ViewKind::Duplicates, t("重复照片"))
                            .clicked()
                        {
                            self.set_view(ViewKind::Duplicates);
                            ui.close();
                        }
                        ui.separator();
                        ui.menu_button(t("图库维护"), |ui| {
                            if ui
                                .add_enabled(
                                    self.maint_rx.is_none(),
                                    egui::Button::new(t("备份索引到…")),
                                )
                                .clicked()
                            {
                                self.start_backup();
                                ui.close();
                            }
                            if ui
                                .add_enabled(
                                    self.maint_rx.is_none(),
                                    egui::Button::new(t("压缩索引")),
                                )
                                .clicked()
                            {
                                self.start_compact();
                                ui.close();
                            }
                            if ui
                                .add_enabled(
                                    self.maint_rx.is_none(),
                                    egui::Button::new(t("清理已缺失文件的索引")),
                                )
                                .clicked()
                            {
                                self.clean_missing();
                                ui.close();
                            }
                            // 目录级失效的用户出口。
                            if ui.button(t("重建全部缩略图")).clicked() {
                                self.force_thumb_update();
                                ui.close();
                            }
                            // 坏文件台账(整批销案在这里)。
                            if ui.button(t("坏文件台账…")).clicked() {
                                self.bad_media_rows = self.store.bad_media();
                                self.show_bad_media = true;
                                ui.close();
                            }
                            if ui
                                .add_enabled(
                                    self.maint_rx.is_none(),
                                    egui::Button::new(t("从 XMP 导入标签…")),
                                )
                                .clicked()
                            {
                                self.start_xmp_import(true);
                                ui.close();
                            }
                            // 变更历史(可回溯的变更日志)。
                            if ui.button(t("变更历史…")).clicked() {
                                self.show_changes = true;
                                self.reload_changes();
                                ui.close();
                            }
                            // 给还没有 uid64 的照片补一个持久唯一标识。
                            if ui.button(t("分配唯一标识(uid64)…")).clicked() {
                                match self.store.backfill_uid64() {
                                    Ok(n) => {
                                        self.status = t_with(
                                            "已为 {n} 张照片分配 uid64",
                                            &[("n", n.to_string())],
                                        );
                                    }
                                    Err(e) => {
                                        self.status =
                                            t_with("分配 uid64 失败: {e}", &[("e", e.to_string())])
                                    }
                                }
                                ui.close();
                            }
                            if ui.button(t("按唯一标识查找…")).clicked() {
                                self.show_uid_lookup = true;
                                self.uid_lookup_result = None;
                                ui.close();
                            }
                            if self.maint_rx.is_some() {
                                ui.spinner();
                            }
                        });
                        if ui.button(t("导出地理标记(KML)…")).clicked() {
                            self.export_kml();
                            ui.close();
                        }
                    });
                    // —— 帮助 ——
                    ui.menu_button(t("帮助(H)"), |ui| {
                        if ui.button(t("键盘快捷键…")).clicked() {
                            self.show_shortcuts = true;
                            ui.close();
                        }
                        if ui.button(t("检查更新…")).clicked() {
                            // 手动入口:立刻联网,不等空闲窗口,也不看检查间隔。
                            self.updater.check_now();
                            self.updater.win_open = true;
                            ui.close();
                        }
                        if ui.button(t("关于 Maliang…")).clicked() {
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
                // 一行排完:输入框在左、类别档位在右,共用同一条水平基线。
                // 输入框按「可用宽 - 档位宽 - 间距」定尺寸,不再用「剩余宽 - 6」
                // 那种估算 —— 文本控件自带内边距,估算偏大就会把档位挤歪或换行。
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let combo_w = 84.0;
                    let clear_w = 26.0;
                    let gap = ui.spacing().item_spacing.x;
                    let edit_w = (ui.available_width() - combo_w - clear_w - gap * 2.0).max(80.0);
                    let resp = ui.add_sized(
                        egui::vec2(edit_w, 24.0),
                        egui::TextEdit::singleline(&mut self.name_query)
                            .hint_text(t("全盘快速查找:文件名 / 文件夹名片段,即输即搜…")),
                    );
                    // 档位框钉进与输入框同高的 24px 槽里再垂直居中:
                    // ComboBox 自身高度跟字体/内边距走,直接放进居中布局
                    // 会和 24 高的输入框差半截 —— 套同高槽位后两者共用
                    // 一条水平基线。
                    ui.allocate_ui_with_layout(
                        egui::vec2(combo_w, 24.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            egui::ComboBox::from_id_salt("top-name-class")
                                .width(combo_w)
                                .selected_text(t(self.name_class.label()))
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
                        },
                    );
                    // 清除按钮:一键清空搜索框并退出查找页(与手动删空
                    // 输入框同一套状态复位)。没有内容时置灰。
                    if ui
                        .add_enabled(!self.name_query.is_empty(), tool_button("x", false))
                        .on_hover_text(t("清除搜索内容"))
                        .clicked()
                    {
                        self.name_query.clear();
                        self.dispatch_name_search();
                        if self.view == ViewKind::Search {
                            self.set_view(ViewKind::Timeline);
                        }
                    }
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
                    ui.label(t_with("共 {p1} 张", &[("p1", (self.rows.len()).to_string())]))
                        // 图库视图是 L2(SQLite)的结果,与顶部整条快速查找(L1
                        // 内存文件名索引)不是同一条路 —— 标出来,免得两处张数对不上时
                        // 用户以为是丢图。
                        .on_hover_text(t("来源 L2:图库数据库(SQLite photos 表)。星级、人物、视频、地理标记等条件过滤都在这一层查;顶部的全盘快速查找走的是另一条路(L1 内存文件名索引)。"));
                    // 过滤开着时必须说清"这是过滤后的张数",否则看起来像丢图。
                    if !self.filters.is_none() {
                        ui.label(t("(视图过滤中)"));
                        if ui.small_button(t("清除过滤")).clicked() {
                            self.filters = Default::default();
                            self.apply_view_state();
                        }
                    }
                    if !self.sel.is_empty() {
                        ui.label(t_with("已选 {p1} 张", &[("p1", (self.sel.len()).to_string())]));
                    }
                    ui.separator();
                    ui.label(&self.status);
                    // 自动更新只在状态栏留一句可点的提示,不常驻弹窗打扰看图。
                    if let Some(hint) = self.updater.phase.status_hint() {
                        ui.separator();
                        if ui.link(t_with("{hint} · 点此查看", &[("hint", hint.to_string())])).clicked() {
                            self.updater.win_open = true;
                        }
                    }
                    // 当前档位的缓存进度(已生成 / 总数)。
                    // 计数只在切档 / 批次回报 / 重建图库时刷新,不逐帧查库。
                    let cached = self.tier_cached;
                    let total = self.rows.len();
                    if cached < total {
                        ui.separator();
                        ui.label(t_with("{p1}缓存 {cached}/{total}", &[("p1", (self.thumb_tier.label()).to_string()), ("cached", cached.to_string()), ("total", total.to_string())]));
                    }
                    // 进度条 + 取消按钮(百分比显示,明细挂悬停;
                    // 扫描与导入共用通道,动词跟着任务走)。
                    if let Some(ctrl) = &self.scan_ctrl {
                        let (done, total) = ctrl.counts();
                        let frac = ctrl.fraction();
                        let verb = self.scan_verb;
                        ui.separator();
                        let pct = (frac * 100.0).round() as i32;
                        ui.label(t_with("正在{verb}… {pct}%", &[("verb", verb.to_string()), ("pct", pct.to_string())]))
                            .on_hover_text(t_with("{done}/{total} 张已完成", &[("done", done.to_string()), ("total", total.to_string())]));
                        ui.add(egui::ProgressBar::new(frac).desired_width(140.0));
                        if ui.button(t_with("取消{verb}", &[("verb", verb.to_string())])).clicked() {
                            ctrl.cancel();
                            self.status = t_with("正在取消{verb}{p1}…", &[("verb", verb.to_string()), ("p1", (self.scan_scope).to_string())]);
                        }
                    }
                    // 人脸扫描进度(分母 = 开扫时全库待分析张数;逐张回报,不会卡在 0%)。
                    #[cfg(feature = "ai")]
                    if self.face_rx.is_some() {
                        let (d, t) = self.face_prog;
                        let frac = if t > 0 { (d as f32 / t as f32).min(1.0) } else { 0.0 };
                        ui.separator();
                        ui.label(t_with("正在扫描人物… {p1}%", &[("p1", ((frac * 100.0).round() as i32).to_string())]))
                            .on_hover_text(t_with("已完成 {d}/{t} 张 · 剩余 {p1} 张", &[("d", d.to_string()), ("t", t.to_string()), ("p1", t.saturating_sub(d).to_string())]));
                        ui.add(egui::ProgressBar::new(frac).desired_width(140.0));
                        if let Some(c) = &self.face_cancel {
                            if ui.button(crate::i18n::t("取消人物扫描")).clicked() {
                                c.store(true, std::sync::atomic::Ordering::Relaxed);
                                self.status = crate::i18n::t("正在取消人物扫描…").into();
                            }
                        }
                    }
                    // 人脸模型下载进度:在下哪个文件、下了多少,状态栏一眼看到。
                    #[cfg(feature = "ai")]
                    if let crate::models::Phase::Downloading {
                        file,
                        idx,
                        count,
                        pct,
                        done,
                        total,
                    } = &self.models.phase
                    {
                        let mb = |n: u64| format!("{:.1} MB", n as f64 / 1_048_576.0);
                        let frac = if *total > 0 { *pct as f32 / 100.0 } else { 0.0 };
                        ui.separator();
                        ui.label(t_with("正在下载人脸模型 {file}… {pct}%", &[("file", file.to_string()), ("pct", pct.to_string())]))
                            .on_hover_text(t_with("第 {idx}/{count} 个:{p1} / {p2}", &[("idx", idx.to_string()), ("count", count.to_string()), ("p1", (mb(*done)).to_string()), ("p2", (mb(*total)).to_string())]));
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
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            self.fx_panel_ui(ui);
                        });
                });
        }

        // 左侧导航面板(分类列表:常驻,内容随选中项切换)。
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
                // 导入页:左栏换成全盘目录树(点目录 = 设为导入源),
                // 右栏抽屉/效果面板也一并收起,见对应开关。
                if self.page == PageKind::Import {
                    self.import_tree_ui(ui);
                    return;
                }
                crate::theme::section_title(ui, t("库"));
                // 单栏抽屉:四个可折叠分类,明细内联在所属分类下,整栏共用一条滚动。
                let recent_n = self.recent_idxs.len();
                let folders_n = self.folders.len();
                let tags_n = self.tags.len();
                let dups_n = self.dups.len();
                #[cfg(feature = "ai")]
                let people_n = self.face_people.len();
                egui::ScrollArea::vertical()
                    .id_salt("nav-groups")
                    // egui 0.33 默认 auto_shrink 两轴都开,横向会缩到内容宽,
                    // 面板右半边留空;这里横向必须撑满。
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        egui::CollapsingHeader::new(egui::RichText::new(t("相册")).strong())
                            .default_open(true)
                            .open(if self.nav_open_drawer == Some("相册") { Some(true) } else { None })
                            .show(ui, |ui| {
                                self.nav_item(ui, t("最近更新"), ViewKind::Recent, Some(recent_n));
                                self.nav_item(ui, t("相簿"), ViewKind::Albums, None);
                                if self.view == ViewKind::Albums {
                                    self.nav_albums_detail(ui);
                                }
                            });
                        egui::CollapsingHeader::new(egui::RichText::new(t("人物")).strong())
                            .default_open(true)
                            .open(if self.nav_open_drawer == Some("人物") { Some(true) } else { None })
                            .show(ui, |ui| {
                                #[cfg(feature = "ai")]
                                {
                                    self.nav_item(ui, t("人物"), ViewKind::Faces, Some(people_n));
                                    if self.view == ViewKind::Faces {
                                        self.nav_faces_detail(ui);
                                    }
                                }
                                #[cfg(not(feature = "ai"))]
                                ui.label(t("(未启用人物功能)"));
                            });
                        egui::CollapsingHeader::new(egui::RichText::new(t("文件夹")).strong())
                            .default_open(true)
                            .open(if self.nav_open_drawer == Some("文件夹") { Some(true) } else { None })
                            .show(ui, |ui| {
                                // 常用位置:四个壳目录 + 用户导入目录。点了直接定位到
                                // 图库里对应的目录,目录没导入时状态栏给出明确提示。
                                for (label, path) in crate::db::common_dirs(&self.store) {
                                    let imported = self
                                        .store
                                        .dir_intent(&path.to_string_lossy())
                                        .is_some_and(|d| d.state == crate::db::INTENT_IMPORTED);
                                    let color = if imported {
                                        crate::theme::p().accent
                                    } else {
                                        crate::theme::p().muted
                                    };
                                    ui.horizontal(|ui| {
                                        ui.add(folder_icon_colored(color));
                                        if ui
                                            .selectable_label(false, label.as_str())
                                            .on_hover_text(t("定位到这个常用位置"))
                                            .clicked()
                                        {
                                            self.goto_known_dir(Some(path), &label);
                                        }
                                    });
                                }
                                // 「所有文件夹」是视图级入口:中间栏一次铺开全部目录。
                                // 不能走 nav_item —— 视图已经是 Folder 时 set_view 会
                                // 提前返回,从某个具体目录点回来就铺不开了。
                                let all_on = self.view == ViewKind::Folder && self.folder_show_all;
                                let rt = egui::RichText::new(t_with(
                                    "所有文件夹 ({folders_n})",
                                    &[("folders_n", folders_n.to_string())],
                                ))
                                .color(if all_on {
                                    crate::theme::p().accent_strong
                                } else {
                                    crate::theme::p().text
                                });
                                if ui
                                    .selectable_label(all_on, if all_on { rt.strong() } else { rt })
                                    .on_hover_text(t("中间栏一次铺开所有文件夹(每组一个相册头)"))
                                    .clicked()
                                {
                                    self.set_view(ViewKind::Folder);
                                    self.folder_show_all = true;
                                    self.subtree_sel = None;
                                }
                                if self.view == ViewKind::Folder {
                                    self.nav_folder_detail(ui, ctx);
                                }
                            });
                        egui::CollapsingHeader::new(egui::RichText::new(t("其他素材")).strong())
                            .default_open(true)
                            .open(if self.nav_open_drawer == Some("其他素材") { Some(true) } else { None })
                            .show(ui, |ui| {
                                self.nav_item(ui, t("时间线"), ViewKind::Timeline, None);
                                self.nav_item(ui, t("标签"), ViewKind::Tags, Some(tags_n));
                                #[cfg(feature = "ai")]
                                self.nav_item(ui, t("语义搜索"), ViewKind::Semantic, None);
                                self.nav_item(
                                    ui,
                                    t("重复照片"),
                                    ViewKind::Duplicates,
                                    Some(dups_n),
                                );
                                self.nav_item(ui, t("快速查找"), ViewKind::Search, None);
                                match self.view {
                                    ViewKind::Tags => self.nav_tags_detail(ui),
                                    #[cfg(feature = "ai")]
                                    ViewKind::Semantic => self.nav_semantic_detail(ui),
                                    _ => {}
                                }
                            });
                    });
                self.nav_open_drawer = None;
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
            // 滚动限速(位移阻尼限速)。
            // 上一帧记下的滚动偏移 + 视口高 + 内容高 → 逐帧质心位移 × 2.7
            // → 阻尼曲线 → 平滑速度 → 换算成本帧的缩略图解码预算。
            // 滚动越快 → 预算越低 → 大图库滚动不掉帧(限速节点的等价作用)。
            self.update_scroll_throttle(
                self.last_scroll_y,
                self.last_viewport_h,
                self.last_content_h,
            );
            // 滚动事件派发后的目标偏移 → 作为 ScrollArea 的初始偏移
            // (egui 会用 `state.offset` 记忆滚动位置,这里只在有目标时覆盖一次)。
            // 视图级常驻工具条先画(在滚动区之外,滚到哪都钉在顶部)。
            match self.view {
                ViewKind::Folder => self.folder_toolbar_ui(ui),
                ViewKind::Duplicates => self.dups_toolbar_ui(ui),
                _ => {}
            }
            let mut gallery = egui::ScrollArea::vertical().id_salt("gallery");
            if let Some(t) = self.gallery_scroll_target.take() {
                gallery = gallery.vertical_scroll_offset(t);
            }
            let out = gallery.show(ui, |ui| {
                match self.view {
                    ViewKind::Recent => {
                        // 首页:相册头(标题 / 日期 / 张数 / 动作) → 已缓存成员的
                        // 行式缩略图。成员清单由 refresh_recent 算好,这里不碰全表。
                        self.recent_header(ui);
                        ui.add_space(6.0);
                        if self.recent_idxs.is_empty() {
                            ui.label(t("最近更新里还没有照片:缩略图生成后会自动收进来,也可到「文件夹」里点开一个目录。"));
                        } else {
                            if self.recent_idxs.len() < 20 && !self.watched.is_empty() {
                                ui.label(
                                    egui::RichText::new(t("首页只显示已纳入管理的文件夹里的照片;要收进更多目录,到「文件夹」视图里添加监听。"))
                                    .size(11.0)
                                    .color(crate::theme::p().muted),
                                );
                                ui.add_space(4.0);
                            }
                            let cells = self.recent_idxs.clone();
                            self.photo_grid(ui, "recent-grid", &cells);
                        }
                    }
                    ViewKind::Timeline => {
                        let months = std::mem::take(&mut self.months);
                        for (label, idxs) in &months {
                            ui.add_space(6.0);
                            // 日期分组头(弱化的小标题,不与缩略图抢视线)。
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
                        if self.dups.is_empty() {
                            ui.label(if self.dup_near {
                                t("没有检测到重复或近似重复")
                            } else {
                                t("没有检测到重复(哈希精确一致才计为一组;点顶部漏斗可含近似重复)")
                            });
                        }
                        let dups = std::mem::take(&mut self.dups);
                        for (h, ids) in &dups {
                            ui.separator();
                            if self.dup_near {
                                ui.heading(t_with("近似组  ·  {p1} 张", &[("p1", (ids.len()).to_string())]));
                            } else {
                                ui.heading(t_with("组 {h}  ·  {p1} 张", &[("h", format!("{h:016x}")), ("p1", (ids.len()).to_string())]));
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
                                ui.heading(t("人脸"));
                                let algo = crate::faces::current_algo(&self.store);
                                if !crate::ai::face::Faces::models_present(algo) {
                                    let (det_f, rec_f) = algo.files();
                                    ui.label(t_with("还没有人脸模型,无法识别(需要 {det_f} 与 {rec_f}):", &[("det_f", det_f.to_string()), ("rec_f", rec_f.to_string())]));
                                    ui.small(
                                        crate::ai::models_dir()
                                            .map(|d| d.display().to_string())
                                            .unwrap_or_default(),
                                    );
                                    self.face_model_compact_ui(ui, algo);
                                } else {
                                    ui.label(
                                        t("左侧选择一个人脸簇查看照片;识别用 工具 > 人脸识别扫描。"),
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
                                let current = self
                                    .face_people
                                    .iter()
                                    .find(|p| p.cluster == c && p.person_id == self.sel_person);
                                let q_txt = current
                                    .map(|p| t_with(" · 平均质量 {p1}", &[("p1", format!("{p1:.2}", p1 = (p.quality)))]))
                                    .unwrap_or_default();
                                let head = match self.sel_person {
                                    Some(pid) => {
                                        let name = current
                                            .and_then(|p| p.person.clone())
                                            .unwrap_or_else(|| pid.to_string());
                                        t_with("人物 {name} · {p1} 张照片 / {p2} 个人脸框{q_txt}", &[("name", name), ("p1", (cells.len()).to_string()), ("p2", (self.face_hits.len()).to_string()), ("q_txt", q_txt.to_string())])
                                    }
                                    None => t_with("簇 {c} · {p1} 张照片 / {p2} 个人脸框{q_txt}", &[("c", c.to_string()), ("p1", (cells.len()).to_string()), ("p2", (self.face_hits.len()).to_string()), ("q_txt", q_txt.to_string())]),
                                };
                                ui.heading(head);
                                let clusters: Vec<i64> = current
                                    .map(|p| p.clusters.clone())
                                    .unwrap_or_else(|| vec![c]);
                                self.photo_grid(ui, "faces-grid", &cells);
                                ui.add_space(8.0);
                                ui.separator();
                                ui.label(t("人脸框明细(按质量降序;质量低的可整簇忽略):"));
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
                                            ui.label(t_with("{fname} · 质量 {q:.2}", &[("fname", fname.to_string()), ("q", format!("{q:.2}"))]));
                                        }
                                    });
                                ui.add_space(8.0);
                                ui.separator();
                                ui.heading(t("重命名这个人物"));
                                ui.horizontal(|ui| {
                                    let resp = ui.text_edit_singleline(&mut self.input_rename);
                                    if (resp.lost_focus()
                                        && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                                        || ui.button(t("重命名")).clicked()
                                    {
                                        let name = std::mem::take(&mut self.input_rename);
                                        if !name.is_empty() {
                                            let res = match self.sel_person {
                                                Some(pid) => crate::faces::rename_person(
                                                    &self.store,
                                                    pid,
                                                    &name,
                                                ),
                                                None => crate::faces::rename_cluster(
                                                    &self.store,
                                                    c,
                                                    &name,
                                                ),
                                            };
                                            match res {
                                                Ok(()) => {
                                                    self.status = t_with(
                                                        "已命名:{name}",
                                                        &[("name", name.clone())],
                                                    );
                                                }
                                                Err(e) => {
                                                    self.status = t_with(
                                                        "命名失败: {e}",
                                                        &[("e", e.to_string())],
                                                    );
                                                }
                                            }
                                            self.refresh_people_and_suggestions();
                                        }
                                    }
                                });
                                // 忽略/恢复整组(跨簇身份的所有簇一并处理)。
                                ui.horizontal(|ui| {
                                    if ui.button(t("忽略此组")).clicked() {
                                        for cid in &clusters {
                                            let _ = crate::faces::ignore_cluster(&self.store, *cid, true);
                                        }
                                        self.face_show_ignored = true;
                                        self.refresh_people_and_suggestions();
                                    }
                                    if ui.button(t("恢复此组")).clicked() {
                                        for cid in &clusters {
                                            let _ = crate::faces::ignore_cluster(&self.store, *cid, false);
                                        }
                                        self.refresh_people_and_suggestions();
                                    }
                                });
                                ui.add_space(8.0);
                                ui.separator();
                                ui.heading(t("合并 / 拆分审核"));
                                let mine: Vec<(i64, i64, crate::ai_review::Kind)> =
                                    crate::ai_review::all(&self.store)
                                        .into_iter()
                                        .filter(|(a, b, _)| {
                                            self.face_hits
                                                .iter()
                                                .any(|h| h.face_id == *a || h.face_id == *b)
                                        })
                                        .collect();
                                if mine.is_empty() {
                                    ui.small(t("这个簇还没有人工约束。"));
                                }
                                let in_this: HashSet<i64> =
                                    self.face_hits.iter().map(|h| h.face_id).collect();
                                for (a, b, k) in mine {
                                    ui.horizontal(|ui| {
                                        let kind = match k {
                                            crate::ai_review::Kind::Must => t("同一人"),
                                            crate::ai_review::Kind::Never => t("不同人"),
                                        };
                                        let clash = k == crate::ai_review::Kind::Never
                                            && in_this.contains(&a)
                                            && in_this.contains(&b);
                                        ui.label(t_with("{kind}: 人脸 {a} ↔ {b}{p1}", &[("kind", kind.to_string()), ("a", a.to_string()), ("b", b.to_string()), ("p1", (if clash { t("(已同簇,需拆分)") } else { "" }).to_string())]));
                                        if ui.small_button(t("删除")).clicked() {
                                            let _ = crate::ai_review::remove(&self.store, a, b);
                                        }
                                    });
                                }
                                let cur_face = self
                                    .face_hits
                                    .iter()
                                    .max_by(|x, y| {
                                        x.quality
                                            .partial_cmp(&y.quality)
                                            .unwrap_or(std::cmp::Ordering::Equal)
                                    })
                                    .map(|h| h.face_id);
                                let others: Vec<(i64, usize)> = self
                                    .face_people
                                    .iter()
                                    .filter(|p| p.cluster != c)
                                    .map(|p| (p.cluster, p.count))
                                    .collect();
                                let mut add_kind: Option<crate::ai_review::Kind> = None;
                                ui.horizontal(|ui| {
                                    egui::ComboBox::from_id_salt("face-constraint-target")
                                        .selected_text(
                                            self.face_merge_target
                                                .map(|t| t_with("簇 {t}", &[("t", t.to_string())]))
                                                .unwrap_or_else(|| t("选择另一个簇").into()),
                                        )
                                        .show_ui(ui, |ui| {
                                            for (cid, cnt) in &others {
                                                ui.selectable_value(
                                                    &mut self.face_merge_target,
                                                    Some(*cid),
                                                    t_with("簇 {cid} · {cnt} 张", &[("cid", cid.to_string()), ("cnt", cnt.to_string())]),
                                                );
                                            }
                                        });
                                    if ui.button(t("同一人(合并)")).clicked() {
                                        add_kind = Some(crate::ai_review::Kind::Must);
                                    }
                                    if ui.button(t("不同人(拆分)")).clicked() {
                                        add_kind = Some(crate::ai_review::Kind::Never);
                                    }
                                });
                                if let (Some(kind), Some(cf), Some(t)) =
                                    (add_kind, cur_face, self.face_merge_target)
                                {
                                    let other = crate::faces::faces_for_cluster(
                                        &self.store, t, false, 0.0,
                                    )
                                    .into_iter()
                                    .max_by(|x, y| {
                                        x.quality
                                            .partial_cmp(&y.quality)
                                            .unwrap_or(std::cmp::Ordering::Equal)
                                    })
                                    .map(|h| h.face_id);
                                    if let Some(of) = other {
                                        let _ = crate::ai_review::add(&self.store, cf, of, kind);
                                        self.status = t_with("已记录约束:簇 {c} ↔ 簇 {t}", &[("c", c.to_string()), ("t", t.to_string())]);
                                    }
                                }
                                ui.small(t("约束在点「重新聚类」后生效;「不同人」只阻止合并。"));
                            }
                        }
                        #[cfg(not(feature = "ai"))]
                        {
                            ui.heading(t("人物"));
                            ui.label(
                                t("人脸功能未编译进当前构建(构建时用了 --no-default-features)。"),
                            );
                        }
                    }
                    ViewKind::Tags => self.tags_view(ui),
                    ViewKind::Semantic => {
                        #[cfg(feature = "ai")]
                        {
                            if self.clip_results.is_empty() {
                                if let Some(name) = self.clip_image_query.clone() {
                                    ui.heading(t_with(
                                        "相似照片(查询:{p1})",
                                        &[("p1", name)]
                                    ));
                                    ui.label(t("没有找到相似照片;索引还没覆盖的照片不会参与比对。"));
                                } else {
                                    ui.heading(t("语义搜索"));
                                    if crate::clip::natural_ready(&self.store) {
                                        ui.label(t("左侧输入关键词或一句话,支持中英文、标签与图中文字。"));
                                    } else {
                                        ui.label(t("左侧输入关键词(英文效果最佳,如 \"a cat\"、\"beach\");首次使用请先 工具 > 语义索引。"));
                                    }
                                }
                            } else {
                                let n = self.clip_results.len().to_string();
                                let head = match self.clip_image_query.clone() {
                                    Some(name) => t_with(
                                        "相似照片 {p1} 张(查询:{p2})",
                                        &[("p1", n), ("p2", name)]
                                    ),
                                    None => {
                                        let q = self.input_query.trim().to_string();
                                        t_with(
                                            "语义结果 {p1} 张(查询:{q})",
                                            &[("p1", n), ("q", q)]
                                        )
                                    }
                                };
                                ui.heading(head);
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
                            ui.heading(t("语义搜索"));
                            ui.label(
                                t("语义搜索未编译进当前构建(构建时用了 --no-default-features)。"),
                            );
                        }
                    }
                    ViewKind::Folder => {
                        if self.folder_show_all {
                            // 平铺图库:一次铺开所有文件夹,
                            // 每个文件夹一个相册组头 + 一组逐行缩略图。
                            let groups = self.take_folder_groups();
                            for (folder, idxs, date) in &groups {
                                self.album_label(ui, folder, date, idxs.len());
                                ui.add_space(4.0);
                                self.photo_grid(ui, &format!("fb-{folder}"), idxs);
                                ui.add_space(16.0);
                            }
                            if self.rows.is_empty() {
                                ui.label(t("图库里还没有照片,或当前过滤把照片都滤掉了。"));
                            }
                            self.folder_groups.1 = groups;
                            return;
                        }
                        // 选中目录:左栏树点中的子树根(可为本目录+全部子孙的递归聚合)
                        // 优先;否则回退到平铺列表/槽位选中的目录。
                        let folder = self
                            .subtree_sel
                            .clone()
                            .or_else(|| self.folders.get(self.selected_folder).cloned())
                            .unwrap_or_default();
                        // 从全部行里按目录筛一遍是 O(行数) 次路径运算,
                        // 只在图库版本变了或换了目录时重算(缓存借出后归还)。
                        // 递归语义:点上级 = 本目录+全部子孙目录的照片都算,
                        // 一路点进下级逐级收窄。
                        let mut idxs = std::mem::take(&mut self.folder_cells.2);
                        let mut date = std::mem::take(&mut self.folder_cells.3);
                        if self.folder_cells.0 != self.rows_gen || self.folder_cells.1 != folder {
                            idxs = self
                                .rows
                                .iter()
                                .enumerate()
                                .filter(|(_, r)| path_under_dir(&r.path, &folder))
                                .map(|(i, _)| i)
                                .collect();
                            date = timeline::group_date(&self.rows, &idxs);
                            self.folder_cells.0 = self.rows_gen;
                            self.folder_cells.1 = folder.clone();
                        }
                        // 过滤词跟着目录走:换了目录就清空(资源管理器直觉,
                        // 免得在别的文件夹里对着"莫名少了一半"的网格发愣)。
                        if self.folder_filter_owner != folder {
                            self.folder_filter.clear();
                            self.folder_filter_owner = folder.clone();
                        }
                        // 文件名过滤(大小写不敏感,只匹配文件名本身,不动缓存)。
                        let needle = self.folder_filter.trim().to_lowercase();
                        let filtered = !needle.is_empty();
                        let shown: Vec<usize> = if filtered {
                            idxs.iter()
                                .copied()
                                .filter(|&i| {
                                    self.rows[i]
                                        .path
                                        .rsplit(['\\', '/'])
                                        .next()
                                        .is_some_and(|n| n.to_lowercase().contains(&needle))
                                })
                                .collect()
                        } else {
                            Vec::new()
                        };
                        let cells: &[usize] = if filtered { &shown } else { &idxs };
                        self.album_label(ui, &folder, &date, cells.len());
                        if filtered {
                            ui.label(
                                egui::RichText::new(t_with("文件名过滤:匹配 {p1} / 共 {p2} 张", &[("p1", (cells.len()).to_string()), ("p2", (idxs.len()).to_string())]))
                                .size(11.5)
                                .color(crate::theme::p().muted),
                            );
                        }
                        ui.add_space(4.0);
                        if cells.is_empty() && filtered {
                            ui.label(t_with("此文件夹里没有文件名含「{p1}」的照片。", &[("p1", (self.folder_filter.trim()).to_string())]));
                        } else if self.folder_details {
                            self.photo_details(ui, "folder-details", cells);
                        } else {
                            self.photo_grid(ui, "folder-grid", cells);
                        }
                        self.folder_cells.2 = idxs;
                        self.folder_cells.3 = date;
                    }
                    // 快速查找视图在进入本面板前已自绘并 return;留空仅为穷尽性。
                    ViewKind::Search => {}
                    ViewKind::Albums => {
                        match self.sel_album {
                            None => {
                                ui.heading(t("相簿"));
                                ui.label(
                                    t("在左侧选择或新建一个相簿。相簿是照片的命名集合,不会移动文件。"),
                                );
                            }
                            Some(a) => {
                                let name = self.store.album_name(a).unwrap_or_default();
                                ui.heading(t_with("相簿:{name}", &[("name", name.to_string())]));
                                // 相簿成员按加入顺序显示(链上 pos 序)。
                                let ids = self.store.album_photo_ids(a);
                                let idxs: Vec<usize> = ids
                                    .iter()
                                    .filter_map(|id| self.id_row.get(id).copied())
                                    .collect();
                                if idxs.is_empty() {
                                    ui.label(t("(空相簿)用照片右键菜单的「加入相簿」添加。"));
                                } else {
                                    self.photo_grid(ui, "album-grid", &idxs);
                                }
                            }
                        }
                    }
                }
            });
            // 把本帧的实际滚动偏移 / 视口高 / 内容高记下来,
            // 下一帧用来算逐帧质心位移(缩略图质心位移算法)。
            self.last_scroll_y = out.state.offset.y;
            self.last_viewport_h = out.inner_rect.height();
            self.last_content_h = out.content_size.y;
        });

        // 单图预览与编辑已迁到 editor 模块的整页视图。

        // WiFi 导入面板(M4a)
        let mut wifi_open = self.wifi_panel;
        if wifi_open {
            egui::Window::new(t("WiFi 导入"))
                .open(&mut wifi_open)
                .default_width(480.0)
                .show(ctx, |ui| {
                    let running = self
                        .wifi
                        .as_ref()
                        .map(|h| (h.addr.clone(), h.received.load(Ordering::SeqCst)));
                    match running {
                        Some((addr, n)) => {
                            ui.label(t_with("运行中:http://{addr}/", &[("addr", addr.to_string())]));
                            ui.label(t_with("已接收 {n} 张", &[("n", n.to_string())]));
                            ui.label(t("手机浏览器打开以上地址,选择照片即自动上传到本机 Incoming 目录并入库。"));
                            ui.add_space(8.0);
                            if ui.button(t("停止")).clicked() {
                                self.toggle_wifi();
                            }
                        }
                        None => {
                            ui.label(t("未启动。启动后手机需与电脑在同一局域网。"));
                            if ui.button(t("启动")).clicked() {
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

        // 工具栏「新建相簿」(有选中就同时放入照片)。
        if self.album_new_win {
            let mut open = self.album_new_win;
            let mut close = false;
            egui::Window::new(t("新建相簿"))
                .open(&mut open)
                .default_width(360.0)
                .show(ctx, |ui| {
                    let n = self.sel.len();
                    ui.small(if n == 0 {
                        t("相簿是照片 id 的命名集合,不移动文件。")
                    } else {
                        t("将把选中的照片一并放入新相簿。")
                    });
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.album_input).hint_text(t("相簿名称")),
                    );
                    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let name = self.album_input.trim().to_string();
                    let ok = (ui.button(t("创建")).clicked() || enter) && !name.is_empty();
                    ui.horizontal(|ui| {
                        if n > 0 {
                            ui.small(t_with("已选中 {n} 张", &[("n", n.to_string())]));
                        }
                        if ui.button(t("取消")).clicked() {
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
                                    t_with("已创建相簿「{name}」", &[("name", name.to_string())])
                                } else {
                                    t_with(
                                        "已创建相簿「{name}」,放入 {added} 张",
                                        &[("name", name.to_string()), ("added", added.to_string())],
                                    )
                                };
                                close = true;
                            }
                            Err(e) => {
                                self.status = t_with("创建相簿失败: {e}", &[("e", e.to_string())])
                            }
                        }
                    }
                });
            self.album_new_win = open && !close;
        }

        // 从设备导入窗口(统一设备列表)。
        if self.show_import_devices {
            let mut open = self.show_import_devices;
            egui::Window::new(t("从设备导入"))
                .open(&mut open)
                .default_width(520.0)
                .show(ctx, |ui| {
                    ui.small(
                        t("WPD 便携设备(相机/手机)+ 可移动磁盘统一列表"),
                    );
                    ui.horizontal(|ui| {
                        if ui.button(t("刷新")).clicked() {
                            self.refresh_import_devices();
                        }
                        ui.small(t_with("共 {p1} 台(便携 {p2} + 磁盘 {p3})", &[("p1", (self.import_devices.len()).to_string()), ("p2", (self.import_devices.iter().filter(|d| matches!(d, crate::wpd::DeviceEntry::Portable(_))).count()).to_string()), ("p3", (self.import_devices.iter().filter(|d| matches!(d, crate::wpd::DeviceEntry::Volume(_))).count()).to_string())]));
                    });
                    if !self.import_devices_err.is_empty() {
                        ui.colored_label(egui::Color32::RED, self.import_devices_err.clone());
                    }
                    ui.separator();
                    if self.import_devices.is_empty() {
                        ui.label(t("未发现设备。接入相机/手机(U 盘、SD 卡读卡器也可以),或点「刷新」重新枚举。"));
                        ui.small(t("提示:WPD 服务不可用时只显示盘符卷(WPD 设备枚举尽力而为)。"));
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
                                            if ui.button(t("扫描此卷")).clicked() {
                                                scan_request = Some(p.clone());
                                            }
                                        }
                                    }
                                });
                            }
                        });
                        if let Some(root) = scan_request {
                            self.start_scan_roots(vec![root], t("设备导入"));
                            self.show_import_devices = false;
                        }
                        ui.separator();
                        ui.small(t("便携设备(MTP/PTP)的照片传输需 WPD 对象流,当前版本未实现—— U 盘/读卡器请用「扫描此卷」。"));
                    }
                });
            self.show_import_devices = open;
        }

        // 邮件发送窗口(尺寸档 1600/1024/800)。
        if self.email_win {
            let mut open = self.email_win;
            egui::Window::new(t("以邮件发送"))
                .open(&mut open)
                .default_width(420.0)
                .show(ctx, |ui| {
                    ui.small(
                        t("把当前视图的照片缩放为 JPEG,交给系统默认邮件客户端(Simple MAPI 链路,MAPI 动态加载)。"),
                    );
                    ui.separator();
                    ui.label(t("附件尺寸:"));
                    for s in crate::share::ShareSize::ALL {
                        ui.radio_value(&mut self.email_size, s, t(s.label()));
                    }
                    ui.separator();
                    let n = self.view_ids().len();
                    if ui
                        .add_enabled(
                            self.share_rx.is_none(),
                            egui::Button::new(t_with("发送当前视图 {n} 张", &[("n", n.to_string())])),
                        )
                        .clicked()
                    {
                        self.start_email();
                    }
                    if self.share_rx.is_some() {
                        ui.small(t("后台准备附件/等待邮件客户端中…"));
                    }
                });
            self.email_win = open;
        }

        // 礼品光盘窗口(IMAPI2 刻录)。
        if self.cd_win {
            let mut open = self.cd_win;
            egui::Window::new(t("制作礼品光盘"))
                .open(&mut open)
                .default_width(420.0)
                .show(ctx, |ui| {
                    ui.small(t(
                        "把当前视图的照片写入光盘(走 IMAPI2 引擎;卷标沿用默认命名)。",
                    ));
                    ui.separator();
                    ui.label(t("照片尺寸:"));
                    for s in crate::share::ShareSize::ALL {
                        ui.radio_value(&mut self.cd_size, s, t(s.label()));
                    }
                    ui.separator();
                    let n = self.view_ids().len();
                    if ui
                        .add_enabled(
                            self.share_rx.is_none(),
                            egui::Button::new(t_with(
                                "刻录当前视图 {n} 张",
                                &[("n", n.to_string())],
                            )),
                        )
                        .clicked()
                    {
                        self.start_gift_cd();
                    }
                    if self.share_rx.is_some() {
                        ui.small(t("后台暂存内容/刻录中…(请放入空盘)"));
                    }
                });
            self.cd_win = open;
        }

        // 变更历史窗口(可回溯的变更日志)。
        if self.show_changes {
            let mut open = self.show_changes;
            egui::Window::new(t("变更历史"))
                .open(&mut open)
                .default_width(720.0)
                .min_height(360.0)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(t_with(
                            "共 {p1} 条记录",
                            &[("p1", (self.store.change_count()).to_string())],
                        ));
                        if ui.button(t("刷新")).clicked() {
                            self.reload_changes();
                        }
                        if ui.button(t("清空")).clicked() {
                            let _ = self.store.clear_changes();
                            self.reload_changes();
                            self.status = t("变更历史已清空").into();
                        }
                    });
                    ui.separator();
                    if self.change_log.is_empty() {
                        ui.label(t("(暂无变更记录 —— 扫描或监听到变化后会记录在这里)"));
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
                    ui.small(t("说明:变更日志持久化存储,可回溯。"));
                });
            self.show_changes = open;
        }

        // 坏文件台账:派生反复失败被立案的文件,不再每轮重试。
        // 这里给整批销案的出口 —— 网格右键只能一张一张来。
        if self.show_bad_media {
            let mut open = self.show_bad_media;
            egui::Window::new(t("坏文件台账"))
                .open(&mut open)
                .default_width(760.0)
                .min_height(320.0)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(t_with("共 {p1} 条", &[("p1", (self.bad_media_rows.len()).to_string())]));
                        if ui.button(t("刷新")).clicked() {
                            self.bad_media_rows = self.store.bad_media();
                        }
                    });
                    ui.separator();
                    if self.bad_media_rows.is_empty() {
                        ui.label(t("(台账是空的 —— 派生连续失败 3 次的文件才会立案到这里)"));
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
                    ui.small(t("立案的文件不再自动重试缩略图。网格右键单个文件可「不隐藏」,或在这里整批销案。"));
                    if ui.button(t("全部销案(重新排队派生)")).clicked() {
                        let n = self.bad_media_rows.len();
                        for b in std::mem::take(&mut self.bad_media_rows) {
                            let _ = self.store.unmark_bad_media(&b.path);
                        }
                        self.rebuild();
                        self.status = t_with("已销案 {n} 条,后台会重新尝试派生", &[("n", n.to_string())]);
                    }
                });
            self.show_bad_media = open;
        }

        // 属性窗口。
        if self.show_props {
            self.props_ui(ctx);
        }
        // 选项窗口。
        if self.show_options {
            self.options_ui(ctx);
        }
        self.tag_batch_window(ctx);

        // 按 uid64 定位照片窗口(外部工具可用持久标识引用照片)。
        if self.show_uid_lookup {
            let mut open = self.show_uid_lookup;
            egui::Window::new(t("按唯一标识(uid64)查找"))
                .open(&mut open)
                .default_width(520.0)
                .show(ctx, |ui| {
                    ui.label(t(
                        "uid64 是照片的持久唯一标识(内容+路径派生,重新扫描不变)。",
                    ));
                    ui.horizontal(|ui| {
                        let mut input = self.uid_input.clone();
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut input)
                                    .hint_text(t("十进制或 0x 十六进制")),
                            )
                            .changed()
                        {
                            self.uid_input = input.clone();
                        }
                        if ui.button(t("查找")).clicked() {
                            self.uid_lookup_result = Some(self.lookup_uid());
                        }
                    });
                    match &self.uid_lookup_result {
                        Some(Ok(path)) => {
                            ui.separator();
                            ui.label(egui::RichText::new(path).strong());
                            if ui.button(t("在资源管理器中显示")).clicked() {
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
                egui::Window::new(t("OCR 识别结果"))
                    .open(&mut ocr_open)
                    .default_width(420.0)
                    .show(ctx, |ui| {
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            if self.ocr_results.is_empty() {
                                ui.label(t("(未识别到文本)"));
                            }
                            for t in &self.ocr_results {
                                ui.label(t_with(
                                    "{p1}   置信度 {p2}",
                                    &[
                                        ("p1", (t.text).to_string()),
                                        ("p2", format!("{p2:.2}", p2 = (t.conf))),
                                    ],
                                ));
                            }
                        });
                    });
            }
            self.ocr_panel = ocr_open;
        }

        // 帮助 > 键盘快捷键
        if self.show_shortcuts {
            let mut open = self.show_shortcuts;
            egui::Window::new(t("键盘快捷键"))
                .open(&mut open)
                .default_width(380.0)
                .show(ctx, |ui| {
                    ui.label(t("Ctrl+A        全选当前视图的照片"));
                    ui.label(t("Ctrl+点击     选中 / 取消选中单张照片"));
                    ui.label(t("F2            保存(烘培)当前预览照片"));
                    ui.label(t("Ctrl+R        预览中顺时针旋转 90°(自定)"));
                    ui.label(t("Ctrl+Shift+R  预览中逆时针旋转 90°(自定)"));
                    ui.label(t("← / →         预览上一张 / 下一张"));
                    ui.label(t("Esc           关闭预览"));
                    ui.label(t("右键          照片上下文菜单"));
                });
            self.show_shortcuts = open;
        }

        // 帮助 > 关于
        if self.show_about {
            let mut open = self.show_about;
            egui::Window::new(t("关于 Maliang"))
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
                            // 英文品牌名
                            ui.label(egui::RichText::new("Maliang").size(18.0).strong());
                            ui.add_space(2.0);
                            // 主标语:大而醒目
                            ui.label(
                                egui::RichText::new(t("神笔马良,本地照片 AI 管家"))
                                    .size(24.0)
                                    .strong(),
                            );
                            // 副标语:比主标语小,但仍醒目
                            ui.label(
                                egui::RichText::new(t("秒查速览、智能标签、对话搜图、一键搞定"))
                                    .size(15.0)
                                    .strong(),
                            );
                            ui.add_space(4.0);
                            ui.label(t_with(
                                "当前版本 {p1} · Rust + egui 编写 · Apache 2.0 开源",
                                &[("p1", (env!("CARGO_PKG_VERSION")).to_string())],
                            ));
                        });
                    });
                    ui.separator();
                    ui.heading(t("更新日志"));
                    egui::ScrollArea::vertical()
                        .id_salt("about_changelog")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            egui_commonmark::CommonMarkViewer::new().show(
                                ui,
                                &mut self.about_cache,
                                about_changelog_md(),
                            );
                        });
                });
            self.show_about = open;
        }

        // 帮助 > 检查更新(自动发现新版本时也会把这个窗口推到前台)
        self.update_window(ctx);
        self.update_confirm_window(ctx);

        // 文件夹管理器:监听文件夹列表 + 添加/全扫/清理 + 每行重新扫描/移除
        if self.show_folder_mgr {
            let mut open = self.show_folder_mgr;
            let mut close = false;
            egui::Window::new(t("文件夹管理器"))
                .open(&mut open)
                .default_width(660.0)
                .min_height(340.0)
                .show(ctx, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if ui.button(t("添加文件夹…")).clicked() {
                            self.add_folder_to_library();
                            close = true;
                        }
                        if ui.button(t("全部重新扫描")).clicked() {
                            let roots: Vec<PathBuf> =
                                self.watched.iter().map(|(p, _)| PathBuf::from(p)).collect();
                            if roots.is_empty() {
                                self.status = t("图库中还没有监听任何文件夹").into();
                            } else {
                                self.start_scan_roots(roots, t("全部文件夹"));
                                close = true;
                            }
                        }
                        if ui.button(t("清理已缺失文件")).clicked() {
                            self.clean_missing();
                            close = true;
                        }
                    });
                    ui.separator();
                    ui.small(t("图标颜色:灰 = 默认目录,蓝 = 导入目录"));
                    if self.watched.is_empty() {
                        ui.label(t("(空)用上面的「添加文件夹…」把图片文件夹加入图库。"));
                    }
                    let watched = self.watched.clone();
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for (path, count) in watched {
                            let exists = Path::new(&path).is_dir();
                            let imported = self
                                .store
                                .dir_intent(&path)
                                .is_some_and(|d| d.state == crate::db::INTENT_IMPORTED);
                            let color = if imported {
                                crate::theme::p().accent
                            } else {
                                crate::theme::p().muted
                            };
                            ui.horizontal(|ui| {
                                ui.add(folder_icon_colored(color));
                                ui.vertical(|ui| {
                                    ui.label(egui::RichText::new(&path).strong());
                                    ui.small(if exists {
                                        t_with(
                                            "{count} 张照片 · 状态正常",
                                            &[("count", count.to_string())],
                                        )
                                    } else {
                                        t_with(
                                            "{count} 张照片 · 文件夹已不在磁盘上(可清理缺失索引)",
                                            &[("count", count.to_string())],
                                        )
                                    });
                                });
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.button(t("从图库移除…")).clicked() {
                                            self.confirm_folder_remove = Some(path.clone());
                                            close = true;
                                        }
                                        if ui.button(t("重新扫描")).clicked() {
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
                    ui.small(t(
                        "提示:移除只删除数据库索引,磁盘文件原样保留;重新扫描会再次加入。",
                    ));
                    // 扫描排除规则 + 监听模式。
                    let excl = crate::scan::Exclusions::from_store(&self.store);
                    ui.separator();
                    ui.label(t("扫描时排除的目录"));
                    ui.small(t(
                        "系统目录、应用缓存、回收站一类不会被收录;可在这里增删规则。",
                    ));
                    let mut to_remove: Option<String> = None;
                    egui::ScrollArea::vertical()
                        .max_height(120.0)
                        .show(ui, |ui| {
                            for name in excl.names() {
                                ui.horizontal(|ui| {
                                    ui.label(name);
                                    // 内置默认项不可删。
                                    let is_default = crate::scan::Exclusions::default()
                                        .names()
                                        .iter()
                                        .any(|d| d.eq_ignore_ascii_case(name));
                                    if is_default {
                                        ui.small(t("(默认)"));
                                    } else if ui.button(t("移除")).clicked() {
                                        to_remove = Some(name.clone());
                                    }
                                });
                            }
                        });
                    // 本程序自己写缩略图的目录:它按完整路径排除,不属于上面那张
                    // 名字表,也没有「移除」—— 用户搬了缓存位置,这里跟着变。
                    let own = excl.own_prefixes().to_vec();
                    if !own.is_empty() {
                        ui.small(t("以下目录由本程序自己写入,始终不会收录:"));
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
                                egui::TextEdit::singleline(&mut input)
                                    .hint_text(t("目录名,如 Cache")),
                            )
                            .changed()
                        {
                            self.exclude_input = input.clone();
                        }
                        if ui.button(t("添加排除目录")).clicked() {
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
                        ui.small(t_with(
                            "文件夹监听模式:{p1}",
                            &[("p1", (mode.label()).to_string())],
                        ));
                    }
                });
            self.show_folder_mgr = open && !close;
        }

        // 文件夹管理器 > 从图库移除(二次确认;只删索引,不删文件)
        if let Some(folder) = self.confirm_folder_remove.clone() {
            let mut open = true;
            egui::Window::new(t("从图库中移除文件夹"))
                .open(&mut open)
                .collapsible(false)
                .default_width(460.0)
                .show(ctx, |ui| {
                    ui.label(t_with(
                        "确定把「{folder}」及其子文件夹下的所有照片从图库索引中移除吗?",
                        &[("folder", folder.to_string())],
                    ));
                    ui.label(t("只移除索引记录,不会删除磁盘上的任何文件。"));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button(t("移除")).clicked() {
                            self.remove_folder_from_library(&folder);
                            self.confirm_folder_remove = None;
                        }
                        if ui.button(t("取消")).clicked() {
                            self.confirm_folder_remove = None;
                        }
                    });
                });
            if !open {
                self.confirm_folder_remove = None;
            }
        }

        // 选项页点了别的识别方案:确认后切换;向量不通用,提示要重建。
        #[cfg(feature = "ai")]
        if let Some(pend) = self.face_algo_pending {
            let mut open = true;
            let mut decision: u8 = 0; // 1=切换并重建 3=取消
            egui::Window::new(t("切换人脸识别方案"))
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .default_width(480.0)
                .show(ctx, |ui| {
                    ui.label(t_with("切换到「{p1}」后,现有的人脸特征向量与簇不通用,需要重建。", &[("p1", (pend.label()).to_string())]));
                    ui.small(t("确认后清除全部人脸数据(含人物命名)并按新方案重新扫描;取消则维持当前方案,不混用新旧结果。"));
                    face_wipe_warning(ui, &t("切换并重建"));
                    ui.add_space(6.0);
                    ui.label(t("扫描范围:"));
                    ui.radio_value(&mut self.face_scope_common, false, t("全盘"));
                    ui.radio_value(&mut self.face_scope_common, true, t("只常用目录"));
                    ui.small(t("常用目录 = 我的文档/图片/视频/桌面 + 你导入的文件夹;其余照片保持未分析,以后选全盘可补扫。"));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button(t("切换并重建")).clicked() {
                            decision = 1;
                        }
                        if ui.button(t("取消")).clicked() {
                            decision = 3;
                        }
                    });
                });
            if decision > 0 || !open {
                self.face_algo_pending = None;
            }
            match decision {
                1 => match crate::faces::wipe_faces(&self.store) {
                    Ok(()) => {
                        let _ = self.store.set_setting("face_algo", pend.key());
                        let tag = crate::faces::algo_tag(&self.store);
                        let _ = self.store.set_setting("face_algo_version", &tag);
                        self.refresh_people_and_suggestions();
                        if crate::ai::face::Faces::models_present(pend) {
                            crate::faces::set_scan_scope(&self.store, self.face_scope_common);
                            self.start_face_scan();
                            self.status = t_with(
                                "已切换到 {p1},现有人脸数据已清除,正在按新方案重新扫描",
                                &[("p1", (pend.label()).to_string())],
                            );
                        } else {
                            self.status = t_with("已切换到 {p1},人脸数据已清除;模型未就位,请到 选项 > AI 模型 下载后再扫描", &[("p1", (pend.label()).to_string())]);
                        }
                    }
                    Err(e) => {
                        self.status = t_with("切换失败: {p1}", &[("p1", e.to_string())]);
                    }
                },
                _ => {}
            }
        }

        // 人脸管线签名不一致:旧结果不参与比对,等用户确认重建。
        #[cfg(feature = "ai")]
        if self.face_version_warn {
            let mut open = self.face_version_warn;
            egui::Window::new(t("人脸数据需要更新"))
                .open(&mut open)
                .collapsible(false)
                .default_width(480.0)
                .show(ctx, |ui| {
                    ui.label(t("人脸数据与当前管线不一致(权重或处理版本已变化),旧结果暂不参与检索、聚类和自动归档。"));
                    ui.label(t("确认后清除全部人脸数据并按当前方案重新扫描;稍后处理则继续保持待重建,不会自动覆盖人工命名。"));
                    face_wipe_warning(ui, &t("清除并重新扫描"));
                    ui.add_space(6.0);
                    ui.label(t("扫描范围:"));
                    ui.radio_value(&mut self.face_scope_common, false, t("全盘"));
                    ui.radio_value(&mut self.face_scope_common, true, t("只常用目录"));
                    ui.small(t("常用目录 = 我的文档/图片/视频/桌面 + 你导入的文件夹;其余照片保持未分析,以后选全盘可补扫。"));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button(t("清除并重新扫描")).clicked() {
                            match crate::faces::wipe_faces(&self.store) {
                                Ok(()) => {
                                    let tag = crate::faces::algo_tag(&self.store);
                                    let _ = self.store.set_setting("face_algo_version", &tag);
                                    self.face_version_warn = false;
                                    crate::faces::set_scan_scope(&self.store, self.face_scope_common);
                                    self.start_face_scan();
                                }
                                Err(e) => {
                                    self.status = t_with("重建失败: {p1}", &[("p1", e.to_string())]);
                                }
                            }
                        }
                        if ui.button(t("稍后处理")).clicked() {
                            self.face_version_warn = false;
                            self.status = t("人脸数据保持待重建,暂不参与检索与归档;可在选项页确认清除后重新扫描").to_string();
                        }
                    });
                });
            self.face_version_warn = open;
        }

        // 初次设置向导:服务授权说明与首启范围两问合在一窗、分步呈现,
        // 不再两个框叠在屏幕中央。只欠其一问时仍是单窗样式;
        // 页序固定:先服务说明(可关可拒),后照片范围(必答一次,无取消)。
        #[cfg(windows)]
        let svc_page_open = self.svc_ask_grant;
        #[cfg(not(windows))]
        let svc_page_open = false;
        if svc_page_open || self.scope_ask {
            let mut open = true;
            #[cfg_attr(not(windows), allow(unused_mut))]
            let mut decided_svc: Option<SvcWant> = None;
            let mut decided_scope: Option<InitialScope> = None;
            let two_pages = svc_page_open && self.scope_ask;
            let on_scope_page = !svc_page_open || self.wizard_page >= 1;
            let title = if on_scope_page {
                t("选择照片范围")
            } else {
                t("启用全盘快速查找")
            };
            // 系统对话框式:屏幕居中、无原生标题条(拖不了就对了);配色自绘,
            // 推荐档绿底,拒绝档深蓝。标题与页码随当前步变化。
            egui::Window::new(title)
                .title_bar(false)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .default_width(580.0)
                .show(ctx, |ui| {
                    let (hdr, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 36.0),
                        egui::Sense::hover(),
                    );
                    ui.painter()
                        .rect_filled(hdr, egui::CornerRadius::same(6), Self::GRANT_GREEN);
                    ui.painter().text(
                        egui::pos2(hdr.center().x, hdr.center().y),
                        egui::Align2::CENTER_CENTER,
                        title,
                        egui::FontId::proportional(17.0),
                        egui::Color32::WHITE,
                    );
                    // 关闭钮只在服务说明页画:照片范围页必须答一次,不给跳过。
                    if !on_scope_page {
                        let x_rect = egui::Rect::from_center_size(
                            egui::pos2(hdr.right() - 20.0, hdr.center().y),
                            egui::vec2(28.0, 28.0),
                        );
                        let x = ui.interact(x_rect, ui.id().with("wizard-x"), egui::Sense::click());
                        // ✕ 字符默认字体缺字形,直接画两笔叉更稳。
                        let xc = x_rect.center();
                        let xr = 5.0;
                        let x_stroke = egui::Stroke::new(
                            if x.hovered() { 2.2_f32 } else { 1.6_f32 },
                            egui::Color32::WHITE,
                        );
                        ui.painter().line_segment(
                            [
                                egui::pos2(xc.x - xr, xc.y - xr),
                                egui::pos2(xc.x + xr, xc.y + xr),
                            ],
                            x_stroke,
                        );
                        ui.painter().line_segment(
                            [
                                egui::pos2(xc.x - xr, xc.y + xr),
                                egui::pos2(xc.x + xr, xc.y - xr),
                            ],
                            x_stroke,
                        );
                        if x.clicked() {
                            open = false;
                        }
                    }
                    ui.add_space(14.0);
                    if two_pages {
                        let step = if on_scope_page { 2 } else { 1 };
                        ui.small(t_with(
                            "第 {step} 步,共 2 步",
                            &[("step", step.to_string())],
                        ));
                        ui.add_space(4.0);
                    }
                    if on_scope_page {
                        self.scope_ask_body(ui, &mut decided_scope);
                    } else {
                        #[cfg(windows)]
                        self.svc_grant_body(ui, &mut decided_svc);
                    }
                });
            #[cfg(windows)]
            let svc_decided = decided_svc;
            #[cfg(not(windows))]
            let svc_decided: Option<SvcWant> = None;
            if let Some(g) = svc_decided {
                let checked = self.svc_never_ask;
                match g {
                    // 没勾「不再提示」的拒绝不算决定:不落意向,下次启动继续问。
                    SvcWant::No if !checked => {
                        self.link_declined = false;
                        let _ = self.store.set_setting("svc_declined", "0");
                        self.status =
                            t("本次跳过全盘快查,下次启动会再问(勾选「不再提示」可免打扰)。").into();
                    }
                    g => {
                        #[cfg(windows)]
                        self.svc_apply_want(g);
                        if matches!(g, SvcWant::No) && checked {
                            let _ = self.store.set_setting("svc_never_ask", "1");
                            self.status = t(
                                "已选「不再提示」:以后启动不再询问全盘快查(入口仍在,随时可启用)。",
                            )
                            .into();
                        }
                    }
                }
                // 服务一问本会话已答完;向导还有下一页就翻过去。
                self.svc_ask_grant = false;
                if self.scope_ask {
                    self.wizard_page = 1;
                }
            } else if !open {
                // 直接关窗 = 还没决定:勾了「不再提示」则尊重落库,否则下次启动再问。
                if self.svc_never_ask {
                    let _ = self.store.set_setting("svc_never_ask", "1");
                    self.status =
                        t("已选「不再提示」:以后启动不再询问全盘快查(入口仍在,随时可启用)。")
                            .into();
                }
                self.svc_ask_grant = false;
                if self.scope_ask {
                    self.wizard_page = 1;
                }
            }
            if let Some(c) = decided_scope {
                self.apply_initial_scope(c, true);
            }
            if !self.svc_ask_grant && !self.scope_ask {
                // 向导走完:复位页码,下次(重装 / 新库)首启从头开始。
                self.wizard_page = 0;
            }
        }

        // 编辑 > 从图库中移除(二次确认;只删索引,不删文件)
        if self.confirm_remove {
            egui::Window::new(t("从图库中移除"))
                .collapsible(false)
                .default_width(400.0)
                .show(ctx, |ui| {
                    ui.label(t_with(
                        "确定把选中的 {p1} 张照片从图库索引中移除吗?",
                        &[("p1", (self.sel.len()).to_string())],
                    ));
                    ui.label(t("只移除索引记录,不会删除磁盘上的原文件。"));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button(t("移除")).clicked() {
                            self.remove_selection();
                            self.confirm_remove = false;
                        }
                        if ui.button(t("取消")).clicked() {
                            self.confirm_remove = false;
                        }
                    });
                });
        }
    }
}
