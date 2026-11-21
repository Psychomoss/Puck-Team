#include <fcntl.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <unistd.h>

#include <algorithm>
#include <bit>
#include <unordered_map>
#include <chrono>
#include <cstdio>
#include <filesystem>
#include <memory>
#include <numeric>

#include "common.hpp"

namespace fs = std::filesystem;

namespace {
constexpr size_t CHUNK = 1 << 20;

struct Timer {
    std::chrono::steady_clock::time_point t;
    double u, s;
    long f;
    static void cpu(double& u, double& s, long& f) {
        rusage r{};
        getrusage(RUSAGE_SELF, &r);
        u = r.ru_utime.tv_sec * 1e3 + r.ru_utime.tv_usec / 1e3;
        s = r.ru_stime.tv_sec * 1e3 + r.ru_stime.tv_usec / 1e3;
        f = r.ru_minflt;
    }
    Timer() : t(std::chrono::steady_clock::now()) { cpu(u, s, f); }
    void lap(const char* what) {
        auto now = std::chrono::steady_clock::now();
        double u2, s2;
        long f2;
        cpu(u2, s2, f2);
        std::fprintf(stderr, "%12s: %8.1f ms   (cpu user %7.0f sys %6.0f, faults %6ld)\n", what,
                     std::chrono::duration<double, std::milli>(now - t).count(), u2 - u, s2 - s, f2 - f);
        t = now;
        u = u2;
        s = s2;
        f = f2;
    }
};

std::string read_file(const fs::path& p) {
    std::string s;
    int fd = ::open(p.c_str(), O_RDONLY | O_CLOEXEC);
    if (fd < 0) return s;
    struct stat st{};
    fstat(fd, &st);
    s.resize(size_t(st.st_size));
    size_t got = 0;
    while (got < s.size()) {
        ssize_t r = pread(fd, s.data() + got, s.size() - got, off_t(got));
        if (r <= 0) break;
        got += size_t(r);
    }
    s.resize(got);
    close(fd);
    return s;
}

std::string json_str(const std::string& text, const char* name) {
    size_t at = text.find(std::string("\"") + name + "\"");
    if (at == std::string::npos) throw std::runtime_error(name);
    at += std::strlen(name) + 2;
    size_t a = text.find('"', at) + 1, b = text.find('"', a);
    return text.substr(a, b - a);
}

template <size_t N, class F>
void read_small_csv(const fs::path& p, F&& f) {
    std::string d = read_file(p);
    size_t h = d.find('\n');
    if (h == std::string::npos) return;
    scan<N>(reinterpret_cast<const uint8_t*>(d.data()) + h + 1, d.size() - h - 1, [&](const Line<N>& l) {
        std::array<std::string, N> fs;
        for (size_t i = 0; i < N; ++i) fs[i] = std::string(l.get(i));
        f(fs);
    });
}

void pread_exact(int fd, uint8_t* p, size_t n, size_t off) {
    while (n) {
        ssize_t r = pread(fd, p, n, off_t(off));
        if (r <= 0) std::abort();
        p += r;
        n -= size_t(r);
        off += size_t(r);
    }
}

int64_t floor_div(int64_t a, int64_t b) { return a >= 0 ? a / b : -((-a + b - 1) / b); }

bool use_pread() {
    const char* v = std::getenv("INPUT_MODE");
    return v && std::string_view(v) == "pread";
}
}  // namespace

Input Input::open(const std::string& dir, bool reverse) {
    std::vector<fs::path> paths;
    std::error_code ec;
    for (auto& e : fs::directory_iterator(dir, ec))
        if (e.path().extension() == ".csv" && fs::is_regular_file(e.path())) paths.push_back(e.path());
    std::sort(paths.begin(), paths.end());
    if (reverse) std::reverse(paths.begin(), paths.end());
    Input in;
    for (auto& p : paths) {
        int fd = ::open(p.c_str(), O_RDONLY | O_CLOEXEC);
        if (fd < 0) std::abort();
        struct stat st{};
        fstat(fd, &st);
        size_t len = size_t(st.st_size);
        if (len == 0) {
            close(fd);
            continue;
        }
        const uint8_t* map = nullptr;
        if (!use_pread()) {
            void* m = mmap(nullptr, len, PROT_READ, MAP_PRIVATE, fd, 0);
            if (m == MAP_FAILED) std::abort();
            map = static_cast<const uint8_t*>(m);
        }
        in.bytes += len;
        for (size_t a = 0; a < len; a += CHUNK) in.chunks.push_back({in.files.size(), a, std::min(a + CHUNK, len)});
        in.files.push_back({fd, len, map});
    }
    if (reverse) std::reverse(in.chunks.begin(), in.chunks.end());
    return in;
}

