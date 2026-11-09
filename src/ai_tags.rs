//! 自动标签:内置词表、灵敏度阈值、打分与分类相簿查询。

use crate::db::Store;
use anyhow::Result;
use rusqlite::params;

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
    ("人物/自拍", "自拍", "selfie", "a selfie photo of a person"),
    (
        "人物/合影",
        "合影",
        "group photo",
        "a group photo of people",
    ),
    ("人物/儿童", "儿童", "child", "a photo of a child"),
    ("人物/婚礼", "婚礼", "wedding", "a wedding photo"),
    (
        "人物/证件照",
        "证件照",
        "portrait",
        "a formal portrait of a person",
    ),
    ("人物/舞台", "舞台", "stage", "people performing on a stage"),
    ("人物/运动", "运动人物", "sports", "a person playing sports"),
    ("人物/背影", "背影", "back view", "the back of a person"),
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

/// 灵敏度档位:top1 下限与 top2 领先幅度。
const SENSITIVITY: [(f32, f32); 3] = [(0.22, 0.03), (0.26, 0.06), (0.30, 0.09)];

/// 内置词表条目数。
pub fn builtin_count() -> usize {
    BUILTIN.len()
}

/// 按灵敏度档位取 (top1 下限, margin)。
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

/// 写入内置词表(幂等;只补缺失 id)。
pub fn seed_labels(store: &Store) -> Result<usize> {
    let ts = crate::ai_queue::now_ms();
    store.write(|conn| -> Result<usize> {
        let tx = conn.transaction()?;
        let mut n = 0usize;
        {
            let mut st = tx.prepare(
                "INSERT OR IGNORE INTO tag_labels(id, node, name_zh, name_en, prompt_en, parent, builtin, ver)
                 VALUES(?1, ?2, ?3, ?4, ?5, NULL, 1, 'v1')",
            )?;
            for (i, (node, zh, en, prompt)) in BUILTIN.iter().enumerate() {
                n += st.execute(params![(i + 1) as i64, node, zh, en, prompt])?;
            }
        }
        tx.commit()?;
        let _ = ts;
        Ok(n)
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

/// 词表版本(条数变化即触发标签阶段重建)。
pub fn vocab_version(store: &Store) -> String {
    let n = label_count(store);
    let custom = store
        .get_setting("ai_labels_ver")
        .unwrap_or_else(|| "v1".into());
    format!("{custom}:{n}")
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

/// 打分并筛选:top1 过下限且领先 top2 达到 margin。
pub fn select(scores: &mut Vec<(i64, f32)>, threshold: f32, margin: f32) -> Vec<(i64, f32)> {
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let Some(&(id, top)) = scores.first() else {
        return Vec::new();
    };
    if top < threshold {
        return Vec::new();
    }
    let second = scores.get(1).map(|s| s.1).unwrap_or(0.0);
    if top - second < margin {
        return Vec::new();
    }
    vec![(id, top)]
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

/// 读取某张照片当前版本的标签(标签 id, 分数, 中文名)。
pub fn photo_tags(store: &Store, photo_id: i64, ver: &str) -> Vec<(i64, f32, String)> {
    store.read(|conn| {
        let mut out = Vec::new();
        let Ok(mut st) = conn.prepare(
            "SELECT t.label_id, t.score, l.name_zh FROM auto_tags t
               JOIN tag_labels l ON l.id = t.label_id
              WHERE t.photo_id=?1 AND t.model_ver=?2 ORDER BY t.score DESC",
        ) else {
            return out;
        };
        if let Ok(rows) = st.query_map(params![photo_id, ver], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, f64>(1)? as f32,
                r.get::<_, String>(2)?,
            ))
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

/// 分类相簿计数(仅当前版本)。
pub fn category_counts(store: &Store, ver: &str) -> Vec<(&'static str, usize)> {
    CATEGORIES
        .iter()
        .map(|(node, label)| (*label, category_ids(store, node, ver, usize::MAX).len()))
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
    fn select_requires_threshold_and_margin() {
        let mut low = vec![(1, 0.20f32), (2, 0.10)];
        assert!(
            select(&mut low, 0.26, 0.06).is_empty(),
            "top1 低于下限应丢弃"
        );
        let mut close = vec![(1, 0.40f32), (2, 0.37)];
        assert!(
            select(&mut close, 0.26, 0.06).is_empty(),
            "领先不足 margin 应丢弃"
        );
        let mut good = vec![(1, 0.45f32), (2, 0.20)];
        let got = select(&mut good, 0.26, 0.06);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 1);
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
}
