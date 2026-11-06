# 第三方声明(Third-Party Notices)

本项目(Maliang / 马良)以 Apache License 2.0 发布(见 [`LICENSE`](./LICENSE))。
仓库中包含或参考了以下第三方作品,特此声明其归属与许可条款。

---

## OpenAI CLIP

* **作品**:CLIP(Contrastive Language–Image Pre-training)
* **来源**:https://github.com/openai/CLIP
* **许可**:MIT License
* **涉及范围**:`src/clip.rs` 中的 BPE 分词器实现参考了该仓库的
  `clip/simple_tokenizer.py`(分词正则、字节↔Unicode 映射规则、BPE 合并逻辑)。

```text
MIT License

Copyright (c) 2021 OpenAI

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

### 模型与权重

本项目**不在仓库中分发**任何模型文件或权重文件。运行时所需资源由用户在应用内
自行获取并放入本地模型目录(默认 `%APPDATA%\Relyto\Maliang\models\`),包括:

| 文件 | 说明 | 来源与许可 |
|---|---|---|
| `face_det.onnx` | 人脸检测(YuNet,2023mar 版) | [opencv/opencv_zoo](https://github.com/opencv/opencv_zoo),Apache-2.0 |
| `face_rec.onnx` | 人脸识别(AdaFace IR-50 导出的 ONNX,512 维) | [mk-minchul/AdaFace](https://github.com/mk-minchul/AdaFace),MIT |
| `face_det_v2.onnx` / `face_rec_v2.onnx` | 可选方案:insightface antelopev2(SCRFD + ArcFace,**权重许可非商用**,自备自用) | [deepinsight/insightface](https://github.com/deepinsight/insightface),权重许可以官方发布为准 |
| `clip_vision.onnx` | 图像编码器(ONNX 导出) | [openai/CLIP](https://github.com/openai/CLIP),MIT |
| `clip_text.onnx` | 文本编码器(ONNX 导出) | [openai/CLIP](https://github.com/openai/CLIP),MIT |
| `clip_merges.txt` | BPE 合并表(随 CLIP 模型一同发布) | [openai/CLIP](https://github.com/openai/CLIP),MIT |

上述资源的权利归各自权利人所有,其许可条款以其发布渠道为准。
各模型的具体获取方式(含 AdaFace 权重转 ONNX 步骤)见
[README.md](./README.md) / [README.zh-CN.md](./README.zh-CN.md)。

### YuNet(人脸检测模型)

* **作品**:YuNet 人脸检测器(`face_detection_yunet_2023mar.onnx`)
* **来源**:https://github.com/opencv/opencv_zoo/tree/main/models/face_detection_yunet
* **许可**:Apache License 2.0(Copyright (c) OpenCV 团队及贡献者,许可原文见该仓库
  [`LICENSE`](https://github.com/opencv/opencv_zoo/blob/master/LICENSE))
* **涉及范围**:仅运行时由用户自行下载使用,不在本仓库分发。

### AdaFace(人脸识别模型)

* **作品**:AdaFace IR-50 预训练权重(MS1MV2 / Glint360K 训练)
* **来源**:https://github.com/mk-minchul/AdaFace
* **许可**:MIT License
* **涉及范围**:用户自行下载权重并转换为 ONNX 后运行时使用,不在本仓库分发。

```text
MIT License

Copyright (c) 2022 Minchul Kim

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

### antelopev2(可选人脸方案)

* **作品**:insightface antelopev2 模型包(SCRFD 检测 + ArcFace 识别)
* **来源**:https://github.com/deepinsight/insightface
* **许可**:代码为 MIT,但**模型权重许可为非商用且不可再分发**
* **涉及范围**:保留为可选方案(选项 > 常规 可切换),权重由用户自行获取、
  自担许可义务,不在本仓库分发,本项目也从不分发其权重。

---

## 依赖crate

本项目通过 Cargo 引入的依赖(egui / eframe / wgpu / image / rusqlite / rayon /
chrono / anyhow / dirs / rfd / clap / windows / ort 等)均为 MIT、Apache-2.0 或
BSD 系许可,兼容本项目的 Apache-2.0 发布协议。各依赖的完整许可文本随其 crate
一并分发,详见 `Cargo.lock` 中记录的版本与各 crate 的 `LICENSE` 文件。

---

## 格式规范与参考

PSD / PSB 只读预览依据 Adobe 公开的《Adobe Photoshop File Formats
Specification》自行实现,仅解析文件头与文件内已合并好的合成图数据。
本仓库不复制任何第三方 PSD 实现代码。

可行性调研参考了 PhotoCraft(https://github.com/storytold/photocraft,
MIT OR Apache-2.0)公开的项目文档与接口说明;其源码、字体、图标与资源均未
进入本仓库。GIF / WebP 解码分别由 `gif`、`image-webp` crate 提供,两者以及
`flate2`、`image` 均为 MIT OR Apache-2.0 许可;WebP 解码使用纯 Rust 实现,
未链接 libwebp,也不分发其代码。

---

## 商标

**Picasa** 是 Google LLC 的商标。本项目是一个**独立的开源实现**,与 Google LLC
无任何隶属、授权或合作关系。本仓库不包含、也不分发任何 Google 的源代码、
二进制文件或资源文件。
项目中对 Picasa 的提及仅用于说明功能参照关系与数据互通(如 `picasa.ini`
这一文件格式名),不表示任何背书或授权。

**Adobe** 与 **Photoshop** 是 Adobe Inc. 的商标或注册商标。此处提及 PSD / PSB 仅用于说明文件格式兼容性,不表示任何隶属、授权、赞助或背书关系;本项目也不声称通过 Adobe 的兼容性认证。