std::pair<const uint8_t*, size_t> Input::read(size_t i, std::vector<uint8_t>& buf) const {
    auto [fi, a, b] = chunks[i];
    auto [fd, len, map] = files[fi];
    if (map) {
        size_t first;
        if (a == 0) {
            auto* p = static_cast<const uint8_t*>(std::memchr(map, '\n', len));
            if (!p) return {map, 0};
            first = size_t(p - map) + 1;
        } else if (map[a - 1] == '\n') {
            first = a;
        } else {
            auto* p = static_cast<const uint8_t*>(std::memchr(map + a, '\n', len - a));
            if (!p) return {map, 0};
            first = size_t(p - map) + 1;
        }
        if (first >= b) return {map, 0};
        // The last line starting before b ends at the first newline at or after b - 1.
        auto* p = static_cast<const uint8_t*>(std::memchr(map + b - 1, '\n', len - (b - 1)));
        size_t last = p ? size_t(p - map) : len;
        return {map + first, last - first};
    }
    size_t start = a ? a - 1 : 0;
    size_t end = std::min(b + 4096, len);
    buf.resize(end - start);
    pread_exact(fd, buf.data(), end - start, start);
    auto grow = [&] {
        size_t ne = std::min(end + (end - start), len), old = buf.size();
        buf.resize(ne - start);
        pread_exact(fd, buf.data() + old, ne - end, end);
        end = ne;
    };
    // First line start at or after a (after the header in the file's first chunk).
    size_t first;
    for (;;) {
        size_t from = a == 0 ? 0 : 1;
        if (a > 0 && buf[0] == '\n') {
            first = 1;
            break;
        }
        from = std::min(from, buf.size());
        auto* p = static_cast<uint8_t*>(std::memchr(buf.data() + from, '\n', buf.size() - from));
        if (p) {
            first = size_t(p - buf.data()) + 1;
            break;
        }
        if (end < len) grow();
        else return {buf.data(), 0};
    }
    if (start + first >= b) return {buf.data(), 0};
    // The last line starting before b ends at the first newline at or after b - 1.
    size_t last;
    for (;;) {
        size_t from = b - 1 - start;
        auto* p = static_cast<uint8_t*>(std::memchr(buf.data() + from, '\n', buf.size() - from));
        if (p) {
            last = size_t(p - buf.data()) + 1;
            break;
        }
        if (end < len) grow();
        else {
            last = buf.size();
            break;
        }
    }
    return {buf.data() + first, last - first};
}

