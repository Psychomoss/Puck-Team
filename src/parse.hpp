#pragma once
// CSV dialect, IDs and timestamps of the rules: the hot parsing code, all inline.

#include <immintrin.h>

#include <algorithm>
#include <array>
#include <atomic>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <string>
#include <string_view>
#include <unordered_map>
#include <vector>

#define HOT [[gnu::always_inline]] inline

constexpr int64_t DAY = 86'400'000;
constexpr int64_t BAD_TS = INT64_MIN;  // "no timestamp": below every valid value

// ---------------------------------------------------------------- scanning

// Newline, comma and quote bit masks of the 64 bytes at data[base..] (zero padded at the end).
HOT void masks(const uint8_t* data, size_t len, size_t base, uint64_t& nl, uint64_t& cm, uint64_t& qt) {
    alignas(32) uint8_t tmp[64];
    const uint8_t* p = data + base;
    if (base + 64 > len) {
        std::memset(tmp, 0, 64);
        std::memcpy(tmp, p, len - base);
        p = tmp;
    }
    __m256i a = _mm256_loadu_si256(reinterpret_cast<const __m256i*>(p));
    __m256i b = _mm256_loadu_si256(reinterpret_cast<const __m256i*>(p + 32));
    auto m = [&](char c) {
        __m256i s = _mm256_set1_epi8(c);
        uint64_t lo = uint32_t(_mm256_movemask_epi8(_mm256_cmpeq_epi8(a, s)));
        uint64_t hi = uint32_t(_mm256_movemask_epi8(_mm256_cmpeq_epi8(b, s)));
        return lo | hi << 32;
    };
    nl = m('\n');
    cm = m(',');
    qt = m('"');
}

HOT uint64_t pack16(const uint8_t* p, size_t l, bool& ok);
HOT uint64_t digits_end(const uint8_t* p, size_t l, bool& ok);
HOT int64_t parse_ts(std::string_view sv);
uint64_t key_slow(std::string_view b);
bool pack(std::string_view b, uint64_t& out);


/// One well-formed line: field i is data[st[i]..en[i]] (quotes already stripped). Keeping the
/// whole buffer lets the parsers read 16 bytes around a field without leaving it.
template <size_t N>
struct Line {
    const uint8_t* data;
    size_t len;
    size_t st[N], en[N];

    [[gnu::always_inline]] std::string_view get(size_t i) const { return {reinterpret_cast<const char*>(data) + st[i], en[i] - st[i]}; }
    [[gnu::always_inline]] bool empty(size_t i) const { return st[i] == en[i]; }
    /// Exact key of field i (packed, or interned when it does not pack).
    [[gnu::always_inline]] uint64_t key(size_t i) const {
        size_t s = st[i], e = en[i];
        if (e - s <= 10 && s + 16 <= len) {
            bool ok;
            uint64_t k = pack16(data + s, e - s, ok);
            if (ok) return k;
        }
        return key_slow(get(i));
    }
    /// Packed key of field i if it packs; never interns.
    [[gnu::always_inline]] bool packed(size_t i, uint64_t& k) const {
        size_t s = st[i], e = en[i];
        if (e - s <= 10 && s + 16 <= len) {
            bool ok;
            k = pack16(data + s, e - s, ok);
            return ok;
        }
        return pack(get(i), k);
    }
    [[gnu::always_inline]] int64_t ts(size_t i) const {
        size_t s = st[i], e = en[i], l = e - s;
        if (l > 0 && l <= 16 && e >= 16) {
            bool ok;
            uint64_t v = digits_end(data + e - 16, l, ok);
            return ok ? int64_t(v) : BAD_TS;
        }
        return parse_ts(get(i));
    }
};

