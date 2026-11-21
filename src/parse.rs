use rustc_hash::FxHashMap;
use std::cmp::Ordering;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

pub const DAY: i64 = 86_400_000;

#[inline(always)]
fn masks(data: &[u8], base: usize) -> (u64, u64, u64) {
    let mut tmp = [0u8; 64];
    let p = if base + 64 <= data.len() {
        unsafe { data.as_ptr().add(base) }
    } else {
        tmp[..data.len() - base].copy_from_slice(&data[base..]);
        tmp.as_ptr()
    };
    #[cfg(target_feature = "avx2")]
    unsafe {
        use std::arch::x86_64::*;
        let a = _mm256_loadu_si256(p as *const __m256i);
        let b = _mm256_loadu_si256(p.add(32) as *const __m256i);
        let m = |c: i8| {
            let s = _mm256_set1_epi8(c);
            let lo = _mm256_movemask_epi8(_mm256_cmpeq_epi8(a, s)) as u32 as u64;
            let hi = _mm256_movemask_epi8(_mm256_cmpeq_epi8(b, s)) as u32 as u64;
            lo | (hi << 32)
        };
        (m(b'\n' as i8), m(b',' as i8), m(b'"' as i8))
    }
    #[cfg(not(target_feature = "avx2"))]
    {
        let s = unsafe { std::slice::from_raw_parts(p, 64) };
        let (mut n, mut c, mut q) = (0u64, 0u64, 0u64);
        for (i, &x) in s.iter().enumerate() {
            n |= ((x == b'\n') as u64) << i;
            c |= ((x == b',') as u64) << i;
            q |= ((x == b'"') as u64) << i;
        }
        (n, c, q)
    }
}

/// One well-formed line: field i is `data[st[i]..en[i]]` (quotes already stripped). Keeping
/// `data` lets the parsers read 16 bytes past a field without leaving the buffer.
pub struct Line<'a, const N: usize> {
    data: &'a [u8],
    st: [usize; N],
    en: [usize; N],
}

impl<'a, const N: usize> Line<'a, N> {
    #[inline(always)]
    pub fn get(&self, i: usize) -> &'a [u8] {
        unsafe { self.data.get_unchecked(self.st[i]..self.en[i]) }
    }
    #[inline(always)]
    pub fn is_empty(&self, i: usize) -> bool {
        self.st[i] == self.en[i]
    }
    #[inline(always)]
    pub fn key(&self, i: usize) -> u64 {
        let (s, e) = (self.st[i], self.en[i]);
        if e - s <= 10 && s + 16 <= self.data.len() {
            if let Some(k) = unsafe { pack16(self.data.as_ptr().add(s), e - s) } {
                return k;
            }
        }
        key(self.get(i))
    }
    /// The packed key of field i if it packs (never interns).
    #[inline(always)]
    pub fn packed(&self, i: usize) -> Option<u64> {
        let (s, e) = (self.st[i], self.en[i]);
        if e - s <= 10 && s + 16 <= self.data.len() {
            return unsafe { pack16(self.data.as_ptr().add(s), e - s) };
        }
        pack(self.get(i))
    }
    #[inline(always)]
    pub fn ts(&self, i: usize) -> Option<i64> {
        let (s, e) = (self.st[i], self.en[i]);
        let l = e - s;
        if l > 0 && l <= 16 && e >= 16 && !(l == 20 || l == 25) {
            return unsafe { digits_end(self.data.as_ptr().add(e - 16), l) }.map(|v| v as i64);
        }
        parse_ts(self.get(i))
    }
}

/// Calls `f` with every well-formed line of `data` that has N fields (rules' dialect: only `\n`
/// ends a line, quotes only around a whole field and stripped). Malformed lines are skipped.
#[cfg_attr(not(test), allow(dead_code))]
pub fn scan_blocks<const N: usize>(data: &[u8], mut f: impl FnMut(&Line<N>)) {
    let len = data.len();
    let mut ls = 0usize;
    let mut nf = 0usize;
    let mut ends = [0usize; 8];
    let mut quotes = 0u32;
    let mut base = 0usize;
    // Branch-free field splitting: which fields are quoted is random, so branches would mispredict.
    let mut emit = |ls: usize, ends: &[usize; 8], mut q: u32| {
        let mut line = Line { data, st: [0; N], en: [0; N] };
        let mut s = ls;
        for i in 0..N {
            let e = ends[i];
            let l = e - s;
            let first = *data.get(s).unwrap_or(&0);
            let last = *data.get(e.wrapping_sub(1)).unwrap_or(&0);
            let st = ((first == b'"') & (last == b'"') & (l >= 2)) as usize;
            q = q.wrapping_sub(2 * st as u32);
            line.st[i] = s + st;
            line.en[i] = e - st;
            s = e + 1;
        }
        if q == 0 {
            f(&line);
        }
    };
    while base < len {
        let (nl, cm, qt) = masks(data, base);
        let mut bits = nl | cm;
        let mut qb = qt;
        while bits != 0 {
            let tz = bits.trailing_zeros();
            let pos = base + tz as usize;
            ends[nf & 7] = pos;
            nf += 1;
            if (nl >> tz) & 1 != 0 {
                let below = (1u64 << tz) - 1;
                let q = quotes + (qb & below).count_ones();
                qb &= !below;
                if nf == N {
                    emit(ls, &ends, q);
                }
                ls = pos + 1;
                nf = 0;
                quotes = 0;
            }
            bits &= bits - 1;
        }
        quotes += qb.count_ones();
        base += 64;
    }
    if ls < len && nf == N - 1 {
        ends[N - 1] = len;
        emit(ls, &ends, quotes);
    }
}

