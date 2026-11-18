//! Isolated PB04 experiment, NOT a Storage implementation or production format.
//! AVL object maps carry exact canonical JSON length; arrays are opaque leaves.
use aq_playbooks_model::{canonical_bytes, hash_bytes};
use postgres::Transaction;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Link {
    pub hash: String,
    pub bytes: u64,
    pub count: u64,
    pub height: u8,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
enum Node {
    Leaf(Value),
    Object(Option<Link>),
    Entry {
        key: String,
        value: Link,
        left: Option<Link>,
        right: Option<Link>,
    },
}
#[derive(Default, Debug, Serialize)]
pub struct Work {
    pub fetched_nodes: usize,
    pub read_sql_calls: usize,
    pub fetched_bytes: usize,
    pub written_nodes: usize,
    pub written_bytes: usize,
}
pub struct Tree<'a, 'b> {
    pub tx: &'a mut Transaction<'b>,
    cache: BTreeMap<String, Node>,
    pending: BTreeMap<String, Vec<u8>>,
    pub work: Work,
    pub batched: bool,
}
fn n(x: &Option<Link>) -> u64 {
    x.as_ref().map_or(0, |r| r.count)
}
fn h(x: &Option<Link>) -> u8 {
    x.as_ref().map_or(0, |r| r.height)
}
fn b(x: &Option<Link>) -> u64 {
    x.as_ref().map_or(0, |r| r.bytes)
}
impl Node {
    fn link(&self) -> Result<Link> {
        let (bytes, count, height) = match self {
            Self::Leaf(v) => (
                canonical_bytes(v)?.len() as u64,
                v.as_array().map_or(0, |a| a.len() as u64),
                0,
            ),
            Self::Object(map) => (2 + b(map) + n(map).saturating_sub(1), n(map), 0),
            Self::Entry {
                key,
                value,
                left,
                right,
            } => {
                if key.len() > 1024 || h(left).max(h(right)) >= 63 {
                    return Err("prototype tree bound".into());
                }
                (
                    b(left) + b(right) + canonical_bytes(key)?.len() as u64 + 1 + value.bytes,
                    1 + n(left) + n(right),
                    1 + h(left).max(h(right)),
                )
            }
        };
        Ok(Link {
            hash: hash_bytes(&canonical_bytes(self)?),
            bytes,
            count,
            height,
        })
    }
}
impl<'a, 'b> Tree<'a, 'b> {
    pub fn new(tx: &'a mut Transaction<'b>) -> Self {
        Self {
            tx,
            cache: BTreeMap::new(),
            pending: BTreeMap::new(),
            work: Work::default(),
            batched: std::env::var_os("AQ_PB04_BATCHED").is_some(),
        }
    }
    fn put(&mut self, node: Node) -> Result<Link> {
        let link = node.link()?;
        let bytes = canonical_bytes(&node)?;
        if bytes.len() > 16 * 1024 * 1024
            || self.pending.len() >= 262144
            || self.cache.len() >= 262144
        {
            return Err("prototype transaction node bound".into());
        }
        self.pending.insert(link.hash.clone(), bytes);
        self.cache.insert(link.hash.clone(), node);
        Ok(link)
    }
    fn read(&mut self, link: &Link) -> Result<Node> {
        if let Some(node) = self.cache.get(&link.hash) {
            if node.link()? != *link {
                return Err("bad cached reference".into());
            }
            return Ok(node.clone());
        }
        self.work.read_sql_calls += 1;
        let bytes: Vec<u8> = self
            .tx
            .query_one(
                "SELECT payload FROM pb04_nodes WHERE hash=$1",
                &[&link.hash],
            )?
            .get(0);
        self.work.fetched_nodes += 1;
        self.work.fetched_bytes += bytes.len();
        if self.work.fetched_bytes > 128 * 1024 * 1024 || self.cache.len() >= 262144 {
            return Err("prototype read bound".into());
        }
        let node: Node = serde_json::from_slice(&bytes)?;
        if hash_bytes(&bytes) != link.hash
            || node.link()? != *link
            || canonical_bytes(&node)? != bytes
        {
            return Err("node integrity".into());
        }
        self.cache.insert(link.hash.clone(), node.clone());
        Ok(node)
    }
    fn fetch_many(&mut self, links: &[Link]) -> Result<()> {
        let expected: BTreeMap<_, _> = links
            .iter()
            .filter(|r| !self.cache.contains_key(&r.hash))
            .map(|r| (r.hash.clone(), r.clone()))
            .collect();
        if expected.is_empty() {
            return Ok(());
        }
        if expected.len() > 4096 {
            return Err("prototype frontier >4096 nodes".into());
        }
        if std::env::var_os("AQ_PB04_LOOKAHEAD").is_some() {
            return self.fetch_lookahead(&expected);
        }
        let keys: Vec<_> = expected.keys().cloned().collect();
        self.work.read_sql_calls += 1;
        let rows = self.tx.query(
            "SELECT hash,payload FROM pb04_nodes WHERE hash=ANY($1)",
            &[&keys],
        )?;
        if rows.len() != keys.len() {
            return Err("missing proof node".into());
        }
        for row in rows {
            let hash: String = row.get(0);
            let bytes: Vec<u8> = row.get(1);
            self.work.fetched_nodes += 1;
            self.work.fetched_bytes += bytes.len();
            if self.work.fetched_bytes > 128 * 1024 * 1024 || self.cache.len() >= 262144 {
                return Err("prototype read bound".into());
            }
            let node: Node = serde_json::from_slice(&bytes)?;
            if hash_bytes(&bytes) != hash
                || canonical_bytes(&node)? != bytes
                || node.link()? != expected[&hash]
            {
                return Err("proof node integrity".into());
            }
            self.cache.insert(hash, node);
        }
        Ok(())
    }
    // SQL only supplies bounded candidate bytes. Local hash/canonical checks and
    // the ordinary expected-edge walk still establish membership and absence.
    // This isolated experiment does not change the authoritative tree format.
    fn fetch_lookahead(&mut self, expected: &BTreeMap<String, Link>) -> Result<()> {
        let keys: Vec<_> = expected.keys().cloned().collect();
        // Unrequested values may be opaque arrays containing all retained
        // history. Default to map structure only; retain the broad experiment
        // behind a second switch so its measured byte slope stays reproducible.
        let values = std::env::var_os("AQ_PB04_LOOKAHEAD_VALUES").is_some();
        for roots in keys.chunks(128) {
            self.work.read_sql_calls += 1;
            let rows = self.tx.query(
                "WITH RECURSIVE candidates(hash,payload,depth) AS (
                   SELECT hash,payload,0 FROM pb04_nodes WHERE hash=ANY($1)
                   UNION
                   SELECT n.hash,n.payload,c.depth+1 FROM candidates c
                   CROSS JOIN LATERAL (SELECT pb04_probe_json(c.payload) AS body) d
                   CROSS JOIN LATERAL (VALUES
                     (d.body#>>'{Object,hash}'),
                     (CASE WHEN $2 THEN d.body#>>'{Entry,value,hash}' ELSE NULL END),
                     (d.body#>>'{Entry,left,hash}'),
                     (d.body#>>'{Entry,right,hash}')
                   ) child(hash)
                   JOIN pb04_nodes n ON n.hash=child.hash
                   WHERE c.depth<2
                 ) SELECT DISTINCT ON(hash) hash,payload FROM candidates ORDER BY hash LIMIT 4097",
                &[&roots, &values],
            )?;
            if rows.len() > 4096 {
                return Err("prototype lookahead node bound".into());
            }
            for row in rows {
                let hash: String = row.get(0);
                let bytes: Vec<u8> = row.get(1);
                self.work.fetched_nodes += 1;
                self.work.fetched_bytes += bytes.len();
                if bytes.len() > 16 * 1024 * 1024
                    || self.work.fetched_bytes > 128 * 1024 * 1024
                    || self.cache.len() >= 262144
                {
                    return Err("prototype lookahead byte bound".into());
                }
                let node: Node = match serde_json::from_slice(&bytes) {
                    Ok(node) => node,
                    Err(_) if !expected.contains_key(&hash) => continue,
                    Err(e) => return Err(e.into()),
                };
                let actual = match node.link() {
                    Ok(link) => link,
                    Err(_) if !expected.contains_key(&hash) => continue,
                    Err(e) => return Err(e),
                };
                if hash_bytes(&bytes) != hash
                    || actual.hash != hash
                    || canonical_bytes(&node)? != bytes
                    || expected.get(&hash).is_some_and(|link| link != &actual)
                {
                    if expected.contains_key(&hash) {
                        return Err("lookahead node integrity".into());
                    }
                    // Speculative neighbors do not widen the original read set's
                    // corruption semantics. A later required-edge read fetches
                    // these bytes normally and must fail its integrity check.
                    continue;
                }
                self.cache.insert(hash, node);
            }
        }
        if expected.keys().any(|hash| !self.cache.contains_key(hash)) {
            return Err("missing lookahead proof root".into());
        }
        Ok(())
    }
    // Returns the first uncached node required to establish one exact path.
    // The SQL result never decides absence: local verified edges do.
    fn missing_path(&self, root: &Link, path: &[&str]) -> Result<Option<Link>> {
        let mut current = root.clone();
        for key in path {
            let Some(node) = self.cache.get(&current.hash) else {
                return Ok(Some(current));
            };
            if node.link()? != current {
                return Err("cached root integrity".into());
            }
            let Node::Object(mut map) = node.clone() else {
                return Err("not object".into());
            };
            let mut found = None;
            for depth in 0..64 {
                let Some(link) = map else { break };
                let Some(node) = self.cache.get(&link.hash) else {
                    return Ok(Some(link));
                };
                if node.link()?.ne(&link) {
                    return Err("cached map integrity".into());
                }
                let Node::Entry {
                    key: k,
                    value,
                    left,
                    right,
                } = node
                else {
                    return Err("not entry".into());
                };
                match key.cmp(&k.as_str()) {
                    std::cmp::Ordering::Equal => {
                        found = Some(value.clone());
                        break;
                    }
                    std::cmp::Ordering::Less => map = left.clone(),
                    std::cmp::Ordering::Greater => map = right.clone(),
                }
                if depth == 63 {
                    return Err("prototype lookup depth".into());
                }
            }
            let Some(next) = found else { return Ok(None) };
            current = next;
        }
        // Target metadata is already authenticated by its parent. Do not fetch
        // opaque history leaves merely to read count/byte quota metadata.
        Ok(None)
    }
    pub fn prefetch_paths(&mut self, root: &Link, paths: &[Vec<&str>]) -> Result<()> {
        if paths.len() > 128 || paths.iter().any(|p| p.len() > 8) {
            return Err("prototype path set bound".into());
        }
        for _ in 0..512 {
            let missing: Vec<_> = paths
                .iter()
                .map(|p| self.missing_path(root, p))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect();
            if missing.is_empty() {
                return Ok(());
            }
            self.fetch_many(&missing)?;
        }
        Err("prototype path proof depth".into())
    }
    /// Explicit bounded subtree audit. Used for whole installed catalog reads,
    /// and OUTSIDE measurement for cold full-State differential audits.
    pub fn prefetch_subtree(&mut self, root: &Link) -> Result<()> {
        let mut frontier = vec![root.clone()];
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..256 {
            if frontier.is_empty() {
                return Ok(());
            }
            for chunk in frontier.chunks(4096) {
                self.fetch_many(chunk)?;
            }
            let mut next = vec![];
            for link in frontier {
                if !seen.insert(link.hash.clone()) {
                    continue;
                }
                match self.read(&link)? {
                    Node::Object(map) => next.extend(map),
                    Node::Entry {
                        value, left, right, ..
                    } => {
                        next.push(value);
                        next.extend(left);
                        next.extend(right)
                    }
                    Node::Leaf(_) => {}
                }
            }
            if seen.len() > 262144 {
                return Err("prototype subtree node bound".into());
            }
            frontier = next;
        }
        Err("prototype subtree depth bound".into())
    }
    pub fn build(&mut self, value: &Value) -> Result<Link> {
        if let Value::Object(map) = value {
            let entries: Vec<_> = map.iter().collect();
            let root = self.build_map(&entries)?;
            self.put(Node::Object(root))
        } else {
            self.put(Node::Leaf(value.clone()))
        }
    }
    fn build_map(&mut self, entries: &[(&String, &Value)]) -> Result<Option<Link>> {
        if entries.is_empty() {
            return Ok(None);
        }
        let mid = entries.len() / 2;
        let left = self.build_map(&entries[..mid])?;
        let right = self.build_map(&entries[mid + 1..])?;
        let value = self.build(entries[mid].1)?;
        Ok(Some(self.entry(
            entries[mid].0.clone(),
            value,
            left,
            right,
        )?))
    }
    fn entry(
        &mut self,
        key: String,
        value: Link,
        left: Option<Link>,
        right: Option<Link>,
    ) -> Result<Link> {
        self.put(Node::Entry {
            key,
            value,
            left,
            right,
        })
    }
    fn insert(&mut self, root: Option<Link>, key: &str, value: Link) -> Result<Link> {
        let Some(root) = root else {
            return self.entry(key.into(), value, None, None);
        };
        let Node::Entry {
            key: k,
            value: v,
            mut left,
            mut right,
        } = self.read(&root)?
        else {
            return Err("not entry".into());
        };
        match key.cmp(&k) {
            std::cmp::Ordering::Equal => return self.entry(k, value, left, right),
            std::cmp::Ordering::Less => left = Some(self.insert(left, key, value)?),
            std::cmp::Ordering::Greater => right = Some(self.insert(right, key, value)?),
        }
        // Standard AVL rotations; unchanged descendant refs remain authenticated.
        if h(&left) > h(&right) + 1 {
            let Node::Entry {
                key: lk,
                value: lv,
                left: ll,
                right: lr,
            } = self.read(left.as_ref().unwrap())?
            else {
                return Err("not entry".into());
            };
            if h(&ll) >= h(&lr) {
                let r = self.entry(k, v, lr, right)?;
                return self.entry(lk, lv, ll, Some(r));
            }
            let Node::Entry {
                key: mk,
                value: mv,
                left: ml,
                right: mr,
            } = self.read(lr.as_ref().unwrap())?
            else {
                return Err("not entry".into());
            };
            let l = self.entry(lk, lv, ll, ml)?;
            let r = self.entry(k, v, mr, right)?;
            return self.entry(mk, mv, Some(l), Some(r));
        }
        if h(&right) > h(&left) + 1 {
            let Node::Entry {
                key: rk,
                value: rv,
                left: rl,
                right: rr,
            } = self.read(right.as_ref().unwrap())?
            else {
                return Err("not entry".into());
            };
            if h(&rr) >= h(&rl) {
                let l = self.entry(k, v, left, rl)?;
                return self.entry(rk, rv, Some(l), rr);
            }
            let Node::Entry {
                key: mk,
                value: mv,
                left: ml,
                right: mr,
            } = self.read(rl.as_ref().unwrap())?
            else {
                return Err("not entry".into());
            };
            let l = self.entry(k, v, left, ml)?;
            let r = self.entry(rk, rv, mr, rr)?;
            return self.entry(mk, mv, Some(l), Some(r));
        }
        self.entry(k, v, left, right)
    }
    pub fn child(&mut self, root: &Link, key: &str) -> Result<Option<Link>> {
        let Node::Object(mut map) = self.read(root)? else {
            return Err("not object".into());
        };
        for _ in 0..64 {
            let Some(link) = map else { return Ok(None) };
            let Node::Entry {
                key: k,
                value,
                left,
                right,
            } = self.read(&link)?
            else {
                return Err("not entry".into());
            };
            match key.cmp(&k) {
                std::cmp::Ordering::Equal => return Ok(Some(value)),
                std::cmp::Ordering::Less => map = left,
                std::cmp::Ordering::Greater => map = right,
            }
        }
        Err("prototype lookup depth".into())
    }
    pub fn path(&mut self, root: &Link, path: &[&str]) -> Result<Option<Link>> {
        let mut current = root.clone();
        for key in path {
            let Some(next) = self.child(&current, key)? else {
                return Ok(None);
            };
            current = next;
        }
        Ok(Some(current))
    }
    pub fn value(&mut self, root: &Link) -> Result<Value> {
        match self.read(root)? {
            Node::Leaf(v) => Ok(v),
            Node::Object(map) => {
                let mut out = serde_json::Map::new();
                self.walk(map, &mut out)?;
                Ok(Value::Object(out))
            }
            _ => Err("not JSON value".into()),
        }
    }
    fn walk(&mut self, map: Option<Link>, out: &mut serde_json::Map<String, Value>) -> Result<()> {
        if let Some(link) = map {
            let Node::Entry {
                key,
                value,
                left,
                right,
            } = self.read(&link)?
            else {
                return Err("not entry".into());
            };
            self.walk(left, out)?;
            out.insert(key, self.value(&value)?);
            self.walk(right, out)?;
        }
        Ok(())
    }
    pub fn at(&mut self, root: &Link, path: &[&str]) -> Result<Option<Value>> {
        match self.path(root, path)? {
            Some(r) => Ok(Some(self.value(&r)?)),
            None => Ok(None),
        }
    }
    pub fn set(&mut self, root: &Link, path: &[&str], value: &Value) -> Result<Link> {
        if path.is_empty() {
            return self.build(value);
        }
        let Node::Object(map) = self.read(root)? else {
            return Err("patch traversed nonobject".into());
        };
        let child = match self.child(root, path[0])? {
            Some(r) => r,
            None => self.build(&serde_json::json!({}))?,
        };
        let value = self.set(&child, &path[1..], value)?;
        let map = self.insert(map, path[0], value)?;
        self.put(Node::Object(Some(map)))
    }
    pub fn flush(&mut self) -> Result<()> {
        let hashes: Vec<String> = self.pending.keys().cloned().collect();
        let payloads: Vec<Vec<u8>> = self.pending.values().cloned().collect();
        self.tx.execute("INSERT INTO pb04_nodes(hash,payload) SELECT * FROM unnest($1::text[],$2::bytea[]) ON CONFLICT DO NOTHING", &[&hashes,&payloads])?;
        // Re-read even existing immutable addresses; an address match must never hide corruption.
        let rows = self.tx.query(
            "SELECT hash,payload FROM pb04_nodes WHERE hash=ANY($1)",
            &[&hashes],
        )?;
        if rows.len() != hashes.len() {
            return Err("missing published node".into());
        }
        for row in rows {
            let hash: String = row.get(0);
            let bytes: Vec<u8> = row.get(1);
            if self.pending.get(&hash) != Some(&bytes) {
                return Err("existing node bytes changed".into());
            }
        }
        self.work.written_nodes = self.pending.len();
        self.work.written_bytes = self.pending.values().map(Vec::len).sum();
        Ok(())
    }
}
