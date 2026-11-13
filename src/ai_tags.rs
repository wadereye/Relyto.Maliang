//! 自动标签:内置词表、灵敏度阈值、打分与分类相簿查询。

use crate::db::Store;
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use std::collections::HashMap;

/// 一条标签定义。
#[derive(Debug, Clone, PartialEq)]
pub struct Label {
    pub id: i64,
    pub node: String,
    pub zh: String,
    pub en: String,
    pub prompt: String,
}

/// 内置词表(节点, 中文, 英文, 英文 prompt)。
const BUILTIN: &[(&str, &str, &str, &str)] = &[
    ("人物/人像", "人像", "portrait", "a photo of a person"),
    (
        "人物/合影",
        "合影",
        "group photo",
        "a group photo of people",
    ),
    ("动物/猫", "猫", "cat", "a photo of a cat"),
    ("动物/狗", "狗", "dog", "a photo of a dog"),
    ("动物/鸟", "鸟", "bird", "a photo of a bird"),
    ("动物/鱼", "鱼", "fish", "a photo of a fish"),
    ("动物/昆虫", "昆虫", "insect", "a photo of an insect"),
    ("动物/马", "马", "horse", "a photo of a horse"),
    ("动物/牛羊", "牛羊", "cattle", "a photo of cattle or sheep"),
    (
        "动物/野生动物",
        "野生动物",
        "wildlife",
        "a photo of wild animals",
    ),
    ("动物/宠物", "宠物", "pet", "a photo of a pet"),
    (
        "动物/海洋生物",
        "海洋生物",
        "marine life",
        "a photo of marine life",
    ),
    (
        "动物/爬行动物",
        "爬行动物",
        "reptile",
        "a photo of a reptile",
    ),
    ("场景/海边", "海边", "beach", "a photo of the beach and sea"),
    ("场景/山", "山", "mountain", "a photo of mountains"),
    ("场景/夜景", "夜景", "night view", "a night view of a city"),
    ("场景/城市", "城市", "city", "a photo of a city street"),
    ("场景/室内", "室内", "indoor", "an indoor scene"),
    ("场景/森林", "森林", "forest", "a photo of a forest"),
    ("场景/天空", "天空", "sky", "a photo of the sky"),
    ("场景/雪景", "雪景", "snow", "a photo of snow"),
    ("场景/沙漠", "沙漠", "desert", "a photo of a desert"),
    ("场景/湖", "湖", "lake", "a photo of a lake"),
    ("场景/河", "河流", "river", "a photo of a river"),
    ("场景/花园", "花园", "garden", "a photo of a garden"),
    ("场景/校园", "校园", "campus", "a photo of a school campus"),
    ("场景/办公室", "办公室", "office", "an office interior"),
    ("场景/家", "家", "home", "a photo of a home interior"),
    ("场景/餐厅", "餐厅", "restaurant", "a photo of a restaurant"),
    ("场景/建筑", "建筑", "building", "a photo of a building"),
    ("场景/桥", "桥", "bridge", "a photo of a bridge"),
    ("场景/车站", "车站", "station", "a photo of a station"),
    ("场景/机场", "机场", "airport", "a photo of an airport"),
    ("美食/菜品", "菜品", "dish", "a photo of a plate of food"),
    ("美食/咖啡", "咖啡", "coffee", "a photo of a cup of coffee"),
    ("美食/甜点", "甜点", "dessert", "a photo of dessert"),
    ("美食/烧烤", "烧烤", "barbecue", "a photo of barbecue food"),
    ("美食/水果", "水果", "fruit", "a photo of fruit"),
    ("美食/蔬菜", "蔬菜", "vegetable", "a photo of vegetables"),
    ("美食/面条", "面条", "noodles", "a photo of noodles"),
    ("美食/米饭", "米饭", "rice", "a photo of rice"),
    ("美食/火锅", "火锅", "hot pot", "a photo of hot pot"),
    ("美食/蛋糕", "蛋糕", "cake", "a photo of a cake"),
    ("美食/面包", "面包", "bread", "a photo of bread"),
    ("美食/饮料", "饮料", "drink", "a photo of a drink"),
    ("美食/酒", "酒", "wine", "a photo of wine or beer"),
    ("美食/早餐", "早餐", "breakfast", "a photo of breakfast"),
    ("美食/海鲜", "海鲜", "seafood", "a photo of seafood"),
    ("美食/小吃", "小吃", "snack", "a photo of snacks"),
    ("美食/午餐盒", "便当", "lunch box", "a photo of a lunch box"),
    ("物品/车", "汽车", "car", "a photo of a car"),
    ("物品/自行车", "自行车", "bicycle", "a photo of a bicycle"),
    (
        "物品/摩托车",
        "摩托车",
        "motorcycle",
        "a photo of a motorcycle",
    ),
    ("物品/飞机", "飞机", "airplane", "a photo of an airplane"),
    ("物品/船", "船", "boat", "a photo of a boat"),
    ("物品/花", "花", "flower", "a photo of flowers"),
    ("物品/书", "书籍", "book", "a photo of books"),
    (
        "物品/数码产品",
        "数码产品",
        "gadget",
        "a photo of a digital device",
    ),
    ("物品/手机", "手机", "phone", "a photo of a smartphone"),
    ("物品/电脑", "电脑", "computer", "a photo of a computer"),
    ("物品/相机", "相机", "camera", "a photo of a camera"),
    ("物品/家具", "家具", "furniture", "a photo of furniture"),
    ("物品/衣服", "衣服", "clothing", "a photo of clothing"),
    ("物品/鞋", "鞋", "shoes", "a photo of shoes"),
    ("物品/包", "包", "bag", "a photo of a bag"),
    ("物品/玩具", "玩具", "toy", "a photo of toys"),
    (
        "物品/乐器",
        "乐器",
        "instrument",
        "a photo of a musical instrument",
    ),
    ("物品/工具", "工具", "tool", "a photo of tools"),
    ("物品/瓶子", "瓶子", "bottle", "a photo of a bottle"),
    ("物品/手表", "手表", "watch", "a photo of a watch"),
    ("物品/首饰", "首饰", "jewelry", "a photo of jewelry"),
    ("物品/钥匙", "钥匙", "key", "a photo of keys"),
    ("物品/轮胎", "轮胎", "tire", "a photo of a tire"),
    ("物品/行李箱", "行李箱", "luggage", "a photo of luggage"),
    ("物品/雨伞", "雨伞", "umbrella", "a photo of an umbrella"),
    ("物品/镜子", "镜子", "mirror", "a photo of a mirror"),
    ("物品/钟表", "钟表", "clock", "a photo of a clock"),
    ("物品/灯", "灯", "lamp", "a photo of a lamp"),
    (
        "文档票据/截图",
        "截图",
        "screenshot",
        "a screenshot of a screen",
    ),
    (
        "文档票据/证件",
        "证件",
        "ID card",
        "a photo of an ID card or certificate",
    ),
    (
        "文档票据/发票",
        "发票",
        "invoice",
        "a photo of an invoice or receipt",
    ),
    (
        "文档票据/白板",
        "白板",
        "whiteboard",
        "a photo of a whiteboard",
    ),
    (
        "文档票据/表格",
        "表格",
        "spreadsheet",
        "a photo of a spreadsheet or table",
    ),
    (
        "文档票据/文档",
        "文档",
        "document",
        "a photo of a document page",
    ),
    (
        "文档票据/名片",
        "名片",
        "business card",
        "a photo of a business card",
    ),
    (
        "文档票据/手写",
        "手写",
        "handwriting",
        "a photo of handwritten text",
    ),
    (
        "文档票据/书籍页",
        "书页",
        "book page",
        "a photo of a book page",
    ),
    (
        "文档票据/二维码",
        "二维码",
        "QR code",
        "a photo of a QR code",
    ),
    ("文档票据/票据", "票据", "ticket", "a photo of a ticket"),
    ("文档票据/合同", "合同", "contract", "a photo of a contract"),
    ("活动/运动", "运动", "sports", "a photo of sports activity"),
    ("活动/旅行", "旅行", "travel", "a travel photo"),
    ("活动/聚会", "聚会", "party", "a photo of a party"),
    (
        "活动/演出",
        "演出",
        "performance",
        "a photo of a performance",
    ),
    ("活动/会议", "会议", "meeting", "a photo of a meeting"),
    ("活动/购物", "购物", "shopping", "a photo of shopping"),
    ("活动/做饭", "做饭", "cooking", "a photo of cooking"),
    ("活动/钓鱼", "钓鱼", "fishing", "a photo of fishing"),
    ("活动/徒步", "徒步", "hiking", "a photo of hiking"),
    ("活动/骑行", "骑行", "cycling", "a photo of cycling"),
    ("活动/游泳", "游泳", "swimming", "a photo of swimming"),
    ("活动/滑雪", "滑雪", "skiing", "a photo of skiing"),
    ("活动/跑步", "跑步", "running", "a photo of running"),
    ("活动/跳舞", "跳舞", "dancing", "a photo of dancing"),
    (
        "活动/拍照",
        "拍照",
        "photography",
        "a photo of someone taking pictures",
    ),
    (
        "活动/展览",
        "展览",
        "exhibition",
        "a photo of an exhibition",
    ),
    ("活动/烟花", "烟花", "fireworks", "a photo of fireworks"),
    ("活动/野餐", "野餐", "picnic", "a photo of a picnic"),
    (
        "风格/黑白",
        "黑白",
        "black and white",
        "a black and white photo",
    ),
    ("风格/逆光", "逆光", "backlight", "a backlit photo"),
    ("风格/微距", "微距", "macro", "a macro close-up photo"),
    (
        "风格/长曝光",
        "长曝光",
        "long exposure",
        "a long exposure photo",
    ),
    (
        "风格/浅景深",
        "浅景深",
        "bokeh",
        "a photo with shallow depth of field",
    ),
    ("风格/复古", "复古", "vintage", "a vintage looking photo"),
    ("风格/胶片", "胶片", "film", "a film look photo"),
    (
        "风格/高对比",
        "高对比",
        "high contrast",
        "a high contrast photo",
    ),
    ("风格/低调", "暗调", "low key", "a low key dark photo"),
    ("风格/高调", "亮调", "high key", "a bright high key photo"),
    (
        "风格/广角",
        "广角",
        "wide angle",
        "a wide angle landscape photo",
    ),
    (
        "风格/特写",
        "特写",
        "close-up",
        "a close-up photo of an object",
    ),
    ("风格/剪影", "剪影", "silhouette", "a silhouette photo"),
    (
        "风格/倒影",
        "倒影",
        "reflection",
        "a photo of reflections in water",
    ),
    ("风格/纹理", "纹理", "texture", "a photo of texture details"),
    (
        "风格/极简",
        "极简",
        "minimal",
        "a minimal composition photo",
    ),
    ("风格/蒸汽", "蒸汽", "steam", "a photo with steam or smoke"),
    (
        "风格/光斑",
        "光斑",
        "light bokeh",
        "a photo with bokeh lights",
    ),
];

