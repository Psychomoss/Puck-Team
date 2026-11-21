mod out;
mod parse;

use memchr::memchr;
use memmap2::Mmap;
use parse::*;
use rustc_hash::FxHashMap;
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const GAP: i64 = 1_800_000;
const WEEK: i64 = 7 * DAY;
const CHUNK: usize = 1 << 20;
const NB: usize = 256;
const ORGANIC: u16 = 0;

/// Runs tasks 0..n on `threads` workers pulling from a shared counter; returns per-worker state.
fn par_tasks<S: Send>(n: usize, threads: usize, init: impl Fn() -> S + Sync, f: impl Fn(&mut S, usize) + Sync) -> Vec<S> {
    let next = AtomicUsize::new(0);
    std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads)
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

struct Input {
    maps: Vec<Mmap>,
    chunks: Vec<(usize, usize, usize)>,
}

impl Input {
    fn chunk(&self, i: usize) -> &[u8] {
        let (f, a, b) = self.chunks[i];
        &self.maps[f][a..b]
    }
}

fn list_csv(dir: &Path, reverse: bool) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "csv") && p.is_file())
            .collect(),
        Err(_) => vec![],
    };
    v.sort();
    if reverse {
        v.reverse();
    }
    v
}

fn open_input(dir: &Path, reverse: bool) -> Input {
    let mut maps = vec![];
    let mut chunks = vec![];
    for p in list_csv(dir, reverse) {
        let file = std::fs::File::open(&p).unwrap();
        if file.metadata().unwrap().len() == 0 {
            continue;
        }
        let m = unsafe { Mmap::map(&file).unwrap() };
        let fi = maps.len();
        let Some(h) = memchr(b'\n', &m) else {
            maps.push(m);
            continue;
        };
        let mut a = h + 1;
        while a < m.len() {
            let mut b = (a + CHUNK).min(m.len());
            if b < m.len() {
                b = match memchr(b'\n', &m[b..]) {
                    Some(o) => b + o + 1,
                    None => m.len(),
                };
            }
            chunks.push((fi, a, b));
            a = b;
        }
        maps.push(m);
    }
    if reverse {
        chunks.reverse();
    }
    Input { maps, chunks }
}

#[inline]
fn for_lines(data: &[u8], mut f: impl FnMut(&[u8])) {
    let mut a = 0;
    while a < data.len() {
        let e = memchr(b'\n', &data[a..]).map_or(data.len(), |o| a + o);
        f(&data[a..e]);
        a = e + 1;
    }
}

fn read_small_csv<const N: usize>(path: &Path, mut f: impl FnMut(&[&[u8]; N])) {
    let data = std::fs::read(path).unwrap_or_default();
    let Some(h) = memchr(b'\n', &data) else { return };
    for_lines(&data[h + 1..], |line| {
        let mut fl = [&b""[..]; N];
        if split(line, &mut fl) {
            f(&fl);
        }
    });
}

#[derive(Clone, Copy)]
struct Ev {
    ekey: u128,
    ukey: u128,
    ingest: i64,
    ts: i64,
    price: u32,
    cat: u16,
    valid: bool,
}

#[derive(Clone, Copy)]
struct PEv {
    pkey: u128,
    ts: i64,
    price: u32,
    cat: u16,
}

#[derive(Clone, Copy)]
struct PCl {
    pkey: u128,
    ckey: u128,
    ts: i64,
    camp: u16,
}

#[derive(Clone, Copy, Default)]
struct CampDay {
    sessions: u64,
    purchasing: u64,
    revenue: u64,
    assisted: u64,
}

pub struct UserRow {
    pub pkey: u128,
    pub day: i32,
    pub sessions: u32,
    pub events: u32,
    pub revenue: u64,
    pub top: u16,
}

struct Timer(Instant);
impl Timer {
    fn lap(&mut self, what: &str) {
        let now = Instant::now();
        eprintln!("{what:>12}: {:8.1} ms", (now - self.0).as_secs_f64() * 1e3);
        self.0 = now;
    }
}

#[inline]
fn bucket(k: u128) -> usize {
    (mix(k) >> 56) as usize % NB
}

fn json_str(text: &str, name: &str) -> String {
    let at = text.find(&format!("\"{name}\"")).expect(name) + name.len() + 2;
    let rest = &text[at..];
    let a = rest.find('"').unwrap() + 1;
    let b = a + rest[a..].find('"').unwrap();
    rest[a..b].to_string()
}

