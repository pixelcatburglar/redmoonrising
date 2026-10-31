//! The star gate network, read once: how many jumps lie between systems.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::OnceLock;

use tokio_postgres::GenericClient;
use tracing::error;

use crate::svc::rows::int;

static GATES: OnceLock<HashMap<i64, Vec<i64>>> = OnceLock::new();

/// Each system's neighbours across a gate.
pub type Graph = HashMap<i64, Vec<i64>>;

pub async fn graph(db: &impl GenericClient) -> Option<&'static Graph> {
    gates(db).await
}

async fn gates(db: &impl GenericClient) -> Option<&'static HashMap<i64, Vec<i64>>> {
    if let Some(g) = GATES.get() {
        return Some(g);
    }
    let sql = r#"SELECT "fromSolarSystemID", "toSolarSystemID" FROM "mapSolarSystemJumps""#;
    let rows = match db.query(sql, &[]).await {
        Ok(rows) => rows,
        Err(e) => {
            error!("reading the star gates: {}", crate::logging::chain(&e));
            return None;
        }
    };
    let mut map: HashMap<i64, Vec<i64>> = HashMap::new();
    for r in rows {
        map.entry(int(&r, 0).unwrap_or(0)).or_default().push(int(&r, 1).unwrap_or(0));
    }
    Some(GATES.get_or_init(|| map))
}

/// The jumps from one system to another, if no more than `limit`.
pub async fn jumps(db: &impl GenericClient, from: i64, to: i64, limit: i64) -> Option<i64> {
    hops(gates(db).await?, from, to, limit)
}

/// Every system within `limit` jumps of `from` (itself included), with its
/// distance, nearest first.
pub async fn within(db: &impl GenericClient, from: i64, limit: i64) -> Vec<(i64, i64)> {
    gates(db).await.map(|g| reach(g, from, limit)).unwrap_or_default()
}

/// The systems to jump through from `from` to `to`, `to` last and `from`
/// left out; None if no gates lead there.
pub fn route(gates: &Graph, from: i64, to: i64) -> Option<Vec<i64>> {
    if from == to {
        return Some(Vec::new());
    }
    let mut came_from: HashMap<i64, i64> = HashMap::from([(from, from)]);
    let mut queue = VecDeque::from([from]);
    while let Some(at) = queue.pop_front() {
        for next in gates.get(&at).into_iter().flatten() {
            if came_from.contains_key(next) {
                continue;
            }
            came_from.insert(*next, at);
            if *next == to {
                let mut path = vec![to];
                let mut step = at;
                while step != from {
                    path.push(step);
                    step = came_from[&step];
                }
                path.reverse();
                return Some(path);
            }
            queue.push_back(*next);
        }
    }
    None
}

/// Every system within `limit` jumps, on a graph already loaded.
pub fn reach_on(gates: &Graph, from: i64, limit: i64) -> Vec<(i64, i64)> {
    reach(gates, from, limit)
}

/// Breadth-first, out to `limit` jumps.
fn hops(gates: &HashMap<i64, Vec<i64>>, from: i64, to: i64, limit: i64) -> Option<i64> {
    reach(gates, from, limit).into_iter().find(|(s, _)| *s == to).map(|(_, n)| n)
}

fn reach(gates: &HashMap<i64, Vec<i64>>, from: i64, limit: i64) -> Vec<(i64, i64)> {
    let mut seen = HashSet::from([from]);
    let mut queue = VecDeque::from([(from, 0)]);
    let mut out = Vec::new();
    while let Some((at, n)) = queue.pop_front() {
        out.push((at, n));
        if n == limit {
            continue;
        }
        for next in gates.get(&at).into_iter().flatten() {
            if seen.insert(*next) {
                queue.push_back((*next, n + 1));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jumps_are_counted_out_to_the_limit() {
        let gates = HashMap::from([(1, vec![2]), (2, vec![1, 3]), (3, vec![2, 4]), (4, vec![3])]);
        assert_eq!(hops(&gates, 1, 1, 0), Some(0));
        assert_eq!(hops(&gates, 1, 3, 5), Some(2));
        assert_eq!(hops(&gates, 1, 4, 2), None);
        assert_eq!(reach(&gates, 2, 1), vec![(2, 0), (1, 1), (3, 1)]);
        assert_eq!(route(&gates, 1, 4), Some(vec![2, 3, 4]));
        assert_eq!(route(&gates, 3, 3), Some(vec![]));
        assert_eq!(route(&gates, 1, 9), None);
    }
}
