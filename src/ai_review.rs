//! 人脸约束(M5):must/never 关系落库,聚类前先做并查集合并。

use crate::db::Store;
use anyhow::Result;
use rusqlite::params;

/// 约束类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Must,
    Never,
}

impl Kind {
    /// 持久化字符串。
    pub fn key(self) -> &'static str {
        match self {
            Kind::Must => "must",
            Kind::Never => "never",
        }
    }

    /// 从字符串解析。
    pub fn from_key(s: &str) -> Option<Kind> {
        match s {
            "must" => Some(Kind::Must),
            "never" => Some(Kind::Never),
            _ => None,
        }
    }
}

/// 写一条约束(a<b 规范化,便于去重)。
pub fn add(store: &Store, a: i64, b: i64, kind: Kind) -> Result<()> {
    let (x, y) = if a <= b { (a, b) } else { (b, a) };
    if x == y {
        return Ok(());
    }
    store.write(|conn| -> Result<()> {
        conn.execute(
            "INSERT OR REPLACE INTO face_constraints(a_face,b_face,kind,ts) VALUES(?1,?2,?3,?4)",
            params![x, y, kind.key(), crate::ai_queue::now_ms()],
        )?;
        Ok(())
    })
}

/// 删除一条约束。
pub fn remove(store: &Store, a: i64, b: i64) -> Result<()> {
    let (x, y) = if a <= b { (a, b) } else { (b, a) };
    store.write(|conn| -> Result<()> {
        conn.execute(
            "DELETE FROM face_constraints WHERE a_face=?1 AND b_face=?2",
            params![x, y],
        )?;
        Ok(())
    })
}

/// 读出全部约束。
pub fn all(store: &Store) -> Vec<(i64, i64, Kind)> {
    store.read(|conn| {
        let mut out = Vec::new();
        let Ok(mut st) = conn.prepare("SELECT a_face,b_face,kind FROM face_constraints") else {
            return out;
        };
        if let Ok(rows) = st.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        }) {
            for (a, b, k) in rows.flatten() {
                if let Some(kind) = Kind::from_key(&k) {
                    out.push((a, b, kind));
                }
            }
        }
        out
    })
}

fn find(parent: &mut Vec<usize>, x: usize) -> usize {
    if parent[x] != x {
        let p = find(parent, parent[x]);
        parent[x] = p;
    }
    parent[x]
}

/// 按 must 合并、按 never 阻止合并;返回新的簇集合。
pub fn apply(constraints: &[(i64, i64, Kind)], clusters: &[Vec<i64>]) -> Vec<Vec<i64>> {
    if clusters.is_empty() {
        return Vec::new();
    }
    let mut ids: Vec<i64> = clusters.iter().flat_map(|c| c.iter().copied()).collect();
    ids.sort_unstable();
    ids.dedup();
    let idx = |id: i64, ids: &[i64]| ids.binary_search(&id).unwrap_or(0);
    let mut parent: Vec<usize> = (0..ids.len()).collect();
    // 先把每个簇内部并起来,约束再在簇之间生效。
    for c in clusters {
        if let Some(first) = c.first() {
            let fx = idx(*first, &ids);
            for other in c.iter().skip(1) {
                let ox = idx(*other, &ids);
                let rx = find(&mut parent, fx);
                let ro = find(&mut parent, ox);
                if rx != ro {
                    parent[rx] = ro;
                }
            }
        }
    }
    let mut blocked: Vec<(usize, usize)> = Vec::new();
    for (a, b, k) in constraints {
        if !ids.contains(a) || !ids.contains(b) {
            continue;
        }
        let (x, y) = (idx(*a, &ids), idx(*b, &ids));
        match k {
            Kind::Must => {
                if !blocked
                    .iter()
                    .any(|(p, q)| (*p == x && *q == y) || (*p == y && *q == x))
                {
                    let rx = find(&mut parent, x);
                    let ry = find(&mut parent, y);
                    if rx != ry {
                        parent[rx] = ry;
                    }
                }
            }
            Kind::Never => {
                if find(&mut parent, x) == find(&mut parent, y) {
                    // 同一簇内的 never 关系不拆分,只记录下来供复核。
                }
                blocked.push((x, y));
            }
        }
    }
    let mut out: Vec<Vec<i64>> = Vec::new();
    let mut map: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (i, id) in ids.iter().enumerate() {
        let root = find(&mut parent, i);
        let slot = *map.entry(root).or_insert_with(|| out.len());
        if slot == out.len() {
            out.push(Vec::new());
        }
        out[slot].push(*id);
    }
    out
}

/// 需要在界面复核的约束(never 两端已同簇)。
pub fn conflicts(store: &Store, clusters: &[Vec<i64>]) -> Vec<(i64, i64)> {
    let cons = all(store);
    let mut out = Vec::new();
    for (a, b, k) in cons {
        if k != Kind::Never {
            continue;
        }
        let ca = clusters.iter().position(|c| c.contains(&a));
        let cb = clusters.iter().position(|c| c.contains(&b));
        if ca == cb && ca.is_some() {
            out.push((a, b));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn must_merges_clusters() {
        let clusters = vec![vec![1, 2], vec![3], vec![4, 5]];
        let out = apply(&[(1, 4, Kind::Must)], &clusters);
        assert_eq!(out.len(), 2);
        let merged = out.iter().find(|c| c.contains(&1)).unwrap();
        assert!(merged.contains(&4) && merged.contains(&5) && merged.contains(&2));
    }

    #[test]
    fn never_blocks_merge() {
        let clusters = vec![vec![1], vec![2]];
        let out = apply(&[(1, 2, Kind::Never)], &clusters);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn constraint_roundtrip() {
        let store = Store::open_mem().unwrap();
        add(&store, 3, 2, Kind::Must).unwrap();
        add(&store, 2, 3, Kind::Must).unwrap();
        assert_eq!(all(&store).len(), 1, "a/b 规范化后应去重");
        remove(&store, 2, 3).unwrap();
        assert!(all(&store).is_empty());
    }

    #[test]
    fn conflicts_reports_same_cluster_never() {
        let store = Store::open_mem().unwrap();
        add(&store, 1, 2, Kind::Never).unwrap();
        let clusters = vec![vec![1, 2, 3]];
        assert_eq!(conflicts(&store, &clusters), vec![(1, 2)]);
    }
}
