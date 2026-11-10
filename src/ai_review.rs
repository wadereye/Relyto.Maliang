//! 人脸约束(M5):must/never 关系落库,聚类前先做并查集合并。

use crate::db::Store;
use anyhow::{bail, Result};
use rusqlite::{params, Connection};
use std::collections::HashMap;

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
        if kind == Kind::Never {
            bail!("同一张人脸不能标为不同人物");
        }
        return Ok(());
    }
    store.write(|conn| -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        let present: i64 = tx.query_row(
            "SELECT COUNT(*) FROM faces WHERE id IN (?1,?2)",
            params![x, y],
            |r| r.get(0),
        )?;
        if present != 2 {
            bail!("人脸记录不存在，可能已被重新扫描删除");
        }
        let mut list = read_constraints(&tx)?;
        list.retain(|(p, q, _)| !((*p == x && *q == y) || (*p == y && *q == x)));
        list.push((x, y, kind));
        check_consistent(&list)?;
        tx.execute(
            "INSERT INTO face_constraints(a_face,b_face,kind,ts) VALUES(?1,?2,?3,?4)
             ON CONFLICT(a_face,b_face) DO UPDATE SET kind=excluded.kind,ts=excluded.ts",
            params![x, y, kind.key(), crate::ai_queue::now_ms()],
        )?;
        tx.commit()?;
        Ok(())
    })
}

/// 删除一条约束。
pub fn remove(store: &Store, a: i64, b: i64) -> Result<()> {
    let (x, y) = if a <= b { (a, b) } else { (b, a) };
    store.write(|conn| -> Result<()> {
        conn.execute(
            "DELETE FROM face_constraints WHERE (a_face=?1 AND b_face=?2) OR (a_face=?2 AND b_face=?1)",
            params![x, y],
        )?;
        Ok(())
    })
}

/// 读出全部约束。
pub fn all(store: &Store) -> Vec<(i64, i64, Kind)> {
    store.read(|conn| read_constraints(conn).unwrap_or_default())
}

fn read_constraints(conn: &Connection) -> Result<Vec<(i64, i64, Kind)>> {
    let mut st = conn.prepare(
        "SELECT a_face,b_face,kind FROM face_constraints ORDER BY a_face,b_face",
    )?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (a, b, key) = row?;
        if let Some(kind) = Kind::from_key(&key) {
            out.push((a, b, kind));
        }
    }
    Ok(out)
}