/// Emits line `data[ls..le]` if it has N fields and legal quotes; field ends in `ends` (absolute).
#[inline(always)]
fn emit_line<const N: usize>(data: &[u8], ls: usize, ends: &[usize; N], mut q: u32, f: &mut impl FnMut(&Line<N>)) {
    let mut line = Line { data, st: [0; N], en: [0; N] };
    let mut s = ls;
    for i in 0..N {
        let e = ends[i];
        let l = e - s;
        let first = unsafe { *data.get_unchecked(s.min(data.len() - 1)) };
        let last = unsafe { *data.get_unchecked(e.wrapping_sub(1).min(data.len() - 1)) };
        let st = ((first == b'"') & (last == b'"') & (l >= 2)) as usize;
        q = q.wrapping_sub(2 * st as u32);
        line.st[i] = s + st;
        line.en[i] = e - st;
        s = e + 1;
    }
    if q == 0 {
        f(&line);
    }
}

const NLF: u32 = 1 << 31;

thread_local! {
    static SCRATCH: std::cell::Cell<(Vec<u32>, Vec<u32>, Vec<u64>)> = const { std::cell::Cell::new((Vec::new(), Vec::new(), Vec::new())) };
}

/// Calls `f` with every well-formed line of `data` that has N fields (rules' dialect: only `\n`
/// ends a line, quotes only around a whole field and stripped). Malformed lines are skipped.
/// Pass 1 flattens all comma/newline positions (no dependency between blocks); pass 2 walks lines
/// from that list, so consecutive lines overlap in the pipeline.
#[inline(always)]
pub fn scan<const N: usize>(data: &[u8], mut f: impl FnMut(&Line<N>)) {
    let len = data.len();
    if len == 0 {
        return;
    }
    assert!(len < NLF as usize);
    let (mut pos, mut cq, mut qb) = SCRATCH.take();
    let nblk = len.div_ceil(64);
    pos.clear();
    pos.reserve(len + 64 + 2 * N);
    cq.clear();
    cq.reserve(nblk + 1);
    qb.clear();
    qb.reserve(nblk + 1);
    let mut n = 0usize;
    let mut quotes = 0u32;
    unsafe {
        let pp = pos.as_mut_ptr();
        let cp = cq.as_mut_ptr();
        let qp = qb.as_mut_ptr();
        for blk in 0..nblk {
            let base = blk * 64;
            let (nl, cm, qt) = masks(data, base);
            *cp.add(blk) = quotes;
            *qp.add(blk) = qt;
            quotes += qt.count_ones();
            let mut st = nl | cm;
            let cnt = st.count_ones() as usize;
            let out = pp.add(n);
            for j in 0..8 {
                let tz = st.trailing_zeros();
                *out.add(j) = (base as u32 + tz) | ((nl.wrapping_shr(tz) as u32 & 1) << 31);
                st &= st.wrapping_sub(1);
            }
            let mut j = 8;
            while st != 0 {
                let tz = st.trailing_zeros();
                *out.add(j) = (base as u32 + tz) | ((nl.wrapping_shr(tz) as u32 & 1) << 31);
                st &= st - 1;
                j += 1;
            }
            n += cnt;
        }
        *cp.add(nblk) = quotes;
        // A last line without '\n' ends at len; padding keeps reads of N entries in bounds.
        if *data.get_unchecked(len - 1) != b'\n' {
            *pp.add(n) = len as u32 | NLF;
            n += 1;
        }
        for j in 0..N {
            *pp.add(n + j) = len as u32 | NLF;
        }
        pos.set_len(n + N);
        *qp.add(nblk) = 0;
        cq.set_len(nblk + 1);
        qb.set_len(nblk + 1);
    }
    let qbefore = |x: usize| -> u32 {
        let b = x >> 6;
        let o = x & 63;
        unsafe { *cq.get_unchecked(b) + (*qb.get_unchecked(b) & ((1u64 << o) - 1)).count_ones() }
    };
    let mut k = 0;
    let mut ls = 0usize;
    let mut qs = 0u32;
    while k < n {
        let e: [u32; N] = std::array::from_fn(|i| unsafe { *pos.get_unchecked(k + i) });
        let mut early = 0;
        for x in &e[..N - 1] {
            early |= *x;
        }
        if early & NLF == 0 && e[N - 1] & NLF != 0 {
            let ends: [usize; N] = std::array::from_fn(|i| (e[i] & !NLF) as usize);
            let le = ends[N - 1];
            let qe = qbefore(le);
            emit_line(data, ls, &ends, qe - qs, &mut f);
            qs = qe;
            ls = le + 1;
            k += N;
        } else {
            while unsafe { *pos.get_unchecked(k) } & NLF == 0 {
                k += 1;
            }
            let le = (pos[k] & !NLF) as usize;
            qs = qbefore(le);
            ls = le + 1;
            k += 1;
        }
    }
    SCRATCH.set((pos, cq, qb));
}