fn main() {
    let t0 = Instant::now();
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

    // Links: union-find over all IDs, each named by the byte-wise smallest of its group.
    let links_in = open_input(&data.join("links"), reverse);
    let pairs: Vec<Vec<(u128, u128)>> = par_tasks(links_in.chunks.len(), threads, Vec::new, |v, i| {
        for_lines(links_in.chunk(i), |line| {
            let mut f = [&b""[..]; 2];
            if split(line, &mut f) && !f[0].is_empty() && !f[1].is_empty() {
                v.push((key(f[0]), key(f[1])));
            }
        });
    });
    tm.lap("links parse");
    let mut ids: FxHashMap<u128, u32> = FxHashMap::default();
    let mut keys: Vec<u128> = vec![];
    let mut parent: Vec<u32> = vec![];
    fn find(parent: &mut [u32], mut x: u32) -> u32 {
        while parent[x as usize] != x {
            let g = parent[parent[x as usize] as usize];
            parent[x as usize] = g;
            x = g;
        }
        x
    }
    for v in &pairs {
        for &(a, b) in v {
            let mut id = |k: u128| {
                *ids.entry(k).or_insert_with(|| {
                    keys.push(k);
                    parent.push(parent.len() as u32);
                    parent.len() as u32 - 1
                })
            };
            let ia = id(a);
            let ib = id(b);
            let (ra, rb) = (find(&mut parent, ia), find(&mut parent, ib));
            if ra != rb {
                parent[ra.max(rb) as usize] = ra.min(rb);
            }
        }
    }
    let mut best: Vec<u128> = keys.clone();
    for i in 0..keys.len() {
        let r = find(&mut parent, i as u32) as usize;
        if cmp_key(keys[i], best[r]) == Ordering::Less {
            best[r] = keys[i];
        }
    }
    let mut person: FxHashMap<u128, u128> = FxHashMap::default();
    for i in 0..keys.len() {
        let r = find(&mut parent, i as u32) as usize;
        if best[r] != keys[i] {
            person.insert(keys[i], best[r]);
        }
    }
    drop((ids, keys, parent, best, pairs));
    let person_of = |u: u128| person.get(&u).copied().unwrap_or(u);
    tm.lap("links uf");

    // Events: parse, validate each copy, bucket by event_id.
    let ev_in = open_input(&data.join("events"), reverse);
    let ev_parts: Vec<Vec<Vec<Ev>>> = par_tasks(
        ev_in.chunks.len(),
        threads,
        || (0..NB).map(|_| Vec::new()).collect::<Vec<Vec<Ev>>>(),
        |bk, i| {
            for_lines(ev_in.chunk(i), |line| {
                let mut f = [&b""[..]; 7];
                if !split(line, &mut f) || f[0].is_empty() {
                    return;
                }
                let ekey = key(f[0]);
                let ingest = parse_ts(f[3]).unwrap_or(i64::MAX);
                let mut e = Ev { ekey, ukey: 0, ingest, ts: 0, price: 0, cat: 0, valid: false };
                let ts = parse_ts(f[2]);
                let ok = (|| {
                    if f[1].is_empty() || f[5].is_empty() {
                        return None;
                    }
                    let ts = ts.filter(|&t| t >= ws && t < we)?;
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
                e.valid = ok.is_some();
                bk[bucket(ekey)].push(e);
            });
        },
    );
    tm.lap("events parse");

    // Dedup per bucket, keep valid survivors, re-bucket by person.
    let pev_parts: Vec<Vec<Vec<PEv>>> = par_tasks(
        NB,
        threads,
        || (0..NB).map(|_| Vec::new()).collect::<Vec<Vec<PEv>>>(),
        |out, b| {
            let n: usize = ev_parts.iter().map(|t| t[b].len()).sum();
            let mut best: FxHashMap<u128, Ev> = FxHashMap::with_capacity_and_hasher(n, Default::default());
            for t in &ev_parts {
                for e in &t[b] {
                    best.entry(e.ekey).and_modify(|x| if e.ingest < x.ingest { *x = *e }).or_insert(*e);
                }
            }
            for e in best.values() {
                if e.valid {
                    let p = person_of(e.ukey);
                    out[bucket(p)].push(PEv { pkey: p, ts: e.ts, price: e.price, cat: e.cat });
                }
            }
        },
    );
    drop(ev_parts);
    tm.lap("dedup");

    // Clicks: duplicates are identical, so they change neither credit nor assists; no dedup needed.
    let cl_in = open_input(&data.join("clicks"), reverse);
    let cws = ws - WEEK;
    let pcl_parts: Vec<Vec<Vec<PCl>>> = par_tasks(
        cl_in.chunks.len(),
        threads,
        || (0..NB).map(|_| Vec::new()).collect::<Vec<Vec<PCl>>>(),
        |bk, i| {
            for_lines(cl_in.chunk(i), |line| {
                let mut f = [&b""[..]; 4];
                if !split(line, &mut f) || f[0].is_empty() || f[1].is_empty() {
                    return;
                }
                let Some(ts) = parse_ts(f[3]).filter(|&t| t >= cws && t < we) else { return };
                let Some(camp) = campaigns.get(f[2]) else { return };
                let p = person_of(key(f[1]));
                bk[bucket(p)].push(PCl { pkey: p, ckey: key(f[0]), ts, camp: camp as u16 });
            });
        },
    );
    tm.lap("clicks");

    // Per person bucket: sessionize, credit, assists, aggregate.
    let mut order: Vec<usize> = (0..NB).collect();
    order.sort_by_key(|&b| std::cmp::Reverse(pev_parts.iter().map(|t| t[b].len()).sum::<usize>()));
    let ncat = cats.len();
    let results: Vec<(Vec<UserRow>, Vec<CampDay>)> = par_tasks(
        NB,
        threads,
        || (Vec::new(), vec![CampDay::default(); ncamp * ndays]),
        |(rows, cd), oi| {
            let b = order[oi];
            let mut evs: Vec<PEv> = pev_parts.iter().flat_map(|t| t[b].iter().copied()).collect();
            let mut cls: Vec<PCl> = pcl_parts.iter().flat_map(|t| t[b].iter().copied()).collect();
            evs.sort_unstable_by(|a, b| a.pkey.cmp(&b.pkey).then(a.ts.cmp(&b.ts)));
            cls.sort_unstable_by(|a, b| a.pkey.cmp(&b.pkey).then(a.ts.cmp(&b.ts)).then_with(|| cmp_key(b.ckey, a.ckey)));
            let mut stamp = vec![u32::MAX; ncamp];
            let mut sid: u32 = 0;
            let mut catsum = vec![0u64; ncat];
            let mut ci = 0;
            let mut i = 0;
            while i < evs.len() {
                let pk = evs[i].pkey;
                let mut pe = i;
                while pe < evs.len() && evs[pe].pkey == pk {
                    pe += 1;
                }
                while ci < cls.len() && cls[ci].pkey < pk {
                    ci += 1;
                }
                let mut ce = ci;
                while ce < cls.len() && cls[ce].pkey == pk {
                    ce += 1;
                }
                let cl = &cls[ci..ce];
                let (mut lo, mut hi) = (0, 0);
                let mut cur: Option<UserRow> = None;
                let mut s = i;
                while s < pe {
                    let start = evs[s].ts;
                    let mut j = s + 1;
                    while j < pe && evs[j].ts - evs[j - 1].ts <= GAP {
                        j += 1;
                    }
                    let day = start.div_euclid(DAY) as i32;
                    let di = (start.div_euclid(DAY) - day0) as usize;
                    let mut rev = 0u64;
                    for e in &evs[s..j] {
                        if e.price > 0 {
                            rev += e.price as u64;
                        }
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
                    if cur.as_ref().is_some_and(|r| r.day != day) {
                        rows.push(finish(cur.take().unwrap(), &mut catsum));
                    }
                    let r = cur.get_or_insert(UserRow { pkey: pk, day, sessions: 0, events: 0, revenue: 0, top: u16::MAX });
                    r.sessions += 1;
                    r.events += (j - s) as u32;
                    r.revenue += rev;
                    for e in &evs[s..j] {
                        if e.price > 0 {
                            catsum[e.cat as usize] += e.price as u64;
                        }
                    }
                    s = j;
                }
                rows.push(finish(cur.take().unwrap(), &mut catsum));
                i = pe;
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
    out::write_all(&outdir, &rows, &cats, &camp_names, &camp_rows, threads);
    tm.lap("write");
    eprintln!("{:>12}: {:8.1} ms", "total", t0.elapsed().as_secs_f64() * 1e3);
    std::process::exit(0);
}

fn finish(mut r: UserRow, catsum: &mut [u64]) -> UserRow {
    let mut best = 0;
    for (i, s) in catsum.iter_mut().enumerate() {
        if *s > best {
            best = *s;
            r.top = i as u16;
        }
        *s = 0;
    }
    r
}
