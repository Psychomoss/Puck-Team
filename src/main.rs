mod links;
mod out;
mod parse;
mod person;

use memchr::memchr;
use memmap2::Mmap;
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

#[derive(Clone, Copy, Default)]
pub struct CampDay {
    pub sessions: u64,
    pub purchasing: u64,
    pub revenue: u64,
    pub assisted: u64,
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
        || (buckets::<PCl>(est), Vec::<PCl>::new()),
        |(bk, tmp), i| {
            tmp.clear();
            scan::<4>(cl_in.chunk(i), |f| {
                if f[0].is_empty() || f[1].is_empty() {
                    return;
                }
                let Some(ts) = parse_ts(f[3]).filter(|&t| t >= cws && t < we) else { return };
                let Some(camp) = campaigns.get(f[2]) else { return };
                tmp.push(PCl { pkey: key(f[1]), ckey: key(f[0]), ts, camp });
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
    let results: Vec<(Vec<UserRow>, Vec<CampDay>, person::Scratch)> = par_tasks(
        NB,
        threads,
        || (Vec::new(), vec![CampDay::default(); ncamp * ndays], person::Scratch::default()),
        |(rows, cd, scratch), oi| {
            let b = order[oi];
            let evs: Vec<&[PEv]> = pev_parts.iter().map(|t| &t[b][..]).collect();
            let cls: Vec<&[PCl]> = pcl_parts.iter().map(|t| &t[b][..]).collect();
            person::process(&ctx, &evs, &cls, scratch, ncamp, ncat, rows, cd);
        },
    );
    tm.lap("sessions");

    let mut cd = vec![CampDay::default(); ncamp * ndays];
    let mut rows: Vec<Vec<UserRow>> = vec![];
    for (r, c, _) in results {
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