/// 灵敏度档位:每条标签下限与同类兼容分差(按真实照片余弦分布标定)。
const SENSITIVITY: [(f32, f32); 3] = [(0.19, 0.03), (0.21, 0.04), (0.23, 0.05)];
const SELECTION_VERSION: &str = "category-multilabel-v3";

/// 内置词表条目数。
pub fn builtin_count() -> usize {
    BUILTIN.len()
}

/// 按灵敏度档位取 (标签下限, 同类分差)。
pub fn thresholds(sensitivity: i32) -> (f32, f32) {
    SENSITIVITY[sensitivity.clamp(0, 2) as usize]
}

/// 从设置读灵敏度档位(缺省中档)。
pub fn sensitivity(store: &Store) -> i32 {
    store
        .get_setting("ai_tag_sensitivity")
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(1)
        .clamp(0, 2)
}

/// 写入内置词表:按 node 认领既有 id,新增条目自增,下线条目连同其自动标签一并清除。
pub fn seed_labels(store: &Store) -> Result<usize> {
    store.write(|conn| -> Result<usize> {
        let tx = conn.transaction()?;
        let mut existing: HashMap<String, i64> = HashMap::new();
        {
            let mut st = tx.prepare("SELECT id, node FROM tag_labels WHERE builtin=1")?;
            let rows = st.query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?;
            for (id, node) in rows.flatten() {
                existing.insert(node, id);
            }
        }
        let mut inserted = 0usize;
        for (node, zh, en, prompt) in BUILTIN.iter() {
            if let Some(id) = existing.remove(*node) {
                tx.execute(
                    "UPDATE tag_labels SET name_zh=?2, name_en=?3, prompt_en=?4 WHERE id=?1",
                    params![id, zh, en, prompt],
                )?;
            } else {
                inserted += tx.execute(
                    "INSERT INTO tag_labels(node, name_zh, name_en, prompt_en, parent, builtin, ver)
                     VALUES(?1, ?2, ?3, ?4, NULL, 1, 'v1')",
                    params![node, zh, en, prompt],
                )?;
            }
        }
        for id in existing.into_values() {
            tx.execute("DELETE FROM auto_tags WHERE label_id=?1", params![id])?;
            tx.execute("DELETE FROM tag_labels WHERE id=?1", params![id])?;
        }
        tx.commit()?;
        Ok(inserted)
    })
}

