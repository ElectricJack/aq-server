//! Lazily bound State containers for the versioned transition capability (D10).
//!
//! Every retained-history container in [`crate::State`] uses one of these types.
//! A container is either *plain* (an ordinary in-memory map/set/vector; the
//! default, used by full `load`/`compare_and_swap` and `MemoryStorage`) or
//! *bound* to an authoritative [`RecordSource`] for one transition. Bound
//! containers read individual records and typed-order successors on demand,
//! track every touched record and emit an exact per-key patch. Any operation
//! that needs the complete container (iteration, ranges, retain, serialization)
//! records a fallback with its caller location, then materializes the whole
//! container from the source, so results always equal the reference State. A
//! source failure poisons the transition and must abort it (fail closed).
use aq_playbooks_model::canonical_bytes;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::DeserializeOwned};
use std::borrow::Borrow;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::{Deref, DerefMut, Index};
use std::panic::Location;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// One record write: its key and new canonical bytes (`None` deletes).
pub type RecordChange = (TreeKey, Option<Vec<u8>>);
/// A touched record: key, original canonical bytes and current value.
pub type Touched<K, V> = (K, Option<Vec<u8>>, Option<V>);
/// Typed physical key. A container's keys all share one variant, so the derived
/// order is the schema comparator: numeric for `U64`/`I64`, Rust string order
/// for `Str` (never lexical order of numeric spellings).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum TreeKey {
    U64(u64),
    I64(i64),
    Str(String),
}
impl TreeKey {
    /// Canonical JSON object-key spelling including quotes and escaping.
    pub fn json_key(&self) -> Vec<u8> {
        let text = match self {
            TreeKey::U64(v) => v.to_string(),
            TreeKey::I64(v) => v.to_string(),
            TreeKey::Str(s) => s.clone(),
        };
        serde_json::to_vec(&text).expect("string serialization is infallible")
    }
    pub fn kind(&self) -> KeyKind {
        match self {
            TreeKey::U64(_) => KeyKind::U64,
            TreeKey::I64(_) => KeyKind::I64,
            TreeKey::Str(_) => KeyKind::Str,
        }
    }
    /// Parse a canonical JSON object key for `kind`, refusing noncanonical
    /// numeric spellings (`09`, `+9`, `-0`) and out-of-range values.
    pub fn parse(kind: KeyKind, text: &str) -> Result<Self, String> {
        let refuse = || format!("noncanonical {kind:?} key {text:?}");
        match kind {
            KeyKind::Str => Ok(TreeKey::Str(text.to_string())),
            KeyKind::U64 => {
                let v: u64 = text.parse().map_err(|_| refuse())?;
                (v.to_string() == text)
                    .then_some(TreeKey::U64(v))
                    .ok_or_else(refuse)
            }
            KeyKind::I64 => {
                let v: i64 = text.parse().map_err(|_| refuse())?;
                (v.to_string() == text)
                    .then_some(TreeKey::I64(v))
                    .ok_or_else(refuse)
            }
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum KeyKind {
    U64,
    I64,
    Str,
}
/// Logical container shape in the schema certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Shape {
    /// JSON object keyed by the comparator.
    Map(KeyKind),
    /// JSON array of ordered unique members; physical key is the member.
    Set(KeyKind),
    /// JSON array in position order; physical key is the U64 position.
    Seq,
    /// JSON object whose values are themselves `Map(inner)` containers.
    Nested(KeyKind, KeyKind),
}
pub trait LazyKey:
    Ord + Clone + fmt::Debug + Serialize + DeserializeOwned + Send + Sync + 'static
{
    const KIND: KeyKind;
    fn to_tree(&self) -> TreeKey;
    fn from_tree(key: &TreeKey) -> Option<Self>;
}
impl LazyKey for String {
    const KIND: KeyKind = KeyKind::Str;
    fn to_tree(&self) -> TreeKey {
        TreeKey::Str(self.clone())
    }
    fn from_tree(key: &TreeKey) -> Option<Self> {
        match key {
            TreeKey::Str(s) => Some(s.clone()),
            _ => None,
        }
    }
}
impl LazyKey for u64 {
    const KIND: KeyKind = KeyKind::U64;
    fn to_tree(&self) -> TreeKey {
        TreeKey::U64(*self)
    }
    fn from_tree(key: &TreeKey) -> Option<Self> {
        match key {
            TreeKey::U64(v) => Some(*v),
            _ => None,
        }
    }
}
impl LazyKey for i64 {
    const KIND: KeyKind = KeyKind::I64;
    fn to_tree(&self) -> TreeKey {
        TreeKey::I64(*self)
    }
    fn from_tree(key: &TreeKey) -> Option<Self> {
        match key {
            TreeKey::I64(v) => Some(*v),
            _ => None,
        }
    }
}
/// Logical container address: a schema field, plus the outer key for the
/// inner maps of a [`Shape::Nested`] field.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContainerPath {
    pub field: &'static str,
    pub outer: Option<TreeKey>,
}
impl ContainerPath {
    pub fn field(field: &'static str) -> Self {
        Self { field, outer: None }
    }
}
/// Authoritative record access for one transition. Reads are complete for the
/// requested key/page or return an error; an error must abort the transition.
pub trait RecordSource: Send + Sync {
    /// Canonical value bytes of one record (for a nested outer key: any marker).
    fn get(&self, path: &ContainerPath, key: &TreeKey) -> Result<Option<Vec<u8>>, String>;
    /// Records strictly after `after` in typed order, at most `limit`.
    fn after(
        &self,
        path: &ContainerPath,
        after: Option<&TreeKey>,
        limit: usize,
    ) -> Result<Vec<(TreeKey, Vec<u8>)>, String>;
    fn len(&self, path: &ContainerPath) -> Result<usize, String>;
    /// Complete container (nested outer values are the inner maps' canonical JSON).
    fn scan(&self, path: &ContainerPath) -> Result<Vec<(TreeKey, Vec<u8>)>, String>;
    /// A full-container operation ran during the transition.
    fn fallback(&self, path: &ContainerPath, at: &'static Location<'static>);
    /// Poison the transition; the adapter must refuse to commit.
    fn fail(&self, error: String);
}
/// Changes to one container: exact per-key writes (`None` deletes), or a
/// complete replacement after a full-container fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerChange {
    Entries(Vec<(TreeKey, Option<Vec<u8>>)>),
    Replace(Vec<(TreeKey, Vec<u8>)>),
}
pub type Changes = Vec<(ContainerPath, ContainerChange)>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
fn encode_value<V: Serialize>(v: &V) -> Result<Vec<u8>, String> {
    canonical_bytes(v).map_err(|e| e.to_string())
}
fn decode_value<V: DeserializeOwned>(bytes: &[u8]) -> Result<V, String> {
    serde_json::from_slice(bytes).map_err(|e| format!("record decode: {e}"))
}