/// Emits line data[ls..] with field ends `ends` if its quotes are legal: a quote may only be the
/// first and last byte of a field, so the line is valid iff its quote count equals twice the
/// number of wrapped fields.
template <size_t N, class F>
HOT void emit_line(const uint8_t* data, size_t len, size_t ls, const size_t (&ends)[N], uint32_t q, F& f) {
    Line<N> line;  // every field is written below
    line.data = data;
    line.len = len;
    size_t s = ls;
    for (size_t i = 0; i < N; ++i) {
        size_t e = ends[i];
        size_t l = e - s;
        uint8_t first = data[std::min(s, len - 1)];
        uint8_t last = data[std::min(e - 1, len - 1)];
        size_t w = (first == '"') & (last == '"') & (l >= 2);
        q -= uint32_t(2 * w);
        line.st[i] = s + w;
        line.en[i] = e - w;
        s = e + 1;
    }
    if (q == 0) f(line);
}

/// Per-thread scanner buffers; grown without zero-filling (every entry read was written first).
struct ScanScratch {
    uint32_t *pos = nullptr, *cq = nullptr;
    uint64_t* qb = nullptr;
    size_t pcap = 0, bcap = 0;
    void reserve(size_t np, size_t nb) {
        if (np > pcap) {
            std::free(pos);
            pcap = std::max(np, 2 * pcap);
            pos = static_cast<uint32_t*>(std::malloc(pcap * sizeof(uint32_t)));
        }
        if (nb > bcap) {
            std::free(cq);
            std::free(qb);
            bcap = std::max(nb, 2 * bcap);
            cq = static_cast<uint32_t*>(std::malloc(bcap * sizeof(uint32_t)));
            qb = static_cast<uint64_t*>(std::malloc(bcap * sizeof(uint64_t)));
        }
        if (!pos || !cq || !qb) std::abort();
    }
};
inline thread_local ScanScratch scan_scratch;

/// Calls f with every well-formed line of data that has N fields (only '\n' ends a line; quotes
/// only around a whole field, stripped). Pass 1 flattens all comma and newline positions (no
/// dependency between blocks); pass 2 walks lines from that list, so lines overlap in the pipeline.
template <size_t N, class F>
HOT void scan(const uint8_t* data, size_t len, F&& f) {
    constexpr uint32_t NLF = 1u << 31;
    if (len == 0) return;
    auto& sc = scan_scratch;
    size_t nblk = (len + 63) / 64;
    sc.reserve(len + 64 + 2 * N, nblk + 1);
    uint32_t* pp = sc.pos;
    uint32_t* cp = sc.cq;
    uint64_t* qp = sc.qb;
    size_t n = 0;
    uint32_t quotes = 0;
    const __m256i vnl = _mm256_set1_epi8('\n'), vcm = _mm256_set1_epi8(','), vqt = _mm256_set1_epi8('"');
    alignas(32) uint8_t tail[64];
    for (size_t blk = 0; blk < nblk; ++blk) {
        size_t base = blk * 64;
        const uint8_t* bp = data + base;
        if (blk + 1 == nblk && base + 64 > len) [[unlikely]] {
            std::memset(tail, 0, 64);
            std::memcpy(tail, bp, len - base);
            bp = tail;
        }
        __m256i va = _mm256_loadu_si256(reinterpret_cast<const __m256i*>(bp));
        __m256i vb = _mm256_loadu_si256(reinterpret_cast<const __m256i*>(bp + 32));
        auto mm = [&](__m256i c) {
            return uint64_t(uint32_t(_mm256_movemask_epi8(_mm256_cmpeq_epi8(va, c)))) |
                   uint64_t(uint32_t(_mm256_movemask_epi8(_mm256_cmpeq_epi8(vb, c)))) << 32;
        };
        uint64_t nl = mm(vnl), cm = mm(vcm), qt = mm(vqt);
        cp[blk] = quotes;
        qp[blk] = qt;
        quotes += uint32_t(__builtin_popcountll(qt));
        uint64_t st = nl | cm;
        size_t cnt = size_t(__builtin_popcountll(st));
        uint32_t* out = pp + n;
        for (int j = 0; j < 8; ++j) {
            uint32_t tz = uint32_t(_tzcnt_u64(st));
            out[j] = (uint32_t(base) + tz) | (uint32_t((nl >> (tz & 63)) & 1) << 31);
            st = _blsr_u64(st);
        }
        for (int j = 8; st; ++j) {
            uint32_t tz = uint32_t(_tzcnt_u64(st));
            out[j] = (uint32_t(base) + tz) | (uint32_t((nl >> tz) & 1) << 31);
            st = _blsr_u64(st);
        }
        n += cnt;
    }
    cp[nblk] = quotes;
    qp[nblk] = 0;
    // A last line without '\n' ends at len; padding keeps reads of N entries in bounds.
    if (data[len - 1] != '\n') pp[n++] = uint32_t(len) | NLF;
    for (size_t j = 0; j < N; ++j) pp[n + j] = uint32_t(len) | NLF;
    auto qbefore = [&](size_t x) -> uint32_t {
        size_t b = x >> 6, o = x & 63;
        return cp[b] + uint32_t(__builtin_popcountll(qp[b] & ((uint64_t(1) << o) - 1)));
    };
    size_t k = 0, ls = 0;
    uint32_t qs = 0;
    while (k < n) {
        uint32_t early = 0;
        for (size_t i = 0; i + 1 < N; ++i) early |= pp[k + i];
        if (!(early & NLF) && (pp[k + N - 1] & NLF)) {
            size_t ends[N];
            for (size_t i = 0; i < N; ++i) ends[i] = pp[k + i] & ~NLF;
            size_t le = ends[N - 1];
            uint32_t qe = qbefore(le);
            emit_line<N>(data, len, ls, ends, qe - qs, f);
            qs = qe;
            ls = le + 1;
            k += N;
        } else {
            while (!(pp[k] & NLF)) ++k;
            size_t le = pp[k] & ~NLF;
            qs = qbefore(le);
            ls = le + 1;
            ++k;
        }
    }
}