/// 读词表(按 id)。
pub fn labels(store: &Store) -> Vec<Label> {
    store.read(|conn| {
        let mut out = Vec::new();
        let Ok(mut st) = conn
            .prepare("SELECT id, node, name_zh, name_en, prompt_en FROM tag_labels ORDER BY id")
        else {
            return out;
        };
        if let Ok(rows) = st.query_map([], |r| {
            Ok(Label {
                id: r.get(0)?,
                node: r.get(1)?,
                zh: r.get(2)?,
                en: r.get(3)?,
                prompt: r.get(4)?,
            })
        }) {
            out.extend(rows.flatten());
        }
        out
    })
}

/// 词表条数。
pub fn label_count(store: &Store) -> usize {
    store.read(|conn| {
        conn.query_row("SELECT COUNT(*) FROM tag_labels", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap_or(0) as usize
    })
}

/// 词表版本包含有序内容与选择算法版本。
pub fn vocab_version(store: &Store) -> String {
    let mut bytes = SELECTION_VERSION.as_bytes().to_vec();
    let custom = store
        .get_setting("ai_labels_ver")
        .unwrap_or_else(|| "v1".into());
    bytes.extend_from_slice(&(custom.len() as u64).to_le_bytes());
    bytes.extend_from_slice(custom.as_bytes());
    for label in labels(store) {
        bytes.extend_from_slice(&label.id.to_le_bytes());
        for field in [&label.node, &label.zh, &label.en, &label.prompt] {
            bytes.extend_from_slice(&(field.len() as u64).to_le_bytes());
            bytes.extend_from_slice(field.as_bytes());
        }
    }
    crate::ai_models::sha256_bytes(&bytes)
}

/// 分类相簿:顶层节点与显示名。
pub const CATEGORIES: [(&str, &str); 8] = [
    ("人物", "人物"),
    ("动物", "动物"),
    ("场景", "场景"),
    ("美食", "美食"),
    ("物品", "物品"),
    ("文档票据", "截图文档"),
    ("活动", "活动"),
    ("风格", "风格"),
];

/// 余弦相似度(输入已归一化,直接点积)。
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// 按绝对门槛筛选,同类最多两条兼容标签,全图最多六条。
pub fn select(
    scores: &[(i64, f32)],
    labels: &[Label],
    threshold: f32,
    margin: f32,
) -> Vec<(i64, f32)> {
    if !threshold.is_finite() || !margin.is_finite() || margin < 0.0 {
        return Vec::new();
    }
    let mut by_id = HashMap::new();
    for label in labels {
        by_id.entry(label.id).or_insert(label);
    }
    let mut candidates: Vec<(&Label, f32)> = scores
        .iter()
        .filter(|(_, score)| score.is_finite() && *score >= threshold)
        .filter_map(|(id, score)| by_id.get(id).map(|label| (*label, *score)))
        .collect();
    candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then_with(|| a.0.id.cmp(&b.0.id)));
    let mut categories: HashMap<&str, (f32, usize)> = HashMap::new();
    let mut selected: Vec<(&Label, f32)> = Vec::new();
    for (label, score) in candidates {
        let category = label.node.split('/').next().unwrap_or_default();
        let (top, count) = categories.entry(category).or_insert((score, 0));
        if *count >= 2 || *top - score > margin {
            continue;
        }
        if selected.iter().any(|(prev, _)| {
            prev.id == label.id
                || prev.node == label.node
                || prev.zh == label.zh
                || prev.en == label.en
        }) {
            continue;
        }
        selected.push((label, score));
        *count += 1;
        if selected.len() == 6 {
            break;
        }
    }
    selected.into_iter().map(|(label, score)| (label.id, score)).collect()
}

