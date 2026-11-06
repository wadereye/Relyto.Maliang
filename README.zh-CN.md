
| [English](./README.md) | 简体中文 |

# Relyto.Maliang **神笔马良,本地照片 AI 管家**
##    **秒查速览、智能标签、对话搜图、一键搞定**

**为什么有这个项目？** Google Picasa 曾是一款非常好用的图片管理与处理软件——快速、简洁、用起来赏心悦目。可惜 Google 在 2016 年将它停更下架，此后也有很多图片处理软件，但我一直没找到自己喜欢的，因此有了神笔马良：用Rust独立编写、开源发布，把这份经典体验延续下去，并补上当年老工具没有的能力。
软件定位是一个面向桌面端的本地照片管理工具，目标是把经典的Picasa 3 的体验现代化：更顺滑的浏览、更智能的组织，以及原生、离线、隐私优先的 AI 能力。全部数据保存在你自己的机器上，不上云、不扫描你的隐私上传。

如果马良对你有用，欢迎给仓库点一个[⭐ Star](https://github.com/wadereye/Relyto.Maliang/stargazers)； 

如果有缺失的功能或遇到问题，欢迎提[Issue](https://github.com/wadereye/Relyto.Maliang/issues)。  

如果有新功能需求，或者要加群沟通，欢迎邮件联系我，或者在页面底部扫描查看我的微信公众号，点「AI交流」菜单，或发「加群」请求，我拉你入群，大家共建共享，共同进步。

## [更新日志](./CHANGELOG.md)

## 功能总览（已实现）

### 浏览与整理

- **全盘扫描索引**：把文件夹加入图库后自动建立本地索引（SQLite）,文件夹优先，绝不复制或移动原图。
- **多种视图**：时间线（按拍摄日期聚合）/ 文件夹 / 相簿（命名集合，不动文件）。
- **左栏目录树**：真实递归展开的目录树，扫描时显示进度百分比。
- **多级缩略图**：按 96 / 256 / 512 / 1024 四档按需生成与缓存，浏览流畅。
- **自动更新**：运行中靠文件系统事件实时更新；程序没开着时的增删，下次启动自动对账补齐。
- **幻灯片放映**：放映选中照片或整个当前视图。
- **排序与缩略图大小**：按拍摄日期 / 文件名、正反序，普通 / 小两档。

### 快速查找

- **全盘文件名索引**：第一次使用时为整块硬盘的文件建立名字索引,（百万级文件约几秒），之后搜照片、视频、文档都是毫秒级出结果。
- **按类型过滤**：全部 / 图片（含 RAW）/ 视频 / 音频 / 压缩包 / 可执行 / 文档。
- **全局文件查找框**：随手输入文件名即可进入快速查找，百万文件秒查返回结果。
- **一次授权、后台静默更新**：管理员授权注册为计划任务后，索引随开机启动，自动保鲜；随时可在 选项 > 常规 撤销。

### 编辑与效果

- **预览编辑**：旋转、裁剪、缩放平移、曲线、曝光、对比度、色温、色调、锐化、模糊、颗粒、暗角等，全部非破坏性；F2 烘焙保存才落盘。
- **一键效果**：自动对比 / 自动颜色 / 暖调 / 冷调 / 正片 / LOMO / 逆光补偿等数十种，行为对齐原版 Picasa。
- **风格滤镜**：数十个成组配方——胶片颗粒、玩具相机、一次成像、宽银幕、 六十年代、铅笔素描、交叉冲印、双色调、霓虹、夜视、装裱卡纸、圆角……
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
- 沿用经典五栏菜单（文件 / 编辑 / 视图 / 工具 / 帮助）；照片原生右键菜单。
- 右栏信息抽屉：属性 / 标签 / 人物 / 地点。
- **全本地、无联网、无遥测。**

### 不打算做（原Picasa有，但目前暂不做）

图片精修编辑链（retouch / picnik）、拼贴 / 海报 / CD 刻录、在线分享  
（Picasa Web / 社交平台 / 邮件 / 打印）、TWAIN 相机扫描、相簿系统云同步、  
Picasa2 迁移。

## 只读速看预览格式

GIF、PSD、PSB 会进索引、能出缩略图与预览，但**只读**：不参与旋转烘培、效果编辑
与重新编码；批量导出只复制原始文件字节，不改动内容。

- **GIF**：只显示第一帧。不播放动画、不逐帧编辑、不导出 GIF。
- **PSD / PSB**：只读文件里已有的合成图，按 Adobe 公开的《Photoshop File
  Formats Specification》自行实现。不渲染图层、混合模式、智能对象，也不处理
  CMYK / Lab 与 16/32 位文档。保存时未生成合成图的文件，在没有系统缩略图提供
  程序的机器上没有预览。
- **WebP**：支持有损与无损解码；动图同样只显示第一帧，导出只写无损 WebP。

## 路线规划
   - 优先完善稳定Windows版本，极速查找图片、极速预览图片、多图片格式解码预览
   - 完善程序启动的稳定性，完善图片查看、预览、的稳定性，实现十万张图片秒查秒看
   - 完善对图片简单效果处理   
   - 实现图片去重、清理等需求
   - 实现照片多渠道备份、同步等需求
   - 实现本地人脸识别、提升准确率，快速查找人脸照片等问题
   - 对图片进行自动标签，解决用文本搜索图片的问题：比如帮我查身份证放在那里
   - 实现多操作系统，如Linux版本的可用
   - 引入自然语言对话处理图片的能力
   - 实现对图片处理的多场景预置


## 技术栈

- **语言**：Rust 2021
- **UI**：[egui](https://github.com/emilk/egui) / [eframe](https://github.com/emilk/egui) 0.33（wgpu 渲染）
- **数据库**：[rusqlite](https://github.com/rusqlite/rusqlite)（bundled SQLite）
- **系统交互**：`windows` crate（WPD / WIA / COM，Windows 平台）
- **图像**：`image` 0.25（JPEG / PNG / WebP / TIFF / BMP / GIF）；PSD / PSB 只读合成图预览为本项目自行实现（见「只读速看预览格式」）

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

### 模型获取（不入仓库，自行放置）

模型与权重不随程序分发（体积与许可原因），运行时放入**数据目录**的 `models\`
子目录（默认 `%APPDATA%\Relyto\Maliang\models\`，数据目录可在 选项 > 常规 迁移）。
缺模型时对应功能会显示放置指引，不影响其他功能。

| 文件 | 用途 | 来源与许可 |
|---|---|---|
| `face_det.onnx` | 人脸检测（YuNet，2023mar 版） | [opencv/opencv_zoo](https://github.com/opencv/opencv_zoo/blob/main/models/face_detection_yunet/)，Apache-2.0 |
| `face_rec.onnx` | 人脸识别，512 维（AdaFace IR-50 导出 ONNX） | [mk-minchul/AdaFace](https://github.com/mk-minchul/AdaFace)，MIT |
| `face_det_v2.onnx` / `face_rec_v2.onnx` | 可选方案：insightface antelopev2（SCRFD + ArcFace，**权重不可商用**，自备自用） | [deepinsight/insightface](https://github.com/deepinsight/insightface)，权重许可以官方发布为准 |
| `ch_PP-OCRv4_det_infer.onnx` / `ch_PP-OCRv4_rec_infer.onnx` / `ppocr_keys_v1.txt` | 中文 OCR | PP-OCRv4（PaddleOCR，Apache-2.0） |
| `clip_vision.onnx` / `clip_text.onnx` / `clip_merges.txt` | 语义搜索 | OpenAI CLIP（MIT）导出的 ONNX |

两套人脸方案都支持，在 **选项 > 常规 > 人脸识别方案** 里选择，默认推荐
YuNet + AdaFace（商业友好许可）；切换后向量不通用，会提示重新扫描。
程序会按输出张量数自动识别检测模型类型，文件按对应方案命名放置即可。

人脸模型放置方法：

- **检测（YuNet）**：直接下载上表的 `face_detection_yunet_2023mar.onnx`，
  重命名为 `face_det.onnx` 放入模型目录即可。
- **识别（AdaFace）**：官方仓库发布的是 PyTorch 权重，需先转成 ONNX。按其
  README 下载 IR-50 权重（如 `adaface_ir50_ms1mv2.ckpt`），在 AdaFace 仓库
  根目录执行：

  ```python
  # pip install torch onnx
  import torch
  from net import build_model

  model = build_model("ir_50")
  ckpt = torch.load("adaface_ir50_ms1mv2.ckpt", map_location="cpu")
  state = ckpt.get("state_dict", ckpt)
  model.load_state_dict({k.replace("module.", ""): v for k, v in state.items()})
  model.eval()
  torch.onnx.export(
      model, torch.randn(1, 3, 112, 112), "face_rec.onnx",
      input_names=["input"], output_names=["embedding"], opset_version=17,
  )
  ```

  生成的 `face_rec.onnx` 放入模型目录即可（输出 512 维，与库中向量维度一致）。

模型文件的许可归各自权利人所有，归属声明与许可原文见
[THIRD_PARTY_NOTICES.md](./THIRD_PARTY_NOTICES.md)。

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
  $env:TMP = "D:\\build-tmp"; $env:TEMP = "D:\\build-tmp"
  cargo build --release
  ```
- 若资源编译报「找不到 Windows SDK / 嵌入 Windows 图标资源失败」，把
  `rc.exe` 的完整路径写进 `RC_PATH` 后重试（Windows SDK 装好后一般存在于
  `C:\Program Files (x86)\Windows Kits\10\bin\<版本>\x64\`）：
  ```powershell
  $env:RC_PATH = "C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\rc.exe"
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

本项目是用 Rust 独立编写的实现。本仓库不包含、也不分发任何 Google 源代码、
二进制或资源文件。界面与交互方式上的相近之处，意在延续熟悉的使用体验，并支持与  
Picasa 的数据互通（如经典的 `picasa.ini` 文件格式）。

Adobe、Photoshop 是 Adobe Inc. 的商标或注册商标。文中提及 PSD / PSB 仅用于说明
文件格式兼容性，不表示任何隶属、授权、赞助或背书关系，也不声称通过 Adobe 的
兼容性认证。

## 第三方声明

本项目自身代码以 Apache License 2.0 发布，此外还包含或参考下列第三方内容。各组件的完整归属声明与许可原文统一收录于  
[THIRD_PARTY_NOTICES.md](./THIRD_PARTY_NOTICES.md)；所有第三方组件均按其原始许可条款使用与分发，本项目的许可不替代、也不改变这些条款。

- **OpenAI CLIP**（MIT 许可）：`src/clip.rs` 中的 BPE 分词器实现参考了其代码；
- **Cargo 依赖**（egui / eframe / wgpu / image / rusqlite / windows 等）：均为 MIT、Apache-2.0 或 BSD 系许可，与本项目的发布协议兼容；
- **模型与权重**：仓库不分发任何模型文件或权重，运行时所需资源由用户自行获取并放入本地模型目录，相关权利归各自权利人所有。

## 协议

本项目自身代码以 [Apache License 2.0](./LICENSE) 发布；第三方组件遵循各自的原始许可条款，详见[第三方声明](#第三方声明)与 [THIRD_PARTY_NOTICES.md](./THIRD_PARTY_NOTICES.md)。  

## 微信公众号二维码
  扫描后点“AI交流”，添加我的微信，我拉你入群。

![微信公众号二维码](assets/images/contact_gzh.jpg)
