mod links;
mod out;
mod parse;
mod person;

use memchr::memchr;
use parse::*;
use std::cell::UnsafeCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const WEEK: i64 = 7 * DAY;
const CHUNK: usize = 1 << 20;
const NB: usize = 1024;

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
    files: Vec<(std::fs::File, usize)>,
    chunks: Vec<(usize, usize, usize)>,
    pub bytes: usize,
}

impl Input {
    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    /// Reads chunk i into `buf` and returns the lines that start in its byte range (the first
    /// line of each file, the header, is skipped).
    pub fn read<'a>(&self, i: usize, buf: &'a mut Vec<u8>) -> &'a [u8] {
        use std::os::unix::fs::FileExt;
        let (fi, a, b) = self.chunks[i];
        let (file, len) = &self.files[fi];
        let start = a.saturating_sub(1);
        let mut end = (b + 4096).min(*len);
        buf.resize(end - start, 0);
        file.read_exact_at(buf, start as u64).unwrap();
        let grow = |buf: &mut Vec<u8>, end: &mut usize| {
            let new_end = (*end + (*end - start)).min(*len);
            let old = buf.len();
            buf.resize(new_end - start, 0);
            file.read_exact_at(&mut buf[old..], *end as u64).unwrap();
            *end = new_end;
        };
        // First line start at or after a (after the header in the file's first chunk).
        let first = loop {
            let from = if a == 0 { 0 } else { 1 };
            if a > 0 && buf[0] == b'\n' {
                break 1;
            }
            match memchr(b'\n', &buf[from.min(buf.len())..]) {
                Some(p) => break from + p + 1,
                None if end < *len => grow(buf, &mut end),
                None => return &[],
            }
        };
        if start + first >= b {
            return &[];
        }
        // Last line starting before b ends at the first newline at or after b - 1.
        let last = loop {
            match memchr(b'\n', &buf[b - 1 - start..]) {
                Some(p) => break b - start + p,
                None if end < *len => grow(buf, &mut end),
                None => break buf.len(),
            }
        };
        &buf[first..last]
    }
}

fn open_input(dir: &Path, reverse: bool) -> Input {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "csv") && p.is_file())
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    if reverse {
        paths.reverse();
    }
    let mut files = vec![];
    let mut chunks = vec![];
    let mut bytes = 0;
    for p in paths {
        let file = std::fs::File::open(&p).unwrap();
        let len = file.metadata().unwrap().len() as usize;
        if len == 0 {
            continue;
        }
        bytes += len;
        for a in (0..len).step_by(CHUNK) {
            chunks.push((files.len(), a, (a + CHUNK).min(len)));
        }
        files.push((file, len));
    }
    if reverse {
        chunks.reverse();
    }
    Input { files, chunks, bytes }
}

