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

| 文件 | 说明 |
|---|---|
| `clip_vision.onnx` | 图像编码器(ONNX 导出) |
| `clip_text.onnx` | 文本编码器(ONNX 导出) |
| `clip_merges.txt` | BPE 合并表(随 CLIP 模型一同发布) |

上述资源的权利归各自权利人所有,其许可条款以其发布渠道为准。

---

## 依赖crate

本项目通过 Cargo 引入的依赖(egui / eframe / wgpu / image / rusqlite / rayon /
chrono / anyhow / dirs / rfd / clap / windows / ort 等)均为 MIT、Apache-2.0 或
BSD 系许可,兼容本项目的 Apache-2.0 发布协议。各依赖的完整许可文本随其 crate
一并分发,详见 `Cargo.lock` 中记录的版本与各 crate 的 `LICENSE` 文件。

---

## 商标

**Picasa** 是 Google LLC 的商标。本项目是一个**独立的开源实现**,与 Google LLC
无任何隶属、授权或合作关系,亦未使用其源代码、二进制文件或专有资源。
项目中对 Picasa 的提及仅用于说明功能参照关系与数据互通(如 `picasa.ini`
这一文件格式名),不表示任何背书或授权。