/// Reference scanner (one bit loop over 64-byte blocks), kept for differential tests.
template <size_t N, class F>
inline void scan_blocks(const uint8_t* data, size_t len, F&& f) {
    size_t ls = 0, nf = 0, base = 0;
    size_t ends[8] = {};
    uint32_t quotes = 0;
    auto emit = [&](size_t ls, uint32_t q) {
        size_t e[N];
        for (size_t i = 0; i < N; ++i) e[i] = ends[i];
        emit_line<N>(data, len, ls, e, q, f);
    };
    while (base < len) {
        uint64_t nl, cm, qt;
        masks(data, len, base, nl, cm, qt);
        uint64_t bits = nl | cm, qb = qt;
        while (bits) {
            uint32_t tz = uint32_t(_tzcnt_u64(bits));
            size_t pos = base + tz;
            ends[nf & 7] = pos;
            ++nf;
            if ((nl >> tz) & 1) {
                uint64_t below = (uint64_t(1) << tz) - 1;
                uint32_t q = quotes + uint32_t(__builtin_popcountll(qb & below));
                qb &= ~below;
                if (nf == N) emit(ls, q);
                ls = pos + 1;
                nf = 0;
                quotes = 0;
            }
            bits &= bits - 1;
        }
        quotes += uint32_t(__builtin_popcountll(qb));
        base += 64;
    }
    if (ls < len && nf == N - 1) {
        ends[N - 1] = len;
        emit(ls, quotes);
    }
}

// ---------------------------------------------------------------- keys

constexpr uint64_t INTERNED = uint64_t(1) << 63;
extern std::atomic<bool> any_interned;

