//! 字符串驻留池(按列隔离的去重池)
//!
//! # 设计背景
//!
//! Picasa 3 用的是自研列式存储,字符串列的**去重字符串本体**与
//! **行记录里存的 32 位整数 id** 分离。本项目沿用同样的**可观测契约**,
//! 但物理存储仍用 SQLite(见 `db.rs`)。
//!
//! | 语义 | 说明 |
//! |---|---|
//! | 池 = 去重哈希表 | 池的建立 |
//! | **id = 字符串数组的下标** | 查重后 append |
//! | 新 id 触发回调通知宿主 | 查重哈希 |
//! | 按 id 取回字符串 | 池内按 id 取串 |
//! | 落盘前字符串原地替换为 id | 字符串入池 |
//! | 读盘时逐条重新入池 | 池读盘重建 |
//! | **无引用计数**;只在整池销毁/整池序列化时释放 | 池序列化 / 池的释放 |
//! | ⇒ 磁盘上等效「写时重建」,**每轮保存重算整个去重池** | 整池序列化 + 只对现存行入池 |
//!
//! # 本模块做什么
//!
//! 实现**可观测契约**(而不是磁盘布局):
//! - 相同字符串重复 intern → **恒返回同一 id**(去重);
//! - id **只增不复用**,append-only;
//! - **无引用计数**,回收靠 `vacuum()` 显式重建(对齐原版"写时重建");
//! - 池按**列**(pool 名)隔离 —— 每列一个池,不是全局一池。
//!
//! # 与 Picasa 3 存储方式的差异(诚实声明)
//!
//! 1. **桶布局简化**:本实现用单组开放定址,查重哈希与探测起点公式保持一致,
//!    故"同串→同 id"的契约一致,但桶内元素排列不同(不可观测)。
//! 2. **不落独立池文件**:我方是 SQLite,池是普通表(见 `db.rs`)。
//! 3. 池内用 `Vec` 保序,便于有序遍历。
//!
//! 池化收益的来源:相机型号(`photos.cam`)、文件夹路径这类**低基数高重复**列,
//! 每行省下重复的字符串本体。

use std::collections::HashMap;

/// 查重哈希(seed `0x12345678`,滚动 xor)。
///
/// 字符串查重哈希:
/// ```c
/// h = 0x12345678;
/// while (*p) {
///   h = h ^ (h * 0x20 + (h >> 2) + (int)(char)*p);
///   p++;
/// }
/// ```
///
/// **注意 `(int)(char)` 是有符号扩展** —— 非 ASCII 字节(UTF-8 续字节
/// ≥0x80)会算成负数。这是原版行为,保持一致。
///
/// `*0x20 == <<5`,`h >> 2` 是算术右移(`h` 为 `int`)。
#[must_use]
pub fn pool_hash(s: &str) -> u32 {
    let mut h: i32 = 0x1234_5678u32 as i32;
    for &b in s.as_bytes() {
        h = h ^ (h.wrapping_mul(0x20).wrapping_add(h >> 2).wrapping_add(b as i8 as i32));
    }
    h as u32
}

/// 单列的驻留池。
#[derive(Debug, Default)]
pub struct ColumnPool {
    /// id → 值。id 就是下标(与原版一致的下标寻址)。
    values: Vec<String>,
    /// 值 → id(去重索引)。原版是开放定址桶,这里用 HashMap;
    /// 契约等价,性能更好。
    index: HashMap<String, i64>,
    /// 桶掩码缓存(原版 `(n-1) & hash` 的 `n-1`)。
    _bucket_hint: usize,
}

impl ColumnPool {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 驻留一个字符串,返回其池 id。
    ///
    /// 语义:同串恒返回同 id;新串 **append 追加**,id = 原长度。
    /// 对应原版字符串查重哈希(链上精确比较)
    /// 与 `:145-175`(未命中则 append)。
    pub fn intern(&mut self, s: &str) -> i64 {
        if let Some(&id) = self.index.get(s) {
            return id;
        }
        let id = self.values.len() as i64;
        self.values.push(s.to_string());
        self.index.insert(s.to_string(), id);
        id
    }

