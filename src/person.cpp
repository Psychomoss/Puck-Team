#include <bit>

#include "common.hpp"

namespace {
constexpr int64_t GAP = 1'800'000;
constexpr uint32_t ORGANIC = 0;

struct SE {
    uint64_t key;
    uint32_t price, cat;
};
struct SC {
    uint64_t key, ckey;
    uint32_t camp;
};

/// Dense ids for the person keys of one bucket.
struct Ids {
    std::vector<uint64_t> keys;
    std::vector<uint32_t> table;
    size_t mask = 0;
    void reset(size_t n) {
        size_t cap = std::max<size_t>(64, std::bit_ceil(std::max<size_t>(1, n / 4)));
        keys.clear();
        table.assign(cap, UINT32_MAX);
        mask = cap - 1;
    }
    std::pair<size_t, uint32_t> find(uint64_t k) const {
        size_t h = size_t(mix(k)) & mask;
        for (;;) {
            uint32_t t = table[h];
            if (t == UINT32_MAX || keys[t] == k) return {h, t};
            h = (h + 1) & mask;
        }
    }
    uint32_t insert(uint64_t k) {
        auto [h, t] = find(k);
        if (t != UINT32_MAX) return t;
        table[h] = uint32_t(keys.size());
        keys.push_back(k);
        if (keys.size() * 2 > table.size()) {
            size_t cap = table.size() * 2;
            table.assign(cap, UINT32_MAX);
            mask = cap - 1;
            for (size_t i = 0; i < keys.size(); ++i) table[find(keys[i]).first] = uint32_t(i);
        }
        return uint32_t(keys.size() - 1);
    }
};

/// LSD radix sort on key(x), 11-bit digits up to `bits`, skipping digits that are all equal.
constexpr size_t RD = 11, RR = size_t(1) << RD, MAXPASS = 6;
using Hist = std::array<std::array<uint32_t, RR>, MAXPASS>;

/// LSD radix sort of a[0..n) on key(x), 11-bit digits up to `bits`, skipping digits that are all
/// equal; ping-pongs between a and tmp and returns where the sorted data ended up.
template <class T, class K>
T* radix(T* a, T* tmp, size_t n, uint32_t bits, Hist& hist, K key) {
    size_t npass = (bits + RD - 1) / RD;
    if (npass > MAXPASS) std::abort();
    for (size_t p = 0; p < npass; ++p) hist[p].fill(0);
    for (size_t i = 0; i < n; ++i) {
        uint64_t k = key(a[i]);
        for (size_t p = 0; p < npass; ++p) ++hist[p][(k >> (p * RD)) & (RR - 1)];
    }
    for (size_t p = 0; p < npass; ++p) {
        auto& h = hist[p];
        bool trivial = false;
        for (uint32_t c : h) trivial |= c == n;
        if (trivial) continue;
        uint32_t sum = 0;
        for (auto& c : h) {
            uint32_t v = c;
            c = sum;
            sum += v;
        }
        for (size_t i = 0; i < n; ++i) tmp[h[(key(a[i]) >> (p * RD)) & (RR - 1)]++] = a[i];
        std::swap(a, tmp);
    }
    return a;
}

uint32_t bits_for(uint64_t v) { return 64 - uint32_t(std::countl_zero(v)); }

int64_t floor_div(int64_t a, int64_t b) { return a >= 0 ? a / b : -((-a + b - 1) / b); }

UserRow finish(UserRow r, std::vector<uint64_t>& catsum) {
    if (r.revenue == 0) return r;
    uint64_t best = 0;
    for (size_t i = 0; i < catsum.size(); ++i) {
        if (catsum[i] > best) {
            best = catsum[i];
            r.top = uint32_t(i);
        }
        catsum[i] = 0;
    }
    return r;
}
}  // namespace