/// 6-bit packing of l (<= 10) bytes at p, reading 16 bytes; !ok if a byte is outside [0-9A-Z_a-z].
/// Codes are order-preserving with 0 as padding, so integer order equals byte order. Classes come
/// from two nibble lookups (allowed high nibbles per low nibble), so every constant is a load.
HOT uint64_t pack16(const uint8_t* p, size_t l, bool& ok) {
    alignas(16) static constexpr uint8_t IDX[16] = {0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15};
    // bit h-3 set: high nibble h (3..7) is a valid class for this low nibble
    alignas(16) static constexpr uint8_t LO[16] = {0x15, 0x1F, 0x1F, 0x1F, 0x1F, 0x1F, 0x1F, 0x1F,
                                                  0x1F, 0x1F, 0x1E, 0x0A, 0x0A, 0x0A, 0x0A, 0x0E};
    alignas(16) static constexpr uint8_t HB[16] = {0, 0, 0, 1, 2, 4, 8, 16, 0, 0, 0, 0, 0, 0, 0, 0};
    alignas(16) static constexpr uint8_t OFF[16] = {0, 0, 0, 47, 54, 54, 59, 59, 0, 0, 0, 0, 0, 0, 0, 0};
    alignas(16) static constexpr uint8_t NIB[16] = {15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15};
    alignas(16) static constexpr uint8_t USC[16] = {'_', '_', '_', '_', '_', '_', '_', '_', '_', '_', '_', '_', '_', '_', '_', '_'};
    alignas(16) static constexpr uint8_t FOUR[16] = {4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4};
    alignas(16) static constexpr uint16_t M1[8] = {0x0140, 0x0140, 0x0140, 0x0140, 0x0140, 0x0140, 0x0140, 0x0140};
    alignas(16) static constexpr uint32_t M2[4] = {0x00011000, 0x00011000, 0x00011000, 0x00011000};
    auto ld = [](const void* q) { return _mm_load_si128(reinterpret_cast<const __m128i*>(q)); };
    __m128i v = _mm_loadu_si128(reinterpret_cast<const __m128i*>(p));
    __m128i inlen = _mm_cmpgt_epi8(_mm_set1_epi8(char(l)), ld(IDX));
    __m128i nib = ld(NIB);
    __m128i hi = _mm_and_si128(_mm_srli_epi16(v, 4), nib);
    __m128i lo = _mm_and_si128(v, nib);
    __m128i cls = _mm_and_si128(_mm_shuffle_epi8(ld(LO), lo), _mm_shuffle_epi8(ld(HB), hi));
    __m128i bad = _mm_and_si128(_mm_cmpeq_epi8(cls, _mm_setzero_si128()), inlen);
    ok = _mm_movemask_epi8(bad) == 0;
    __m128i off = _mm_add_epi8(_mm_shuffle_epi8(ld(OFF), hi), _mm_and_si128(_mm_cmpeq_epi8(v, ld(USC)), ld(FOUR)));
    __m128i code = _mm_and_si128(_mm_sub_epi8(v, off), inlen);
    __m128i m1 = _mm_maddubs_epi16(code, ld(M1));
    __m128i m2 = _mm_madd_epi16(m1, ld(M2));
    uint64_t q0 = uint32_t(_mm_cvtsi128_si32(m2));
    uint64_t q1 = uint32_t(_mm_extract_epi32(m2, 1));
    uint64_t q2 = uint32_t(_mm_extract_epi32(m2, 2));
    return q0 << 36 | q1 << 12 | q2 >> 12;
}

std::string key_bytes(uint64_t k);
void key_append(uint64_t k, std::string& out);
/// Byte-wise order of the strings behind two keys (-1, 0, 1).
int cmp_key(uint64_t a, uint64_t b);

inline uint64_t mix(uint64_t k) {
    uint64_t x = k * 0x9E3779B97F4A7C15ull;
    return x ^ (x >> 31);
}

// ---------------------------------------------------------------- timestamps

inline uint32_t dg(uint8_t c) { return uint32_t(uint8_t(c - '0')); }

/// Days since 1970-01-01 for a valid proleptic Gregorian date with 0 <= y <= 9999.
inline int64_t days_from_civil(uint32_t y, uint32_t m, uint32_t d) {
    static constexpr uint32_t CUM[13] = {0, 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334};
    bool leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    int64_t yd = int64_t(y) - 1;
    auto fdiv = [](int64_t a, int64_t b) { return a >= 0 ? a / b : -((-a + b - 1) / b); };
    int64_t before = 365 * int64_t(y) + fdiv(yd, 4) - fdiv(yd, 100) + fdiv(yd, 400) + 1;
    return before - 719528 + int64_t(CUM[m] + (m > 2 && leap) + d - 1);
}

