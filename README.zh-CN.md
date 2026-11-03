<div align="right">

|                        |      |
| ---------------------- | ---- |
| [English](./README.md) | 简体中文 |

</div>

# Relyto.Maliang（马良）

> 用 AI 参考 Picasa 3 的 Picasa 的本地照片管理应用。

**为什么有这个项目。** Google Picasa 曾是一款非常好用的图片管理与处理  
软件——快速、简洁、用起来赏心悦目。可惜 Google 在 2016 年将它停更下架，  
此后一直没有一款现代工具能真正填补这个空缺。因此，本项目决定用 Rust  
参考 Picasa 3 的 并开源，让这份经典体验延续下去。

如果马良对你有用，欢迎给仓库点一个  
[⭐ Star](https://github.com/wadereye/Relyto.Maliang/stargazers)；  
如果有缺失的功能或遇到问题，欢迎提  
[Issue](https://github.com/wadereye/Relyto.Maliang/issues)。  
功能需求随时欢迎。

Maliang（马良）是一个面向桌面端的本地照片管理工具，目标是把经典的  
Picasa 3 的体验现代化：更顺滑的浏览、更智能的组织，以及原生、离线、  
隐私优先的 AI 能力。全部数据保存在你自己的机器上，不上云、不扫描你的  
隐私上传。

项目已发布 v0.1（2026-10-31），v0.2 正在收尾。各版本的新增内容见  
[更新日志](./CHANGELOG.md)。

## 功能总览

### 浏览与整理

- **全盘扫描索引**：把文件夹加入图库后自动建立本地索引（SQLite）；  
  文件夹优先，绝不复制或移动原图。
- **多种视图**：时间线（按拍摄日期聚合）/ 文件夹 / 相簿（命名集合，不动文件）。
- **左栏目录树**：真实递归展开的目录树，扫描时显示进度百分比。
- **多级缩略图**：按 96 / 256 / 512 / 1024 四档按需生成与缓存，浏览流畅。
- **自动更新**：运行中靠文件系统事件实时跟新；程序没开着时的增删，  
  下次启动自动对账补齐。
- **幻灯片放映**：放映选中照片或整个当前视图。
- **排序与缩略图大小**：按拍摄日期 / 文件名、正反序，普通 / 小两档。

### 快速查找

- **全盘文件名索引**：第一次使用时为整块硬盘的文件建立名字索引  
  （百万级文件约几秒），之后搜照片、视频、文档都是毫秒级出结果。
- **按类型过滤**：全部 / 图片（含 RAW）/ 视频 / 音频 / 压缩包 / 可执行 / 文档。
- **顶栏全局查找框**：随手输入文件名即可进入快速查找。
- **一次授权、后台静默更新**：管理员授权注册为计划任务后，索引随开机  
  自动保鲜；随时可在 选项 > 常规 撤销。

### 编辑与效果

- **预览编辑**：旋转、裁剪、缩放平移、曲线、曝光、对比度、色温、色调、  
  锐化、模糊、颗粒、暗角等，全部非破坏性；F2 烘焙保存才落盘。
- **一键效果**：自动对比 / 自动颜色 / 暖调 / 冷调 / 正片 / LOMO / 逆光补偿等  
  数十种，行为对齐原版 Picasa。
- **风格滤镜**：数十个成组配方——胶片颗粒、玩具相机、一次成像、宽银幕、  
  六十年代、铅笔素描、交叉冲印、双色调、霓虹、夜视、装裱卡纸、圆角……
- **着色器效果**：六边形单元、块状波纹、圆形泼溅、晶格化、瀑布、HSL 分通道。

### AI 能力（默认启用 `ai` 特性，可精简关闭）

- **OCR 文字识别**：图片里的文字可以被搜到。
- **人脸整理**：检测、聚类、质量评分、半自动命名建议、忽略簇。
- **CLIP 语义搜索**：中英双语，「找有猫的照片」这类描述性搜索。

### 导入与导出

- **WiFi 手机导入**：手机浏览器访问电脑地址，把照片传进 Incoming。
- **设备导入**：USB 接入相机、手机等便携设备（WPD / WIA，Windows）。
- **导出与烘焙**：按当前视图范围导出，编辑参数一并烘进成品；原图始终不动。
- **视频**：识别常见格式、抽取缩略图、调用系统播放器。

### 管理与互操作

- **重复照片检测**：感知哈希找出视觉近似的照片（超出原版能力）。
- **彩色标签 / 星标**：读写经典 `picasa.ini`，与原版 Picasa 互通。
- **XMP 镜像**：编辑与元数据镜像为 XMP，便于与其他工具互通。
- **EXIF / GPS**：读取拍摄参数与经纬度，支持 KML 地理标记导出。
- **文件夹管理器**：增删监听目录、逐个重扫、清理已缺失文件。
- **索引备份与压缩**；**数据目录可迁移**（缩略图 / 索引 / 模型一并跟走）。

### 界面与体验

- 深色 / 浅色双主题；内置阿里普惠体中文字体；图标使用开源 Lucide。
- 菜单结构参照 Picasa 3（文件 / 编辑 / 视图 / 工具 / 帮助）；照片原生右键菜单。
- 右栏信息抽屉：属性 / 标签 / 人物 / 地点。
- **全本地、无联网、无遥测。**

### 有意不做（范围外）

图片精修编辑链（retouch / picnik）、拼贴 / 海报 / CD 刻录、在线分享  
（Picasa Web / 社交平台 / 邮件 / 打印）、TWAIN 相机扫描、相簿系统云同步、  
Picasa2 迁移。

## 技术栈

- **语言**：Rust 2021
- **UI**：[egui](https://github.com/emilk/egui) / [eframe](https://github.com/emilk/egui) 0.33（wgpu 渲染）
- **数据库**：[rusqlite](https://github.com/rusqlite/rusqlite)（bundled SQLite）
- **系统交互**：`windows` crate（WPD / WIA / COM，Windows 平台）
- **图像**：`image` 0.25（JPEG / PNG / WebP / TIFF / BMP / GIF）

## 编译方法

### 环境要求

- Windows 10/11（本项目当前仅面向 Windows，依赖 `windows` crate 的设备导入能力）
- [Rust 工具链](https://www.rust-lang.org/tools/install)（stable，建议较新版本）
- 构建过程需要从 crates.io 下载依赖，请确保网络可用

### 标准构建

```bash
# 克隆（如需）
git clone https://github.com/wadereye/Relyto.Maliang
cd Relyto.Maliang

# 调试构建
cargo build

# 发布构建（推荐，开启 LTO 与单 codegen unit）
cargo build --release

# 运行
cargo run            # 或运行 target/release/maliang.exe
```

### 精简构建（不含 AI）

`ai` 特性会引入 ONNX Runtime 与模型文件（OCR / 人脸 / CLIP），**默认已启用**：

```bash
cargo build --release          # 默认 = 含 ai
```

不想要这份体积与依赖时用：

```bash
cargo build --release --no-default-features
```

精简构建下「人物」与「语义搜索」视图会提示未编译，其余功能不受影响。  
启用前请确保已配置 `onnxruntime` 运行环境与对应的模型权重文件。

### 常用命令

```bash
cargo check          # 仅类型检查（快）
cargo test           # 运行测试（默认含 AI 模块测试）
cargo test --no-default-features   # 精简构建的测试
```

### 构建排错（Windows）

- 依赖 `windows` crate 与 COM，请使用官方 Rust 安装器提供的原生 MSVC 工具链。
- 若 C 盘空间紧张导致 `cargo` 临时文件报错，可指定临时目录：
  ```powershell
  $env:TMP = "D:\tmp-maliang"; $env:TEMP = "D:\tmp-maliang"
  cargo build --release
  ```
- 首次构建会编译大量依赖，耗时较长，请耐心等待。

## 目录结构

```
Relyto.Maliang/
├── Cargo.toml          # 包定义与依赖
├── Cargo.lock
├── assets/
│   └── presets.dat     # 内置效果参数资源包（编译期内嵌，可用命令行覆盖）
├── src/
│   ├── main.rs         # 入口（UI 线程提权、打开索引库、启动 eframe）
│   ├── app.rs          # 主界面（视图 / 排序 / 文件夹管理器）
│   ├── scan.rs         # 目录扫描与索引
│   ├── thumbs.rs       # 缩略图生成与缓存
│   ├── timeline.rs     # 时间线聚合
│   ├── watch.rs        # 目录变化监控
│   ├── db.rs           # SQLite 存储层
│   ├── pool.rs         # 字符串池（列式存储）
│   ├── effects.rs / curves.rs / oneclick.rs  # 编辑与增强
│   ├── recipes.rs                            # 成组滤镜配方与基础像素算子
│   ├── assets.rs / assets_format.rs           # 内置参数资源包的读取与格式
│   ├── faces.rs / clip.rs / ai.rs            # AI 能力（ai 特性）
│   ├── device.rs / wpd.rs / win.rs           # 设备导入与系统交互
│   ├── video.rs / xmp.rs / export.rs / save.rs
│   └── ...
└── target/             # 构建产物（已 gitignore）
```

## 内置效果参数（资源包）

曲线预设、亮度权重、黑白 / 双色调 / 颗粒的默认参数、模糊形状系数、滚动阻尼  
常量等**可调数值**不写在源码里，而是集中存放于 `assets/presets.dat`——  
一个带 magic 与版本号的小端二进制容器（段名 + f64 数值列表）。这样调整观感  
只需改数据、不必改代码。

程序启动时的取值顺序：

1. 用户目录下的 `%APPDATA%\Maliang\presets.dat`（若存在且格式正确）——可覆盖；
2. 编译期内嵌的 `assets/presets.dat`（始终可用，作为兜底）。

文本与二进制之间可以互转：

```bash
# 导出当前生效的资源包为可读文本
cargo run -- --export-assets my-presets.txt

# 编辑后写回用户目录（下次启动生效）
cargo run -- --import-assets my-presets.txt
```

资源包损坏时会自动回退到内嵌副本，不会导致程序无法启动。

## 商标与免责声明

Picasa 是 Google LLC 的商标。本项目是一个**独立的开源实现**，与 Google LLC  
没有任何关联，也未获得其授权、赞助或背书。

本项目是一个自行实现的独立实现，不包含 Picasa 的源代码、二进制文件或资源  
文件构建；界面与交互方式上的相近之处，意在延续熟悉的使用体验，并支持与  
Picasa 的数据互通（如经典的 `picasa.ini` 文件格式）。

## 第三方声明

本项目自身代码以 Apache License 2.0 发布，此外还包含或参考下列第三方内容。各组件的完整归属声明与许可原文统一收录于  
[THIRD_PARTY_NOTICES.md](./THIRD_PARTY_NOTICES.md)；所有第三方组件均按其原始许可条款使用与分发，本项目的许可不替代、也不改变这些条款。

- **OpenAI CLIP**（MIT 许可）：`src/clip.rs` 中的 BPE 分词器实现参考了其代码；
- **Cargo 依赖**（egui / eframe / wgpu / image / rusqlite / windows 等）：均为 MIT、Apache-2.0 或 BSD 系许可，与本项目的发布协议兼容；
- **模型与权重**：仓库不分发任何模型文件或权重，运行时所需资源由用户自行获取并放入本地模型目录，相关权利归各自权利人所有。

## 协议

本项目自身代码以 [Apache License 2.0](./LICENSE) 发布；第三方组件遵循各自的原始许可条款，详见[第三方声明](#第三方声明)与 [THIRD_PARTY_NOTICES.md](./THIRD_PARTY_NOTICES.md)。  