struct PersonScratch {
    Ids ids;
    RawBuf<SE> se, se2;
    RawBuf<SC> sc, sc2;
    Hist hist;
    std::vector<int64_t> starts, ia, ib;
    std::vector<uint32_t> sdays, run_end, seen, active;
    std::vector<uint64_t> catsum;
    uint32_t stamp = 0;
};

PersonScratch* new_person_scratch(size_t max_events, size_t max_clicks) {
    auto* s = new PersonScratch;
    s->se.get(max_events);
    s->se2.get(max_events);
    s->sc.get(max_clicks);
    s->sc2.get(max_clicks);
    return s;
}

void process_bucket(const PersonCtx& ctx, const std::vector<const BlockVec<PEv>*>& evs,
                    const std::vector<const BlockVec<PCl>*>& cls, const std::vector<std::vector<uint64_t>>& sides,
                    PersonScratch& s, size_t ncamp, size_t ncat, std::vector<UserRow>& rows, Cred* cr, uint32_t* asst) {
    size_t n = 0;
    for (auto* v : evs) n += v->size();
    if (n == 0) return;
    Ids& ids = s.ids;
    ids.reset(n);
    SE* sea = s.se.get(n);
    SE* seb = s.se2.get(n);
    size_t nse = 0;
    for (auto* v : evs)
        v->each([&](const PEv& e) {
            uint64_t pid = ids.insert(e.pkey);
            uint32_t price = 0, cat = 0;
            if (e.pc != 0) {
                uint32_t h = e.pc - 1;
                uint64_t x = sides[h >> 26][h & ((1u << 26) - 1)];
                price = uint32_t(x);
                cat = uint32_t(x >> 32);
            }
            sea[nse++] = {pid << ctx.ev_bits | e.ts, price, cat};
        });
    uint32_t pid_bits = bits_for(ids.keys.size());
    const SE* se = radix(sea, seb, nse, ctx.ev_bits + pid_bits, s.hist, [](const SE& x) { return x.key; });
    size_t ncl_all = 0;
    for (auto* v : cls) ncl_all += v->size();
    SC* sca = s.sc.get(ncl_all);
    SC* scb = s.sc2.get(ncl_all);
    size_t nsc = 0;
    for (auto* v : cls)
        v->each([&](const PCl& c) {
            uint32_t t = ids.find(c.pkey).second;
            if (t != UINT32_MAX) sca[nsc++] = {uint64_t(t) << ctx.cl_bits | c.ts, c.ckey, c.camp};
        });
    const SC* sc = radix(sca, scb, nsc, ctx.cl_bits + pid_bits, s.hist, [](const SC& x) { return x.key; });
    if (s.seen.size() != ncamp) {
        s.seen.assign(ncamp, 0);
        s.ia.assign(ncamp, 0);
        s.ib.assign(ncamp, 0);
        s.catsum.assign(ncat, 0);
    }

    const int64_t ws = ctx.ws, cws = ctx.cws;
    const size_t nd = ctx.ndays;
    const uint64_t em = (uint64_t(1) << ctx.ev_bits) - 1, cm = (uint64_t(1) << ctx.cl_bits) - 1;
    auto ets = [&](const SE& x) { return ws + int64_t(x.key & em); };
    auto cts = [&](const SC& x) { return cws + int64_t(x.key & cm); };
    size_t ci = 0, i = 0;
    while (i < nse) {
        uint64_t pid = se[i].key >> ctx.ev_bits;
        size_t pe = i + 1;
        while (pe < nse && se[pe].key >> ctx.ev_bits == pid) ++pe;
        while (ci < nsc && sc[ci].key >> ctx.cl_bits < pid) ++ci;
        size_t ce = ci;
        while (ce < nsc && sc[ce].key >> ctx.cl_bits == pid) ++ce;
        const SE* ev = se + i;
        size_t nev = pe - i;
        const SC* cl = sc + ci;
        size_t ncl = ce - ci;
        uint64_t pkey = ids.keys[pid];
        s.starts.clear();
        s.sdays.clear();
        size_t lo = 0, hi = 0;
        UserRow row{pkey, INT32_MIN, 0, 0, 0, UINT32_MAX};
        size_t a = 0;
        while (a < nev) {
            int64_t start = ets(ev[a]);
            size_t j = a + 1;
            int64_t prev = start;
            uint64_t rev = ev[a].price;
            while (j < nev) {
                int64_t t = ets(ev[j]);
                if (t - prev > GAP) break;
                prev = t;
                rev += ev[j].price;
                ++j;
            }
            int64_t day = floor_div(start, DAY);
            size_t di = size_t(day - ctx.day0);
            while (hi < ncl && cts(cl[hi]) <= start) ++hi;
            while (lo < hi && cts(cl[lo]) < start - WEEK) ++lo;
            uint32_t camp = ORGANIC;
            if (hi > lo) {
                int64_t t = cts(cl[hi - 1]);
                size_t best = hi - 1, k = hi - 1;
                while (k > lo && cts(cl[k - 1]) == t) {
                    --k;
                    if (cmp_key(cl[k].ckey, cl[best].ckey) < 0) best = k;
                }
                camp = cl[best].camp;
            }
            Cred& c = cr[camp * nd + di];
            ++c.sessions;
            c.revenue += rev;
            c.purchasing += rev > 0;
            s.starts.push_back(start);
            s.sdays.push_back(uint32_t(di));
            if (row.day != int32_t(day)) {
                if (row.day != INT32_MIN) rows.push_back(finish(row, s.catsum));
                row = UserRow{pkey, int32_t(day), 0, 0, 0, UINT32_MAX};
            }
            if (rev > 0)
                for (size_t q = a; q < j; ++q) s.catsum[ev[q].cat] += ev[q].price;
            ++row.sessions;
            row.events += uint32_t(j - a);
            row.revenue += rev;
            a = j;
        }
        rows.push_back(finish(row, s.catsum));

        // Assists: campaign c assists every session whose start lies in the union of [t, t + 7d]
        // over c's clicks t; each merged interval adds its sessions per day.
        if (ncl) {
            size_t ns = s.starts.size();
            s.run_end.assign(ns, uint32_t(ns));
            for (size_t k = ns - 1; k-- > 0;) s.run_end[k] = s.sdays[k] == s.sdays[k + 1] ? s.run_end[k + 1] : uint32_t(k + 1);
            if (++s.stamp == 0) {
                std::fill(s.seen.begin(), s.seen.end(), 0);
                s.stamp = 1;
            }
            s.active.clear();
            auto flush = [&](uint32_t c, int64_t lo_t, int64_t hi_t) {
                const auto& st = s.starts;
                size_t x = size_t(std::lower_bound(st.begin(), st.end(), lo_t) - st.begin());
                size_t y = size_t(std::upper_bound(st.begin(), st.end(), hi_t) - st.begin());
                while (x < y) {
                    size_t k = std::min<size_t>(s.run_end[x], y);
                    asst[c * nd + s.sdays[x]] += uint32_t(k - x);
                    x = k;
                }
            };
            for (size_t q = 0; q < ncl; ++q) {
                uint32_t c = cl[q].camp;
                int64_t t = cts(cl[q]);
                if (s.seen[c] != s.stamp) {
                    s.seen[c] = s.stamp;
                    s.ia[c] = t;
                    s.ib[c] = t + WEEK;
                    s.active.push_back(c);
                } else if (t > s.ib[c]) {
                    flush(c, s.ia[c], s.ib[c]);
                    s.ia[c] = t;
                    s.ib[c] = t + WEEK;
                } else {
                    s.ib[c] = t + WEEK;
                }
            }
            for (uint32_t c : s.active) flush(c, s.ia[c], s.ib[c]);
        }
        i = pe;
        ci = ce;
    }
}
