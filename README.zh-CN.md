
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

版本清单地址内置在程序里（**全新安装或清掉本机配置也不会丢**）；模型目录则
**出厂默认不填**——配好 `ModelURL` 后，默认方案（SFace）的两个模型可以代下，
其权重为 Apache-2.0（含权重，可商用）；AdaFace / antelopev2 权重的许可链
需要用户自行审查（见下方「非默认方案的权重许可声明」），程序不默认替用户下载
任何模型，界面只给手动放置指引。
统一配置 `maliang.ini` 的 `[update]` 段里：`UpdateURL`（清单地址）不写就用
内置地址，改成自己的地址即换源，写成空值停用自动更新；`ModelURL=https://你的域名/maliang/models/`
（自建服务端模型目录，结尾带 `/`）配好后，程序才能代取默认方案的两个人脸模型：
启动时检查模型目录，缺件则提示，由用户在提示窗或
**选项 > 常规 > 人脸识别方案** 点「下载模型」才联网（识别模型体积较大，
不默认替用户花流量）；下载在后台分段进行，状态栏与设置页显示进度，中断会保留断点，
再点一次续传。落盘前用 ONNX Runtime 实际加载校验，加载不了的内容直接丢弃。
`ModelURL` 留空或写成空值都回到手动放置指引；`antelopev2` 的权重许可不可再分发，
任何情况下都只提供手动放置。自建服务端分发模型前，请自行确认分发行为符合相应许可。
配置文件以 exe 同目录那份优先 = 便携部署，否则用 `%APPDATA%\Relyto\Maliang\maliang.ini`，
「帮助 > 检查更新」与 选项 > 常规 人脸方案区都有「打开配置文件」按钮可直达。

程序只按所选方案的固定文件名取件（默认方案 `<ModelURL>face_det.onnx` +
`<ModelURL>face_rec_sface.onnx`，其余见下表）；
下面列的官方模型地址是给你上架时对照、以及手动放置时用的，
优先用官方链接，官方地址不通再退到国内加速镜像。

