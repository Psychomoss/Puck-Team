mod links;
mod out;
mod parse;

use memchr::memchr;
use memmap2::Mmap;
use parse::*;
use std::cell::UnsafeCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const GAP: i64 = 1_800_000;
const WEEK: i64 = 7 * DAY;
const CHUNK: usize = 1 << 20;
const NB: usize = 1024;
const ORGANIC: u32 = 0;

/// Runs tasks 0..n on `threads` workers pulling from a shared counter; returns per-worker state.
pub fn par_tasks<S: Send>(n: usize, threads: usize, init: impl Fn() -> S + Sync, f: impl Fn(&mut S, usize) + Sync) -> Vec<S> {
    let next = AtomicUsize::new(0);
    std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads.min(n.max(1)))
            .map(|_| {
                sc.spawn(|| {
                    let mut s = init();
                    loop {
                        let i = next.fetch_add(1, Relaxed);
                        if i >= n {
                            break;
                        }
                        f(&mut s, i);
                    }
                    s
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

pub struct SendPtr<T>(pub *mut T);
unsafe impl<T> Sync for SendPtr<T> {}
unsafe impl<T> Send for SendPtr<T> {}
impl<T> SendPtr<T> {
    pub fn ptr(&self) -> *mut T {
        self.0
    }
}

/// Per-task exclusive slots: task i may take slot i mutably.
pub struct Slots<T>(Vec<UnsafeCell<T>>);
unsafe impl<T: Send> Sync for Slots<T> {}
impl<T> Slots<T> {
    pub fn new(v: Vec<T>) -> Self {
        Slots(v.into_iter().map(UnsafeCell::new).collect())
    }
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get(&self, i: usize) -> &mut T {
        unsafe { &mut *self.0[i].get() }
    }
}

pub struct Input {
    maps: Vec<Mmap>,
    chunks: Vec<(usize, usize, usize)>,
    pub bytes: usize,
}

impl Input {
    pub fn chunk(&self, i: usize) -> &[u8] {
        let (f, a, b) = self.chunks[i];
        &self.maps[f][a..b]
    }
    pub fn len(&self) -> usize {
        self.chunks.len()
    }
}

fn open_input(dir: &Path, reverse: bool) -> Input {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "csv") && p.is_file())
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    if reverse {
        files.reverse();
    }
    let mut maps = vec![];
    let mut chunks = vec![];
    let mut bytes = 0;
    for p in files {
        let file = std::fs::File::open(&p).unwrap();
        if file.metadata().unwrap().len() == 0 {
            continue;
        }
        let m = unsafe { Mmap::map(&file).unwrap() };
        let fi = maps.len();
        if let Some(h) = memchr(b'\n', &m) {
            let mut a = h + 1;
            bytes += m.len() - a;
            while a < m.len() {
                let mut b = (a + CHUNK).min(m.len());
                if b < m.len() {
                    b = memchr(b'\n', &m[b..]).map_or(m.len(), |o| b + o + 1);
                }
                chunks.push((fi, a, b));
                a = b;
            }
        }
        maps.push(m);
    }
    // Largest-first is not needed: chunks are equal-sized; interleave files only when reversing.
    if reverse {
        chunks.reverse();
    }
    Input { maps, chunks, bytes }
}

fn read_small_csv<const N: usize>(path: &Path, mut f: impl FnMut(&[&[u8]; N])) {
    let data = std::fs::read(path).unwrap_or_default();
    let Some(h) = memchr(b'\n', &data) else { return };
    scan::<N>(&data[h + 1..], |fl| f(fl));
}

#[derive(Clone, Copy)]
struct Ev {
    ekey: u64,
    ukey: u64,
    ingest: i64,
    ts: i64,
    price: u32,
    cat: u16,
    valid: u16,
}

#[derive(Clone, Copy)]
struct PEv {
    pkey: u64,
    ts: i64,
    price: u32,
    cat: u32,
}

#[derive(Clone, Copy)]
struct PCl {
    pkey: u64,
    ckey: u64,
    ts: i64,
    camp: u32,
}

#[derive(Clone, Copy, Default)]
struct CampDay {
    sessions: u64,
    purchasing: u64,
    revenue: u64,
    assisted: u64,
}

pub struct UserRow {
    pub pkey: u64,
    pub day: i32,
    pub sessions: u32,
    pub events: u32,
    pub revenue: u64,
    pub top: u32,
}

struct Timer(Instant);
impl Timer {
    fn lap(&mut self, what: &str) {
        let now = Instant::now();
        eprintln!("{what:>12}: {:8.1} ms", (now - self.0).as_secs_f64() * 1e3);
        self.0 = now;
    }
}

#[inline(always)]
fn bucket(k: u64) -> usize {
    (mix(k) >> 54) as usize
}

fn json_str(text: &str, name: &str) -> String {
    let at = text.find(&format!("\"{name}\"")).expect(name) + name.len() + 2;
    let rest = &text[at..];
    let a = rest.find('"').unwrap() + 1;
    let b = a + rest[a..].find('"').unwrap();
    rest[a..b].to_string()
}

fn buckets<T>(cap: usize) -> Vec<Vec<T>> {
    (0..NB).map(|_| Vec::with_capacity(cap)).collect()
}

/// Groups `items` by person (keys -> dense ids via a small open-addressing table), stable within a person.
fn group_by<T: Copy>(items: impl Iterator<Item = T> + Clone, pk: impl Fn(&T) -> u64, n: usize, ids: &mut PersonIds) -> (Vec<T>, Vec<u32>) {
    let idx: Vec<u32> = items.clone().map(|it| ids.get(pk(&it))).collect();
    let mut counts = vec![0u32; ids.keys.len() + 1];
    for &id in &idx {
        counts[id as usize] += 1;
    }
    let mut start = vec![0u32; counts.len() + 1];
    for i in 0..counts.len() {
        start[i + 1] = start[i] + counts[i];
    }
    let mut pos = start.clone();
    let mut out: Vec<T> = Vec::with_capacity(n);
    let o = out.spare_capacity_mut();
    for (it, &id) in items.zip(idx.iter()) {
        o[pos[id as usize] as usize].write(it);
        pos[id as usize] += 1;
    }
    unsafe { out.set_len(n) };
    (out, start)
}

/// Dense ids for person keys of one bucket; id == keys.len() means "absent" (lookup-only mode).
struct PersonIds {
    keys: Vec<u64>,
    table: Vec<u32>,
    mask: usize,
    insert: bool,
}

impl PersonIds {
    fn new(n: usize) -> Self {
        let cap = (n * 2).next_power_of_two().max(16);
        PersonIds { keys: Vec::new(), table: vec![u32::MAX; cap], mask: cap - 1, insert: true }
    }
    #[inline]
    fn get(&mut self, k: u64) -> u32 {
        let mut h = (mix(k) as usize) & self.mask;
        loop {
            let t = self.table[h];
            if t == u32::MAX {
                if !self.insert {
                    return self.keys.len() as u32;
                }
                self.table[h] = self.keys.len() as u32;
                self.keys.push(k);
                return self.table[h];
            }
            if self.keys[t as usize] == k {
                return t;
            }
            h = (h + 1) & self.mask;
        }
    }
}

fn main() {
    let t0 = Instant::now();
    // mi_option_purge_delay = -1: keep freed pages for reuse instead of returning them to the OS.
    unsafe { libmimalloc_sys::mi_option_set(15, -1) };
    let mut tm = Timer(t0);
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let data = PathBuf::from(env("DATA_DIR", "/data"));
    let outdir = PathBuf::from(env("OUTPUT_DIR", "/output"));
    let threads: usize = std::env::var("NCPU")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(8, |n| n.get()))
        .max(1);
    let reverse = std::env::var("REVERSE_FILES").is_ok_and(|v| v == "1");

    let manifest = std::fs::read_to_string(data.join("manifest.json")).unwrap();
    let ws = parse_ts(json_str(&manifest, "window_start").as_bytes()).unwrap();
    let we = parse_ts(json_str(&manifest, "window_end").as_bytes()).unwrap();
    let day0 = ws.div_euclid(DAY);
    let ndays = ((we - 1).div_euclid(DAY) - day0 + 1).max(1) as usize;

    let mut prod_raw: Vec<(Vec<u8>, Vec<u8>)> = vec![];
    read_small_csv::<2>(&data.join("products.csv"), |f| prod_raw.push((f[0].to_vec(), f[1].to_vec())));
    let mut cats: Vec<Vec<u8>> = prod_raw.iter().map(|p| p.1.clone()).collect();
    cats.push(b"unknown".to_vec());
    cats.sort();
    cats.dedup();
    let unknown = cats.binary_search(&b"unknown".to_vec()).unwrap() as u16;
    let mut products = RefMap::default();
    for (p, c) in &prod_raw {
        products.insert(p, cats.binary_search(c).unwrap() as u32);
    }
    let mut camp_names: Vec<(Vec<u8>, Vec<u8>)> = vec![(b"organic".to_vec(), b"organic".to_vec())];
    let mut campaigns = RefMap::default();
    read_small_csv::<2>(&data.join("campaigns.csv"), |f| {
        if campaigns.get(f[0]).is_none() {
            campaigns.insert(f[0], camp_names.len() as u32);
            camp_names.push((f[0].to_vec(), f[1].to_vec()));
        }
    });
    let ncamp = camp_names.len();
    tm.lap("reference");

    let persons = links::build(&open_input(&data.join("links"), reverse), threads, &mut tm);

    // Events: parse, validate each copy, bucket by event_id.
    let ev_in = open_input(&data.join("events"), reverse);
    let est = ev_in.bytes / 60 / NB / threads * 2 + 16;
    let ev_parts: Vec<Vec<Vec<Ev>>> = par_tasks(
        ev_in.len(),
        threads,
        || buckets::<Ev>(est),
        |bk, i| {
            scan::<7>(ev_in.chunk(i), |f| {
                if f[0].is_empty() {
                    return;
                }
                let ekey = key(f[0]);
                let ingest = parse_ts(f[3]).unwrap_or(i64::MAX);
                let mut e = Ev { ekey, ukey: 0, ingest, ts: 0, price: 0, cat: 0, valid: 0 };
                let ok = (|| {
                    if f[1].is_empty() || f[5].is_empty() {
                        return None;
                    }
                    let ts = parse_ts(f[2]).filter(|&t| t >= ws && t < we)?;
                    match f[4] {
                        b"view" | b"cart" => {}
                        b"purchase" => {
                            e.price = parse_price(f[6])?;
                            e.cat = products.get(f[5]).map_or(unknown, |c| c as u16);
                        }
                        _ => return None,
                    }
                    if ingest - ts > DAY {
                        return None;
                    }
                    e.ts = ts;
                    e.ukey = key(f[1]);
                    Some(())
                })();
                e.valid = ok.is_some() as u16;
                bk[bucket(ekey)].push(e);
            });
        },
    );
    tm.lap("events parse");

    // Dedup per bucket (smallest ingest_ts wins), keep valid survivors, re-bucket by person.
    let mut by_bucket: Vec<Vec<Vec<Ev>>> = (0..NB).map(|_| Vec::with_capacity(ev_parts.len())).collect();
    for t in ev_parts {
        for (b, v) in t.into_iter().enumerate() {
            by_bucket[b].push(v);
        }
    }
    let ev_slots = Slots::new(by_bucket);
    let est = ev_in.bytes / 64 / NB / threads * 2 + 16;
    let pev_parts: Vec<Vec<Vec<PEv>>> = par_tasks(
        NB,
        threads,
        || (buckets::<PEv>(est), Vec::<u32>::new(), Vec::<u64>::new()),
        |(out, table, tkeys), b| {
            let parts = unsafe { ev_slots.get(b) };
            let n: usize = parts.iter().map(|p| p.len()).sum();
            let cap = (n * 2).next_power_of_two().max(16);
            table.clear();
            table.resize(cap, u32::MAX);
            tkeys.clear();
            tkeys.resize(cap, 0);
            let mask = cap - 1;
            for t in 0..parts.len() {
                for i in 0..parts[t].len() {
                    let e = parts[t][i];
                    let mut h = (mix(e.ekey) as usize >> 10) & mask;
                    loop {
                        let s = table[h];
                        if s == u32::MAX {
                            table[h] = (t as u32) << 24 | i as u32;
                            tkeys[h] = e.ekey;
                            break;
                        }
                        if tkeys[h] == e.ekey {
                            let (pt, pi) = ((s >> 24) as usize, (s & 0xFF_FFFF) as usize);
                            if e.ingest < parts[pt][pi].ingest {
                                parts[pt][pi].valid = 0;
                                table[h] = (t as u32) << 24 | i as u32;
                            } else {
                                parts[t][i].valid = 0;
                            }
                            break;
                        }
                        h = (h + 1) & mask;
                    }
                }
            }
            for p in parts.iter() {
                for (i, e) in p.iter().enumerate() {
                    if let Some(f) = p.get(i + 16) {
                        persons.prefetch(f.ukey);
                    }
                    if e.valid != 0 {
                        let pk = persons.get(e.ukey);
                        out[bucket(pk)].push(PEv { pkey: pk, ts: e.ts, price: e.price, cat: e.cat as u32 });
                    }
                }
            }
            *parts = Vec::new();
        },
    )
    .into_iter()
    .map(|x| x.0)
    .collect();
    drop(ev_slots);
    tm.lap("dedup");

    // Clicks: duplicates are identical, so they change neither credit nor assists; no dedup needed.
    let cl_in = open_input(&data.join("clicks"), reverse);
    let cws = ws - WEEK;
    let est = cl_in.bytes / 40 / NB / threads * 2 + 16;
    let pcl_parts: Vec<Vec<Vec<PCl>>> = par_tasks(
        cl_in.len(),
        threads,
        || buckets::<PCl>(est),
        |bk, i| {
            scan::<4>(cl_in.chunk(i), |f| {
                if f[0].is_empty() || f[1].is_empty() {
                    return;
                }
                let Some(ts) = parse_ts(f[3]).filter(|&t| t >= cws && t < we) else { return };
                let Some(camp) = campaigns.get(f[2]) else { return };
                let p = persons.get(key(f[1]));
                bk[bucket(p)].push(PCl { pkey: p, ckey: key(f[0]), ts, camp });
            });
        },
    );
    tm.lap("clicks");

    // Per person bucket: group, sessionize, credit, assists, aggregate.
    let mut order: Vec<usize> = (0..NB).collect();
    order.sort_by_key(|&b| std::cmp::Reverse(pev_parts.iter().map(|t| t[b].len()).sum::<usize>()));
    let ncat = cats.len();
    let results: Vec<(Vec<UserRow>, Vec<CampDay>)> = par_tasks(
        NB,
        threads,
        || (Vec::new(), vec![CampDay::default(); ncamp * ndays]),
        |(rows, cd), oi| {
            let b = order[oi];
            let n: usize = pev_parts.iter().map(|t| t[b].len()).sum();
            if n == 0 {
                return;
            }
            let mut ids = PersonIds::new(n);
            let (mut evs, es) = group_by(pev_parts.iter().flat_map(|t| t[b].iter().copied()), |e| e.pkey, n, &mut ids);
            ids.insert = false;
            let nc: usize = pcl_parts.iter().map(|t| t[b].len()).sum();
            let (mut cls, cs) = group_by(pcl_parts.iter().flat_map(|t| t[b].iter().copied()), |c| c.pkey, nc, &mut ids);
            let mut stamp = vec![u32::MAX; ncamp];
            let mut sid: u32 = 0;
            let mut catsum = vec![0u64; ncat];
            for p in 0..ids.keys.len() {
                let pk = ids.keys[p];
                let ev = &mut evs[es[p] as usize..es[p + 1] as usize];
                ev.sort_unstable_by_key(|e| e.ts);
                let cl = &mut cls[cs[p] as usize..cs[p + 1] as usize];
                cl.sort_unstable_by(|a, b| a.ts.cmp(&b.ts).then_with(|| cmp_key(b.ckey, a.ckey)));
                let (mut lo, mut hi) = (0, 0);
                let mut cur: Option<UserRow> = None;
                let mut s = 0;
                while s < ev.len() {
                    let start = ev[s].ts;
                    let mut j = s + 1;
                    while j < ev.len() && ev[j].ts - ev[j - 1].ts <= GAP {
                        j += 1;
                    }
                    let day = start.div_euclid(DAY);
                    let di = (day - day0) as usize;
                    let mut rev = 0u64;
                    for e in &ev[s..j] {
                        rev += e.price as u64;
                    }
                    while hi < cl.len() && cl[hi].ts <= start {
                        hi += 1;
                    }
                    while lo < hi && cl[lo].ts < start - WEEK {
                        lo += 1;
                    }
                    let camp = if hi > lo { cl[hi - 1].camp } else { ORGANIC };
                    let c = &mut cd[camp as usize * ndays + di];
                    c.sessions += 1;
                    c.revenue += rev;
                    c.purchasing += (rev > 0) as u64;
                    for k in &cl[lo..hi] {
                        if stamp[k.camp as usize] != sid {
                            stamp[k.camp as usize] = sid;
                            cd[k.camp as usize * ndays + di].assisted += 1;
                        }
                    }
                    sid += 1;
                    let day = day as i32;
                    if cur.as_ref().is_some_and(|r| r.day != day) {
                        rows.push(finish(cur.take().unwrap(), &mut catsum));
                    }
                    if rev > 0 {
                        for e in &ev[s..j] {
                            catsum[e.cat as usize] += e.price as u64;
                        }
                    }
                    let r = cur.get_or_insert(UserRow { pkey: pk, day, sessions: 0, events: 0, revenue: 0, top: u32::MAX });
                    r.sessions += 1;
                    r.events += (j - s) as u32;
                    r.revenue += rev;
                    s = j;
                }
                rows.push(finish(cur.take().unwrap(), &mut catsum));
            }
        },
    );
    tm.lap("sessions");

    let mut cd = vec![CampDay::default(); ncamp * ndays];
    let mut rows: Vec<Vec<UserRow>> = vec![];
    for (r, c) in results {
        for (a, b) in cd.iter_mut().zip(c) {
            a.sessions += b.sessions;
            a.purchasing += b.purchasing;
            a.revenue += b.revenue;
            a.assisted += b.assisted;
        }
        rows.push(r);
    }
    let mut camp_rows = vec![];
    for c in 0..ncamp {
        for d in 0..ndays {
            let x = cd[c * ndays + d];
            if x.sessions > 0 || x.assisted > 0 {
                camp_rows.push((c, (day0 + d as i64) as i32, x.sessions, x.purchasing, x.revenue, x.assisted));
            }
        }
    }
    out::write_all(&outdir, &rows, &cats, &camp_names, &camp_rows);
    tm.lap("write");
    eprintln!("{:>12}: {:8.1} ms", "total", t0.elapsed().as_secs_f64() * 1e3);
    std::process::exit(0);
}

fn finish(mut r: UserRow, catsum: &mut [u64]) -> UserRow {
    let mut best = 0;
    for (i, x) in catsum.iter_mut().enumerate() {
        if *x > best {
            best = *x;
            r.top = i as u32;
        }
        *x = 0;
    }
    r
}
