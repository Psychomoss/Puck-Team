// Unit tests: timestamps, scanner, keys and prices against plain scalar references.
#include <cstdio>
#include <cstdlib>
#include <string>
#include <vector>

#include "../src/parse.hpp"

static int failures = 0;
#define CHECK(c, ...)                                   \
    do {                                                \
        if (!(c)) {                                     \
            ++failures;                                 \
            std::fprintf(stderr, "FAIL %s:%d: ", __FILE__, __LINE__); \
            std::fprintf(stderr, __VA_ARGS__);          \
            std::fprintf(stderr, "\n");                 \
        }                                               \
    } while (0)

static uint64_t seed = 12345;
static uint64_t rnd() {
    seed ^= seed << 13;
    seed ^= seed >> 7;
    seed ^= seed << 17;
    return seed;
}

static void register_dates() {
    set_dates(days_from_civil(2028, 2, 1), days_from_civil(2028, 3, 5));
    set_dates(days_from_civil(2026, 9, 25), days_from_civil(2026, 10, 10));
    set_dates(days_from_civil(2027, 12, 20), days_from_civil(2028, 1, 3));
}

// The same field through Line::ts, padded so every fast path is in bounds.
static int64_t line_ts(const std::string& s) {
    std::string buf(16, 'x');
    buf += s;
    buf += std::string(16, 'x');
    Line<1> l{reinterpret_cast<const uint8_t*>(buf.data()), buf.size(), {16}, {16 + s.size()}};
    return l.ts(0);
}

static void timestamps() {
    int64_t f1 = parse_ts("2026-10-03T23:58:10Z");
    CHECK(f1 == 1791071890000, "f1 %lld", (long long)f1);
    CHECK(parse_ts("2026-10-03T19:58:10-04:00") == f1, "f2");
    CHECK(parse_ts("1791071890000") == f1, "f3");
    CHECK(parse_ts("0001791071890000") == f1, "f3 zeros");
    CHECK(parse_ts("00000000000000000001791071890000") == f1, "f3 long zeros");
    CHECK(parse_ts("2026-10-04T01:30:00+02:00") == parse_ts("2026-10-03T23:30:00Z"), "offset date change");
    CHECK(parse_ts("2026-10-03T23:58:10-00:00") == f1, "-00:00");
    CHECK(parse_ts("2028-02-29T00:00:00Z") == days_from_civil(2028, 2, 29) * DAY, "leap");
    CHECK(parse_ts("1970-01-02T00:00:00Z") == DAY, "epoch+1");
    CHECK(parse_ts("1969-12-31T00:00:00Z") == -DAY, "epoch-1");
    CHECK(parse_ts("0000-01-01T00:00:00Z") == -62167219200000, "year 0");
    CHECK(parse_ts("9999-12-31T23:59:59Z") == 253402300799000, "year 9999");
    for (const char* bad : {"", "2026-10-03T23:58:10.000Z", "2026-10-03T23:58:10z", "2026-10-03t23:58:10Z",
                            "2026-10-03 23:58:10Z", "2026-10-03T23:58:10", "1791071890000.0", "1.79e12", "-5",
                            "2026-13-03T23:58:10Z", "2026-02-30T23:58:10Z", "2027-02-29T00:00:00Z", "2026-10-03T24:00:00Z",
                            "2026-10-03T23:59:60Z", "2026-10-03T23:58:10+24:00", "2026-10-03T23:58:10+05:60",
                            "2026-10-03T23:58:10+0200", "2026-10-03T23:58:10Z\r", "99999999999999999999999",
                            "\xef\xbc\x92\xef\xbc\x90" "26-10-03T23:58:10Z", "2026-10-03T23:58:10+02:00\r", "2100-02-29T00:00:00Z",
                            "2026-00-03T23:58:10Z", "2026-10-00T23:58:10Z", "2026-10-03T23:58:10+2:00 ", "12345678901234567890",
                            "2026-10-03T23:58:1aZ", "2026-10-03T23:58:10*02:00", "2026-10-03T23:5:10Z0"}) {
        CHECK(parse_ts(bad) == BAD_TS, "bad accepted: %s", bad);
        CHECK(line_ts(bad) == BAD_TS, "bad accepted (line): %s", bad);
    }
    CHECK(parse_ts("2000-02-29T00:00:00Z") != BAD_TS, "2000 leap");
    CHECK(parse_ts("1234567890123456789") == 1234567890123456789, "19 digits");
}