    /// 按 id 取回字符串。原版池内按 id 取串。
    ///
    /// 越界返回 `None`(原版越界会落到空指针分支,这里显式化)。
    #[must_use]
    pub fn get(&self, id: i64) -> Option<&str> {
        if id < 0 {
            return None;
        }
        self.values.get(id as usize).map(String::as_str)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// 所有值的快照(重建落盘用)。
    #[must_use]
    pub fn values(&self) -> &[String] {
        &self.values
    }

    /// 写时重建:只保留 `live` 中仍被引用的 id,**其余全部丢弃并重新编号**。
    ///
    /// 这是原版"无引用计数 + 每轮保存重算整个池"的等价实现
    /// (池序列化整池序列化 + 字符串入池只对现存行入池)。
    ///
    /// `live` 给出**旧 id 集合**;返回 `旧id → 新id` 的重映射表
    /// (调用方据此 UPDATE 业务表的池列)。未在 `live` 中的旧 id 不出现在映射里。
    pub fn vacuum(&mut self, live: &HashSet) -> HashMap<i64, i64> {
        let mut remap = HashMap::new();
        let mut next: Vec<String> = Vec::new();
        let mut new_index = HashMap::new();
        // 按**旧 id 升序**遍历,保证新 id 分配确定(可复现),
        // 而不是依赖 HashMap 的迭代序(不确定)。
        let mut old_ids: Vec<i64> = live.iter().copied().collect();
        old_ids.sort_unstable();
        for old in old_ids {
            let Some(s) = self.values.get(old as usize) else {
                continue;
            };
            // live 是集合,理论上不会重复;防御性去重,保证 append-only 不变体。
            if new_index.contains_key(s) {
                continue;
            }
            let new = next.len() as i64;
            next.push(s.clone());
            new_index.insert(s.clone(), new);
            remap.insert(old, new);
        }
        self.values = next;
        self.index = new_index;
        remap
    }

    /// 整池清空(对齐池的释放的池析构:
    /// 遍历全部桶逐条释放字符串,无引用计数)。
    ///
    /// `vacuum_pool` 走的是 `vacuum()`(保留存活项);
    /// 本方法对应原版**整池销毁**那条路径。生产代码不需要显式调用
    /// (池是短生命周期对象,随宿主一起释放),保留是为了让内存池与
    /// 原版 API 一一对应,且可独立验证。
    #[cfg(test)]
    pub fn clear(&mut self) {
        self.values.clear();
        self.index.clear();
    }
}

/// 原版用开放定址桶,这里显式别名以便测试与 vacuum 参数类型一致。
pub type HashSet = std::collections::HashSet<i64>;

/// 多列池集合 —— key = 池名(`<表>_<列>`)。
///
/// 关键:**按列隔离,不共用一个池**。每列一个池,
/// 不同列的 id 空间互相独立 —— `albumdata_name` 的 id=5 和
/// `catdata_name` 的 id=5 是两个不同的字符串。
#[derive(Debug, Default)]
pub struct PoolSet {
    pools: HashMap<String, ColumnPool>,
}

impl PoolSet {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn column_mut(&mut self, name: &str) -> &mut ColumnPool {
        self.pools.entry(name.to_string()).or_default()
    }

    #[must_use]
    pub fn column(&self, name: &str) -> Option<&ColumnPool> {
        self.pools.get(name)
    }

    pub fn intern(&mut self, pool: &str, s: &str) -> i64 {
        self.column_mut(pool).intern(s)
    }

    #[must_use]
    pub fn get(&self, pool: &str, id: i64) -> Option<&str> {
        self.pools.get(pool).and_then(|p| p.get(id))
    }

    /// 只保留 `live_by_pool[pool]` 中仍被引用的 id,并做 id 重映射。
    ///
    /// 返回 `{池名 → {旧id → 新id}}`。
    pub fn vacuum_all(&mut self, live_by_pool: &HashMap<String, HashSet>) -> HashMap<String, HashMap<i64, i64>> {
        let mut out = HashMap::new();
        for (name, pool) in &mut self.pools {
            let empty = HashSet::new();
            let live = live_by_pool.get(name).unwrap_or(&empty);
            out.insert(name.clone(), pool.vacuum(live));
        }
        out
    }

    /// 池名列表(诊断/统计用)。
    pub fn names(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.pools.keys().map(String::as_str).collect();
        v.sort_unstable();
        v
    }