| 文件 | 用途 | 来源与许可 |
|---|---|---|
| `face_det.onnx` | 人脸检测（YuNet，2023mar 版） | [opencv/opencv_zoo](https://github.com/opencv/opencv_zoo/blob/main/models/face_detection_yunet/)，Apache-2.0 |
| `face_rec_sface.onnx` | 人脸识别，128 维（**默认方案** SFace） | [opencv/opencv_zoo](https://github.com/opencv/opencv_zoo/blob/main/models/face_recognition_sface/)，Apache-2.0（**含权重**，可商用） |
| `face_rec.onnx` | 可选方案：人脸识别，512 维（AdaFace IR-50 导出 ONNX） | [mk-minchul/AdaFace](https://github.com/mk-minchul/AdaFace)，代码 MIT；权重训练数据（MS1MV2）非商用，见下方声明 |
| `face_det_v2.onnx` / `face_rec_v2.onnx` | 可选方案：insightface antelopev2（SCRFD + ArcFace，**权重不可商用**，自备自用） | [deepinsight/insightface](https://github.com/deepinsight/insightface)，权重许可以官方发布为准 |
| `ch_PP-OCRv4_det_infer.onnx` / `ch_PP-OCRv4_rec_infer.onnx` / `ppocr_keys_v1.txt` | 中文 OCR | PP-OCRv4（PaddleOCR，Apache-2.0） |
| `clip_vision.onnx` / `clip_text.onnx` / `clip_merges.txt` | 语义搜索 | OpenAI CLIP（MIT）导出的 ONNX |

#### 人脸模型的可用下载地址（2026-11-07 逐个核对）

| 程序内文件名 | 官方地址 | 国内可达的加速镜像 | 核对结果 |
|---|---|---|---|
| `face_det.onnx` | <https://media.githubusercontent.com/media/opencv/opencv_zoo/main/models/face_detection_yunet/face_detection_yunet_2023mar.onnx> | OpenCV 官方 HuggingFace 仓库 <https://huggingface.co/opencv/face_detection_yunet>，其镜像 <https://hf-mirror.com/opencv/face_detection_yunet/resolve/main/face_detection_yunet_2023mar.onnx> | 官方地址 200 / 232,589 字节；`hf-mirror.com` 同样 200 / 232,589 字节（`huggingface.co` 本身要看网络，本机测试时它无响应）。⚠️ `raw.githubusercontent.com` 的同一路径只返回 131 字节的 Git-LFS 指针，必须走 `media.githubusercontent.com`；opencv_zoo 的 GitHub Release 里没有模型资产。GitCode 上的 opencv_zoo 镜像**不能当直连源**：`.../raw/main/...onnx` 回 200 但给的是 6 KB 网页壳，`file.gitcode.com` 回 401，`raw.gitcode.com` 该路径 404 |
| `face_rec_sface.onnx` | <https://media.githubusercontent.com/media/opencv/opencv_zoo/main/models/face_recognition_sface/face_recognition_sface_2021dec.onnx> | OpenCV 官方 HuggingFace 仓库 <https://huggingface.co/opencv/face_recognition_sface>，其镜像 <https://hf-mirror.com/opencv/face_recognition_sface/resolve/main/face_recognition_sface_2021dec.onnx> | 与 YuNet 同仓同规则：走 `media.githubusercontent.com` 或 HuggingFace，`raw.githubusercontent.com` 只给 LFS 指针。官方文件 200 / 38,696,353 字节（约 36.9 MiB）；Apache-2.0 **含权重**，程序可在配置 ModelURL 后代下 |
| `face_rec.onnx` | AdaFace 官方只发布 PyTorch 权重，放在 Google Drive（IR-50 / MS1MV2：<https://drive.google.com/file/d/1eUaSHG4pGlIZK7hBkqjyp2fc2epKoBvI>） | 没有官方镜像 | 官方仓库没有任何 ONNX 资产（GitHub Releases 为空），作者后续的 CVLFace 仓库在 HuggingFace 上也只有 `model.safetensors`。所以这一件**要么按下面的脚本自己转 ONNX，要么由你的服务端代收转换好的 ONNX**；网上流传的 AdaFace ONNX 都不是官方发布的。⚠️ 该权重以 MS1MV2（研究用途数据集）训练，见「非默认方案的权重许可声明」 |
| `face_det_v2.onnx`、`face_rec_v2.onnx` | <https://github.com/deepinsight/insightface/releases/download/v0.7/antelopev2.zip>（Release tag `v0.7` 的资产，`buffalo_l.zip` 是同页的另一个包） | ModelScope 的 `deepghs/insightface` 是社区自动镜像，非官方 | 官方包 360,662,982 字节（约 344 MiB，体积以 GitHub API 返回的 size 为准）。解包后把检测件（SCRFD 10G，包内名 `scrfd_10g_bnkps.onnx`）改名成 `face_det_v2.onnx`、识别件（ArcFace R100，包内名 `glintr100.onnx`，512 维来自它）改名成 `face_rec_v2.onnx`，具体以包内实际文件名为准；`buffalo_l.zip` 里的 `det_10g.onnx` / `w600k_r50.onnx` 是另一套方案的件，不要拿来改名。**许可不可再分发，程序不提供代下载** |

三套人脸方案都支持，在 **选项 > 常规 > 人脸识别方案** 里选择，**默认
YuNet + SFace**（两个模型都是 Apache-2.0 含权重，可商用，配 ModelURL 后可代下）；
切换后向量不通用，会提示重新扫描。各方案的聚类/建议阈值随方案自动切换，
程序会按输出张量数自动识别检测模型类型，文件按对应方案命名放置即可。

#### 非默认方案的权重许可声明（选用前请先读）

- **YuNet / SFace（默认方案，供对照）**：两个模型均出自 OpenCV Zoo，
  以 Apache-2.0 发布且**许可覆盖权重文件本身**，允许商用与再分发
  （保留许可与归属声明即可，见 THIRD_PARTY_NOTICES.md）。
- **AdaFace（可选，自备）**：代码 MIT（© 2022 Minchul Kim），但官方模型卡
  明确要求 *follow the license of the training dataset*；其预训练权重的训练集
  MS1MV2（MS-Celeb-1M 清洗版）与 CASIA-WebFace 条款均为**非商用、仅限研究
  用途**。代码的 MIT 许可**不覆盖**训练数据，把权重转成 ONNX（或任何其他
  格式）也不产生新的授权。选用即表示你自备权重并自行承担许可审查责任。
- **antelopev2（可选，自备）**：insightface 官方对其发布的预训练模型的
  声明原文为 *The pretrained models we provided with this library are
  available for non-commercial research purposes only*（仅限非商用研究用途，
  含自动下载与手动下载的所有模型包）。该权重**不可再分发**，程序任何情况下
  都不代下，只提供手动放置。

本项目对非默认方案的处理：

- **手动下载是推荐路径**：按下方地址自行取得权重并（对 AdaFace）转 ONNX
  放入模型目录，许可责任由使用者自担。
- 若你运营自己的服务端并愿意分发 AdaFace 权重，可在 `ModelURL` 填自己的地址——
  分发行为及其许可后果由服务端运营方承担，与本程序无关；antelopev2 无论如何
  都不进代下通道。
- 对许可要求更高的场景，可关注上游以更清晰条款重新发布的权重，或使用
  自行训练的模型（AdaFace 的网络结构与转换脚本同样是 MIT）。

人脸模型放置方法：

- **检测（YuNet，默认方案与 AdaFace 方案共用）**：直接下载上表的
  `face_detection_yunet_2023mar.onnx`，
  重命名为 `face_det.onnx` 放入模型目录即可。取件走
  `media.githubusercontent.com` 或 HuggingFace；用 `raw.githubusercontent.com`
  拿到的是 131 字节的 Git-LFS 指针文本，程序会在加载校验那一步拒掉。
- **识别（SFace，默认方案）**：下载上表的 `face_recognition_sface_2021dec.onnx`
  （Apache-2.0 含权重），重命名为 `face_rec_sface.onnx` 放入模型目录即可
  （输出 128 维）。配置 `ModelURL` 后也可由程序代下这两个文件。
- **识别（AdaFace，可选）**：官方仓库发布的是 PyTorch 权重，需先转成 ONNX。按其
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
- **可选方案（antelopev2）**：解官方 `antelopev2.zip`，把包内
  `scrfd_10g_bnkps.onnx` 改名成 `face_det_v2.onnx`、`glintr100.onnx` 改名成
  `face_rec_v2.onnx`（512 维来自后者）后放入模型目录。同名 zip 之外的
  `det_10g.onnx` / `w600k_r50.onnx` 属于 buffalo_l 方案，不是本方案的件，
  别按上面的名字改。该方案权重许可不可再分发（声明见上节），程序只给手动放置，
  不提供代下载。

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
