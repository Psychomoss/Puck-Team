use memchr::memchr;
use rustc_hash::FxHashMap;
use std::cmp::Ordering;
use std::sync::Mutex;

pub const DAY: i64 = 86_400_000;

/// Splits a line into exactly N fields per the rules' dialect; false if malformed.
#[inline]
pub fn split<'a, const N: usize>(line: &'a [u8], f: &mut [&'a [u8]; N]) -> bool {
    let mut start = 0;
    let mut n = 0;
    for (i, &c) in line.iter().enumerate() {
        if c == b',' {
            if n + 1 >= N {
                return false;
            }
            f[n] = &line[start..i];
            n += 1;
            start = i + 1;
        }
    }
    if n != N - 1 {
        return false;
    }
    f[N - 1] = &line[start..];
    if memchr(b'"', line).is_some() {
        for x in f.iter_mut() {
            match unquote(x) {
                Some(v) => *x = v,
                None => return false,
            }
        }
    }
    true
}

#[inline]
fn unquote(x: &[u8]) -> Option<&[u8]> {
    if memchr(b'"', x).is_none() {
        return Some(x);
    }
    let l = x.len();
    if l >= 2 && x[0] == b'"' && x[l - 1] == b'"' && memchr(b'"', &x[1..l - 1]).is_none() {
        Some(&x[1..l - 1])
    } else {
        None
    }
}

#[inline]
fn d2(s: &[u8], i: usize) -> Option<i64> {
    let a = s[i].wrapping_sub(b'0');
    let b = s[i + 1].wrapping_sub(b'0');
    if a < 10 && b < 10 { Some((a * 10 + b) as i64) } else { None }
}

pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// Epoch ms for F1/F2/F3, None for any bad form (an F3 overflowing i64 is also None:
/// it is outside every window anyway).
pub fn parse_ts(s: &[u8]) -> Option<i64> {
    let l = s.len();
    if l == 0 {
        return None;
    }
    if (l == 20 || l == 25) && s[4] == b'-' {
        return parse_iso(s);
    }
    let mut v: i64 = 0;
    for &c in s {
        let d = c.wrapping_sub(b'0');
        if d >= 10 {
            return None;
        }
        v = v.checked_mul(10)?.checked_add(d as i64)?;
    }
    Some(v)
}

