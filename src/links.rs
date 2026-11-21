use crate::parse::{ANY_INTERNED, cmp_key, key, mix, scan};
use crate::{Input, SendPtr, Timer, par_tasks};
use std::cmp::Ordering;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

/// Maps a user ID key to its person's name key (the byte-wise smallest linked ID).
pub struct Persons {
    table: Vec<[u64; 2]>,
    mask: usize,
}

impl Persons {
    #[inline(always)]
    pub fn prefetch(&self, k: u64) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use std::arch::x86_64::*;
            _mm_prefetch(self.table.as_ptr().add(slot(k, self.mask)) as *const i8, _MM_HINT_T0);
        }
    }
    #[inline]
    pub fn get(&self, k: u64) -> u64 {
        let mut h = slot(k, self.mask);
        loop {
            let e = unsafe { self.table.get_unchecked(h) };
            if e[0] == k {
                return e[1];
            }
            if e[0] == 0 {
                return k;
            }
            h = (h + 1) & self.mask;
        }
    }
}

#[inline(always)]
fn slot(k: u64, mask: usize) -> usize {
    (mix(k) >> 20) as usize & mask
}

fn zeroed<T>(n: usize) -> Vec<T> {
    let v: Vec<u64> = vec![0; n * size_of::<T>() / 8];
    let mut v = std::mem::ManuallyDrop::new(v);
    unsafe { Vec::from_raw_parts(v.as_mut_ptr() as *mut T, n, n) }
}

fn find(p: &[AtomicU32], mut x: u32) -> u32 {
    loop {
        let px = p[x as usize].load(Relaxed);
        if px == x {
            return x;
        }
        let gp = p[px as usize].load(Relaxed);
        if gp != px {
            p[x as usize].store(gp, Relaxed);
        }
        x = gp;
    }
}

const STEP: usize = 1 << 16;

pub fn build(input: &Input, threads: usize, tm: &mut Timer) -> Persons {
    let pairs: Vec<Vec<[u64; 2]>> = par_tasks(input.len(), threads, || (Vec::new(), Vec::new()), |(v, buf), i| {
        scan::<2>(input.read(i, buf), |f| {
            if !f[0].is_empty() && !f[1].is_empty() {
                v.push([key(f[0]), key(f[1])]);
            }
        });
    })
    .into_iter()
    .map(|x| x.0)
    .collect();
    let sides: usize = pairs.iter().map(|v| v.len() * 2).sum();
    let cap = (sides * 3 / 2).next_power_of_two().max(16);
    let mask = cap - 1;
    let keys: Vec<AtomicU64> = zeroed(cap);
    let tasks: Vec<(usize, usize)> =
        pairs.iter().enumerate().flat_map(|(t, v)| (0..v.len()).step_by(STEP).map(move |a| (t, a))).collect();
    let mut pairs = pairs;
    let pp: Vec<(SendPtr<[u64; 2]>, usize)> = pairs.iter_mut().map(|v| (SendPtr(v.as_mut_ptr()), v.len())).collect();
    let insert = |k: u64| -> u64 {
        let mut h = slot(k, mask);
        loop {
            match keys[h].compare_exchange(0, k, Relaxed, Relaxed) {
                Ok(_) => return h as u64,
                Err(cur) if cur == k => return h as u64,
                Err(_) => h = (h + 1) & mask,
            }
        }
    };
    // Tasks touch disjoint ranges of the pair vectors.
    par_tasks(tasks.len(), threads, || (), |_, i| {
        let (t, a) = tasks[i];
        let v = unsafe { std::slice::from_raw_parts_mut(pp[t].0.ptr(), pp[t].1) };
        let end = (a + STEP).min(v.len());
        for p in &mut v[a..end] {
            *p = [insert(p[0]), insert(p[1])];
        }
    });
    tm.lap("links parse");

    let parent: Vec<AtomicU32> = (0..cap as u32).map(AtomicU32::new).collect();
    par_tasks(tasks.len(), threads, || (), |_, i| {
        let (t, a) = tasks[i];
        let v = &pairs[t];
        for p in &v[a..(a + STEP).min(v.len())] {
            let (mut x, mut y) = (p[0] as u32, p[1] as u32);
            loop {
                x = find(&parent, x);
                y = find(&parent, y);
                if x == y {
                    break;
                }
                let (lo, hi) = (x.min(y), x.max(y));
                if parent[hi as usize].compare_exchange(hi, lo, Relaxed, Relaxed).is_ok() {
                    break;
                }
            }
        }
    });
    let best: Vec<AtomicU64> = (0..cap).map(|_| AtomicU64::new(u64::MAX)).collect();
    let nblk = cap.div_ceil(STEP);
    par_tasks(nblk, threads, || (), |_, b| {
        for s in b * STEP..((b + 1) * STEP).min(cap) {
            let k = keys[s].load(Relaxed);
            if k != 0 {
                best[find(&parent, s as u32) as usize].fetch_min(k, Relaxed);
            }
        }
    });
    if ANY_INTERNED.load(Relaxed) {
        let mut b2 = vec![0u64; cap];
        for s in 0..cap {
            let k = keys[s].load(Relaxed);
            if k != 0 {
                let r = find(&parent, s as u32) as usize;
                if b2[r] == 0 || cmp_key(k, b2[r]) == Ordering::Less {
                    b2[r] = k;
                }
            }
        }
        for s in 0..cap {
            if b2[s] != 0 {
                best[s].store(b2[s], Relaxed);
            }
        }
    }
    let mut table: Vec<[u64; 2]> = zeroed(cap);
    let tp = SendPtr(table.as_mut_ptr());
    par_tasks(nblk, threads, || (), |_, b| {
        for s in b * STEP..((b + 1) * STEP).min(cap) {
            let k = keys[s].load(Relaxed);
            if k != 0 {
                unsafe { *tp.ptr().add(s) = [k, best[find(&parent, s as u32) as usize].load(Relaxed)] };
            }
        }
    });
    tm.lap("links uf");
    Persons { table, mask }
}