/// 写某张照片的自动标签(同版本覆盖)。
pub fn save(
    store: &Store,
    photo_id: i64,
    model_ver: &str,
    tags: &[(i64, f32)],
) -> Result<(), String> {
    store
        .write(|conn| -> Result<(), String> {
            let tx = conn.transaction().map_err(|e| e.to_string())?;
            tx.execute(
                "DELETE FROM auto_tags WHERE photo_id=?1 AND model_ver=?2",
                params![photo_id, model_ver],
            )
            .map_err(|e| e.to_string())?;
            {
                let mut st = tx
                    .prepare("INSERT OR REPLACE INTO auto_tags(photo_id, label_id, score, model_ver) VALUES(?1,?2,?3,?4)")
                    .map_err(|e| e.to_string())?;
                for (id, score) in tags {
                    st.execute(params![photo_id, id, *score as f64, model_ver])
                        .map_err(|e| e.to_string())?;
                }
            }
            tx.commit().map_err(|e| e.to_string())?;
            Ok(())
        })
}

/// 人物类节点由人脸检出决定,不参与语义打分。
pub fn is_people_node(node: &str) -> bool {
    node == "人物" || node.starts_with("人物/")
}

/// 按人脸数重写人物标签:0 脸清除,1 脸人像,≥2 脸合影;其余标签保留,合计仍限六条。
pub fn sync_people_tags(store: &Store, photo_ids: &[i64], ver: &str) -> Result<usize> {
    if photo_ids.is_empty() {
        return Ok(0);
    }
    let usable = !crate::faces::effective_sig(store).is_empty();
    store.write(|conn| -> Result<usize> {
        let tx = conn.transaction()?;
        let mut portrait: Option<i64> = None;
        let mut group: Option<i64> = None;
        {
            let mut st =
                tx.prepare("SELECT node, id FROM tag_labels WHERE node IN ('人物/人像','人物/合影')")?;
            let rows = st.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?;
            for (node, id) in rows.flatten() {
                if node == "人物/人像" {
                    portrait = Some(id);
                } else {
                    group = Some(id);
                }
            }
        }
        let mut changed = 0usize;
        for &pid in photo_ids {
            let (n, conf) = if usable {
                tx.query_row(
                    "SELECT COUNT(*), COALESCE(MAX(score), 0.0) FROM faces
                      WHERE photo_id=?1 AND ignored=0",
                    params![pid],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)),
                )?
            } else {
                (0, 0.0)
            };
            let people = match n {
                0 => None,
                1 => portrait.map(|id| (id, conf)),
                _ => group.map(|id| (id, conf)),
            };
            let prev: Option<(i64, f64)> = tx
                .query_row(
                    "SELECT t.label_id, t.score FROM auto_tags t JOIN tag_labels l ON l.id=t.label_id
                      WHERE t.photo_id=?1 AND t.model_ver=?2 AND (l.node='人物' OR l.node LIKE '人物/%')",
                    params![pid, ver],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let mut keep: Vec<(i64, f64)> = Vec::new();
            {
                let mut st = tx.prepare(
                    "SELECT t.label_id, t.score FROM auto_tags t JOIN tag_labels l ON l.id=t.label_id
                      WHERE t.photo_id=?1 AND t.model_ver=?2
                        AND NOT (l.node='人物' OR l.node LIKE '人物/%')
                      ORDER BY t.score DESC",
                )?;
                let rows = st.query_map(params![pid, ver], |r| Ok((r.get(0)?, r.get(1)?)))?;
                keep.extend(rows.flatten());
            }
            let same = match (prev, people) {
                (None, None) => true,
                (Some(a), Some(b)) => a.0 == b.0 && (a.1 - b.1).abs() < 1e-6,
                _ => false,
            };
            if same && keep.len() + usize::from(people.is_some()) <= 6 {
                continue;
            }
            tx.execute(
                "DELETE FROM auto_tags WHERE photo_id=?1 AND model_ver=?2",
                params![pid, ver],
            )?;
            let mut out = Vec::with_capacity(keep.len() + 1);
            out.extend(people);
            out.extend(keep);
            out.truncate(6);
            {
                let mut st = tx.prepare(
                    "INSERT OR REPLACE INTO auto_tags(photo_id, label_id, score, model_ver)
                     VALUES(?1, ?2, ?3, ?4)",
                )?;
                for (id, score) in out {
                    st.execute(params![pid, id, score, ver])?;
                }
            }
            changed += 1;
        }
        tx.commit()?;
        Ok(changed)
    })
}