    /// 总条目数(诊断:对比业务表行数即得去重率)。
    #[must_use]
    pub fn total_entries(&self) -> usize {
        self.pools.values().map(ColumnPool::len).sum()
    }

    /// 供 UI/诊断遍历某一列的全部值(池快照,按 id 升序)。
    #[must_use]
    pub fn column_values(&self, name: &str) -> Option<&[String]> {
        self.pools.get(name).map(ColumnPool::values)
    }
}

/// 预估一次 vacuum 能省下多少条:返回 `(池名, 当前条数, 存活条数)`。
///
/// 纯函数,不改任何状态 —— 给 UI 在真正重建**之前**展示"能省多少"。
/// 用存活 id 集合查询当前池,复用 `ColumnPool::get` 做孤儿判定
/// (对齐原版池序列化遍历全部 id 决定去留的逻辑)。
#[must_use]
pub fn vacuum_plan(set: &PoolSet, live_by_pool: &HashMap<String, HashSet>) -> Vec<(String, usize, usize)> {
    let empty = HashSet::new();
    let mut out = Vec::new();
    for name in set.names() {
        let Some(col) = set.column(name) else { continue };
        let live = live_by_pool.get(name).unwrap_or(&empty);
        // 存活条目数 = 存活集合里能在池中解析到字符串的个数
        let alive = live.iter().filter(|&&id| col.get(id).is_some()).count();
        out.push((name.to_string(), col.len(), alive));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_deterministic_and_order_sensitive() {
        // 原版是滚动 xor,顺序敏感(不是可交换的乘加哈希)
        assert_eq!(pool_hash("abc"), pool_hash("abc"));
        assert_ne!(pool_hash("abc"), pool_hash("acb"));
    }

    #[test]
    fn hash_matches_original_seed_path() {
        // 手工按字符串查重哈希复算 "a" = 0x61
        let mut h: i32 = 0x1234_5678u32 as i32;
        h = h ^ (h.wrapping_mul(0x20).wrapping_add(h >> 2).wrapping_add(0x61i32));
        assert_eq!(pool_hash("a"), h as u32);
    }

    #[test]
    fn hash_sign_extends_non_ascii_bytes() {
        // UTF-8 的 ≥0x80 续字节按 (int)(char) 变负数 —— 原版行为
        let mut a: i32 = 0x1234_5678u32 as i32;
        for &b in "é".as_bytes() {
            a = a ^ (a.wrapping_mul(0x20).wrapping_add(a >> 2).wrapping_add(b as i8 as i32));
        }
        assert_eq!(pool_hash("é"), a as u32);
        // 若按无符号算会不同
        let mut c: i32 = 0x1234_5678u32 as i32;
        for &b in "é".as_bytes() {
            c = c ^ (c.wrapping_mul(0x20).wrapping_add(c >> 2).wrapping_add(b as i32));
        }
        assert_ne!(pool_hash("é"), c as u32);
    }

    #[test]
    fn empty_string_hashes_to_seed() {
        assert_eq!(pool_hash(""), 0x1234_5678u32);
    }

    #[test]
    fn same_string_returns_same_id() {
        let mut p = ColumnPool::new();
        let a = p.intern("Canon EOS R5");
        let b = p.intern("Canon EOS R5");
        assert_eq!(a, b);
        assert_eq!(p.len(), 1);
    }

    #[test]
    fn ids_are_append_only_and_equal_to_index() {
        // 原版 id = 字符串指针数组下标(字符串查重哈希)
        let mut p = ColumnPool::new();
        assert_eq!(p.intern("a"), 0);
        assert_eq!(p.intern("b"), 1);
        assert_eq!(p.intern("c"), 2);
        assert_eq!(p.intern("a"), 0, "重复不追加");
        assert_eq!(p.len(), 3);
    }

    #[test]
    fn get_roundtrips_and_out_of_range_is_none() {
        let mut p = ColumnPool::new();
        let id = p.intern("D:\\Photos");
        assert_eq!(p.get(id), Some("D:\\Photos"));
        assert_eq!(p.get(99), None);
        assert_eq!(p.get(-1), None);
    }

    #[test]
    fn empty_pool_get_is_none() {
        let p = ColumnPool::new();
        assert!(p.is_empty());
        assert_eq!(p.get(0), None);
    }

    #[test]
    fn vacuum_keeps_live_and_renumbers_deterministically() {
        let mut p = ColumnPool::new();
        for s in ["a", "b", "c", "d"] {
            p.intern(s);
        }
        // 只留 0(a) 和 2(c) → 新 id 应为 0 和 1
        let live: HashSet = [0i64, 2].into_iter().collect();
        let remap = p.vacuum(&live);
        assert_eq!(remap.get(&0), Some(&0));
        assert_eq!(remap.get(&2), Some(&1));
        assert_eq!(p.len(), 2);
        assert_eq!(p.get(0), Some("a"));
        assert_eq!(p.get(1), Some("c"));
        assert!(remap.get(&1).is_none(), "死 id 不该出现在映射里");
    }

    #[test]
    fn vacuum_remap_is_independent_of_set_iteration_order() {
        // 关键:HashSet 迭代序不确定,重映射必须按旧 id 升序分配才可复现
        let build = |n: usize| {
let mut p = ColumnPool::new();
        for i in 0..n {
            p.intern(&format!("v{i}"));
        }
        p
        };
        let live: HashSet = (0..50).filter(|i| i % 3 == 0).collect();
        let a = build(50).vacuum(&live);
        // 用不同的插入顺序构造"同一个集合",验证重映射只依赖升序遍历
        let shuffled: HashSet = (0..50).rev().filter(|i| i % 3 == 0).collect();
        assert_ne!(
            live.iter().copied().collect::<Vec<_>>(),
            shuffled.iter().copied().collect::<Vec<_>>(),
            "两个集合的迭代序应不同(否则本测试无意义)"
        );
        let b = build(50).vacuum(&shuffled);
        assert_eq!(a, b);
    }

    #[test]
    fn vacuum_all_drops_orphan_ids() {
        let mut ps = PoolSet::new();
        ps.intern("cam", "Nikon D850");
        ps.intern("cam", "Nikon Z9");
        ps.intern("cam", "Canon R5");
        ps.intern("parent", "D:\\A");
        // cam 只保留 Nikon D850(旧 id 0);parent 全部保留
        let mut live: HashMap<String, HashSet> = HashMap::new();
        live.insert("cam".into(), [0i64].into_iter().collect());
        live.insert("parent".into(), [0i64].into_iter().collect());
        let remap = ps.vacuum_all(&live);
        assert_eq!(ps.column("cam").unwrap().len(), 1);
        assert_eq!(ps.column("parent").unwrap().len(), 1);
        assert_eq!(remap["cam"].len(), 1);
    }

    #[test]
    fn pools_are_isolated_per_column() {
        // 原版每列一个池,id 空间互相独立
        let mut ps = PoolSet::new();
        assert_eq!(ps.intern("cam", "X"), 0);
        assert_eq!(ps.intern("parent", "Y"), 0);
        assert_eq!(ps.intern("cam", "Z"), 1);
        assert_eq!(ps.get("cam", 0), Some("X"));
        assert_eq!(ps.get("parent", 0), Some("Y"));
        // 同一个 id(0)在两列指向不同字符串 —— 池不能混用
        assert_ne!(ps.get("cam", 0), ps.get("parent", 0));
        assert_eq!(ps.get("cam", 1), Some("Z"));
    }

    #[test]
    fn names_are_sorted_and_total_entries_sums() {
        let mut ps = PoolSet::new();
        ps.intern("zzz", "a");
        ps.intern("aaa", "b");
        ps.intern("aaa", "c");
        assert_eq!(ps.names(), vec!["aaa", "zzz"]);
        assert_eq!(ps.total_entries(), 3);
    }

    #[test]
    fn clear_empties_pool() {
        let mut p = ColumnPool::new();
        p.intern("a");
        p.clear();
        assert!(p.is_empty());
        assert_eq!(p.intern("a"), 0, "清空后重新分配");
    }

    #[test]
    fn realistic_dedup_ratio() {
        // 1000 行照片,只有 5 种相机型号 → 池里只有 5 条
        let mut p = ColumnPool::new();
        let cams = ["Nikon D850", "Canon R5", "Sony A7IV", "Fuji X-T5", "iPhone 15"];
        for i in 0..1000 {
            p.intern(cams[i % 5]);
        }
        assert_eq!(p.len(), 5);
    }
}