int main() {
    auto t0 = std::chrono::steady_clock::now();
    Timer tm;
    auto env = [](const char* k, const char* d) {
        const char* v = std::getenv(k);
        return std::string(v && *v ? v : d);
    };
    std::string data = env("DATA_DIR", "/data"), outdir = env("OUTPUT_DIR", "/output");
    size_t threads = 0;
    if (const char* v = std::getenv("NCPU")) threads = size_t(std::atol(v));
    if (!threads) threads = std::max(1u, std::thread::hardware_concurrency());
    threads = std::clamp<size_t>(threads, 1, 62);
    bool reverse = env("REVERSE_FILES", "") == "1";
    // Queue readahead for every input file up front, so a cold page cache is read at full device depth.
    std::thread([data] {
        for (const char* d : {"links", "events", "clicks"}) {
            std::error_code ec;
            for (auto& e : fs::directory_iterator(fs::path(data) / d, ec)) {
                int fd = ::open(e.path().c_str(), O_RDONLY | O_CLOEXEC);
                if (fd < 0) continue;
                posix_fadvise(fd, 0, 0, POSIX_FADV_WILLNEED);
                close(fd);
            }
        }
    }).detach();

    std::string manifest = read_file(fs::path(data) / "manifest.json");
    int64_t ws = parse_ts(json_str(manifest, "window_start")), we = parse_ts(json_str(manifest, "window_end"));
    if (ws == BAD_TS || we == BAD_TS) throw std::runtime_error("bad manifest");
    int64_t day0 = floor_div(ws, DAY);
    set_dates(floor_div(ws - WEEK, DAY) - 2, floor_div(we, DAY) + 7);
    size_t ndays = size_t(std::max<int64_t>(1, floor_div(we - 1, DAY) - day0 + 1));

    std::vector<std::pair<std::string, std::string>> prod_raw;
    read_small_csv<2>(fs::path(data) / "products.csv", [&](auto& f) { prod_raw.push_back({f[0], f[1]}); });
    std::vector<std::string> cats;
    for (auto& p : prod_raw) cats.push_back(p.second);
    cats.push_back("unknown");
    std::sort(cats.begin(), cats.end());
    cats.erase(std::unique(cats.begin(), cats.end()), cats.end());
    auto cat_index = [&](const std::string& c) { return uint32_t(std::lower_bound(cats.begin(), cats.end(), c) - cats.begin()); };
    const uint32_t unknown = cat_index("unknown");
    RefMap products, campaigns;
    {
        std::vector<std::pair<std::string, uint32_t>> items;
        for (auto& [p, c] : prod_raw) items.push_back({p, cat_index(c)});
        // Last one wins for repeated IDs, as a map insert would.
        std::reverse(items.begin(), items.end());
        products.build(items);
    }
    std::vector<std::pair<std::string, std::string>> camp_names{{"organic", "organic"}};
    {
        std::vector<std::pair<std::string, uint32_t>> items;
        std::unordered_map<std::string, uint32_t> seen;
        read_small_csv<2>(fs::path(data) / "campaigns.csv", [&](auto& f) {
            if (seen.try_emplace(f[0], uint32_t(camp_names.size())).second) {
                items.push_back({f[0], uint32_t(camp_names.size())});
                camp_names.push_back({f[0], f[1]});
            }
        });
        campaigns.build(items);
    }
    const size_t ncamp = camp_names.size();
    tm.lap("reference");

    Input links_in = Input::open((fs::path(data) / "links").string(), reverse);
    const Persons persons = build_persons(links_in, threads);
    tm.lap("links");

    // Events: parse, validate each copy, bucket by event_id. Valid copies get their person while
    // parsing: the lookup is prefetched and resolved in batches of 32 lines, so its memory latency
    // hides behind the parsing work.
    Input ev_in = Input::open((fs::path(data) / "events").string(), reverse);
    if (we - (ws - WEEK) >= int64_t(UINT32_MAX)) throw std::runtime_error("window longer than 49 days");
    std::atomic<uint32_t> tids{0};
    struct EvState {
        std::vector<BlockVec<Ev>> bk = buckets<Ev>();
        uint32_t tid = 0;
        std::vector<uint64_t> side;
        std::vector<Ev> pend;
        std::vector<uint8_t> buf;
    };
    auto flush = [&](EvState& st) {
        for (Ev e : st.pend) {
            if (e.ts != UINT32_MAX) e.ukey = persons.get(e.ukey);
            st.bk[bucket(e.ekey)].push(e);
        }
        st.pend.clear();
    };
    auto ev_parts = par_tasks<EvState>(
        ev_in.size(), threads,
        [&] {
            EvState s;
            s.tid = tids.fetch_add(1);
            s.pend.reserve(32);
            return s;
        },
        [&](EvState& st, size_t i) {
            auto [p, n] = ev_in.read(i, st.buf);
            scan<7>(p, n, [&](const Line<7>& f) {
                if (f.empty(0)) return;
                int64_t ingest = f.ts(3);
                Ev e{f.key(0), 0, ingest == BAD_TS ? INT64_MAX : ingest, UINT32_MAX, 0};
                [&] {
                    if (f.empty(1) || f.empty(5)) return;
                    int64_t ts = f.ts(2);
                    if (ts < ws || ts >= we) return;
                    bool purchase = false;
                    uint64_t pv = 0;
                    std::string_view type = f.get(4);
                    if (type == "purchase") {
                        uint32_t price = parse_price(f.get(6));
                        if (!price) return;
                        uint32_t cat;
                        if (!products.get_field(f, 5, cat)) cat = unknown;
                        purchase = true;
                        pv = price | uint64_t(cat) << 32;
                    } else if (type != "view" && type != "cart") {
                        return;
                    }
                    if (e.ingest - ts > DAY) return;
                    if (purchase) {
                        if (st.side.size() >= (1u << 26)) std::abort();
                        e.pc = ((st.tid << 26) | uint32_t(st.side.size())) + 1;
                        st.side.push_back(pv);
                    }
                    e.ts = uint32_t(ts - ws);
                    e.ukey = f.key(1);
                    persons.prefetch_key(e.ukey);
                }();
                st.pend.push_back(e);
                if (st.pend.size() == 32) flush(st);
            });
            flush(st);
        });
    std::vector<std::vector<uint64_t>> sides(ev_parts.size());
    for (auto& t : ev_parts) sides[t.tid] = std::move(t.side);
    tm.lap("events parse");

    // Dedup per bucket (smallest ingest_ts wins), keep valid survivors, re-bucket by person.
    struct DedupState {
        std::vector<BlockVec<PEv>> out = buckets<PEv>();
        std::vector<uint32_t> table;
        std::unique_ptr<uint64_t[]> tkeys;
        size_t tcap = 0;
    };
    const size_t nt = ev_parts.size();
    auto pev_parts = par_tasks<DedupState>(NB, threads, [] { return DedupState{}; }, [&](DedupState& st, size_t b) {
        size_t n = 0;
        for (auto& t : ev_parts) n += t.bk[b].size();
        size_t cap = std::max<size_t>(16, std::bit_ceil(n * 2));
        st.table.assign(cap, UINT32_MAX);
        // Only slots whose table entry is set are ever read, so the keys stay uninitialized.
        if (st.tcap < cap) {
            st.tkeys.reset(new uint64_t[cap]);
            st.tcap = cap;
        }
        uint64_t* tkeys = st.tkeys.get();
        uint32_t* table = st.table.data();
        size_t mask = cap - 1;
        for (size_t t = 0; t < nt; ++t) {
            BlockVec<Ev>& part = ev_parts[t].bk[b];
            size_t len = part.size();
            if (len >= (1u << 24)) std::abort();
            for (size_t i = 0; i < len; ++i) {
                if (i + 16 < len) {
                    size_t h2 = size_t(mix(part[i + 16].ekey) >> 10) & mask;
                    prefetch(table + h2);
                    prefetch(tkeys + h2);
                }
                Ev& e = part[i];
                size_t h = size_t(mix(e.ekey) >> 10) & mask;
                for (;;) {
                    uint32_t s = table[h];
                    if (s == UINT32_MAX) {
                        table[h] = uint32_t(t) << 24 | uint32_t(i);
                        tkeys[h] = e.ekey;
                        break;
                    }
                    if (tkeys[h] == e.ekey) {
                        Ev& kept = ev_parts[s >> 24].bk[b][s & 0xFFFFFF];
                        if (e.ingest < kept.ingest) {
                            kept.ts = UINT32_MAX;
                            table[h] = uint32_t(t) << 24 | uint32_t(i);
                        } else {
                            e.ts = UINT32_MAX;
                        }
                        break;
                    }
                    h = (h + 1) & mask;
                }
            }
        }
        for (auto& t : ev_parts)
            t.bk[b].each([&](const Ev& e) {
                if (e.ts != UINT32_MAX) st.out[bucket(e.ukey)].push(PEv{e.ukey, e.ts, e.pc});
            });
    });
    tm.lap("dedup");

    // Clicks: duplicates are identical, so they change neither credit nor assists; no dedup needed.
    Input cl_in = Input::open((fs::path(data) / "clicks").string(), reverse);
    const int64_t cws = ws - WEEK;
    struct ClState {
        std::vector<BlockVec<PCl>> bk = buckets<PCl>();
        std::vector<PCl> tmp;
        std::vector<uint8_t> buf;
    };
    auto pcl_parts = par_tasks<ClState>(cl_in.size(), threads, [] { return ClState{}; }, [&](ClState& st, size_t i) {
        st.tmp.clear();
        auto [p, n] = cl_in.read(i, st.buf);
        scan<4>(p, n, [&](const Line<4>& f) {
            if (f.empty(0) || f.empty(1)) return;
            int64_t ts = f.ts(3);
            if (ts < cws || ts >= we) return;
            uint32_t camp;
            if (!campaigns.get_field(f, 2, camp)) return;
            st.tmp.push_back(PCl{f.key(1), f.key(0), uint32_t(ts - cws), camp});
        });
        for (size_t j = 0; j < st.tmp.size(); ++j) {
            if (j + 32 < st.tmp.size()) persons.prefetch_key(st.tmp[j + 32].pkey);
            PCl c = st.tmp[j];
            c.pkey = persons.get(c.pkey);
            st.bk[bucket(c.pkey)].push(c);
        }
    });
    tm.lap("clicks");

    // Per person bucket: sort by (person, ts), sessionize, credit, assists, aggregate.
    std::vector<size_t> order(NB), bsize(NB, 0);
    size_t max_ev = 0, max_cl = 0, all_ev = 0;
    for (size_t b = 0; b < NB; ++b) {
        size_t c = 0;
        for (auto& t : pev_parts) bsize[b] += t.out[b].size();
        for (auto& t : pcl_parts) c += t.bk[b].size();
        max_ev = std::max(max_ev, bsize[b]);
        max_cl = std::max(max_cl, c);
        all_ev += bsize[b];
    }
    std::iota(order.begin(), order.end(), 0);
    std::stable_sort(order.begin(), order.end(), [&](size_t x, size_t y) { return bsize[x] > bsize[y]; });
    const size_t ncat = cats.size();
    auto bits = [](int64_t v) { return 64 - uint32_t(std::countl_zero(uint64_t(std::max<int64_t>(v, 1)) - 1)); };
    PersonCtx ctx{ws, cws, day0, ndays, bits(we - ws), bits(we - cws)};
    if (ctx.cl_bits > 40) throw std::runtime_error("window too long");
    struct SessState {
        std::vector<UserRow> rows;
        std::vector<Cred> cr;
        std::vector<uint32_t> asst;
        PersonScratch* scratch;
    };
    auto results = par_tasks<SessState>(
        NB, threads,
        [&] {
            SessState st{{}, std::vector<Cred>(ncamp * ndays), std::vector<uint32_t>(ncamp * ndays),
                         new_person_scratch(max_ev, max_cl)};
            st.rows.reserve(all_ev / threads / 4 + 1024);
            return st;
        },
        [&](SessState& st, size_t oi) {
            size_t b = order[oi];
            std::vector<const BlockVec<PEv>*> evs;
            std::vector<const BlockVec<PCl>*> cls;
            for (auto& t : pev_parts) evs.push_back(&t.out[b]);
            for (auto& t : pcl_parts) cls.push_back(&t.bk[b]);
            process_bucket(ctx, evs, cls, sides, *st.scratch, ncamp, ncat, st.rows, st.cr.data(), st.asst.data());
        });
    tm.lap("sessions");

    std::vector<std::array<uint64_t, 4>> cd(ncamp * ndays, {0, 0, 0, 0});
    std::vector<std::vector<UserRow>> rows;
    for (auto& r : results) {
        for (size_t i = 0; i < cd.size(); ++i) {
            cd[i][0] += r.cr[i].sessions;
            cd[i][1] += r.cr[i].purchasing;
            cd[i][2] += r.cr[i].revenue;
            cd[i][3] += r.asst[i];
        }
        rows.push_back(std::move(r.rows));
    }
    std::vector<CampRow> camp_rows;
    for (size_t c = 0; c < ncamp; ++c)
        for (size_t d = 0; d < ndays; ++d) {
            auto& x = cd[c * ndays + d];
            if (x[0] > 0 || x[3] > 0) camp_rows.push_back({uint32_t(c), int32_t(day0 + int64_t(d)), x[0], x[1], x[2], x[3]});
        }
    write_outputs(outdir, rows, cats, camp_names, camp_rows, threads);
    tm.lap("write");
    std::fprintf(stderr, "%12s: %8.1f ms\n", "total",
                 std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - t0).count());
    // Every output file is closed: skip freeing gigabytes and exit right away.
    std::fflush(nullptr);
    _exit(0);
}