thread_local! {
    static HEADER_MODE: Cell<bool> = const { Cell::new(false) };
}
/// Canonical bytes of `value` with every lazy container serialized empty. The
/// complete canonical length is this length plus each container's exact length
/// minus its two empty delimiter bytes.
pub fn header_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            HEADER_MODE.with(|h| h.set(self.0));
        }
    }
    let _reset = Reset(HEADER_MODE.with(|h| h.replace(true)));
    encode_value(value)
}
fn header_mode() -> bool {
    HEADER_MODE.with(|h| h.get())
}

struct Slot<V> {
    original: Option<Vec<u8>>,
    current: Option<V>,
}
impl<V: Clone> Clone for Slot<V> {
    fn clone(&self) -> Self {
        Self {
            original: self.original.clone(),
            current: self.current.clone(),
        }
    }
}
type Encode<K, V> = fn(&K, &V) -> Result<Vec<u8>, String>;
type Decode<K, V> = fn(&K, &[u8]) -> Result<V, String>;

/// Shared bound-container core: an overlay of touched records over a source.
struct Bound<K, V> {
    source: Arc<dyn RecordSource>,
    path: ContainerPath,
    cache: Mutex<BTreeMap<K, Box<Slot<V>>>>,
    base_len: OnceLock<usize>,
    view: OnceLock<BTreeMap<K, V>>,
    encode: Encode<K, V>,
    decode: Decode<K, V>,
}
impl<K: LazyKey, V: Clone> Clone for Bound<K, V> {
    fn clone(&self) -> Self {
        let cache = lock(&self.cache).clone();
        Self {
            source: self.source.clone(),
            path: self.path.clone(),
            cache: Mutex::new(cache),
            base_len: self.base_len.clone(),
            view: OnceLock::new(),
            encode: self.encode,
            decode: self.decode,
        }
    }
}
impl<K: LazyKey, V: Clone> Bound<K, V> {
    fn new(
        source: Arc<dyn RecordSource>,
        path: ContainerPath,
        encode: Encode<K, V>,
        decode: Decode<K, V>,
    ) -> Self {
        Self {
            source,
            path,
            cache: Mutex::new(BTreeMap::new()),
            base_len: OnceLock::new(),
            view: OnceLock::new(),
            encode,
            decode,
        }
    }
    fn decoded(&self, key: &K, bytes: Option<Vec<u8>>) -> Slot<V> {
        let current = match bytes.as_deref().map(|b| (self.decode)(key, b)) {
            Some(Ok(v)) => Some(v),
            Some(Err(e)) => {
                self.source.fail(e);
                None
            }
            None => None,
        };
        Slot {
            original: bytes,
            current,
        }
    }
    fn fetch(&self, key: &K) -> Slot<V> {
        match self.source.get(&self.path, &key.to_tree()) {
            Ok(bytes) => self.decoded(key, bytes),
            Err(e) => {
                self.source.fail(e);
                Slot {
                    original: None,
                    current: None,
                }
            }
        }
    }
    fn slot(&self, key: &K) -> &Slot<V> {
        let mut cache = lock(&self.cache);
        if !cache.contains_key(key) {
            let slot = self.fetch(key);
            cache.insert(key.clone(), Box::new(slot));
        }
        let ptr: *const Slot<V> = &**cache.get(key).expect("inserted");
        drop(cache);
        // SAFETY: slots are boxed, never removed or replaced through `&self`, and
        // only mutated through `&mut self`, which this shared borrow excludes.
        unsafe { &*ptr }
    }
    fn slot_mut(&mut self, key: &K) -> &mut Slot<V> {
        self.view.take();
        if !self
            .cache
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(key)
        {
            let slot = self.fetch(key);
            self.cache
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.clone(), Box::new(slot));
        }
        self.cache
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(key)
            .expect("inserted")
    }
    /// Insert a record known to be absent from the source without reading it.
    fn insert_absent(&mut self, key: K, value: V) {
        self.view.take();
        self.cache
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                key,
                Box::new(Slot {
                    original: None,
                    current: Some(value),
                }),
            );
    }
    fn base_len(&self) -> usize {
        *self
            .base_len
            .get_or_init(|| match self.source.len(&self.path) {
                Ok(n) => n,
                Err(e) => {
                    self.source.fail(e);
                    0
                }
            })
    }
    fn len(&self) -> usize {
        let base = self.base_len() as isize;
        let delta: isize = lock(&self.cache)
            .values()
            .map(|s| s.current.is_some() as isize - s.original.is_some() as isize)
            .sum();
        (base + delta).max(0) as usize
    }
    /// First live key strictly after `after` in typed order.
    fn next_after(&self, after: Option<&K>) -> Option<K> {
        self.next_after_paged(after, 16)
    }
    /// [`Self::next_after`], reading the source `page` records at a time.
    fn next_after_paged(&self, after: Option<&K>, page: usize) -> Option<K> {
        let mut cache = lock(&self.cache);
        let overlay = match after {
            Some(a) => cache
                .range::<K, _>((std::ops::Bound::Excluded(a), std::ops::Bound::Unbounded))
                .find(|(_, s)| s.current.is_some())
                .map(|(k, _)| k.clone()),
            None => cache
                .iter()
                .find(|(_, s)| s.current.is_some())
                .map(|(k, _)| k.clone()),
        };
        let mut cursor = after.map(LazyKey::to_tree);
        loop {
            let page = match self.source.after(&self.path, cursor.as_ref(), page) {
                Ok(page) => page,
                Err(e) => {
                    self.source.fail(e);
                    return None;
                }
            };
            if page.is_empty() {
                return overlay;
            }
            for (tree_key, bytes) in &page {
                let Some(key) = K::from_tree(tree_key) else {
                    self.source
                        .fail(format!("{:?}: comparator mismatch", self.path));
                    return None;
                };
                if overlay.as_ref().is_some_and(|o| &key >= o) {
                    return overlay;
                }
                if cache.contains_key(&key) {
                    continue;
                }
                let slot = self.decoded(&key, Some(bytes.clone()));
                cache.insert(key.clone(), Box::new(slot));
                return Some(key);
            }
            cursor = page.last().map(|(k, _)| k.clone());
        }
    }
    #[track_caller]
    fn full(&self, at: &'static Location<'static>) -> BTreeMap<K, V> {
        self.source.fallback(&self.path, at);
        let mut map = BTreeMap::new();
        match self.source.scan(&self.path) {
            Ok(rows) => {
                for (tree_key, bytes) in rows {
                    let Some(key) = K::from_tree(&tree_key) else {
                        self.source
                            .fail(format!("{:?}: comparator mismatch", self.path));
                        continue;
                    };
                    match (self.decode)(&key, &bytes) {
                        Ok(v) => {
                            map.insert(key, v);
                        }
                        Err(e) => self.source.fail(e),
                    }
                }
            }
            Err(e) => self.source.fail(e),
        }
        for (key, slot) in lock(&self.cache).iter() {
            match &slot.current {
                Some(v) => {
                    map.insert(key.clone(), v.clone());
                }
                None => {
                    map.remove(key);
                }
            }
        }
        map
    }
    fn view(&self, at: &'static Location<'static>) -> &BTreeMap<K, V> {
        self.view.get_or_init(|| self.full(at))
    }
    fn changes(&mut self) -> Result<Vec<RecordChange>, String> {
        let mut out = Vec::new();
        for (key, slot) in self
            .cache
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            let current = slot
                .current
                .as_ref()
                .map(|v| (self.encode)(key, v))
                .transpose()?;
            if current != slot.original {
                out.push((key.to_tree(), current));
            }
        }
        Ok(out)
    }
    fn touched(&self) -> Vec<(K, Option<Vec<u8>>, Option<V>)> {
        lock(&self.cache)
            .iter()
            .map(|(k, s)| (k.clone(), s.original.clone(), s.current.clone()))
            .collect()
    }
}