fn read_small_csv<const N: usize>(path: &Path, mut f: impl FnMut(&[&[u8]; N])) {
    let data = std::fs::read(path).unwrap_or_default();
    let Some(h) = memchr(b'\n', &data) else { return };
    scan::<N>(&data[h + 1..], |l| f(&std::array::from_fn(|i| l.get(i))));
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
pub struct PEv {
    pub pkey: u64,
    pub ts: i64,
    pub price: u32,
    pub cat: u32,
}

#[derive(Clone, Copy)]
pub struct PCl {
    pub pkey: u64,
    pub ckey: u64,
    pub ts: i64,
    pub camp: u32,
}

/// Per (campaign, day) credited counters; assists live in a separate compact u32 array.
#[derive(Clone, Copy, Default)]
pub struct Cred {
    pub sessions: u32,
    pub purchasing: u32,
    pub revenue: u64,
}

pub struct UserRow {
    pub pkey: u64,
    pub day: i32,
    pub sessions: u32,
    pub events: u32,
    pub revenue: u64,
    pub top: u32,
}

pub struct Timer(Instant, f64, f64);
fn cpu_times() -> (f64, f64) {
    let mut r: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut r) };
    let t = |v: libc::timeval| v.tv_sec as f64 * 1e3 + v.tv_usec as f64 / 1e3;
    (t(r.ru_utime), t(r.ru_stime))
}
impl Timer {
    fn new() -> Self {
        let (u, s) = cpu_times();
        Timer(Instant::now(), u, s)
    }
    pub fn lap(&mut self, what: &str) {
        let now = Instant::now();
        let (u, s) = cpu_times();
        eprintln!("{what:>12}: {:8.1} ms   (cpu user {:7.0} sys {:6.0})", (now - self.0).as_secs_f64() * 1e3, u - self.1, s - self.2);
        *self = Timer(now, u, s);
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

/// Append-only list of fixed-size blocks: no reallocation copies and no large untouched reservations.
pub struct BlockVec<T> {
    blocks: Vec<Vec<T>>,
    len: usize,
}

const BLOCK: usize = 1024;

impl<T> BlockVec<T> {
    fn new() -> Self {
        BlockVec { blocks: Vec::new(), len: 0 }
    }
    #[inline(always)]
    fn push(&mut self, x: T) {
        if self.len % BLOCK == 0 {
            self.blocks.push(Vec::with_capacity(BLOCK));
        }
        unsafe { self.blocks.last_mut().unwrap_unchecked().push(x) };
        self.len += 1;
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn blocks(&self) -> impl Iterator<Item = &[T]> {
        self.blocks.iter().map(|b| &b[..])
    }
}

impl<T> std::ops::Index<usize> for BlockVec<T> {
    type Output = T;
    #[inline(always)]
    fn index(&self, i: usize) -> &T {
        &self.blocks[i / BLOCK][i % BLOCK]
    }
}

impl<T> std::ops::IndexMut<usize> for BlockVec<T> {
    #[inline(always)]
    fn index_mut(&mut self, i: usize) -> &mut T {
        &mut self.blocks[i / BLOCK][i % BLOCK]
    }
}

fn buckets<T>() -> Vec<BlockVec<T>> {
    (0..NB).map(|_| BlockVec::new()).collect()
}

fn main() {
    let t0 = Instant::now();
    // mi_option_purge_delay = -1: keep freed pages for reuse instead of returning them to the OS.
    unsafe { libmimalloc_sys::mi_option_set(15, -1) };
    let mut tm = Timer::new();
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
    let ev_parts: Vec<Vec<BlockVec<Ev>>> = par_tasks(
        ev_in.len(),
        threads,
        || (buckets::<Ev>(), Vec::new()),
        |(bk, buf), i| {
            scan::<7>(ev_in.read(i, buf), |f| {
                if f.is_empty(0) {
                    return;
                }
                let ekey = f.key(0);
                let ingest = f.ts(3).unwrap_or(i64::MAX);
                let mut e = Ev { ekey, ukey: 0, ingest, ts: 0, price: 0, cat: 0, valid: 0 };
                let ok = (|| {
                    if f.is_empty(1) || f.is_empty(5) {
                        return None;
                    }
                    let ts = f.ts(2).filter(|&t| t >= ws && t < we)?;
                    match f.get(4) {
                        b"view" | b"cart" => {}
                        b"purchase" => {
                            e.price = parse_price(f.get(6))?;
                            e.cat = products.get(f.get(5)).map_or(unknown, |c| c as u16);
                        }
                        _ => return None,
                    }
                    if ingest - ts > DAY {
                        return None;
                    }
                    e.ts = ts;
                    e.ukey = f.key(1);
                    Some(())
                })();
                e.valid = ok.is_some() as u16;
                bk[bucket(ekey)].push(e);
            });
        },
    )
    .into_iter()
    .map(|x| x.0)
    .collect();
    tm.lap("events parse");

    // Dedup per bucket (smallest ingest_ts wins), keep valid survivors, re-bucket by person.
    let mut by_bucket: Vec<Vec<BlockVec<Ev>>> = (0..NB).map(|_| Vec::with_capacity(ev_parts.len())).collect();
    for t in ev_parts {
        for (b, v) in t.into_iter().enumerate() {
            by_bucket[b].push(v);
        }
    }
    let ev_slots = Slots::new(by_bucket);
    let pev_parts: Vec<Vec<BlockVec<PEv>>> = par_tasks(
        NB,
        threads,
        || (buckets::<PEv>(), Vec::<u32>::new(), Vec::<u64>::new()),
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
            for p in parts.iter().flat_map(|p| p.blocks()) {
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
    let pcl_parts: Vec<Vec<BlockVec<PCl>>> = par_tasks(
        cl_in.len(),
        threads,
        || (buckets::<PCl>(), Vec::<PCl>::new(), Vec::new()),
        |(bk, tmp, buf), i| {
            tmp.clear();
            scan::<4>(cl_in.read(i, buf), |f| {
                if f.is_empty(0) || f.is_empty(1) {
                    return;
                }
                let Some(ts) = f.ts(3).filter(|&t| t >= cws && t < we) else { return };
                let Some(camp) = campaigns.get(f.get(2)) else { return };
                tmp.push(PCl { pkey: f.key(1), ckey: f.key(0), ts, camp });
            });
            for j in 0..tmp.len() {
                if let Some(n) = tmp.get(j + 16) {
                    persons.prefetch(n.pkey);
                }
                let mut c = tmp[j];
                c.pkey = persons.get(c.pkey);
                bk[bucket(c.pkey)].push(c);
            }
        },
    )
    .into_iter()
    .map(|x| x.0)
    .collect();
    tm.lap("clicks");

    // Per person bucket: sort by (person, ts), sessionize, credit, assists, aggregate.
    let mut order: Vec<usize> = (0..NB).collect();
    order.sort_by_key(|&b| std::cmp::Reverse(pev_parts.iter().map(|t| t[b].len()).sum::<usize>()));
    let ncat = cats.len();
    let bits = |v: i64| 64 - (v.max(1) as u64 - 1).leading_zeros();
    let ctx = person::Ctx { ws, cws, day0, ndays, ev_bits: bits(we - ws), cl_bits: bits(we - cws) };
    assert!(ctx.cl_bits <= 40, "window too long");
    let results: Vec<(Vec<UserRow>, Vec<Cred>, Vec<u32>, person::Scratch)> = par_tasks(
        NB,
        threads,
        || (Vec::new(), vec![Cred::default(); ncamp * ndays], vec![0u32; ncamp * ndays], person::Scratch::default()),
        |(rows, cr, asst, scratch), oi| {
            let b = order[oi];
            let evs: Vec<&[PEv]> = pev_parts.iter().flat_map(|t| t[b].blocks()).collect();
            let cls: Vec<&[PCl]> = pcl_parts.iter().flat_map(|t| t[b].blocks()).collect();
            person::process(&ctx, &evs, &cls, scratch, ncamp, ncat, rows, cr, asst);
        },
    );
    tm.lap("sessions");

    let mut cd = vec![(0u64, 0u64, 0u64, 0u64); ncamp * ndays];
    let mut rows: Vec<Vec<UserRow>> = vec![];
    for (r, cr, asst, _) in results {
        for ((a, b), x) in cd.iter_mut().zip(cr).zip(asst) {
            a.0 += b.sessions as u64;
            a.1 += b.purchasing as u64;
            a.2 += b.revenue;
            a.3 += x as u64;
        }
        rows.push(r);
    }
    let mut camp_rows = vec![];
    for c in 0..ncamp {
        for d in 0..ndays {
            let x = cd[c * ndays + d];
            if x.0 > 0 || x.3 > 0 {
                camp_rows.push((c, (day0 + d as i64) as i32, x.0, x.1, x.2, x.3));
            }
        }
    }
    out::write_all(&outdir, &rows, &cats, &camp_names, &camp_rows, threads);
    tm.lap("write");
    eprintln!("{:>12}: {:8.1} ms", "total", t0.elapsed().as_secs_f64() * 1e3);
    std::process::exit(0);
}