/// 6-bit packing of `l` (<= 10) bytes at p, reading 16 bytes; None if a byte is outside [0-9A-Z_a-z].
#[cfg(target_feature = "avx2")]
#[inline(always)]
unsafe fn pack16(p: *const u8, l: usize) -> Option<u64> {
    use std::arch::x86_64::*;
    unsafe {
        let v = _mm_loadu_si128(p as *const __m128i);
        let inlen = _mm_cmpgt_epi8(_mm_set1_epi8(l as i8), _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15));
        let range = |lo: u8, hi: u8| _mm_and_si128(_mm_cmpgt_epi8(v, _mm_set1_epi8(lo as i8 - 1)), _mm_cmpgt_epi8(_mm_set1_epi8(hi as i8 + 1), v));
        let dig = range(b'0', b'9');
        let up = range(b'A', b'Z');
        let us = _mm_cmpeq_epi8(v, _mm_set1_epi8(b'_' as i8));
        let lo = range(b'a', b'z');
        let valid = _mm_or_si128(_mm_or_si128(dig, up), _mm_or_si128(us, lo));
        if _mm_movemask_epi8(_mm_andnot_si128(valid, inlen)) != 0 {
            return None;
        }
        let off = _mm_or_si128(
            _mm_or_si128(_mm_and_si128(dig, _mm_set1_epi8(47)), _mm_and_si128(up, _mm_set1_epi8(54))),
            _mm_or_si128(_mm_and_si128(us, _mm_set1_epi8(58)), _mm_and_si128(lo, _mm_set1_epi8(59))),
        );
        let code = _mm_and_si128(_mm_sub_epi8(v, off), inlen);
        let m1 = _mm_maddubs_epi16(code, _mm_set1_epi16(0x0140));
        let m2 = _mm_madd_epi16(m1, _mm_set1_epi32(0x0001_1000));
        let q0 = _mm_cvtsi128_si32(m2) as u32 as u64;
        let q1 = _mm_extract_epi32(m2, 1) as u32 as u64;
        let q2 = _mm_extract_epi32(m2, 2) as u32 as u64;
        Some(q0 << 36 | q1 << 12 | q2 >> 12)
    }
}

#[cfg(not(target_feature = "avx2"))]
unsafe fn pack16(p: *const u8, l: usize) -> Option<u64> {
    pack(unsafe { std::slice::from_raw_parts(p, l) })
}

/// Digits-only value of the `l` (<= 16) bytes ending at p + 16, reading 16 bytes from p.
#[cfg(target_feature = "avx2")]
#[inline(always)]
unsafe fn digits_end(p: *const u8, l: usize) -> Option<u64> {
    use std::arch::x86_64::*;
    unsafe {
        let v = _mm_loadu_si128(p as *const __m128i);
        let keep = _mm_cmpgt_epi8(_mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15), _mm_set1_epi8(15 - l as i8));
        let d = _mm_and_si128(_mm_sub_epi8(v, _mm_set1_epi8(b'0' as i8)), keep);
        if _mm_movemask_epi8(_mm_cmpeq_epi8(_mm_min_epu8(d, _mm_set1_epi8(9)), d)) != 0xFFFF {
            return None;
        }
        let m1 = _mm_maddubs_epi16(d, _mm_set1_epi16(0x010A));
        let m2 = _mm_madd_epi16(m1, _mm_set1_epi32(0x0001_0064));
        let m3 = _mm_packus_epi32(m2, m2);
        let m4 = _mm_madd_epi16(m3, _mm_set1_epi32(0x0001_2710));
        let r = _mm_cvtsi128_si64(m4) as u64;
        Some((r & 0xFFFF_FFFF) * 100_000_000 + (r >> 32))
    }
}

#[cfg(not(target_feature = "avx2"))]
unsafe fn digits_end(p: *const u8, l: usize) -> Option<u64> {
    digits16(unsafe { std::slice::from_raw_parts(p.add(16 - l), l) })
}

#[inline(always)]
fn dg(c: u8) -> u32 {
    c.wrapping_sub(b'0') as u32
}

const CUM: [u32; 13] = [0, 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];

/// Days since 1970-01-01 for a valid proleptic Gregorian date with 0 <= y <= 9999.
#[inline(always)]
pub fn days_from_civil(y: u32, m: u32, d: u32) -> i64 {
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let yd = y as i64 - 1;
    let before = 365 * y as i64 + yd.div_euclid(4) - yd.div_euclid(100) + yd.div_euclid(400) + 1;
    before - 719_528 + (CUM[m as usize] + (m > 2 && leap) as u32 + d - 1) as i64
}

/// Epoch ms for F1/F2/F3; None for any bad form. An F3 overflowing i64 is None too (it is
/// outside every window anyway).
#[inline]
pub fn parse_ts(s: &[u8]) -> Option<i64> {
    let l = s.len();
    if (l == 20 || l == 25) && s[4] == b'-' {
        return iso(s);
    }
    if l == 0 {
        return None;
    }
    if l <= 16 {
        return digits16(s).map(|v| v as i64);
    }
    let mut v: i64 = 0;
    for &c in s {
        let d = dg(c);
        if d >= 10 {
            return None;
        }
        v = v.checked_mul(10)?.checked_add(d as i64)?;
    }
    Some(v)
}

#[cfg(target_feature = "avx2")]
#[inline(always)]
fn digits16(s: &[u8]) -> Option<u64> {
    use std::arch::x86_64::*;
    let mut b = [b'0'; 16];
    b[16 - s.len()..].copy_from_slice(s);
    unsafe {
        let d = _mm_sub_epi8(_mm_loadu_si128(b.as_ptr() as *const __m128i), _mm_set1_epi8(b'0' as i8));
        if _mm_movemask_epi8(_mm_cmpeq_epi8(_mm_min_epu8(d, _mm_set1_epi8(9)), d)) != 0xFFFF {
            return None;
        }
        let m1 = _mm_maddubs_epi16(d, _mm_set1_epi16(0x010A));
        let m2 = _mm_madd_epi16(m1, _mm_set1_epi32(0x0001_0064));
        let m3 = _mm_packus_epi32(m2, m2);
        let m4 = _mm_madd_epi16(m3, _mm_set1_epi32(0x0001_2710));
        let r = _mm_cvtsi128_si64(m4) as u64;
        Some((r & 0xFFFF_FFFF) * 100_000_000 + (r >> 32))
    }
}

