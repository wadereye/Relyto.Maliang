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
| `face_rec_sface.onnx` | 人脸识别(SFace,**默认方案**,128 维) | [opencv/opencv_zoo](https://github.com/opencv/opencv_zoo),Apache-2.0(**许可覆盖权重文件**) |
| `face_rec.onnx` | 可选方案:人脸识别(AdaFace IR-50 导出的 ONNX,512 维) | [mk-minchul/AdaFace](https://github.com/mk-minchul/AdaFace),代码 MIT;**权重训练数据(MS1MV2 等)为非商用,商用前自行审查** |
| `face_det_v2.onnx` / `face_rec_v2.onnx` | 可选方案:insightface antelopev2(SCRFD + ArcFace,**权重许可非商用**,自备自用) | [deepinsight/insightface](https://github.com/deepinsight/insightface),权重许可以官方发布为准 |
| `clip_vision.onnx` | 图像编码器(ONNX 导出) | [openai/CLIP](https://github.com/openai/CLIP),MIT |
| `clip_text.onnx` | 文本编码器(ONNX 导出) | [openai/CLIP](https://github.com/openai/CLIP),MIT |
| `clip_merges.txt` | BPE 合并表(随 CLIP 模型一同发布) | [openai/CLIP](https://github.com/openai/CLIP),MIT |
| `clip_vision/…/vision.onnx` | MobileCLIP2-S2 图像编码器 | [apple/ml-mobileclip](https://github.com/apple/ml-mobileclip),许可以官方仓库与模型卡为准 |
| `clip_text/…/text.onnx` + `merges.txt` | MobileCLIP2-S2 文本编码器与分词表 | 同上 |
| `mt_zh_en/…/encoder.onnx`、`decoder.onnx`、`vocab.txt` | Opus-MT 中→英翻译 | [Helsinki-NLP/opus-mt-zh-en](https://huggingface.co/Helsinki-NLP/opus-mt-zh-en),CC-BY-4.0 |
| `ocr_det/…/det.onnx` | PP-OCRv4 文字检测 | [PaddlePaddle/PaddleOCR](https://github.com/PaddlePaddle/PaddleOCR),Apache-2.0 |
| `ocr_cls/…/cls.onnx` | PP-OCR 文字方向分类 | 同上 |
| `ocr_rec/…/rec.onnx`、`ppocr_keys_v1.txt` | PP-OCRv4 文字识别与字典 | 同上 |
| `llm_parse/…/model.onnx` | 可选查询解析模型 | 由用户自行获取,许可以所选模型发布为准 |

上述资源的权利归各自权利人所有,其许可条款以其发布渠道为准。
各模型的具体获取方式(含 AdaFace 权重转 ONNX 步骤)见
[README.md](./README.md) / [README.zh-CN.md](./README.zh-CN.md)。

### YuNet(人脸检测模型)

* **作品**:YuNet 人脸检测器(`face_detection_yunet_2023mar.onnx`)
* **来源**:https://github.com/opencv/opencv_zoo/tree/main/models/face_detection_yunet
* **许可**:Apache License 2.0(Copyright (c) OpenCV 团队及贡献者,许可原文见该仓库
  [`LICENSE`](https://github.com/opencv/opencv_zoo/blob/master/LICENSE))
* **涉及范围**:仅运行时由用户自行下载使用,不在本仓库分发。

### SFace(人脸识别模型,默认方案)

* **作品**:SFace 人脸识别器(`face_recognition_sface_2021dec.onnx`,128 维嵌入)
* **来源**:https://github.com/opencv/opencv_zoo/tree/main/models/face_recognition_sface
* **许可**:Apache License 2.0,Copyright (C) 2021, Shenzhen Institute of
  Artificial Intelligence and Robotics for Society(SAIRI),许可覆盖**权重文件
  本身**,允许商用与再分发(保留许可与归属声明即可)
* **涉及范围**:默认方案;可由用户配置 ModelURL 后由程序代下,不在本仓库分发。

### AdaFace(可选人脸识别模型)

* **作品**:AdaFace IR-50 预训练权重(MS1MV2 / Glint360K 训练)
* **来源**:https://github.com/mk-minchul/AdaFace
* **许可**:MIT License(代码)
* **原厂授权声明**:官方模型卡(HuggingFace `minchul/cvlface_adaface_ir101_ms1mv2`)
  原文要求 *Please cite the original paper and follow the license of the
  training dataset*;训练集 MS1MV2(MS-Celeb-1M 清洗版)与 CASIA-WebFace 的
  自身条款均为**非商用、仅限研究用途**。代码的 MIT 许可不覆盖训练数据,
  转成 ONNX 也不产生新的授权,商业使用前请自行审查上游条款并做尽职调查。
* **涉及范围**:可选方案,用户自行下载权重并转换为 ONNX 后运行时使用,
  不在本仓库分发。

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
* **原厂授权声明**:insightface 官方模型页原文 *The pretrained models we
  provided with this library are available for non-commercial research
  purposes only, including both auto-downloading models and
  manual-downloading models*(仅限非商用研究用途,涵盖其发布的全部模型包)。
* **涉及范围**:保留为可选方案(选项 > 常规 可切换),权重由用户自行获取、
  自担许可义务,不在本仓库分发,本项目也从不分发其权重、从不代其下载。

### MobileCLIP2(默认语义编码器)

* **作品**:MobileCLIP2-S2 视觉/文本双编码器(ONNX 导出)
* **来源**:https://github.com/apple/ml-mobileclip;官方权重
  `mobileclip2_s2.pt`(https://huggingface.co/apple/MobileCLIP2-S2)经
  `reparameterize_model` 后导出为 `vision.onnx` / `text.onnx`
* **许可**:以官方仓库与 HuggingFace 模型卡发布为准;权重不在本仓库分发,
  由应用在运行时从 ModelURL(内置默认地址)下载。
* **涉及范围**:`src/clip.rs` 的推理与预处理(短边 256、居中裁剪、0~1 缩放,
  不做均值方差归一化)按公开输入签名实现,未复制其训练代码。

### Opus-MT(中文查询翻译)

* **作品**:Helsinki-NLP Opus-MT zh→en 翻译模型(encoder/decoder 与词表)
* **来源**:https://huggingface.co/Helsinki-NLP/opus-mt-zh-en
* **许可**:CC-BY-4.0,使用时保留归属与许可声明;权重由用户或应用在运行时获取,
  本项目不在仓库中分发。
* **涉及范围**:`src/ai_text.rs` 的 `MtTranslator` 为独立实现的贪心解码器,
  未复制上游代码;分词仅做词表最长匹配。

### PP-OCRv4(文字识别)

* **作品**:PaddleOCR PP-OCRv4 检测/方向/识别模型与 `ppocr_keys_v1.txt` 字典
* **来源**:https://github.com/PaddlePaddle/PaddleOCR;ONNX 权重取自 RapidOCR 镜像
  https://www.modelscope.cn/models/RapidAI/RapidOCR
* **许可**:Apache License 2.0;权重由用户或应用在运行时获取,不在本仓库分发。
* **涉及范围**:`src/ai.rs` 的 OCR 前处理、裁剪与解码按公开模型输入输出实现。

### 可选查询解析模型

* **作品**:0.5~0.6B instruct 模型(仅 M5 可选能力使用)
* **来源与许可**:由用户自行选择并获取,以所选模型的发布条款为准。
* **涉及范围**:本项目只调用其 ONNX 推理接口并解析 JSON 输出,不分发权重。

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
