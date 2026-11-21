use crate::parse::{DAY, cmp_key, mix};
use crate::{Cred, PCl, PEv, UserRow};
use std::cmp::Ordering;

const GAP: i64 = 1_800_000;
const WEEK: i64 = 7 * DAY;
const ORGANIC: u32 = 0;

pub struct Ctx {
    pub ws: i64,
    pub cws: i64,
    pub day0: i64,
    pub ndays: usize,
    pub ev_bits: u32,
    pub cl_bits: u32,
}

#[derive(Clone, Copy)]
struct SE {
    key: u64,
    price: u32,
    cat: u32,
}

#[derive(Clone, Copy)]
struct SC {
    key: u64,
    ckey: u64,
    camp: u32,
}

/// Dense ids for the person keys of one bucket.
struct Ids {
    keys: Vec<u64>,
    table: Vec<u32>,
    mask: usize,
}

impl Ids {
    fn reset(&mut self, n: usize) {
        let cap = (n / 4).next_power_of_two().max(64);
        self.keys.clear();
        self.table.clear();
        self.table.resize(cap, u32::MAX);
        self.mask = cap - 1;
    }
    #[inline]
    fn find(&self, k: u64) -> (usize, u32) {
        let mut h = (mix(k) as usize) & self.mask;
        loop {
            let t = self.table[h];
            if t == u32::MAX || self.keys[t as usize] == k {
                return (h, t);
            }
            h = (h + 1) & self.mask;
        }
    }
    #[inline]
    fn insert(&mut self, k: u64) -> u32 {
        let (h, t) = self.find(k);
        if t != u32::MAX {
            return t;
        }
        self.table[h] = self.keys.len() as u32;
        self.keys.push(k);
        if self.keys.len() * 2 > self.table.len() {
            let cap = self.table.len() * 2;
            self.table.clear();
            self.table.resize(cap, u32::MAX);
            self.mask = cap - 1;
            for (i, &k) in self.keys.iter().enumerate() {
                let (h, _) = self.find(k);
                self.table[h] = i as u32;
            }
        }
        self.keys.len() as u32 - 1
    }
}

#[derive(Default)]
pub struct Scratch {
    ids: Option<Ids>,
    se: Vec<SE>,
    se2: Vec<SE>,
    sc: Vec<SC>,
    sc2: Vec<SC>,
    starts: Vec<i64>,
    sdays: Vec<u32>,
    run_end: Vec<u32>,
    seen: Vec<u32>,
    ia: Vec<i64>,
    ib: Vec<i64>,
    active: Vec<u32>,
    catsum: Vec<u64>,
    stamp: u32,
}

/// LSD radix sort on `key(x)`, 11-bit digits up to `bits`, skipping digits that are all equal.
fn radix<T: Copy>(a: &mut Vec<T>, tmp: &mut Vec<T>, bits: u32, key: impl Fn(&T) -> u64) {
    const D: usize = 11;
    let npass = (bits as usize).div_ceil(D);
    let mut hist = vec![[0u32; 1 << D]; npass];
    for x in a.iter() {
        let k = key(x);
        for (p, h) in hist.iter_mut().enumerate() {
            h[((k >> (p * D)) & ((1 << D) - 1)) as usize] += 1;
        }
    }
    let n = a.len();
    for (p, h) in hist.iter_mut().enumerate() {
        if h.iter().any(|&c| c as usize == n) {
            continue;
        }
        let mut sum = 0;
        for c in h.iter_mut() {
            let v = *c;
            *c = sum;
            sum += v;
        }
        tmp.clear();
        tmp.reserve(n);
        let out = tmp.spare_capacity_mut();
        for x in a.iter() {
            let d = ((key(x) >> (p * D)) & ((1 << D) - 1)) as usize;
            out[h[d] as usize].write(*x);
            h[d] += 1;
        }
        unsafe { tmp.set_len(n) };
        std::mem::swap(a, tmp);
    }
}

fn bits_for(v: u64) -> u32 {
    64 - v.leading_zeros()
}