/// Object container (`BTreeMap` compatible).
pub struct LazyMap<K, V>(MapRepr<K, V>);
enum MapRepr<K, V> {
    Plain(BTreeMap<K, V>),
    Bound(Box<Bound<K, V>>),
    /// Materialized after a full-container mutation; patched as a replacement.
    Promoted(BTreeMap<K, V>),
}
pub trait Record: Clone + Serialize + DeserializeOwned + Send + Sync + 'static {}
impl<T: Clone + Serialize + DeserializeOwned + Send + Sync + 'static> Record for T {}

fn map_encode<K, V: Serialize>(_: &K, v: &V) -> Result<Vec<u8>, String> {
    encode_value(v)
}
fn map_decode<K, V: DeserializeOwned>(_: &K, b: &[u8]) -> Result<V, String> {
    decode_value(b)
}
impl<K: LazyKey, V: Record> LazyMap<K, V> {
    pub fn new() -> Self {
        Self(MapRepr::Plain(BTreeMap::new()))
    }
    pub fn bound(source: Arc<dyn RecordSource>, path: ContainerPath) -> Self {
        Self(MapRepr::Bound(Box::new(Bound::new(
            source,
            path,
            map_encode::<K, V>,
            map_decode::<K, V>,
        ))))
    }
    pub fn is_bound(&self) -> bool {
        matches!(self.0, MapRepr::Bound(_))
    }
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        match &self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => m.get(key),
            MapRepr::Bound(b) => b.slot(&key.to_owned()).current.as_ref(),
        }
    }
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        match &mut self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => m.get_mut(key),
            MapRepr::Bound(b) => b.slot_mut(&key.to_owned()).current.as_mut(),
        }
    }
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        self.get(key).is_some()
    }
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        match &mut self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => m.insert(key, value),
            MapRepr::Bound(b) => b.slot_mut(&key).current.replace(value),
        }
    }
    /// Insert a record the caller has just proven absent (for example a fresh
    /// sequence-derived identifier); bound containers skip the source read.
    pub fn insert_new(&mut self, key: K, value: V) {
        match &mut self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => {
                m.insert(key, value);
            }
            MapRepr::Bound(b) => {
                if lock(&b.cache).contains_key(&key) {
                    b.slot_mut(&key).current = Some(value);
                } else {
                    b.insert_absent(key, value)
                }
            }
        }
    }
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        match &mut self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => m.remove(key),
            MapRepr::Bound(b) => b.slot_mut(&key.to_owned()).current.take(),
        }
    }
    pub fn len(&self) -> usize {
        match &self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => m.len(),
            MapRepr::Bound(b) => b.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn entry(&mut self, key: K) -> Entry<'_, K, V> {
        Entry { map: self, key }
    }
    /// First key strictly after `after` (or the first key) in typed order,
    /// without materializing the container.
    pub fn next_key<Q>(&self, after: Option<&Q>) -> Option<K>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        match &self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => match after {
                Some(a) => m
                    .range::<Q, _>((std::ops::Bound::Excluded(a), std::ops::Bound::Unbounded))
                    .next()
                    .map(|(k, _)| k.clone()),
                None => m.keys().next().cloned(),
            },
            MapRepr::Bound(b) => b.next_after(after.map(|a| a.to_owned()).as_ref()),
        }
    }
    /// [`Self::next_key`] reading one record at a time from a bound source,
    /// for a caller that needs only the next key (the head of an order), so
    /// its cost does not depend on how many records follow it.
    pub fn next_key_by_one<Q>(&self, after: Option<&Q>) -> Option<K>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        match &self.0 {
            MapRepr::Bound(b) => b.next_after_paged(after.map(|a| a.to_owned()).as_ref(), 1),
            _ => self.next_key(after),
        }
    }
    /// Up to `limit` keys strictly after `after`, in typed order.
    pub fn keys_after<Q>(&self, after: Option<&Q>, limit: usize) -> Vec<K>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        let mut out: Vec<K> = Vec::new();
        while out.len() < limit {
            let next = match out.last() {
                Some(last) => self.next_key::<K>(Some(last)),
                None => self.next_key(after),
            };
            match next {
                Some(k) => out.push(k),
                None => break,
            }
        }
        out
    }
    /// Touched records `(key, original canonical bytes, current value)`; `None`
    /// unless bound (callers then use the complete map).
    pub fn touched(&self) -> Option<Vec<Touched<K, V>>> {
        match &self.0 {
            MapRepr::Bound(b) => Some(b.touched()),
            _ => None,
        }
    }
    pub fn into_plain(self) -> BTreeMap<K, V> {
        match self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => m,
            MapRepr::Bound(b) => b.full(Location::caller()),
        }
    }
    fn changes(&mut self) -> Result<ContainerChange, String> {
        match &mut self.0 {
            MapRepr::Bound(b) => Ok(ContainerChange::Entries(b.changes()?)),
            MapRepr::Plain(m) | MapRepr::Promoted(m) => Ok(ContainerChange::Replace(
                m.iter()
                    .map(|(k, v)| Ok((k.to_tree(), encode_value(v)?)))
                    .collect::<Result<_, String>>()?,
            )),
        }
    }
}
impl<K: LazyKey, V: Record> Default for LazyMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}
impl<K: LazyKey, V: Record> Clone for LazyMap<K, V> {
    fn clone(&self) -> Self {
        Self(match &self.0 {
            MapRepr::Plain(m) => MapRepr::Plain(m.clone()),
            MapRepr::Promoted(m) => MapRepr::Promoted(m.clone()),
            MapRepr::Bound(b) => MapRepr::Bound(b.clone()),
        })
    }
}
impl<K: LazyKey, V: Record + fmt::Debug> fmt::Debug for LazyMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => m.fmt(f),
            MapRepr::Bound(b) => write!(f, "LazyMap({:?})", b.path),
        }
    }
}
impl<K: LazyKey, V: Record + PartialEq> PartialEq for LazyMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}
impl<K: LazyKey, V: Record> Deref for LazyMap<K, V> {
    type Target = BTreeMap<K, V>;
    #[track_caller]
    fn deref(&self) -> &BTreeMap<K, V> {
        match &self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => m,
            MapRepr::Bound(b) => b.view(Location::caller()),
        }
    }
}
impl<K: LazyKey, V: Record> DerefMut for LazyMap<K, V> {
    #[track_caller]
    fn deref_mut(&mut self) -> &mut BTreeMap<K, V> {
        if let MapRepr::Bound(b) = &self.0 {
            let full = b.full(Location::caller());
            self.0 = MapRepr::Promoted(full);
        }
        match &mut self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => m,
            MapRepr::Bound(_) => unreachable!(),
        }
    }
}
impl<K: LazyKey + Borrow<Q>, V: Record, Q: Ord + ?Sized + ToOwned<Owned = K>> Index<&Q>
    for LazyMap<K, V>
{
    type Output = V;
    fn index(&self, key: &Q) -> &V {
        self.get(key).expect("no entry found for key")
    }
}
impl<'a, K: LazyKey, V: Record> IntoIterator for &'a LazyMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = std::collections::btree_map::Iter<'a, K, V>;
    #[track_caller]
    fn into_iter(self) -> Self::IntoIter {
        self.deref().iter()
    }
}
impl<'a, K: LazyKey, V: Record> IntoIterator for &'a mut LazyMap<K, V> {
    type Item = (&'a K, &'a mut V);
    type IntoIter = std::collections::btree_map::IterMut<'a, K, V>;
    #[track_caller]
    fn into_iter(self) -> Self::IntoIter {
        self.deref_mut().iter_mut()
    }
}
impl<K: LazyKey, V: Record> From<BTreeMap<K, V>> for LazyMap<K, V> {
    fn from(m: BTreeMap<K, V>) -> Self {
        Self(MapRepr::Plain(m))
    }
}
impl<K: LazyKey, V: Record> FromIterator<(K, V)> for LazyMap<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self(MapRepr::Plain(iter.into_iter().collect()))
    }
}
impl<K: LazyKey, V: Record> Serialize for LazyMap<K, V> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if header_mode() {
            return BTreeMap::<K, V>::new().serialize(s);
        }
        self.deref().serialize(s)
    }
}
impl<'de, K: LazyKey, V: Record> Deserialize<'de> for LazyMap<K, V> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self(MapRepr::Plain(BTreeMap::deserialize(d)?)))
    }
}
pub struct Entry<'a, K: LazyKey, V: Record> {
    map: &'a mut LazyMap<K, V>,
    key: K,
}
impl<'a, K: LazyKey, V: Record> Entry<'a, K, V> {
    pub fn and_modify(self, f: impl FnOnce(&mut V)) -> Self {
        if let Some(v) = self.map.get_mut::<K>(&self.key) {
            f(v);
        }
        self
    }
    pub fn or_insert_with(self, f: impl FnOnce() -> V) -> &'a mut V {
        if !self.map.contains_key::<K>(&self.key) {
            self.map.insert(self.key.clone(), f());
        }
        self.map.get_mut::<K>(&self.key).expect("present")
    }
    pub fn or_insert(self, value: V) -> &'a mut V {
        self.or_insert_with(|| value)
    }
    pub fn or_default(self) -> &'a mut V
    where
        V: Default,
    {
        self.or_insert_with(V::default)
    }
}

