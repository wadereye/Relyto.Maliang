# Relyto.Maliang（马良）

> 用 AI 重新诠释 Picasa 的本地照片管理应用。

Maliang（马良）是一个面向桌面端的本地照片管理工具，目标是把经典的
Picasa 3 的体验现代化：更顺滑的浏览、更智能的组织，以及原生、离线、
隐私优先的 AI 能力。全部数据保存在你自己的机器上，不上云、不扫描你的
隐私上传。

项目目前处于早期开发阶段（v0.1.0），核心浏览、索引与缩略图链路已可运行。

## 功能规划

### 已实现 / 进行中
- **照片索引与浏览**：扫描本地目录，建立 SQLite 索引，支持时间线 / 文件夹视图。
- **多级缩略图**：后台常驻 worker，按 Small96 / Normal256 / Big512 / Preview1024
  四档生成与缓存，界面流畅不卡顿。
- **时间线视图**：按拍摄日期聚合，参照 Picasa 的视图与排序方式。
- **文件夹视图**：监控目录变化（事件去抖 + 轮询 + 基线三档门控），自动增量更新索引。
- **标签 / 星标**：读写经典 `picasa.ini`，与原版 Picasa 互通。
- **重复检测**：基于 dHash 的感知哈希，找出视觉近似的照片（超出原版能力）。
- **EXIF 与 GPS**：读取拍摄参数、经纬度，支持 KML 导出。
- **人脸与相簿**：人脸检测、模板匹配、聚类与命名（随 `ai` 特性默认启用）。
- **编辑与一键增强**：曲线（自然三次样条）、曝光 / 黑白 / 颗粒等效果；
  一键增强（oneclick）除黑白、棕褐、颗粒、锐化外，另含自主设计的
  暖调、冷调、暗角、褪色，以及伽马、线性模糊、焦点柔化、焦点去色、
  方向性亮度 / 饱和度 / 锐度。效果参数集中在资源包中，便于调校。
- **风格配方**：28 个成组滤镜（浓郁 / 画框 / 投影 / 中心马赛克 / 变焦拖影 /
  胶片颗粒 / 强动态 / 热成像 / 玩具相机 / 红外胶片 / 局部对比 / 鲜艳暗角 /
  边缘提亮 / 装裱卡纸 / 霓虹 / 夜视仪 / 马赛克 / 一次成像 / 海报色阶 / 圆角 /
  单色洗 / 双色调 / 漫画 / 柔光 / 铅笔素描 / 交叉冲印 / 宽银幕 / 六十年代）。
  每个配方是一串固定顺序的像素操作，强度统一控制与原图的混合比例；
  另有一组可复用的基础算子（混合模式、遮罩、盒式模糊、线性模糊、渐变映射、
  边缘检测、变焦模糊、色阶压缩、边框、投影、旋转、方向性调整等）。
- **设备导入**：通过 WPD / WIA 接入相机、手机等便携设备（Windows）。
- **视频支持**：识别常见视频格式，抽取缩略图，调用系统播放器播放。
- **XMP 镜像**：将编辑与元数据镜像为 XMP，便于与其他工具互通。
- **导出 / 保存**：带编辑参数烘焙导出，保留原始文件。

### AI 能力（默认启用 `ai` 特性）
- OCR 文字识别
- 人脸自动归类建议与半自动聚类
- CLIP 语义搜索（"找有猫的照片"）

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

本项目不包含 Picasa 的任何二进制文件、源代码或专有资源文件；对 Picasa 界面与
交互方式的参照仅用于提供熟悉的使用体验与数据互通（如 `picasa.ini`）。

## 第三方声明

本项目参考了 OpenAI CLIP（MIT 许可）的 BPE 分词器实现，完整归属声明与许可
原文见 [THIRD_PARTY_NOTICES.md](./THIRD_PARTY_NOTICES.md)。

仓库**不分发**任何模型文件或权重；运行时所需的 ONNX 模型与 BPE 合并表由用户
自行获取后放入本地模型目录。

## 协议

本项目以 [Apache License 2.0](./LICENSE) 发布。