#[cfg(not(target_feature = "avx2"))]
fn digits16(s: &[u8]) -> Option<u64> {
    let mut v = 0u64;
    for &c in s {
        let d = dg(c);
        if d >= 10 {
            return None;
        }
        v = v * 10 + d as u64;
    }
    Some(v)
}

#[inline(always)]
fn civil_ms(y: u32, mo: u32, d: u32, h: u32, mi: u32, se: u32) -> Option<i64> {
    if mo.wrapping_sub(1) >= 12 || d == 0 || h > 23 || mi > 59 || se > 59 {
        return None;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let dim = match mo {
        2 => 28 + leap as u32,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if d > dim {
        return None;
    }
    Some((days_from_civil(y, mo, d) * 86_400 + (h * 3600 + mi * 60 + se) as i64) * 1000)
}

/// Valid dates near the window: slot (month parity, day digits) -> ("YYYY-MM-", "DD", epoch ms of
/// midnight). Filled once before parsing starts; dates not in it take the full path.
static DATES: [[AtomicU64; 2]; 128] = [const { [AtomicU64::new(0), AtomicU64::new(0)] }; 128];

#[inline(always)]
fn date_slot(p6: u8, p8: u8, p9: u8) -> usize {
    ((p6 as usize & 1) << 6) | ((p8 as usize & 3) << 4) | (p9 as usize & 15)
}

/// Registers every date from day `d0` to day `d1` (days since epoch) in the date table.
pub fn set_dates(d0: i64, d1: i64) {
    for d in d0..=d1 {
        let (y, m, dd) = civil_from_days(d);
        if !(0..=9999).contains(&y) {
            continue;
        }
        let txt = format!("{y:04}-{m:02}-{dd:02}");
        let b = txt.as_bytes();
        let slot = &DATES[date_slot(b[6], b[8], b[9])];
        if slot[0].load(Relaxed) != 0 {
            continue;
        }
        let k = u64::from_le_bytes(b[..8].try_into().unwrap());
        let v = (u16::from_le_bytes([b[8], b[9]]) as u64) << 48 | (d * DAY - MIN_DATE_MS) as u64;
        slot[1].store(v, Relaxed);
        slot[0].store(k, std::sync::atomic::Ordering::Release);
    }
}

const MIN_DATE_MS: i64 = -62_167_219_200_000;

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Seconds of day for "HH:MM:SS" at p (8 bytes), validated with SWAR.
#[inline(always)]
unsafe fn hms(p: *const u8) -> Option<u32> {
    let x = unsafe { (p as *const u64).read_unaligned() };
    const CM: u64 = 0x0000_FF00_00FF_0000;
    if x & CM != 0x0000_3A00_003A_0000 {
        return None;
    }
    let y = (x & !CM) | 0x0000_3000_0030_0000;
    const HI: u64 = 0xF0F0_F0F0_F0F0_F0F0;
    if (y & HI) != 0x3030_3030_3030_3030 || (y.wrapping_add(0x0606_0606_0606_0606) & HI) != 0x3030_3030_3030_3030 {
        return None;
    }
    let d = y - 0x3030_3030_3030_3030;
    let t = d * 10 + (d >> 8);
    let (h, m, s) = ((t & 0xFF) as u32, ((t >> 24) & 0xFF) as u32, ((t >> 48) & 0xFF) as u32);
    if h > 23 || m > 59 || s > 59 {
        return None;
    }
    Some(h * 3600 + m * 60 + s)
}

/// F1/F2 via the date table; None means "not decided here" (fall back to the full parser).
#[inline(always)]
fn iso_fast(s: &[u8]) -> Option<Option<i64>> {
    let p = s.as_ptr();
    let e = &DATES[date_slot(s[6], s[8], s[9])];
    let k = unsafe { (p as *const u64).read_unaligned() };
    let dd = unsafe { (p.add(8) as *const u16).read_unaligned() };
    let ek = e[0].load(std::sync::atomic::Ordering::Acquire);
    let v = e[1].load(Relaxed);
    if ek != k || (v >> 48) as u16 != dd {
        return None;
    }
    let base = (v & 0xFFFF_FFFF_FFFF) as i64 + MIN_DATE_MS;
    if s[10] != b'T' {
        return Some(None);
    }
    let Some(sod) = (unsafe { hms(p.add(11)) }) else { return Some(None) };
    let t = base + sod as i64 * 1000;
    if s.len() == 20 {
        return Some((s[19] == b'Z').then_some(t));
    }
    let sign = match s[19] {
        b'+' => 1,
        b'-' => -1,
        _ => return Some(None),
    };
    let (a, b, c, d) = (dg(s[20]), dg(s[21]), dg(s[23]), dg(s[24]));
    if s[22] != b':' || a >= 10 || b >= 10 || c >= 10 || d >= 10 {
        return Some(None);
    }
    let (oh, om) = (a * 10 + b, c * 10 + d);
    if oh > 23 || om > 59 {
        return Some(None);
    }
    Some(Some(t - sign * (oh * 60 + om) as i64 * 60_000))
}

/// F1 (20 bytes) or F2 (25 bytes): every fixed character and digit checked with two 16-byte compares.
#[cfg(target_feature = "avx2")]
#[inline(always)]
fn iso(s: &[u8]) -> Option<i64> {
    if let Some(r) = iso_fast(s) {
        return r;
    }
    iso_full(s)
}

#[cfg(target_feature = "avx2")]
#[inline(never)]
fn iso_full(s: &[u8]) -> Option<i64> {
    use std::arch::x86_64::*;
    let f2 = s.len() == 25;
    unsafe {
        let v0 = _mm_loadu_si128(s.as_ptr() as *const __m128i);
        let v1 = _mm_loadu_si128(s.as_ptr().add(s.len() - 16) as *const __m128i);
        let z = _mm_set1_epi8(b'0' as i8);
        let nine = _mm_set1_epi8(9);
        let d0 = _mm_sub_epi8(v0, z);
        let d1 = _mm_sub_epi8(v1, z);
        let dig0 = _mm_movemask_epi8(_mm_cmpeq_epi8(_mm_min_epu8(d0, nine), d0)) as u32;
        let dig1 = _mm_movemask_epi8(_mm_cmpeq_epi8(_mm_min_epu8(d1, nine), d1)) as u32;
        const P0: [u8; 16] = *b"0000-00-00T00:00";
        let p0 = _mm_movemask_epi8(_mm_cmpeq_epi8(v0, _mm_loadu_si128(P0.as_ptr() as *const __m128i))) as u32;
        // lanes: digits 0-3,5,6,8,9,11,12,14,15; fixed 4,7,10,13
        if dig0 & 0xDB6F != 0xDB6F || p0 & 0x2490 != 0x2490 {
            return None;
        }
        let (sec_at, off) = if !f2 {
            // v1 = s[4..20]: ':' lane 12, digits 13,14, 'Z' lane 15
            if dig1 & 0x6000 != 0x6000 || s[16] != b':' || s[19] != b'Z' {
                return None;
            }
            (17, 0i64)
        } else {
            // v1 = s[9..25]: ':' lane 7, digits 8,9, sign 10, digits 11,12, ':' 13, digits 14,15
            if dig1 & 0xDB00 != 0xDB00 || s[16] != b':' || s[22] != b':' {
                return None;
            }
            let sign = match s[19] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let oh = dg(s[20]) * 10 + dg(s[21]);
            let om = dg(s[23]) * 10 + dg(s[24]);
            if oh > 23 || om > 59 {
                return None;
            }
            (17, sign * (oh * 60 + om) as i64 * 60_000)
        };
        let pairs = _mm_shuffle_epi8(d0, _mm_setr_epi8(0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, -1, -1, -1, -1));
        let m = _mm_maddubs_epi16(pairs, _mm_set1_epi16(0x010A));
        let mut w = [0u16; 8];
        _mm_storeu_si128(w.as_mut_ptr() as *mut __m128i, m);
        let se = dg(s[sec_at]) * 10 + dg(s[sec_at + 1]);
        let y = w[0] as u32 * 100 + w[1] as u32;
        Some(civil_ms(y, w[2] as u32, w[3] as u32, w[4] as u32, w[5] as u32, se)? - off)
    }
}

#[cfg(not(target_feature = "avx2"))]
fn iso(s: &[u8]) -> Option<i64> {
    parse_iso_ref(s)
}

/// Epoch ms for F1/F2/F3; None for any bad form. An F3 overflowing i64 is None too (it is
/// outside every window anyway).
#[cfg_attr(not(test), allow(dead_code))]
pub fn parse_ts_ref(s: &[u8]) -> Option<i64> {
    let l = s.len();
    if l == 20 || l == 25 {
        if s[4] == b'-' {
            return parse_iso_ref(s);
        }
    }
    if l == 0 {
        return None;
    }
    let mut v: i64 = 0;
    if l <= 18 {
        for &c in s {
            let d = dg(c);
            if d >= 10 {
                return None;
            }
            v = v * 10 + d as i64;
        }
        return Some(v);
    }
    for &c in s {
        let d = dg(c);
        if d >= 10 {
            return None;
        }
        v = v.checked_mul(10)?.checked_add(d as i64)?;
    }
    Some(v)
}

fn parse_iso_ref(s: &[u8]) -> Option<i64> {
    let ds = [dg(s[0]), dg(s[1]), dg(s[2]), dg(s[3]), dg(s[5]), dg(s[6]), dg(s[8]), dg(s[9]), dg(s[11]), dg(s[12]), dg(s[14]), dg(s[15]), dg(s[17]), dg(s[18])];
    let mut bad = (s[7] != b'-') | (s[10] != b'T') | (s[13] != b':') | (s[16] != b':');
    for d in ds {
        bad |= d >= 10;
    }
    if bad {
        return None;
    }
    let y = ds[0] * 1000 + ds[1] * 100 + ds[2] * 10 + ds[3];
    let mo = ds[4] * 10 + ds[5];
    let d = ds[6] * 10 + ds[7];
    let h = ds[8] * 10 + ds[9];
    let mi = ds[10] * 10 + ds[11];
    let se = ds[12] * 10 + ds[13];
    if mo.wrapping_sub(1) >= 12 || d == 0 || h > 23 || mi > 59 || se > 59 {
        return None;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let dim = match mo {
        2 => 28 + leap as u32,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if d > dim {
        return None;
    }
    let mut off = 0i64;
    if s.len() == 20 {
        if s[19] != b'Z' {
            return None;
        }
    } else {
        let sign = match s[19] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let (a, b, c, e) = (dg(s[20]), dg(s[21]), dg(s[23]), dg(s[24]));
        if s[22] != b':' || a >= 10 || b >= 10 || c >= 10 || e >= 10 {
            return None;
        }
        let (oh, om) = (a * 10 + b, c * 10 + e);
        if oh > 23 || om > 59 {
            return None;
        }
        off = sign * (oh * 60 + om) as i64 * 60_000;
    }
    Some((days_from_civil(y, mo, d) * 86_400 + (h * 3600 + mi * 60 + se) as i64) * 1000 - off)
}

/// Digits-only price in 1..=10_000_000.
#[inline]
pub fn parse_price(s: &[u8]) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for &c in s {
        let d = dg(c);
        if d >= 10 {
            return None;
        }
        v = v * 10 + d as u64;
        if v > 10_000_000 {
            return None;
        }
    }
    if v == 0 { None } else { Some(v as u32) }
}

const fn code_table() -> [u8; 256] {
    let mut t = [0x80u8; 256];
    let mut c = 0;
    while c < 256 {
        let b = c as u8;
        t[c] = if b.is_ascii_digit() {
            b - b'0' + 1
        } else if b.is_ascii_uppercase() {
            b - b'A' + 11
        } else if b == b'_' {
            37
        } else if b.is_ascii_lowercase() {
            b - b'a' + 38
        } else {
            0x80
        };
        c += 1;
    }
    t
}
static CODE: [u8; 256] = code_table();
const ALPHA: &[u8; 64] = b"\x000123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz";
pub const INTERNED: u64 = 1 << 63;

static LONG: Mutex<Option<(FxHashMap<Vec<u8>, u64>, Vec<Vec<u8>>)>> = Mutex::new(None);
pub static ANY_INTERNED: AtomicBool = AtomicBool::new(false);

/// Packs up to 10 chars of [0-9A-Z_a-z] into 6-bit order-preserving codes (0 = padding), so
/// u64 order equals byte order. None if the string does not fit.
#[inline(always)]
fn pack(b: &[u8]) -> Option<u64> {
    if b.len() > 10 {
        return None;
    }
    let mut v = 0u64;
    let mut bad = 0u8;
    for &c in b {
        let t = CODE[c as usize];
        bad |= t;
        v = (v << 6) | (t & 63) as u64;
    }
    if bad & 0x80 != 0 {
        return None;
    }
    Some(v << (6 * (10 - b.len())))
}

/// An exact u64 key for a string. Strings that do not pack get an interned id with the top bit set.
#[inline]
pub fn key(b: &[u8]) -> u64 {
    match pack(b) {
        Some(v) => v,
        None => intern(b),
    }
}

#[cold]
fn intern(b: &[u8]) -> u64 {
    ANY_INTERNED.store(true, Relaxed);
    let mut g = LONG.lock().unwrap();
    let (map, list) = g.get_or_insert_with(Default::default);
    *map.entry(b.to_vec()).or_insert_with(|| {
        list.push(b.to_vec());
        INTERNED | (list.len() as u64 - 1)
    })
}

pub fn key_bytes(k: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    key_append(k, &mut out);
    out
}

/// Appends the string behind key k to `out`.
#[inline]
pub fn key_append(k: u64, out: &mut Vec<u8>) {
    if k & INTERNED != 0 {
        let g = LONG.lock().unwrap();
        out.extend_from_slice(&g.as_ref().unwrap().1[(k & !INTERNED) as usize]);
        return;
    }
    for i in 0..10 {
        let c = (k >> (6 * (9 - i))) & 63;
        if c == 0 {
            break;
        }
        out.push(ALPHA[c as usize]);
    }
}

/// Byte-wise order of the strings behind two keys.
#[inline]
pub fn cmp_key(a: u64, b: u64) -> Ordering {
    if (a | b) & INTERNED == 0 { a.cmp(&b) } else { key_bytes(a).cmp(&key_bytes(b)) }
}

#[inline(always)]
pub fn mix(k: u64) -> u64 {
    let x = k.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^ (x >> 31)
}

/// Lookup table for the clean reference files (products, campaigns); never interns.
#[derive(Default)]
pub struct RefMap {
    short: FxHashMap<u64, u32>,
    long: FxHashMap<Vec<u8>, u32>,
}

impl RefMap {
    pub fn insert(&mut self, b: &[u8], v: u32) {
        match pack(b) {
            Some(k) => self.short.insert(k, v),
            None => self.long.insert(b.to_vec(), v),
        };
    }
    /// Lookup of field i of a line, using the SIMD packing fast path.
    #[inline(always)]
    pub fn get_field<const N: usize>(&self, l: &Line<N>, i: usize) -> Option<u32> {
        match l.packed(i) {
            Some(k) => self.short.get(&k).copied(),
            None => self.long.get(l.get(i)).copied(),
        }
    }
    #[inline]
    pub fn get(&self, b: &[u8]) -> Option<u32> {
        match pack(b) {
            Some(k) => self.short.get(&k).copied(),
            None => self.long.get(b).copied(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split<const N: usize>(line: &[u8]) -> Option<Vec<Vec<u8>>> {
        let mut out = None;
        scan::<N>(line, |f| out = Some((0..N).map(|i| f.get(i).to_vec()).collect()));
        out
    }

    #[test]
    fn timestamps() {
        set_dates(days_from_civil(2028, 2, 1), days_from_civil(2028, 3, 5));
        set_dates(days_from_civil(2026, 9, 25), days_from_civil(2026, 10, 10));
        let f1 = parse_ts(b"2026-10-03T23:58:10Z").unwrap();
        assert_eq!(f1, 1791071890000);
        assert_eq!(parse_ts(b"2026-10-03T19:58:10-04:00"), Some(f1));
        assert_eq!(parse_ts(b"1791071890000"), Some(1791071890000));
        assert_eq!(parse_ts(b"0001791071890000"), Some(1791071890000));
        assert_eq!(parse_ts(b"00000000000000000001791071890000"), Some(1791071890000));
        assert_eq!(parse_ts(b"2026-10-04T01:30:00+02:00"), parse_ts(b"2026-10-03T23:30:00Z"));
        assert_eq!(parse_ts(b"2026-10-03T23:58:10-00:00"), Some(f1));
        assert_eq!(parse_ts(b"2028-02-29T00:00:00Z"), Some(days_from_civil(2028, 2, 29) * DAY));
        assert_eq!(parse_ts(b"1970-01-02T00:00:00Z"), Some(DAY));
        assert_eq!(parse_ts(b"1969-12-31T00:00:00Z"), Some(-DAY));
        assert_eq!(parse_ts(b"2000-03-01T00:00:00Z"), Some(951868800000));
        assert_eq!(parse_ts(b"0000-01-01T00:00:00Z"), Some(-62167219200000));
        assert_eq!(parse_ts(b"9999-12-31T23:59:59Z"), Some(253402300799000));
        for bad in [
            &b""[..], b"2026-10-03T23:58:10.000Z", b"2026-10-03T23:58:10z", b"2026-10-03t23:58:10Z",
            b"2026-10-03 23:58:10Z", b"2026-10-03T23:58:10", b"1791071890000.0", b"1.79e12", b"-5",
            b"2026-13-03T23:58:10Z", b"2026-02-30T23:58:10Z", b"2027-02-29T00:00:00Z", b"2026-10-03T24:00:00Z",
            b"2026-10-03T23:59:60Z", b"2026-10-03T23:58:10+24:00", b"2026-10-03T23:58:10+05:60",
            b"2026-10-03T23:58:10+0200", b"2026-10-03T23:58:10Z\r", b"99999999999999999999999",
            "２０26-10-03T23:58:10Z".as_bytes(), "١٧٩١٠٧١٨٩٠٠٠٠".as_bytes(), b"2026-10-03T23:58:10+02:00\r",
            b"2100-02-29T00:00:00Z", b"2026-00-03T23:58:10Z", b"2026-10-00T23:58:10Z", b"2026-10-03T23:58:10+2:00 ",
            b"12345678901234567890", b"2026-10-03T23:58:1aZ", b"2026-10-03T23:58:10*02:00",
        ] {
            assert_eq!(parse_ts(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
        assert!(parse_ts(b"2000-02-29T00:00:00Z").is_some());
        assert_eq!(parse_ts(b"1234567890123456789"), Some(1234567890123456789));
    }

    #[test]
    fn simd_timestamps_match_reference() {
        set_dates(days_from_civil(2028, 2, 1), days_from_civil(2028, 3, 5));
        set_dates(days_from_civil(2026, 9, 25), days_from_civil(2026, 10, 10));
        set_dates(days_from_civil(2027, 12, 20), days_from_civil(2028, 1, 3));
        let mut seed = 12345u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alphabet = b"0123456789-T:Z+zt .e\r";
        let bases: [&[u8]; 6] = [b"2026-10-03T23:58:10Z", b"2028-02-29T00:00:00+12:00", b"1791071890000", b"0001791071890000",
            b"2027-12-31T23:59:59-10:00", b"9999-12-31T23:59:59Z"];
        let mut n = 0;
        for _ in 0..300_000 {
            let mut v = bases[(rnd() % 6) as usize].to_vec();
            for _ in 0..(rnd() % 3) {
                let i = (rnd() as usize) % v.len();
                v[i] = alphabet[(rnd() as usize) % alphabet.len()];
            }
            if rnd() % 7 == 0 {
                v.truncate((rnd() as usize) % (v.len() + 1));
            }
            assert_eq!(parse_ts(&v), parse_ts_ref(&v), "{:?}", String::from_utf8_lossy(&v));
            n += parse_ts(&v).is_some() as u32;
        }
        assert!(n > 1000);
        for y in [0u32, 1, 1969, 1970, 2000, 2024, 2025, 2028, 2100, 9999] {
            for m in 0..=13 {
                for d in [0, 1, 28, 29, 30, 31, 32] {
                    for t in ["00:00:00Z", "23:59:59Z", "24:00:00Z", "12:60:00Z", "12:00:60Z", "05:06:07+23:59", "05:06:07-00:00", "05:06:07+24:00", "05:06:07+05:60", "05:06:07*05:00"] {
                        let v = format!("{y:04}-{m:02}-{d:02}T{t}");
                        assert_eq!(parse_ts(v.as_bytes()), parse_ts_ref(v.as_bytes()), "{v}");
                    }
                }
            }
        }
        for v in ["1", "0", "9999999999999999", "99999999999999999", "999999999999999999", "9223372036854775807", "9223372036854775808", "0000000000000000000000001"] {
            assert_eq!(parse_ts(v.as_bytes()), parse_ts_ref(v.as_bytes()), "{v}");
        }
    }

    #[test]
    fn civil_round_trip() {
        for d in -719_528..2_932_896 {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y as u32, m, dd), d, "{d}");
        }
    }

    #[test]
    fn civil_matches_reference() {
        fn reference(y: i64, m: i64, d: i64) -> i64 {
            let y = if m <= 2 { y - 1 } else { y };
            let era = y.div_euclid(400);
            let yoe = y - era * 400;
            let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
            era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
        }
        for y in (0..=9999).step_by(7).chain(1995..2035) {
            for m in 1..=12 {
                for d in [1, 15, 28] {
                    assert_eq!(days_from_civil(y, m, d), reference(y as i64, m as i64, d as i64), "{y}-{m}-{d}");
                }
            }
        }
    }

    #[test]
    fn prices() {
        assert_eq!(parse_price(b"0250"), Some(250));
        assert_eq!(parse_price(b"000000000000000000000250"), Some(250));
        assert_eq!(parse_price(b"10000000"), Some(10_000_000));
        for bad in [&b"10000001"[..], b"0", b"", b"19.99", b"-5", b"+5", b"1000\r", "２５０".as_bytes(), b"99999999999999999999999"] {
            assert_eq!(parse_price(bad), None);
        }
    }

    #[test]
    fn scanning() {
        let v = |x: &[&[u8]]| Some(x.iter().map(|y| y.to_vec()).collect::<Vec<_>>());
        assert_eq!(split::<2>(b"\"u4\",u1"), v(&[b"u4", b"u1"]));
        assert_eq!(split::<2>(b"\"\",u1"), v(&[b"", b"u1"]));
        assert_eq!(split::<2>(b"u1,u5\""), None);
        assert_eq!(split::<2>(b"u1,u2,u3"), None);
        assert_eq!(split::<2>(b"u1"), None);
        assert_eq!(split::<2>(b"\",u1"), None);
        assert_eq!(split::<2>(b"\"\"\",u1"), None);
        assert_eq!(split::<2>(b"\"a\"b\",u1"), None);
        assert_eq!(split::<2>(b"a\"\"b,u1"), None);
        assert_eq!(split::<2>(b" u1 ,\"\r\""), v(&[b" u1 ", b"\r"]));
        let mut lines = vec![];
        let long = format!("{},x\n", "y".repeat(150));
        let data = format!("a,b\n\n\"c\",\"d\"\nbad\ne,f,g\n{long}h,\"i\"\"\nj,k\r\nl,m");
        scan::<2>(data.as_bytes(), |f| lines.push((f.get(0).to_vec(), f.get(1).to_vec())));
        let want: Vec<(Vec<u8>, Vec<u8>)> = [("a", "b"), ("c", "d"), (&*"y".repeat(150), "x"), ("j", "k\r"), ("l", "m")]
            .iter()
            .map(|(a, b)| (a.as_bytes().to_vec(), b.as_bytes().to_vec()))
            .collect();
        assert_eq!(lines, want);
        // Every line split position across a 64-byte block boundary.
        for pad in 0..70 {
            let data = format!("{}\n\"u\",\"v\"\nw,x\"\n", "z".repeat(pad));
            let mut got = vec![];
            scan::<2>(data.as_bytes(), |f| got.push((f.get(0).to_vec(), f.get(1).to_vec())));
            assert_eq!(got, vec![(b"u".to_vec(), b"v".to_vec())], "pad {pad}");
        }
    }

    #[test]
    fn scan_matches_block_scanner() {
        let mut seed = 7u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alpha = b"ab,,\"\"\n\r 0";
        fn collect<const N: usize>(d: &[u8], blocks: bool) -> Vec<Vec<Vec<u8>>> {
            let mut v = vec![];
            let mut f = |l: &Line<N>| v.push((0..N).map(|i| l.get(i).to_vec()).collect());
            if blocks { scan_blocks::<N>(d, &mut f) } else { scan::<N>(d, &mut f) }
            v
        }
        for round in 0..3000 {
            let n = (rnd() % 400) as usize;
            let mut d: Vec<u8> = (0..n).map(|_| alpha[(rnd() as usize) % alpha.len()]).collect();
            if round % 3 == 0 {
                let line = b"\"e1\",u2,1791071890000,2026-10-03T23:58:10Z,\"view\",p1,\"\"\n";
                for _ in 0..(rnd() % 8) {
                    let at = (rnd() as usize) % (d.len() + 1);
                    d.splice(at..at, line.iter().copied());
                }
            }
            assert_eq!(collect::<2>(&d, false), collect::<2>(&d, true));
            assert_eq!(collect::<4>(&d, false), collect::<4>(&d, true));
            assert_eq!(collect::<7>(&d, false), collect::<7>(&d, true));
        }
    }

    #[test]
    fn line_fast_paths_match() {
        let mut seed = 99u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alpha = b"0123456789AZaz_ ,-:\r09azAZ@[`{/";
        let mut data = Vec::new();
        for _ in 0..200_000 {
            let n1 = (rnd() % 13) as usize;
            for _ in 0..n1 {
                data.push(alpha[(rnd() as usize) % alpha.len()]);
            }
            data.push(b',');
            let n2 = (rnd() % 18) as usize;
            for _ in 0..n2 {
                data.push(if rnd() % 9 == 0 { alpha[(rnd() as usize) % alpha.len()] } else { b'0' + (rnd() % 10) as u8 });
            }
            data.push(b'\n');
        }
        let mut n = 0;
        scan::<2>(&data, |f| {
            assert_eq!(f.key(0), key(f.get(0)), "{:?}", String::from_utf8_lossy(f.get(0)));
            assert_eq!(f.ts(1), parse_ts_ref(f.get(1)), "{:?}", String::from_utf8_lossy(f.get(1)));
            n += 1;
        });
        assert!(n > 100_000);
    }

    #[test]
    fn keys() {
        let ids: [&[u8]; 9] = [b"", b"0", b"U1", b"_", b"u1", b"u10", b"u2", b"zzzzzzzzzz", b"zzzzzzzzzzz"];
        for w in ids.windows(2) {
            assert_eq!(cmp_key(key(w[0]), key(w[1])), Ordering::Less, "{:?}", w);
        }
        for id in [&b" u1"[..], b"u1 ", b"a_very_long_identifier", b"\xd9\xa2", b"u-1", b"e3\r"] {
            assert_eq!(key_bytes(key(id)), id.to_vec());
            assert!(key(id) & INTERNED != 0);
            assert_eq!(key(id), key(id));
        }
        assert_eq!(key_bytes(key(b"Az_09z")), b"Az_09z".to_vec());
        assert_eq!(cmp_key(key(b" u1"), key(b"U1")), Ordering::Less);
        assert_eq!(cmp_key(key(b"u1 "), key(b"u1")), Ordering::Greater);
        assert_eq!(cmp_key(key(b"u1 "), key(b"u10")), Ordering::Less);
    }
}