/// Ordered unique-member array (`BTreeSet` compatible).
pub struct LazySet<K>(SetRepr<K>);
enum SetRepr<K> {
    Plain(BTreeSet<K>),
    Bound(Box<Bound<K, ()>>, OnceLock<BTreeSet<K>>),
    Promoted(BTreeSet<K>),
}
fn set_encode<K: Serialize>(k: &K, _: &()) -> Result<Vec<u8>, String> {
    encode_value(k)
}
fn set_decode<K>(_: &K, _: &[u8]) -> Result<(), String> {
    Ok(())
}
impl<K: LazyKey> LazySet<K> {
    pub fn new() -> Self {
        Self(SetRepr::Plain(BTreeSet::new()))
    }
    pub fn bound(source: Arc<dyn RecordSource>, path: ContainerPath) -> Self {
        Self(SetRepr::Bound(
            Box::new(Bound::new(source, path, set_encode::<K>, set_decode::<K>)),
            OnceLock::new(),
        ))
    }
    pub fn contains<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        match &self.0 {
            SetRepr::Plain(m) | SetRepr::Promoted(m) => m.contains(key),
            SetRepr::Bound(b, _) => b.slot(&key.to_owned()).current.is_some(),
        }
    }
    pub fn insert(&mut self, key: K) -> bool {
        match &mut self.0 {
            SetRepr::Plain(m) | SetRepr::Promoted(m) => m.insert(key),
            SetRepr::Bound(b, v) => {
                v.take();
                b.slot_mut(&key).current.replace(()).is_none()
            }
        }
    }
    pub fn remove<Q>(&mut self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        match &mut self.0 {
            SetRepr::Plain(m) | SetRepr::Promoted(m) => m.remove(key),
            SetRepr::Bound(b, v) => {
                v.take();
                b.slot_mut(&key.to_owned()).current.take().is_some()
            }
        }
    }
    pub fn len(&self) -> usize {
        match &self.0 {
            SetRepr::Plain(m) | SetRepr::Promoted(m) => m.len(),
            SetRepr::Bound(b, _) => b.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn next_key<Q>(&self, after: Option<&Q>) -> Option<K>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        match &self.0 {
            SetRepr::Plain(m) | SetRepr::Promoted(m) => match after {
                Some(a) => m
                    .range::<Q, _>((std::ops::Bound::Excluded(a), std::ops::Bound::Unbounded))
                    .next()
                    .cloned(),
                None => m.iter().next().cloned(),
            },
            SetRepr::Bound(b, _) => b.next_after(after.map(|a| a.to_owned()).as_ref()),
        }
    }
    pub fn keys_after<Q>(&self, after: Option<&Q>, limit: usize) -> Vec<K>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        let mut out: Vec<K> = Vec::new();
        while out.len() < limit {
            let next = match out.last() {
                Some(last) => self.next_key::<K>(Some(last)),
                None => self.next_key(after),
            };
            match next {
                Some(k) => out.push(k),
                None => break,
            }
        }
        out
    }
    fn changes(&mut self) -> Result<ContainerChange, String> {
        match &mut self.0 {
            SetRepr::Bound(b, _) => Ok(ContainerChange::Entries(b.changes()?)),
            SetRepr::Plain(m) | SetRepr::Promoted(m) => Ok(ContainerChange::Replace(
                m.iter()
                    .map(|k| Ok((k.to_tree(), encode_value(k)?)))
                    .collect::<Result<_, String>>()?,
            )),
        }
    }
}
impl<K: LazyKey> Default for LazySet<K> {
    fn default() -> Self {
        Self::new()
    }
}
impl<K: LazyKey> Clone for LazySet<K> {
    fn clone(&self) -> Self {
        Self(match &self.0 {
            SetRepr::Plain(m) => SetRepr::Plain(m.clone()),
            SetRepr::Promoted(m) => SetRepr::Promoted(m.clone()),
            SetRepr::Bound(b, _) => SetRepr::Bound(b.clone(), OnceLock::new()),
        })
    }
}
impl<K: LazyKey> fmt::Debug for LazySet<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            SetRepr::Plain(m) | SetRepr::Promoted(m) => m.fmt(f),
            SetRepr::Bound(b, _) => write!(f, "LazySet({:?})", b.path),
        }
    }
}
impl<K: LazyKey> PartialEq for LazySet<K> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}
impl<K: LazyKey> Deref for LazySet<K> {
    type Target = BTreeSet<K>;
    #[track_caller]
    fn deref(&self) -> &BTreeSet<K> {
        match &self.0 {
            SetRepr::Plain(m) | SetRepr::Promoted(m) => m,
            SetRepr::Bound(b, view) => {
                let at = Location::caller();
                view.get_or_init(|| b.full(at).into_keys().collect())
            }
        }
    }
}
impl<K: LazyKey> DerefMut for LazySet<K> {
    #[track_caller]
    fn deref_mut(&mut self) -> &mut BTreeSet<K> {
        if let SetRepr::Bound(b, _) = &self.0 {
            let full = b.full(Location::caller()).into_keys().collect();
            self.0 = SetRepr::Promoted(full);
        }
        match &mut self.0 {
            SetRepr::Plain(m) | SetRepr::Promoted(m) => m,
            SetRepr::Bound(..) => unreachable!(),
        }
    }
}
impl<'a, K: LazyKey> IntoIterator for &'a LazySet<K> {
    type Item = &'a K;
    type IntoIter = std::collections::btree_set::Iter<'a, K>;
    #[track_caller]
    fn into_iter(self) -> Self::IntoIter {
        self.deref().iter()
    }
}
impl<K: LazyKey> IntoIterator for LazySet<K> {
    type Item = K;
    type IntoIter = std::collections::btree_set::IntoIter<K>;
    #[track_caller]
    fn into_iter(mut self) -> Self::IntoIter {
        std::mem::take(self.deref_mut()).into_iter()
    }
}
impl<K: LazyKey> From<BTreeSet<K>> for LazySet<K> {
    fn from(m: BTreeSet<K>) -> Self {
        Self(SetRepr::Plain(m))
    }
}
impl<K: LazyKey> FromIterator<K> for LazySet<K> {
    fn from_iter<I: IntoIterator<Item = K>>(iter: I) -> Self {
        Self(SetRepr::Plain(iter.into_iter().collect()))
    }
}
impl<K: LazyKey> Serialize for LazySet<K> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if header_mode() {
            return BTreeSet::<K>::new().serialize(s);
        }
        self.deref().serialize(s)
    }
}
impl<'de, K: LazyKey> Deserialize<'de> for LazySet<K> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self(SetRepr::Plain(BTreeSet::deserialize(d)?)))
    }
}