fn constraint_ids(list: &[(i64, i64, Kind)]) -> Vec<i64> {
    let mut ids: Vec<i64> = list.iter().flat_map(|(a, b, _)| [*a, *b]).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn check_consistent(list: &[(i64, i64, Kind)]) -> Result<()> {
    let ids = constraint_ids(list);
    let slot = |id: i64| ids.binary_search(&id).unwrap();
    let mut parent: Vec<usize> = (0..ids.len()).collect();
    for &(a, b, kind) in list {
        if kind == Kind::Must {
            let ra = find(&mut parent, slot(a));
            let rb = find(&mut parent, slot(b));
            if ra != rb {
                parent[rb] = ra;
            }
        }
    }
    for &(a, b, kind) in list {
        if kind == Kind::Never && find(&mut parent, slot(a)) == find(&mut parent, slot(b)) {
            bail!("人物约束冲突：同一人物连通关系中已有不同人物标记，请先移除冲突约束");
        }
    }
    Ok(())
}

fn find(parent: &mut Vec<usize>, x: usize) -> usize {
    let mut root = x;
    while parent[root] != root {
        root = parent[root];
    }
    let mut cur = x;
    while parent[cur] != cur {
        let next = parent[cur];
        parent[cur] = root;
        cur = next;
    }
    root
}

/// 合并前检查两棵树之间是否存在 never 边。
fn links_never(
    parent: &mut Vec<usize>,
    never: &[Vec<usize>],
    ra: usize,
    rb: usize,
) -> bool {
    for (x, edges) in never.iter().enumerate() {
        if find(parent, x) != ra {
            continue;
        }
        if edges.iter().any(|&y| find(parent, y) == rb) {
            return true;
        }
    }
    false
}

/// 按 must 合并、按 never 拆分;旧簇只作软约束。
pub fn apply(constraints: &[(i64, i64, Kind)], clusters: &[Vec<i64>]) -> Vec<Vec<i64>> {
    let mut ids: Vec<i64> = clusters.iter().flatten().copied().collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Vec::new();
    }
    let index: HashMap<i64, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    let mut never: Vec<Vec<usize>> = vec![Vec::new(); ids.len()];
    let mut must: Vec<(usize, usize)> = Vec::new();
    for &(a, b, kind) in constraints {
        let (Some(&x), Some(&y)) = (index.get(&a), index.get(&b)) else {
            continue;
        };
        if x == y {
            continue;
        }
        match kind {
            Kind::Never => {
                never[x].push(y);
                never[y].push(x);
            }
            Kind::Must => must.push((x, y)),
        }
    }
    let mut parent: Vec<usize> = (0..ids.len()).collect();
    // never 优先:与 never 直接冲突或跨越 never 的 must 让步。
    must.sort_unstable();
    for (x, y) in must {
        if never[x].contains(&y) {
            continue;
        }
        let rx = find(&mut parent, x);
        let ry = find(&mut parent, y);
        if rx == ry || links_never(&mut parent, &never, rx, ry) {
            continue;
        }
        parent[ry] = rx;
    }
    // 旧簇是软约束:同簇尽量合并,但越过 never 就拆开。
    for cluster in clusters {
        let mut roots: Vec<usize> = cluster
            .iter()
            .filter_map(|id| index.get(id).copied())
            .map(|i| find(&mut parent, i))
            .collect();
        roots.sort_unstable();
        roots.dedup();
        let Some(&first) = roots.first() else {
            continue;
        };
        for &root in roots.iter().skip(1) {
            let ra = find(&mut parent, first);
            let rb = find(&mut parent, root);
            if ra != rb && !links_never(&mut parent, &never, ra, rb) {
                parent[rb] = ra;
            }
        }
    }
    let mut out: Vec<Vec<i64>> = Vec::new();
    let mut slots: HashMap<usize, usize> = HashMap::new();
    for (i, &id) in ids.iter().enumerate() {
        let root = find(&mut parent, i);
        let slot = *slots.entry(root).or_insert_with(|| {
            out.push(Vec::new());
            out.len() - 1
        });
        out[slot].push(id);
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

    fn store_with_faces(ids: &[i64]) -> Store {
        let store = Store::open_mem().unwrap();
        store
            .write(|conn| -> Result<()> {
                conn.execute(
                    "INSERT INTO photos(id,path,parent,mtime,size) VALUES(1,'a','a',0,0)",
                    [],
                )?;
                for id in ids {
                    conn.execute("INSERT INTO faces(id,photo_id) VALUES(?1,1)", params![id])?;
                }
                Ok(())
            })
            .unwrap();
        store
    }

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
    fn never_splits_existing_cluster() {
        let clusters = vec![vec![1, 2, 3]];
        let out = apply(&[(1, 2, Kind::Never)], &clusters);
        assert_eq!(out.len(), 2, "never 两端必须落到不同簇");
        let a = out.iter().find(|c| c.contains(&1)).unwrap();
        let b = out.iter().find(|c| c.contains(&2)).unwrap();
        assert!(!a.contains(&2) && !b.contains(&1));
        assert!(a.contains(&3) || b.contains(&3));
    }

    #[test]
    fn never_beats_must_in_apply() {
        let clusters = vec![vec![1], vec![2], vec![3]];
        let cons = [
            (1, 2, Kind::Must),
            (2, 3, Kind::Must),
            (1, 3, Kind::Never),
        ];
        let out = apply(&cons, &clusters);
        let a = out.iter().position(|c| c.contains(&1)).unwrap();
        let b = out.iter().position(|c| c.contains(&2)).unwrap();
        let c = out.iter().position(|c| c.contains(&3)).unwrap();
        assert_eq!(a, b, "不冲突的 must 仍应合并");
        assert_ne!(a, c, "never 必须阻止传递合并");
    }

    #[test]
    fn apply_is_deterministic() {
        let clusters = vec![vec![5, 1, 3], vec![2, 4]];
        let cons = [(1, 4, Kind::Must)];
        assert_eq!(apply(&cons, &clusters), apply(&cons, &clusters));
    }

    #[test]
    fn constraint_roundtrip() {
        let store = store_with_faces(&[2, 3]);
        add(&store, 3, 2, Kind::Must).unwrap();
        add(&store, 2, 3, Kind::Must).unwrap();
        assert_eq!(all(&store).len(), 1, "a/b 规范化后应去重");
        remove(&store, 2, 3).unwrap();
        assert!(all(&store).is_empty());
    }

    #[test]
    fn add_rejects_conflicting_never() {
        let store = store_with_faces(&[1, 2, 3]);
        add(&store, 1, 2, Kind::Must).unwrap();
        add(&store, 2, 3, Kind::Must).unwrap();
        assert!(add(&store, 1, 3, Kind::Never).is_err());
        assert_eq!(all(&store).len(), 2, "冲突约束不得落库");
    }

    #[test]
    fn add_rejects_missing_face_and_self_never() {
        let store = store_with_faces(&[1]);
        assert!(add(&store, 1, 9, Kind::Must).is_err());
        assert!(add(&store, 1, 1, Kind::Never).is_err());
        assert!(all(&store).is_empty());
    }

    #[test]
    fn conflicting_kind_can_be_corrected() {
        let store = store_with_faces(&[1, 2]);
        add(&store, 1, 2, Kind::Must).unwrap();
        add(&store, 1, 2, Kind::Never).unwrap();
        assert_eq!(all(&store), vec![(1, 2, Kind::Never)]);
    }

    #[test]
    fn conflicts_reports_same_cluster_never() {
        let store = store_with_faces(&[1, 2]);
        add(&store, 1, 2, Kind::Never).unwrap();
        let clusters = vec![vec![1, 2, 3]];
        assert_eq!(conflicts(&store, &clusters), vec![(1, 2)]);
    }
}