pub fn process(
    ctx: &Ctx,
    evs: &[&[PEv]],
    cls: &[&[PCl]],
    s: &mut Scratch,
    ncamp: usize,
    ncat: usize,
    rows: &mut Vec<UserRow>,
    cr: &mut [Cred],
    asst: &mut [u32],
) {
    let n: usize = evs.iter().map(|v| v.len()).sum();
    if n == 0 {
        return;
    }
    let ids = s.ids.get_or_insert_with(|| Ids { keys: vec![], table: vec![], mask: 0 });
    ids.reset(n);
    s.se.clear();
    s.se.reserve(n);
    for v in evs {
        for e in v.iter() {
            let pid = ids.insert(e.pkey) as u64;
            s.se.push(SE { key: pid << ctx.ev_bits | (e.ts - ctx.ws) as u64, price: e.price, cat: e.cat });
        }
    }
    let pid_bits = bits_for(ids.keys.len() as u64);
    radix(&mut s.se, &mut s.se2, ctx.ev_bits + pid_bits, |x| x.key);
    s.sc.clear();
    for v in cls {
        for c in v.iter() {
            let (_, t) = ids.find(c.pkey);
            if t != u32::MAX {
                s.sc.push(SC { key: (t as u64) << ctx.cl_bits | (c.ts - ctx.cws) as u64, ckey: c.ckey, camp: c.camp });
            }
        }
    }
    radix(&mut s.sc, &mut s.sc2, ctx.cl_bits + pid_bits, |x| x.key);
    if s.seen.len() != ncamp {
        s.seen = vec![0; ncamp];
        s.ia = vec![0; ncamp];
        s.ib = vec![0; ncamp];
        s.catsum = vec![0; ncat];
    }

    let (ws, cws, nd) = (ctx.ws, ctx.cws, ctx.ndays);
    let em = (1u64 << ctx.ev_bits) - 1;
    let cm = (1u64 << ctx.cl_bits) - 1;
    let ets = |x: &SE| ws + (x.key & em) as i64;
    let cts = |x: &SC| cws + (x.key & cm) as i64;
    let (se, sc) = (&s.se, &s.sc);
    let mut ci = 0;
    let mut i = 0;
    while i < se.len() {
        let pid = se[i].key >> ctx.ev_bits;
        let mut pe = i + 1;
        while pe < se.len() && se[pe].key >> ctx.ev_bits == pid {
            pe += 1;
        }
        while ci < sc.len() && sc[ci].key >> ctx.cl_bits < pid {
            ci += 1;
        }
        let mut ce = ci;
        while ce < sc.len() && sc[ce].key >> ctx.cl_bits == pid {
            ce += 1;
        }
        let ev = &se[i..pe];
        let cl = &sc[ci..ce];
        let pkey = ids.keys[pid as usize];
        s.starts.clear();
        s.sdays.clear();
        let (mut lo, mut hi) = (0, 0);
        let mut row = UserRow { pkey, day: i32::MIN, sessions: 0, events: 0, revenue: 0, top: u32::MAX };
        let mut a = 0;
        while a < ev.len() {
            let start = ets(&ev[a]);
            let mut j = a + 1;
            let mut prev = start;
            let mut rev = ev[a].price as u64;
            while j < ev.len() {
                let t = ets(&ev[j]);
                if t - prev > GAP {
                    break;
                }
                prev = t;
                rev += ev[j].price as u64;
                j += 1;
            }
            let day = start.div_euclid(DAY);
            let di = (day - ctx.day0) as usize;
            while hi < cl.len() && cts(&cl[hi]) <= start {
                hi += 1;
            }
            while lo < hi && cts(&cl[lo]) < start - WEEK {
                lo += 1;
            }
            let camp = if hi > lo {
                let t = cts(&cl[hi - 1]);
                let mut best = hi - 1;
                let mut k = hi - 1;
                while k > lo && cts(&cl[k - 1]) == t {
                    k -= 1;
                    if cmp_key(cl[k].ckey, cl[best].ckey) == Ordering::Less {
                        best = k;
                    }
                }
                cl[best].camp
            } else {
                ORGANIC
            };
            let c = &mut cr[camp as usize * nd + di];
            c.sessions += 1;
            c.revenue += rev;
            c.purchasing += (rev > 0) as u32;
            s.starts.push(start);
            s.sdays.push(di as u32);
            let day = day as i32;
            if row.day != day {
                if row.day != i32::MIN {
                    rows.push(finish(row, &mut s.catsum));
                }
                row = UserRow { pkey, day, sessions: 0, events: 0, revenue: 0, top: u32::MAX };
            }
            if rev > 0 {
                for e in &ev[a..j] {
                    s.catsum[e.cat as usize] += e.price as u64;
                }
            }
            row.sessions += 1;
            row.events += (j - a) as u32;
            row.revenue += rev;
            a = j;
        }
        rows.push(finish(row, &mut s.catsum));

        // Assists: campaign c assists every session whose start lies in the union of [t, t + 7d]
        // over c's clicks t; each merged interval adds its sessions per day.
        if !cl.is_empty() {
            let ns = s.starts.len();
            s.run_end.clear();
            s.run_end.resize(ns, ns as u32);
            for k in (0..ns.saturating_sub(1)).rev() {
                s.run_end[k] = if s.sdays[k] == s.sdays[k + 1] { s.run_end[k + 1] } else { k as u32 + 1 };
            }
            s.stamp = s.stamp.wrapping_add(1);
            if s.stamp == 0 {
                s.seen.fill(0);
                s.stamp = 1;
            }
            s.active.clear();
            let flush = |c: u32, a: i64, b: i64, asst: &mut [u32], st: &[i64], days: &[u32], re: &[u32]| {
                let mut i = st.partition_point(|&x| x < a);
                let j = st.partition_point(|&x| x <= b);
                while i < j {
                    let k = (re[i] as usize).min(j);
                    asst[c as usize * nd + days[i] as usize] += (k - i) as u32;
                    i = k;
                }
            };
            for k in cl {
                let (c, t) = (k.camp as usize, cts(k));
                if s.seen[c] != s.stamp {
                    s.seen[c] = s.stamp;
                    s.ia[c] = t;
                    s.ib[c] = t + WEEK;
                    s.active.push(c as u32);
                } else if t > s.ib[c] {
                    flush(c as u32, s.ia[c], s.ib[c], asst, &s.starts, &s.sdays, &s.run_end);
                    s.ia[c] = t;
                    s.ib[c] = t + WEEK;
                } else {
                    s.ib[c] = t + WEEK;
                }
            }
            for &c in &s.active {
                flush(c, s.ia[c as usize], s.ib[c as usize], asst, &s.starts, &s.sdays, &s.run_end);
            }
        }
        i = pe;
        ci = ce;
    }
}

fn finish(mut r: UserRow, catsum: &mut [u64]) -> UserRow {
    if r.revenue == 0 {
        return r;
    }
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