/// Position-ordered array (`Vec` compatible); physical key is the position.
/// Bound sequences support append and positional access; any other mutation
/// (retain, truncate, removal) promotes to a full replacement.
pub struct LazySeq<T>(SeqRepr<T>);
enum SeqRepr<T> {
    Plain(Vec<T>),
    Bound(Box<Bound<u64, T>>, OnceLock<Vec<T>>),
    Promoted(Vec<T>),
}
impl<T: Record> LazySeq<T> {
    pub fn new() -> Self {
        Self(SeqRepr::Plain(Vec::new()))
    }
    pub fn bound(source: Arc<dyn RecordSource>, path: ContainerPath) -> Self {
        Self(SeqRepr::Bound(
            Box::new(Bound::new(
                source,
                path,
                map_encode::<u64, T>,
                map_decode::<u64, T>,
            )),
            OnceLock::new(),
        ))
    }
    pub fn len(&self) -> usize {
        match &self.0 {
            SeqRepr::Plain(v) | SeqRepr::Promoted(v) => v.len(),
            SeqRepr::Bound(b, _) => b.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn push(&mut self, value: T) {
        match &mut self.0 {
            SeqRepr::Plain(v) | SeqRepr::Promoted(v) => v.push(value),
            SeqRepr::Bound(b, view) => {
                view.take();
                let at = b.len() as u64;
                b.insert_absent(at, value);
            }
        }
    }
    pub fn get(&self, index: usize) -> Option<&T> {
        match &self.0 {
            SeqRepr::Plain(v) | SeqRepr::Promoted(v) => v.get(index),
            SeqRepr::Bound(b, _) => {
                if index >= b.len() {
                    return None;
                }
                b.slot(&(index as u64)).current.as_ref()
            }
        }
    }
    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        match &mut self.0 {
            SeqRepr::Plain(v) | SeqRepr::Promoted(v) => v.get_mut(index),
            SeqRepr::Bound(b, view) => {
                if index >= b.len() {
                    return None;
                }
                view.take();
                b.slot_mut(&(index as u64)).current.as_mut()
            }
        }
    }
    pub fn last(&self) -> Option<&T> {
        self.len().checked_sub(1).and_then(|i| self.get(i))
    }
    fn changes(&mut self) -> Result<ContainerChange, String> {
        match &mut self.0 {
            SeqRepr::Bound(b, _) => Ok(ContainerChange::Entries(b.changes()?)),
            SeqRepr::Plain(v) | SeqRepr::Promoted(v) => Ok(ContainerChange::Replace(
                v.iter()
                    .enumerate()
                    .map(|(i, x)| Ok((TreeKey::U64(i as u64), encode_value(x)?)))
                    .collect::<Result<_, String>>()?,
            )),
        }
    }
}
impl<T: Record> Default for LazySeq<T> {
    fn default() -> Self {
        Self::new()
    }
}
impl<T: Record> Clone for LazySeq<T> {
    fn clone(&self) -> Self {
        Self(match &self.0 {
            SeqRepr::Plain(v) => SeqRepr::Plain(v.clone()),
            SeqRepr::Promoted(v) => SeqRepr::Promoted(v.clone()),
            SeqRepr::Bound(b, _) => SeqRepr::Bound(b.clone(), OnceLock::new()),
        })
    }
}
impl<T: Record + fmt::Debug> fmt::Debug for LazySeq<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            SeqRepr::Plain(v) | SeqRepr::Promoted(v) => v.fmt(f),
            SeqRepr::Bound(b, _) => write!(f, "LazySeq({:?})", b.path),
        }
    }
}
impl<T: Record + PartialEq> PartialEq for LazySeq<T> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}
impl<T: Record> Deref for LazySeq<T> {
    type Target = Vec<T>;
    #[track_caller]
    fn deref(&self) -> &Vec<T> {
        match &self.0 {
            SeqRepr::Plain(v) | SeqRepr::Promoted(v) => v,
            SeqRepr::Bound(b, view) => {
                let at = Location::caller();
                view.get_or_init(|| b.full(at).into_values().collect())
            }
        }
    }
}
impl<T: Record> DerefMut for LazySeq<T> {
    #[track_caller]
    fn deref_mut(&mut self) -> &mut Vec<T> {
        if let SeqRepr::Bound(b, _) = &self.0 {
            let full = b.full(Location::caller()).into_values().collect();
            self.0 = SeqRepr::Promoted(full);
        }
        match &mut self.0 {
            SeqRepr::Plain(v) | SeqRepr::Promoted(v) => v,
            SeqRepr::Bound(..) => unreachable!(),
        }
    }
}
impl<T: Record> Index<usize> for LazySeq<T> {
    type Output = T;
    fn index(&self, index: usize) -> &T {
        self.get(index).expect("sequence index out of range")
    }
}
impl<'a, T: Record> IntoIterator for &'a LazySeq<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    #[track_caller]
    fn into_iter(self) -> Self::IntoIter {
        self.deref().iter()
    }
}
impl<T: Record> From<Vec<T>> for LazySeq<T> {
    fn from(v: Vec<T>) -> Self {
        Self(SeqRepr::Plain(v))
    }
}
impl<T: Record> Serialize for LazySeq<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if header_mode() {
            return Vec::<T>::new().serialize(s);
        }
        self.deref().serialize(s)
    }
}
impl<'de, T: Record> Deserialize<'de> for LazySeq<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self(SeqRepr::Plain(Vec::deserialize(d)?)))
    }
}