void civil_from_days(int64_t z, int64_t& y, uint32_t& m, uint32_t& d);

/// Digits-only value of the l (<= 16) bytes ending at p + 16, reading 16 bytes from p.
HOT uint64_t digits_end(const uint8_t* p, size_t l, bool& ok) {
    __m128i v = _mm_loadu_si128(reinterpret_cast<const __m128i*>(p));
    __m128i keep = _mm_cmpgt_epi8(_mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15), _mm_set1_epi8(char(15 - l)));
    __m128i d = _mm_and_si128(_mm_sub_epi8(v, _mm_set1_epi8('0')), keep);
    ok = _mm_movemask_epi8(_mm_cmpeq_epi8(_mm_min_epu8(d, _mm_set1_epi8(9)), d)) == 0xFFFF;
    __m128i m1 = _mm_maddubs_epi16(d, _mm_set1_epi16(0x010A));
    __m128i m2 = _mm_madd_epi16(m1, _mm_set1_epi32(0x00010064));
    __m128i m3 = _mm_packus_epi32(m2, m2);
    __m128i m4 = _mm_madd_epi16(m3, _mm_set1_epi32(0x00012710));
    uint64_t r = uint64_t(_mm_cvtsi128_si64(m4));
    return (r & 0xFFFFFFFF) * 100000000 + (r >> 32);
}

/// Valid dates near the window: slot (month parity, day digits) -> ("YYYY-MM-", "DD" << 48 |
/// midnight ms - MIN_DATE_MS). Written once before parsing; dates not in it take the full path.
extern std::atomic<uint64_t> date_tab[128][2];
constexpr int64_t MIN_DATE_MS = -62'167'219'200'000;
void set_dates(int64_t d0, int64_t d1);

inline size_t date_slot(uint8_t p6, uint8_t p8, uint8_t p9) {
    return (size_t(p6 & 1) << 6) | (size_t(p8 & 3) << 4) | size_t(p9 & 15);
}

/// Seconds of day for "HH:MM:SS" at p (8 bytes), validated with SWAR; -1 if invalid.
HOT int64_t hms(const uint8_t* p) {
    uint64_t x;
    std::memcpy(&x, p, 8);
    constexpr uint64_t CM = 0x0000FF0000FF0000ull;
    if ((x & CM) != 0x00003A00003A0000ull) return -1;
    uint64_t y = (x & ~CM) | 0x0000300000300000ull;
    constexpr uint64_t HI = 0xF0F0F0F0F0F0F0F0ull;
    if ((y & HI) != 0x3030303030303030ull || ((y + 0x0606060606060606ull) & HI) != 0x3030303030303030ull) return -1;
    uint64_t d = y - 0x3030303030303030ull;
    uint64_t t = d * 10 + (d >> 8);
    uint32_t h = uint32_t(t & 0xFF), m = uint32_t((t >> 24) & 0xFF), s = uint32_t((t >> 48) & 0xFF);
    if (h > 23 || m > 59 || s > 59) return -1;
    return int64_t(h * 3600 + m * 60 + s);
}