/// 读取某张照片当前版本的标签(标签 id, 分数, 当前界面语言的名称)。
pub fn photo_tags(store: &Store, photo_id: i64, ver: &str) -> Vec<(i64, f32, String)> {
    let en = crate::i18n::lang() == crate::i18n::Lang::En;
    store.read(|conn| {
        let mut out = Vec::new();
        let Ok(mut st) = conn.prepare(
            "SELECT t.label_id, t.score, l.name_zh, l.name_en FROM auto_tags t
               JOIN tag_labels l ON l.id = t.label_id
              WHERE t.photo_id=?1 AND t.model_ver=?2 ORDER BY t.score DESC",
        ) else {
            return out;
        };
        if let Ok(rows) = st.query_map(params![photo_id, ver], |r| {
            let name = if en {
                r.get::<_, String>(3)?
            } else {
                r.get::<_, String>(2)?
            };
            Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)? as f32, name))
        }) {
            out.extend(rows.flatten());
        }
        out
    })
}

/// 某分类下的照片 id。
pub fn category_ids(store: &Store, category: &str, ver: &str, limit: usize) -> Vec<i64> {
    store.read(|conn| {
        let mut out = Vec::new();
        let Ok(mut st) = conn.prepare(
            "SELECT DISTINCT t.photo_id FROM auto_tags t
               JOIN tag_labels l ON l.id = t.label_id
              WHERE t.model_ver=?1 AND (l.node=?2 OR l.node LIKE ?3)
              ORDER BY t.photo_id LIMIT ?4",
        ) else {
            return out;
        };
        let like = format!("{category}/%");
        if let Ok(rows) = st.query_map(params![ver, category, like, limit as i64], |r| {
            r.get::<_, i64>(0)
        }) {
            out.extend(rows.flatten());
        }
        out
    })
}

/// 某分类下的照片总数(不受展示上限影响)。
pub fn category_count(store: &Store, category: &str, ver: &str) -> usize {
    store.read(|conn| {
        conn.query_row(
            "SELECT COUNT(DISTINCT t.photo_id) FROM auto_tags t
               JOIN tag_labels l ON l.id = t.label_id
              WHERE t.model_ver=?1 AND (l.node=?2 OR l.node LIKE ?3)",
            params![ver, category, format!("{category}/%")],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0) as usize
    })
}

/// 分类相簿计数(仅当前版本):一次分组统计,不把 id 拉进内存。
pub fn category_counts(store: &Store, ver: &str) -> Vec<(&'static str, usize)> {
    let mut top: HashMap<String, usize> = HashMap::new();
    store.read(|conn| {
        let Ok(mut st) = conn.prepare(
            "SELECT CASE WHEN instr(l.node,'/')>0
                         THEN substr(l.node,1,instr(l.node,'/')-1)
                         ELSE l.node END AS cat,
                    COUNT(DISTINCT t.photo_id)
               FROM auto_tags t JOIN tag_labels l ON l.id=t.label_id
              WHERE t.model_ver=?1 GROUP BY cat",
        ) else {
            return;
        };
        if let Ok(rows) = st.query_map(params![ver], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
        }) {
            top.extend(rows.flatten());
        };
    });
    CATEGORIES
        .iter()
        .map(|(node, label)| (*label, top.get(*node).copied().unwrap_or(0)))
        .collect()
}