static void fuzz_timestamps() {
    const char* alphabet = "0123456789-T:Z+zt .e\r";
    const char* bases[] = {"2026-10-03T23:58:10Z", "2028-02-29T00:00:00+12:00", "1791071890000", "0001791071890000",
                           "2027-12-31T23:59:59-10:00", "9999-12-31T23:59:59Z", "2026-10-01T00:00:00Z", "2028-02-28T23:59:59-00:00"};
    int ok = 0;
    for (int it = 0; it < 400000; ++it) {
        std::string v = bases[rnd() % 8];
        for (uint64_t k = rnd() % 3; k > 0; --k) v[rnd() % v.size()] = alphabet[rnd() % std::strlen(alphabet)];
        if (rnd() % 7 == 0) v.resize(rnd() % (v.size() + 1));
        int64_t want = parse_ts_ref(v);
        CHECK(parse_ts(v) == want, "fuzz %s", v.c_str());
        CHECK(line_ts(v) == want, "fuzz line %s", v.c_str());
        ok += want != BAD_TS;
    }
    CHECK(ok > 1000, "too few valid");
    for (int y : {0, 1, 1969, 1970, 2000, 2024, 2026, 2027, 2028, 2100, 9999})
        for (int m = 0; m <= 13; ++m)
            for (int d : {0, 1, 28, 29, 30, 31, 32})
                for (const char* t : {"00:00:00Z", "23:59:59Z", "24:00:00Z", "12:60:00Z", "12:00:60Z", "05:06:07+23:59",
                                      "05:06:07-00:00", "05:06:07+24:00", "05:06:07+05:60", "05:06:07*05:00", "1:06:07Z00"}) {
                    char b[40];
                    std::snprintf(b, sizeof b, "%04d-%02d-%02dT%s", y, m, d, t);
                    CHECK(parse_ts(b) == parse_ts_ref(b), "grid %s", b);
                    CHECK(line_ts(b) == parse_ts_ref(b), "grid line %s", b);
                }
    for (const char* v : {"1", "0", "9999999999999999", "99999999999999999", "999999999999999999", "9223372036854775807",
                          "9223372036854775808", "0000000000000000000000001"})
        CHECK(parse_ts(v) == parse_ts_ref(v), "num %s", v);
}

static void civil() {
    for (int64_t d = -719528; d < 2932896; ++d) {
        int64_t y;
        uint32_t m, dd;
        civil_from_days(d, y, m, dd);
        if (days_from_civil(uint32_t(y), m, dd) != d) {
            CHECK(false, "civil round trip %lld", (long long)d);
            break;
        }
    }
}

static void prices() {
    CHECK(parse_price("0250") == 250, "0250");
    CHECK(parse_price("000000000000000000000250") == 250, "zeros");
    CHECK(parse_price("10000000") == 10000000, "max");
    for (const char* bad : {"10000001", "0", "", "19.99", "-5", "+5", "1000\r", "99999999999999999999999"})
        CHECK(parse_price(bad) == 0, "bad price %s", bad);
}

template <size_t N>
static std::vector<std::vector<std::string>> collect(const std::string& d, bool blocks) {
    std::vector<std::vector<std::string>> v;
    auto f = [&](const Line<N>& l) {
        std::vector<std::string> r;
        for (size_t i = 0; i < N; ++i) r.emplace_back(l.get(i));
        v.push_back(r);
    };
    auto* p = reinterpret_cast<const uint8_t*>(d.data());
    if (blocks) scan_blocks<N>(p, d.size(), f);
    else scan<N>(p, d.size(), f);
    return v;
}