/// F1/F2 through the date table; false means "not decided here" (take the full parser).
HOT bool iso_fast(const uint8_t* s, size_t l, int64_t& out) {
    auto& e = date_tab[date_slot(s[6], s[8], s[9])];
    uint64_t k;
    uint16_t dd;
    std::memcpy(&k, s, 8);
    std::memcpy(&dd, s + 8, 2);
    uint64_t ek = e[0].load(std::memory_order_acquire);
    uint64_t v = e[1].load(std::memory_order_relaxed);
    if (ek != k || uint16_t(v >> 48) != dd) return false;
    out = BAD_TS;
    int64_t base = int64_t(v & 0xFFFFFFFFFFFFull) + MIN_DATE_MS;
    if (s[10] != 'T') return true;
    int64_t sod = hms(s + 11);
    if (sod < 0) return true;
    int64_t t = base + sod * 1000;
    if (l == 20) {
        if (s[19] == 'Z') out = t;
        return true;
    }
    int64_t sign;
    if (s[19] == '+') sign = 1;
    else if (s[19] == '-') sign = -1;
    else return true;
    uint32_t a = dg(s[20]), b = dg(s[21]), c = dg(s[23]), d = dg(s[24]);
    if (s[22] != ':' || a >= 10 || b >= 10 || c >= 10 || d >= 10) return true;
    uint32_t oh = a * 10 + b, om = c * 10 + d;
    if (oh > 23 || om > 59) return true;
    out = t - sign * int64_t(oh * 60 + om) * 60000;
    return true;
}

int64_t iso_full(const uint8_t* s, size_t l);

/// Epoch ms for F1/F2/F3; BAD_TS for any bad form (an F3 overflowing int64 is bad too: it is
/// outside every window anyway).
HOT int64_t parse_ts(std::string_view sv) {
    const uint8_t* s = reinterpret_cast<const uint8_t*>(sv.data());
    size_t l = sv.size();
    if ((l == 20 || l == 25) && s[4] == '-') {
        int64_t r;
        if (iso_fast(s, l, r)) return r;
        return iso_full(s, l);
    }
    if (l == 0) return BAD_TS;
    if (l <= 16) {
        alignas(16) uint8_t b[16];
        std::memset(b, '0', 16);
        std::memcpy(b + 16 - l, s, l);
        bool ok;
        uint64_t v = digits_end(b, 16, ok);
        return ok ? int64_t(v) : BAD_TS;
    }
    int64_t v = 0;
    for (size_t i = 0; i < l; ++i) {
        uint32_t d = dg(s[i]);
        if (d >= 10) return BAD_TS;
        if (__builtin_mul_overflow(v, 10, &v) || __builtin_add_overflow(v, int64_t(d), &v)) return BAD_TS;
    }
    return v;
}

/// Plain scalar parser, the reference for tests.
int64_t parse_ts_ref(std::string_view s);

/// Digits-only price in 1..=10,000,000; 0 if bad.
HOT uint32_t parse_price(std::string_view s) {
    if (s.empty()) return 0;
    uint64_t v = 0;
    for (char c : s) {
        uint32_t d = dg(uint8_t(c));
        if (d >= 10) return 0;
        v = v * 10 + d;
        if (v > 10'000'000) return 0;
    }
    return uint32_t(v);
}

// ---------------------------------------------------------------- reference lookups

/// Lookup table for the clean reference files (products, campaigns); never interns.
struct RefMap {
    std::vector<uint64_t> keys;
    std::vector<uint32_t> vals;
    size_t mask = 0;
    std::unordered_map<std::string, uint32_t> longs;

    void build(const std::vector<std::pair<std::string, uint32_t>>& items);
    bool get_short(uint64_t k, uint32_t& v) const {
        if (k == 0 || keys.empty()) return false;
        size_t h = size_t(mix(k)) & mask;
        for (;;) {
            uint64_t x = keys[h];
            if (x == k) {
                v = vals[h];
                return true;
            }
            if (x == 0) return false;
            h = (h + 1) & mask;
        }
    }
    bool get_long(std::string_view b, uint32_t& v) const {
        if (longs.empty()) return false;
        auto it = longs.find(std::string(b));
        if (it == longs.end()) return false;
        v = it->second;
        return true;
    }
    bool get(std::string_view b, uint32_t& v) const {
        uint64_t k;
        if (pack(b, k) && k) return get_short(k, v);
        return get_long(b, v);
    }
    template <size_t N>
    bool get_field(const Line<N>& l, size_t i, uint32_t& v) const {
        uint64_t k;
        if (l.packed(i, k) && k) return get_short(k, v);
        return get_long(l.get(i), v);
    }
};
