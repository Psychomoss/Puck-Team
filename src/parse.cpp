#include "parse.hpp"

#include <cstdio>
#include <mutex>
#include <unordered_map>

std::atomic<bool> any_interned{false};
std::atomic<uint64_t> date_tab[128][2];

namespace {
constexpr std::array<uint8_t, 256> code_table() {
    std::array<uint8_t, 256> t{};
    for (int c = 0; c < 256; ++c) {
        uint8_t v = 0x80;
        if (c >= '0' && c <= '9') v = uint8_t(c - '0' + 1);
        else if (c >= 'A' && c <= 'Z') v = uint8_t(c - 'A' + 11);
        else if (c == '_') v = 37;
        else if (c >= 'a' && c <= 'z') v = uint8_t(c - 'a' + 38);
        t[size_t(c)] = v;
    }
    return t;
}
constexpr auto CODE = code_table();
constexpr char ALPHA[] = "\0" "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz";

struct Interner {
    std::mutex mu;
    std::unordered_map<std::string, uint64_t> map;
    std::vector<std::string> list;
};
Interner& interner() {
    static Interner* i = new Interner;
    return *i;
}
}  // namespace

bool pack(std::string_view b, uint64_t& out) {
    if (b.size() > 10) return false;
    uint64_t v = 0;
    uint8_t bad = 0;
    for (char c : b) {
        uint8_t t = CODE[uint8_t(c)];
        bad |= t;
        v = (v << 6) | (t & 63);
    }
    if (bad & 0x80) return false;
    out = v << (6 * (10 - b.size()));
    return true;
}

uint64_t key_slow(std::string_view b) {
    uint64_t k;
    if (pack(b, k)) return k;
    auto& in = interner();
    any_interned.store(true, std::memory_order_relaxed);
    std::lock_guard g(in.mu);
    auto [it, fresh] = in.map.try_emplace(std::string(b), INTERNED | in.list.size());
    if (fresh) in.list.emplace_back(b);
    return it->second;
}

void key_append(uint64_t k, std::string& out) {
    if (k & INTERNED) {
        auto& in = interner();
        std::lock_guard g(in.mu);
        out += in.list[k & ~INTERNED];
        return;
    }
    for (int i = 0; i < 10; ++i) {
        uint64_t c = (k >> (6 * (9 - i))) & 63;
        if (!c) break;
        out.push_back(ALPHA[c]);
    }
}

std::string key_bytes(uint64_t k) {
    std::string s;
    key_append(k, s);
    return s;
}

int cmp_key(uint64_t a, uint64_t b) {
    if (!((a | b) & INTERNED)) return a < b ? -1 : a > b;
    std::string x = key_bytes(a), y = key_bytes(b);
    int c = x.compare(y);
    return c < 0 ? -1 : c > 0;
}

void civil_from_days(int64_t z, int64_t& y, uint32_t& m, uint32_t& d) {
    z += 719468;
    int64_t era = (z >= 0 ? z : z - 146096) / 146097;
    int64_t doe = z - era * 146097;
    int64_t yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    y = yoe + era * 400;
    int64_t doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    int64_t mp = (5 * doy + 2) / 153;
    d = uint32_t(doy - (153 * mp + 2) / 5 + 1);
    m = uint32_t(mp < 10 ? mp + 3 : mp - 9);
    if (m <= 2) ++y;
}

void set_dates(int64_t d0, int64_t d1) {
    for (int64_t d = d0; d <= d1; ++d) {
        int64_t y;
        uint32_t m, dd;
        civil_from_days(d, y, m, dd);
        if (y < 0 || y > 9999) continue;
        char b[16];
        std::snprintf(b, sizeof b, "%04d-%02u-%02u", int(y), m, dd);
        auto& slot = date_tab[date_slot(uint8_t(b[6]), uint8_t(b[8]), uint8_t(b[9]))];
        if (slot[0].load(std::memory_order_relaxed) != 0) continue;
        uint64_t k;
        uint16_t dv;
        std::memcpy(&k, b, 8);
        std::memcpy(&dv, b + 8, 2);
        slot[1].store(uint64_t(dv) << 48 | uint64_t(d * DAY - MIN_DATE_MS), std::memory_order_relaxed);
        slot[0].store(k, std::memory_order_release);
    }
}

namespace {
int64_t civil_ms(uint32_t y, uint32_t mo, uint32_t d, uint32_t h, uint32_t mi, uint32_t se) {
    if (mo - 1 >= 12 || d == 0 || h > 23 || mi > 59 || se > 59) return BAD_TS;
    bool leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    uint32_t dim = mo == 2 ? 28 + leap : (mo == 4 || mo == 6 || mo == 9 || mo == 11) ? 30 : 31;
    if (d > dim) return BAD_TS;
    return (days_from_civil(y, mo, d) * 86400 + int64_t(h * 3600 + mi * 60 + se)) * 1000;
}
}  // namespace