/// 是否文字类照片(OCR 智能触发用)。
pub fn is_text_photo(store: &Store, photo_id: i64, ver: &str) -> bool {
    store.read(|conn| {
        conn.query_row(
            "SELECT COUNT(*) FROM auto_tags t JOIN tag_labels l ON l.id=t.label_id
              WHERE t.photo_id=?1 AND t.model_ver=?2 AND l.node LIKE '文档票据/%'",
            params![photo_id, ver],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
            > 0
    })
}

/// 一批 id 中已判定为文字类的照片(分块绑定,避开 SQLite 变量上限)。
pub fn text_photo_ids(store: &Store, ver: &str, ids: &[i64]) -> std::collections::HashSet<i64> {
    use rusqlite::types::Value;
    let mut out = std::collections::HashSet::new();
    if ids.is_empty() {
        return out;
    }
    store.read(|conn| {
        for chunk in ids.chunks(900) {
            let sql = format!(
                "SELECT DISTINCT t.photo_id FROM auto_tags t JOIN tag_labels l ON l.id=t.label_id
                  WHERE l.node LIKE '文档票据/%' AND t.model_ver=?{} AND t.photo_id IN ({})",
                chunk.len() + 1,
                (1..=chunk.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(",")
            );
            let Ok(mut st) = conn.prepare(&sql) else {
                continue;
            };
            let binds = rusqlite::params_from_iter(
                chunk
                    .iter()
                    .map(|id| Value::from(*id))
                    .chain(std::iter::once(Value::from(ver.to_string()))),
            );
            let Ok(rows) = st.query_map(binds, |r| r.get::<_, i64>(0)) else {
                continue;
            };
            out.extend(rows.flatten());
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_vocabulary_is_complete() {
        assert!(
            builtin_count() >= 120,
            "词表应不少于 120 条,实际 {}",
            builtin_count()
        );
        let mut nodes = std::collections::HashSet::new();
        for (node, _, _, _) in BUILTIN {
            nodes.insert(node.split('/').next().unwrap());
        }
        assert_eq!(nodes.len(), 8, "应覆盖 8 个大类");
    }

    #[test]
    fn seeding_is_idempotent() {
        let store = Store::open_mem().unwrap();
        assert_eq!(seed_labels(&store).unwrap(), builtin_count());
        assert_eq!(seed_labels(&store).unwrap(), 0);
        assert_eq!(label_count(&store), builtin_count());
    }

    #[test]
    fn text_photo_ids_spans_binding_chunks() {
        let store = Store::open_mem().unwrap();
        seed_labels(&store).unwrap();
        let lid: i64 = store.read(|c| {
            c.query_row(
                "SELECT id FROM tag_labels WHERE node='文档票据/截图'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        });
        let n = 905i64;
        let marked = [1i64, 900, 901, 905];
        store
            .write(|c| -> Result<()> {
                let tx = c.transaction()?;
                {
                    let mut st = tx.prepare(
                        "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,'C:\\',0,0)",
                    )?;
                    for id in 1..=n {
                        st.execute(params![id, format!("C:\\{id}.jpg")])?;
                    }
                    let mut tg = tx.prepare(
                        "INSERT INTO auto_tags(photo_id,label_id,score,model_ver) VALUES(?1,?2,0.9,'v0')",
                    )?;
                    for id in marked {
                        tg.execute(params![id, lid])?;
                    }
                }
                tx.commit()?;
                Ok(())
            })
            .unwrap();
        let ids: Vec<i64> = (1..=n).collect();
        let got = text_photo_ids(&store, "v0", &ids);
        assert_eq!(
            got,
            marked.into_iter().collect::<std::collections::HashSet<i64>>(),
            "跨分块边界(900/901)与末尾都要命中"
        );
        assert!(text_photo_ids(&store, "other", &ids).is_empty());
    }

    fn label(id: i64, node: &str, zh: &str, en: &str) -> Label {
        Label {
            id,
            node: node.into(),
            zh: zh.into(),
            en: en.into(),
            prompt: String::new(),
        }
    }

    #[test]
    fn select_requires_threshold_and_margin() {
        let labels = vec![
            label(1, "人物/合影", "合影", "group photo"),
            label(2, "人物/自拍", "自拍", "selfie"),
        ];
        assert!(
            select(&[(1, 0.20f32), (2, 0.10)], &labels, 0.26, 0.06).is_empty(),
            "低于下限应丢弃"
        );
        let gap = select(&[(1, 0.40f32), (2, 0.20)], &labels, 0.26, 0.06);
        assert_eq!(gap.len(), 1, "落后超过 margin 的同类标签应丢弃");
        assert_eq!(gap[0].0, 1);
        let close = select(&[(1, 0.40f32), (2, 0.37)], &labels, 0.26, 0.06);
        assert_eq!(close.len(), 2, "margin 内的同类标签可同时命中");
    }

    #[test]
    fn select_ignores_unknown_label_ids() {
        let labels = vec![label(1, "动物/猫", "猫", "cat")];
        let got = select(&[(1, 0.50f32), (99, 0.90)], &labels, 0.26, 0.06);
        assert_eq!(got, vec![(1, 0.50)]);
    }

    #[test]
    fn cosine_handles_mismatched_lengths() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0]), 0.0);
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn category_ids_matches_top_level_and_children() {
        let store = Store::open_mem().unwrap();
        store
            .write(|conn| {
                conn.execute_batch(
                    "INSERT INTO photos(id,path,parent,mtime,size) VALUES(1,'a','a',0,0);
                     INSERT INTO tag_labels(id,node,name_zh,name_en,prompt_en,builtin,ver)
                         VALUES(1,'人物','人','person','a person',1,'v1');
                     INSERT INTO tag_labels(id,node,name_zh,name_en,prompt_en,builtin,ver)
                         VALUES(2,'人物/合影','合影','group','a group',1,'v1');
                     INSERT INTO auto_tags(photo_id,label_id,score,model_ver)
                         VALUES(1,2,0.9,'v1');",
                )
            })
            .unwrap();
        assert_eq!(category_ids(&store, "人物", "v1", 10), vec![1]);
        assert!(category_ids(&store, "动物", "v1", 10).is_empty());
        assert!(category_ids(&store, "人物", "v0", 10).is_empty());
    }

    /// 分类计数与 id 口径一致:同图多标签只算一张,展示上限不影响总数。
    #[test]
    fn category_counts_match_distinct_photos() {
        let store = Store::open_mem().unwrap();
        seed_labels(&store).unwrap();
        let cat = node_id(&store, "动物/猫");
        let dog = node_id(&store, "动物/狗");
        let shot = node_id(&store, "文档票据/截图");
        for id in 1..=3 {
            add_photo(&store, id);
        }
        store
            .write(|c| {
                c.execute_batch(&format!(
                    "INSERT INTO auto_tags(photo_id,label_id,score,model_ver) VALUES
                       (1,{cat},0.5,'v1'),(1,{dog},0.4,'v1'),
                       (2,{cat},0.6,'v1'),(3,{shot},0.7,'v1'),
                       (2,{cat},0.9,'v0');"
                ))
                .unwrap();
            });
        let counts = category_counts(&store, "v1");
        assert_eq!(counts.len(), CATEGORIES.len());
        let animals = counts.iter().find(|(n, _)| *n == "动物").unwrap().1;
        assert_eq!(animals, 2, "同图两个动物标签只算一张");
        assert_eq!(
            animals,
            category_ids(&store, "动物", "v1", usize::MAX).len(),
            "计数与 id 口径必须一致"
        );
        assert_eq!(category_count(&store, "动物", "v1"), 2);
        assert_eq!(
            category_ids(&store, "动物", "v1", 1).len(),
            1,
            "展示上限只截断 id"
        );
        assert_eq!(category_count(&store, "动物", "v0"), 1, "旧版本单独计数");
        let docs = counts.iter().find(|(n, _)| *n == "截图文档").unwrap().1;
        assert_eq!(docs, 1);
        assert_eq!(counts.iter().find(|(n, _)| *n == "人物").unwrap().1, 0);
    }

    /// 标签名跟着界面语言走,英文界面不再显示中文。
    #[test]
    fn photo_tags_follow_ui_language() {
        let store = Store::open_mem().unwrap();
        seed_labels(&store).unwrap();
        let cat = node_id(&store, "动物/猫");
        add_photo(&store, 1);
        save(&store, 1, "v1", &[(cat, 0.9)]).unwrap();
        {
            let _g = crate::i18n::LangGuard::to(crate::i18n::Lang::Zh);
            assert_eq!(photo_tags(&store, 1, "v1")[0].2, "猫");
        }
        let _g = crate::i18n::LangGuard::to(crate::i18n::Lang::En);
        assert_eq!(photo_tags(&store, 1, "v1")[0].2, "cat");
    }

    fn node_id(store: &Store, node: &str) -> i64 {
        store.read(|c| {
            c.query_row(
                "SELECT id FROM tag_labels WHERE node=?1",
                params![node],
                |r| r.get(0),
            )
            .unwrap()
        })
    }

    fn add_photo(store: &Store, id: i64) {
        store
            .write(|c| {
                c.execute(
                    "INSERT INTO photos(id,path,parent,mtime,size) VALUES(?1,?2,?2,0,0)",
                    params![id, format!("p{id}")],
                )
            })
            .unwrap();
    }

    #[test]
    fn seeding_keeps_ids_and_prunes_retired_nodes() {
        let store = Store::open_mem().unwrap();
        seed_labels(&store).unwrap();
        let group = node_id(&store, "人物/合影");
        let cat = node_id(&store, "动物/猫");
        add_photo(&store, 1);
        store
            .write(|c| {
                c.execute_batch(
                    "UPDATE tag_labels SET prompt_en='drifted' WHERE node='人物/合影';
                     INSERT INTO tag_labels(id,node,name_zh,name_en,prompt_en,builtin,ver)
                         VALUES(9001,'人物/旧标签','旧','old','old',1,'v1');
                     INSERT INTO auto_tags(photo_id,label_id,score,model_ver) VALUES(1,9001,0.5,'v1');",
                )
            })
            .unwrap();
        assert_eq!(seed_labels(&store).unwrap(), 0, "无新增条目时返回 0");
        assert_eq!(node_id(&store, "人物/合影"), group, "既有 id 不因词表改动而漂移");
        assert_eq!(node_id(&store, "动物/猫"), cat);
        let prompt: String = store
            .read(|c| {
                c.query_row(
                    "SELECT prompt_en FROM tag_labels WHERE id=?1",
                    params![group],
                    |r| r.get(0),
                )
                .unwrap()
            });
        assert_ne!(prompt, "drifted", "内置条目的 prompt 应被词表复位");
        let left: i64 = store
            .read(|c| {
                c.query_row(
                    "SELECT (SELECT COUNT(*) FROM tag_labels WHERE id=9001)
                          + (SELECT COUNT(*) FROM auto_tags WHERE label_id=9001)",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
            });
        assert_eq!(left, 0, "下线条目连同其自动标签一并清除");
    }

    #[test]
    fn seeding_readds_missing_node_with_fresh_id() {
        let store = Store::open_mem().unwrap();
        seed_labels(&store).unwrap();
        let cat = node_id(&store, "动物/猫");
        store
            .write(|c| c.execute("DELETE FROM tag_labels WHERE node='动物/狗'", []))
            .unwrap();
        assert_eq!(seed_labels(&store).unwrap(), 1);
        assert_eq!(node_id(&store, "动物/猫"), cat, "补条目不得挤占其他 id");
        assert_ne!(node_id(&store, "动物/狗"), cat);
        assert_eq!(label_count(&store), builtin_count());
    }

    #[test]
    fn people_tags_follow_face_count() {
        let store = Store::open_mem().unwrap();
        seed_labels(&store).unwrap();
        for id in 1..=4 {
            add_photo(&store, id);
        }
        store
            .write(|c| {
                c.execute_batch(
                    "INSERT INTO faces(id,photo_id,score,ignored) VALUES(1,2,0.9,0);
                     INSERT INTO faces(id,photo_id,score,ignored) VALUES(2,3,0.8,0),(3,3,0.7,0);
                     INSERT INTO faces(id,photo_id,score,ignored) VALUES(4,4,0.95,1);",
                )
            })
            .unwrap();
        let portrait = node_id(&store, "人物/人像");
        let group = node_id(&store, "人物/合影");
        assert_eq!(sync_people_tags(&store, &[1, 2, 3, 4], "v1").unwrap(), 2);
        assert!(photo_tags(&store, 1, "v1").is_empty(), "无人脸不打人物标签");
        assert_eq!(photo_tags(&store, 2, "v1")[0].0, portrait);
        assert_eq!(photo_tags(&store, 3, "v1")[0].0, group);
        assert!(photo_tags(&store, 4, "v1").is_empty(), "已忽略的人脸不计数");
        assert_eq!(sync_people_tags(&store, &[1, 2, 3, 4], "v1").unwrap(), 0, "重复同步不改写");
        assert_eq!(category_ids(&store, "人物", "v1", 10), vec![2, 3]);
    }

    #[test]
    fn people_tag_keeps_total_within_six() {
        let store = Store::open_mem().unwrap();
        seed_labels(&store).unwrap();
        add_photo(&store, 1);
        store
            .write(|c| {
                c.execute_batch(
                    "INSERT INTO faces(id,photo_id,score,ignored) VALUES(1,1,0.9,0),(2,1,0.8,0);",
                )
            })
            .unwrap();
        let others: Vec<i64> = store.read(|c| {
            let mut st = c
                .prepare("SELECT id FROM tag_labels WHERE node NOT LIKE '人物%' ORDER BY id LIMIT 6")
                .unwrap();
            let rows = st.query_map([], |r| r.get::<_, i64>(0)).unwrap();
            rows.flatten().collect()
        });
        let tags: Vec<(i64, f32)> = others
            .iter()
            .enumerate()
            .map(|(i, id)| (*id, 0.30 - i as f32 * 0.01))
            .collect();
        save(&store, 1, "v1", &tags).unwrap();
        sync_people_tags(&store, &[1], "v1").unwrap();
        let got = photo_tags(&store, 1, "v1");
        assert_eq!(got.len(), 6, "人物标签挤掉最低分,总数仍为六条");
        assert_eq!(got[0].0, node_id(&store, "人物/合影"));
        assert!(!got.iter().any(|(id, _, _)| *id == others[5]));
    }

    #[test]
    fn people_tags_drop_when_pipeline_stale() {
        let store = Store::open_mem().unwrap();
        seed_labels(&store).unwrap();
        add_photo(&store, 1);
        store
            .write(|c| {
                c.execute_batch(
                    "INSERT INTO faces(id,photo_id,score,ignored) VALUES(1,1,0.9,0);",
                )
            })
            .unwrap();
        sync_people_tags(&store, &[1], "v1").unwrap();
        assert_eq!(photo_tags(&store, 1, "v1").len(), 1);
        store
            .write(|c| {
                c.execute(
                    "INSERT INTO face_templates(face_id,dim,vec,sig) VALUES(1,128,x'00','old')",
                    [],
                )
            })
            .unwrap();
        sync_people_tags(&store, &[1], "v1").unwrap();
        assert!(photo_tags(&store, 1, "v1").is_empty(), "待重建期间人物标签作废");
    }

    #[test]
    fn people_nodes_are_excluded_from_semantic_vocab() {
        for &(node, _, _, _) in BUILTIN {
            if is_people_node(node) {
                assert!(
                    node == "人物/人像" || node == "人物/合影",
                    "人物类只保留人脸可判定的两条:{node}"
                );
            }
        }
        assert!(!is_people_node("人物写真/海报"));
    }
}