/// One schema-certified lazy container field of the State.
pub trait LazyField {
    fn shape(&self) -> Shape;
    /// Bind an (empty, header-deserialized) field to the source.
    fn bind_field(&mut self, source: Arc<dyn RecordSource>, field: &'static str);
    /// Emit this field's changes (a plain field emits a full replacement,
    /// which doubles as the export used to build a fresh record store).
    fn field_changes(&mut self, field: &'static str, out: &mut Changes) -> Result<(), String>;
    /// True when a bound field was materialized by a full-container mutation.
    fn promoted(&self) -> bool;
}
impl<K: LazyKey, V: Record> LazyField for LazyMap<K, V> {
    fn shape(&self) -> Shape {
        Shape::Map(K::KIND)
    }
    fn bind_field(&mut self, source: Arc<dyn RecordSource>, field: &'static str) {
        *self = Self::bound(source, ContainerPath::field(field));
    }
    fn field_changes(&mut self, field: &'static str, out: &mut Changes) -> Result<(), String> {
        out.push((ContainerPath::field(field), self.changes()?));
        Ok(())
    }
    fn promoted(&self) -> bool {
        matches!(self.0, MapRepr::Promoted(_))
    }
}
impl<K: LazyKey> LazyField for LazySet<K> {
    fn shape(&self) -> Shape {
        Shape::Set(K::KIND)
    }
    fn bind_field(&mut self, source: Arc<dyn RecordSource>, field: &'static str) {
        *self = Self::bound(source, ContainerPath::field(field));
    }
    fn field_changes(&mut self, field: &'static str, out: &mut Changes) -> Result<(), String> {
        out.push((ContainerPath::field(field), self.changes()?));
        Ok(())
    }
    fn promoted(&self) -> bool {
        matches!(self.0, SetRepr::Promoted(_))
    }
}
impl<T: Record> LazyField for LazySeq<T> {
    fn shape(&self) -> Shape {
        Shape::Seq
    }
    fn bind_field(&mut self, source: Arc<dyn RecordSource>, field: &'static str) {
        *self = Self::bound(source, ContainerPath::field(field));
    }
    fn field_changes(&mut self, field: &'static str, out: &mut Changes) -> Result<(), String> {
        out.push((ContainerPath::field(field), self.changes()?));
        Ok(())
    }
    fn promoted(&self) -> bool {
        matches!(self.0, SeqRepr::Promoted(_))
    }
}

impl<K: LazyKey, V: Record> IntoIterator for LazyMap<K, V> {
    type Item = (K, V);
    type IntoIter = std::collections::btree_map::IntoIter<K, V>;
    #[track_caller]
    fn into_iter(mut self) -> Self::IntoIter {
        std::mem::take(self.deref_mut()).into_iter()
    }
}
impl<K: LazyKey + Borrow<Q>, V: Record, Q: Ord + ?Sized + ToOwned<Owned = K>> std::ops::IndexMut<&Q>
    for LazyMap<K, V>
{
    fn index_mut(&mut self, key: &Q) -> &mut V {
        self.get_mut(key).expect("no entry found for key")
    }
}
impl<T: Record> IntoIterator for LazySeq<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;
    #[track_caller]
    fn into_iter(mut self) -> Self::IntoIter {
        std::mem::take(self.deref_mut()).into_iter()
    }
}
impl<T: Record> std::ops::IndexMut<usize> for LazySeq<T> {
    fn index_mut(&mut self, index: usize) -> &mut T {
        self.get_mut(index).expect("sequence index out of range")
    }
}