// F1 (20 bytes) or F2 (25 bytes): every fixed character and digit checked with two 16-byte compares.
int64_t iso_full(const uint8_t* s, size_t l) {
    bool f2 = l == 25;
    __m128i v0 = _mm_loadu_si128(reinterpret_cast<const __m128i*>(s));
    __m128i v1 = _mm_loadu_si128(reinterpret_cast<const __m128i*>(s + l - 16));
    __m128i z = _mm_set1_epi8('0'), nine = _mm_set1_epi8(9);
    __m128i d0 = _mm_sub_epi8(v0, z), d1 = _mm_sub_epi8(v1, z);
    uint32_t dig0 = uint32_t(_mm_movemask_epi8(_mm_cmpeq_epi8(_mm_min_epu8(d0, nine), d0)));
    uint32_t dig1 = uint32_t(_mm_movemask_epi8(_mm_cmpeq_epi8(_mm_min_epu8(d1, nine), d1)));
    static const char P0[17] = "0000-00-00T00:00";
    uint32_t p0 = uint32_t(_mm_movemask_epi8(_mm_cmpeq_epi8(v0, _mm_loadu_si128(reinterpret_cast<const __m128i*>(P0)))));
    // lanes: digits 0-3,5,6,8,9,11,12,14,15; fixed 4,7,10,13
    if ((dig0 & 0xDB6F) != 0xDB6F || (p0 & 0x2490) != 0x2490) return BAD_TS;
    int64_t off = 0;
    if (!f2) {
        if ((dig1 & 0x6000) != 0x6000 || s[16] != ':' || s[19] != 'Z') return BAD_TS;
    } else {
        if ((dig1 & 0xDB00) != 0xDB00 || s[16] != ':' || s[22] != ':') return BAD_TS;
        int64_t sign;
        if (s[19] == '+') sign = 1;
        else if (s[19] == '-') sign = -1;
        else return BAD_TS;
        uint32_t oh = dg(s[20]) * 10 + dg(s[21]), om = dg(s[23]) * 10 + dg(s[24]);
        if (oh > 23 || om > 59) return BAD_TS;
        off = sign * int64_t(oh * 60 + om) * 60000;
    }
    __m128i pairs = _mm_shuffle_epi8(d0, _mm_setr_epi8(0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, -1, -1, -1, -1));
    __m128i m = _mm_maddubs_epi16(pairs, _mm_set1_epi16(0x010A));
    alignas(16) uint16_t w[8];
    _mm_store_si128(reinterpret_cast<__m128i*>(w), m);
    uint32_t se = dg(s[17]) * 10 + dg(s[18]);
    int64_t r = civil_ms(uint32_t(w[0]) * 100 + w[1], w[2], w[3], w[4], w[5], se);
    return r == BAD_TS ? BAD_TS : r - off;
}

namespace {
int64_t parse_iso_ref(const uint8_t* s, size_t l) {
    uint32_t ds[14] = {dg(s[0]), dg(s[1]), dg(s[2]), dg(s[3]), dg(s[5]), dg(s[6]), dg(s[8]),
                       dg(s[9]), dg(s[11]), dg(s[12]), dg(s[14]), dg(s[15]), dg(s[17]), dg(s[18])};
    bool bad = s[7] != '-' || s[10] != 'T' || s[13] != ':' || s[16] != ':';
    for (uint32_t d : ds) bad |= d >= 10;
    if (bad) return BAD_TS;
    uint32_t y = ds[0] * 1000 + ds[1] * 100 + ds[2] * 10 + ds[3];
    int64_t t = civil_ms(y, ds[4] * 10 + ds[5], ds[6] * 10 + ds[7], ds[8] * 10 + ds[9], ds[10] * 10 + ds[11], ds[12] * 10 + ds[13]);
    if (t == BAD_TS) return BAD_TS;
    if (l == 20) return s[19] == 'Z' ? t : BAD_TS;
    int64_t sign;
    if (s[19] == '+') sign = 1;
    else if (s[19] == '-') sign = -1;
    else return BAD_TS;
    uint32_t a = dg(s[20]), b = dg(s[21]), c = dg(s[23]), e = dg(s[24]);
    if (s[22] != ':' || a >= 10 || b >= 10 || c >= 10 || e >= 10) return BAD_TS;
    uint32_t oh = a * 10 + b, om = c * 10 + e;
    if (oh > 23 || om > 59) return BAD_TS;
    return t - sign * int64_t(oh * 60 + om) * 60000;
}
}  // namespace

int64_t parse_ts_ref(std::string_view sv) {
    const uint8_t* s = reinterpret_cast<const uint8_t*>(sv.data());
    size_t l = sv.size();
    if ((l == 20 || l == 25) && s[4] == '-') return parse_iso_ref(s, l);
    if (l == 0) return BAD_TS;
    int64_t v = 0;
    for (size_t i = 0; i < l; ++i) {
        uint32_t d = dg(s[i]);
        if (d >= 10) return BAD_TS;
        if (__builtin_mul_overflow(v, 10, &v) || __builtin_add_overflow(v, int64_t(d), &v)) return BAD_TS;
    }
    return v;
}

void RefMap::build(const std::vector<std::pair<std::string, uint32_t>>& items) {
    size_t cap = 16;
    while (cap < items.size() * 2) cap *= 2;
    keys.assign(cap, 0);
    vals.assign(cap, 0);
    mask = cap - 1;
    for (auto& [s, v] : items) {
        uint64_t k;
        if (!pack(s, k) || k == 0) {
            longs.try_emplace(s, v);
            continue;
        }
        size_t h = size_t(mix(k)) & mask;
        while (keys[h] != 0 && keys[h] != k) h = (h + 1) & mask;
        if (keys[h] == 0) {
            keys[h] = k;
            vals[h] = v;
        }
    }
}