static void scanning() {
    using V = std::vector<std::vector<std::string>>;
    CHECK((collect<2>("\"u4\",u1", false) == V{{"u4", "u1"}}), "quoted");
    CHECK((collect<2>("\"\",u1", false) == V{{"", "u1"}}), "empty quoted");
    for (const char* bad : {"u1,u5\"", "u1,u2,u3", "u1", "\",u1", "\"\"\",u1", "\"a\"b\",u1", "a\"\"b,u1"})
        CHECK(collect<2>(bad, false).empty(), "malformed accepted: %s", bad);
    CHECK((collect<2>(" u1 ,\"\r\"", false) == V{{" u1 ", "\r"}}), "spaces and cr");
    std::string longf(150, 'y');
    std::string data = "a,b\n\n\"c\",\"d\"\nbad\ne,f,g\n" + longf + ",x\nh,\"i\"\"\nj,k\r\nl,m";
    CHECK((collect<2>(data, false) == V{{"a", "b"}, {"c", "d"}, {longf, "x"}, {"j", "k\r"}, {"l", "m"}}), "mixed");
    for (int pad = 0; pad < 70; ++pad) {
        std::string d = std::string(pad, 'z') + "\n\"u\",\"v\"\nw,x\"\n";
        CHECK((collect<2>(d, false) == V{{"u", "v"}}), "pad %d", pad);
    }
    const char* alpha = "ab,,\"\"\n\r 0";
    for (int round = 0; round < 3000; ++round) {
        std::string d;
        for (uint64_t n = rnd() % 400; n > 0; --n) d += alpha[rnd() % std::strlen(alpha)];
        if (round % 3 == 0)
            for (uint64_t k = rnd() % 8; k > 0; --k)
                d.insert(rnd() % (d.size() + 1), "\"e1\",u2,1791071890000,2026-10-03T23:58:10Z,\"view\",p1,\"\"\n");
        CHECK(collect<2>(d, false) == collect<2>(d, true), "scan2 round %d", round);
        CHECK(collect<4>(d, false) == collect<4>(d, true), "scan4 round %d", round);
        CHECK(collect<7>(d, false) == collect<7>(d, true), "scan7 round %d", round);
    }
}

static void keys() {
    const char* ids[] = {"", "0", "U1", "_", "u1", "u10", "u2", "zzzzzzzzzz", "zzzzzzzzzzz"};
    for (size_t i = 0; i + 1 < 9; ++i) CHECK(cmp_key(key_slow(ids[i]), key_slow(ids[i + 1])) < 0, "order %s %s", ids[i], ids[i + 1]);
    for (const char* id : {" u1", "u1 ", "a_very_long_identifier", "\xd9\xa2", "u-1", "e3\r"}) {
        CHECK(key_bytes(key_slow(id)) == id, "round trip %s", id);
        CHECK(key_slow(id) & INTERNED, "interned %s", id);
        CHECK(key_slow(id) == key_slow(id), "stable %s", id);
    }
    CHECK(key_bytes(key_slow("Az_09z")) == "Az_09z", "packed round trip");
    CHECK(cmp_key(key_slow(" u1"), key_slow("U1")) < 0, "space first");
    CHECK(cmp_key(key_slow("u1 "), key_slow("u1")) > 0, "longer later");
    CHECK(cmp_key(key_slow("u1 "), key_slow("u10")) < 0, "space before digit");
    // SIMD packing against the scalar packer, every byte value in every position.
    uint8_t buf[32];
    for (int it = 0; it < 300000; ++it) {
        size_t l = rnd() % 11;
        for (auto& b : buf) b = uint8_t(rnd());
        for (size_t i = 0; i < l; ++i) {
            uint64_t r = rnd() % 10;
            if (r < 7) buf[i] = uint8_t("0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz"[rnd() % 63]);
        }
        bool ok;
        uint64_t k = pack16(buf, l, ok), want;
        bool wok = pack(std::string_view(reinterpret_cast<char*>(buf), l), want);
        CHECK(ok == wok && (!ok || k == want), "pack16 len %zu", l);
    }
    for (int c = 0; c < 256; ++c) {
        uint8_t b[16] = {uint8_t(c)};
        bool ok;
        uint64_t k = pack16(b, 1, ok), want;
        bool wok = pack(std::string_view(reinterpret_cast<char*>(b), 1), want);
        CHECK(ok == wok && (!ok || k == want), "pack16 byte %d", c);
    }
}

int main() {
    register_dates();
    timestamps();
    fuzz_timestamps();
    civil();
    prices();
    scanning();
    keys();
    if (failures) {
        std::fprintf(stderr, "%d failures\n", failures);
        return 1;
    }
    std::puts("all tests passed");
}