fn parse_iso(s: &[u8]) -> Option<i64> {
    if s[7] != b'-' || s[10] != b'T' || s[13] != b':' || s[16] != b':' {
        return None;
    }
    let y = d2(s, 0)? * 100 + d2(s, 2)?;
    let mo = d2(s, 5)?;
    let d = d2(s, 8)?;
    let h = d2(s, 11)?;
    let mi = d2(s, 14)?;
    let se = d2(s, 17)?;
    if !(1..=12).contains(&mo) || d < 1 || h > 23 || mi > 59 || se > 59 {
        return None;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let dim = match mo {
        2 => 28 + leap as i64,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if d > dim {
        return None;
    }
    let mut off = 0;
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
        if s[22] != b':' {
            return None;
        }
        let oh = d2(s, 20)?;
        let om = d2(s, 23)?;
        if oh > 23 || om > 59 {
            return None;
        }
        off = sign * (oh * 60 + om) * 60_000;
    }
    Some((days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se) * 1000 - off)
}

/// Digits-only price in 1..=10_000_000.
#[inline]
pub fn parse_price(s: &[u8]) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for &c in s {
        let d = c.wrapping_sub(b'0');
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

static LONG: Mutex<Option<(FxHashMap<Vec<u8>, u32>, Vec<Vec<u8>>)>> = Mutex::new(None);

/// An exact key for a string: up to 15 bytes are packed big-endian with the length in the
/// low byte, so u128 order equals byte order. Longer strings are interned (low byte 0xFF).
#[inline]
pub fn key(b: &[u8]) -> u128 {
    if b.len() <= 15 {
        let mut buf = [0u8; 16];
        buf[..b.len()].copy_from_slice(b);
        buf[15] = b.len() as u8;
        u128::from_be_bytes(buf)
    } else {
        let mut g = LONG.lock().unwrap();
        let (map, list) = g.get_or_insert_with(Default::default);
        let id = *map.entry(b.to_vec()).or_insert_with(|| {
            list.push(b.to_vec());
            list.len() as u32 - 1
        });
        ((id as u128) << 8) | 0xFF
    }
}

pub fn key_bytes(k: u128) -> Vec<u8> {
    if k & 0xFF == 0xFF {
        let g = LONG.lock().unwrap();
        g.as_ref().unwrap().1[(k >> 8) as usize].clone()
    } else {
        let b = k.to_be_bytes();
        b[..b[15] as usize].to_vec()
    }
}

/// Byte-wise order of the strings behind two keys.
#[inline]
pub fn cmp_key(a: u128, b: u128) -> Ordering {
    if a & 0xFF != 0xFF && b & 0xFF != 0xFF {
        a.cmp(&b)
    } else {
        key_bytes(a).cmp(&key_bytes(b))
    }
}

#[inline]
pub fn mix(k: u128) -> u64 {
    let x = (k as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ ((k >> 64) as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    x ^ (x >> 29)
}

/// Lookup table for the clean reference files (products, campaigns).
#[derive(Default)]
pub struct RefMap {
    short: FxHashMap<u128, u32>,
    long: FxHashMap<Vec<u8>, u32>,
}

impl RefMap {
    pub fn insert(&mut self, b: &[u8], v: u32) {
        if b.len() <= 15 {
            self.short.insert(key(b), v);
        } else {
            self.long.insert(b.to_vec(), v);
        }
    }
    #[inline]
    pub fn get(&self, b: &[u8]) -> Option<u32> {
        if b.len() <= 15 { self.short.get(&key(b)).copied() } else { self.long.get(b).copied() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        let f1 = parse_ts(b"2026-10-03T23:58:10Z").unwrap();
        assert_eq!(parse_ts(b"2026-10-03T19:58:10-04:00"), Some(f1));
        assert_eq!(parse_ts(b"1791071890000"), Some(1791071890000));
        assert_eq!(parse_ts(b"0001791071890000"), Some(1791071890000));
        assert_eq!(parse_ts(b"2026-10-04T01:30:00+02:00"), parse_ts(b"2026-10-03T23:30:00Z"));
        assert_eq!(parse_ts(b"2026-10-03T23:58:10-00:00"), Some(f1));
        assert_eq!(parse_ts(b"2028-02-29T00:00:00Z").is_some(), true);
        for bad in [
            &b""[..], b"2026-10-03T23:58:10.000Z", b"2026-10-03T23:58:10z", b"2026-10-03t23:58:10Z",
            b"2026-10-03 23:58:10Z", b"2026-10-03T23:58:10", b"1791071890000.0", b"1.79e12", b"-5",
            b"2026-13-03T23:58:10Z", b"2026-02-30T23:58:10Z", b"2027-02-29T00:00:00Z", b"2026-10-03T24:00:00Z",
            b"2026-10-03T23:59:60Z", b"2026-10-03T23:58:10+24:00", b"2026-10-03T23:58:10+05:60",
            b"2026-10-03T23:58:10+0200", b"2026-10-03T23:58:10Z\r", b"99999999999999999999999",
            "２０26-10-03T23:58:10Z".as_bytes(), "١٧٩١٠٧١٨٩٠٠٠٠".as_bytes(), b"2026-10-03T23:58:10+02:00\r",
        ] {
            assert_eq!(parse_ts(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
        assert_eq!(parse_ts(b"1970-01-02T00:00:00Z"), Some(DAY));
    }

    #[test]
    fn prices_and_split() {
        assert_eq!(parse_price(b"0250"), Some(250));
        assert_eq!(parse_price(b"10000000"), Some(10_000_000));
        for bad in [&b"10000001"[..], b"0", b"", b"19.99", b"-5", b"+5", b"1000\r", "２５０".as_bytes()] {
            assert_eq!(parse_price(bad), None);
        }
        let mut f = [&b""[..]; 2];
        assert!(split(b"\"u4\",u1", &mut f) && f == [&b"u4"[..], b"u1"]);
        assert!(split(b"\"\",u1", &mut f) && f[0].is_empty());
        assert!(!split(b"u1,u5\"", &mut f));
        assert!(!split(b"u1,u2,u3", &mut f));
        assert!(!split(b"", &mut f));
        assert!(!split(b"\",u1", &mut f));
        assert!(!split(b"\"\"\",u1", &mut f));
    }

    #[test]
    fn key_order() {
        let ids: [&[u8]; 6] = [b"", b"U1", b"u1", b"u1\x00", b"u10", b"u2"];
        for w in ids.windows(2) {
            assert_eq!(cmp_key(key(w[0]), key(w[1])), Ordering::Less);
        }
        let long = b"a_very_long_identifier_xyz";
        assert_eq!(key_bytes(key(long)), long.to_vec());
        assert_eq!(cmp_key(key(long), key(b"b")), Ordering::Less);
        assert_eq!(cmp_key(key(b"a_very"), key(long)), Ordering::Less);
    }
}
