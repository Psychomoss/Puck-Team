// A minimal Parquet writer for the two output tables: one row group per file, PLAIN encoded
// data pages (v1) of up to 64K rows, zstd compressed, metadata in the Thrift compact protocol.
#include <fcntl.h>
#include <unistd.h>
#include <zstd.h>

#include <filesystem>
#include <stdexcept>
#include <thread>

#include "common.hpp"

namespace fs = std::filesystem;

namespace {
constexpr size_t PAGE_ROWS = 1 << 16;
constexpr int ZSTD_LEVEL = 1;

enum PType : int32_t { INT32 = 1, INT64 = 2, BYTE_ARRAY = 6 };

/// Thrift compact protocol writer (only what parquet.thrift metadata needs).
struct Thrift {
    std::vector<uint8_t>& o;
    std::vector<int16_t> last{0};
    void varint(uint64_t v) {
        while (v >= 0x80) {
            o.push_back(uint8_t(v | 0x80));
            v >>= 7;
        }
        o.push_back(uint8_t(v));
    }
    void zz(int64_t v) { varint((uint64_t(v) << 1) ^ uint64_t(v >> 63)); }
    void field(int16_t id, uint8_t type) {
        int16_t d = id - last.back();
        if (d > 0 && d <= 15) o.push_back(uint8_t(d << 4 | type));
        else {
            o.push_back(type);
            zz(id);
        }
        last.back() = id;
    }
    void i32(int16_t id, int64_t v) {
        field(id, 5);
        zz(v);
    }
    void i64(int16_t id, int64_t v) {
        field(id, 6);
        zz(v);
    }
    void str(int16_t id, std::string_view s) {
        field(id, 8);
        varint(s.size());
        o.insert(o.end(), s.begin(), s.end());
    }
    void begin(int16_t id) {
        field(id, 12);
        last.push_back(0);
    }
    void end() {
        o.push_back(0);
        last.pop_back();
    }
    void list(int16_t id, uint8_t etype, size_t n) {
        field(id, 9);
        if (n < 15) o.push_back(uint8_t(n << 4 | etype));
        else {
            o.push_back(uint8_t(0xF0 | etype));
            varint(n);
        }
    }
    // A struct element of a list: fields start from id 0 again.
    void begin_elem() { last.push_back(0); }
    void str_elem(std::string_view s) {
        varint(s.size());
        o.insert(o.end(), s.begin(), s.end());
    }
    void i32_elem(int64_t v) { zz(v); }
};

struct Column {
    const char* name;
    PType type;
    bool optional = false;
    int32_t converted = -1;  // UTF8 = 0, DATE = 6
    // Filled while writing:
    int64_t first_page = 0, uncompressed = 0, compressed = 0;
};

/// Builds one Parquet file in memory and writes it with one call.
struct FileWriter {
    std::vector<Column> cols;
    std::vector<uint8_t> out{'P', 'A', 'R', '1'};
    std::vector<uint8_t> page, zbuf;
    ZSTD_CCtx* zc = ZSTD_createCCtx();
    size_t rows = 0;

    ~FileWriter() { ZSTD_freeCCtx(zc); }

    /// Appends one data page of `n` values whose (levels + values) body is in `page`.
    void flush_page(Column& c, size_t n) {
        zbuf.resize(ZSTD_compressBound(page.size()));
        size_t z = ZSTD_compressCCtx(zc, zbuf.data(), zbuf.size(), page.data(), page.size(), ZSTD_LEVEL);
        if (ZSTD_isError(z)) throw std::runtime_error("zstd");
        std::vector<uint8_t> h;
        Thrift t{h};
        t.i32(1, 0);  // DATA_PAGE
        t.i32(2, int64_t(page.size()));
        t.i32(3, int64_t(z));
        t.begin(5);
        t.i32(1, int64_t(n));
        t.i32(2, 0);  // PLAIN
        t.i32(3, 3);  // RLE definition levels
        t.i32(4, 3);  // RLE repetition levels
        t.end();
        h.push_back(0);
        if (c.uncompressed == 0) c.first_page = int64_t(out.size());
        c.uncompressed += int64_t(h.size() + page.size());
        c.compressed += int64_t(h.size() + z);
        out.insert(out.end(), h.begin(), h.end());
        out.insert(out.end(), zbuf.begin(), zbuf.begin() + z);
    }

    template <class T, class Get>
    void fixed(Column& c, size_t n, Get get) {
        size_t a = 0;
        do {  // an empty table still gets one (empty) page
            size_t b = std::min(n, a + PAGE_ROWS);
            page.resize((b - a) * sizeof(T));
            T* p = reinterpret_cast<T*>(page.data());
            for (size_t i = a; i < b; ++i) p[i - a] = T(get(i));
            flush_page(c, b - a);
            a = b;
        } while (a < n);
    }

    /// Strings; `get(i, present)` returns the value (ignored when !present for optional columns).
    template <class Get>
    void strings(Column& c, size_t n, Get get) {
        size_t a = 0;
        do {  // an empty table still gets one (empty) page
            size_t b = std::min(n, a + PAGE_ROWS);
            page.clear();
            if (c.optional) {
                // Definition levels: one bit-packed run of width 1, prefixed by its byte length.
                size_t groups = (b - a + 7) / 8;
                std::vector<uint8_t> lv;
                Thrift t{lv};
                t.varint(groups << 1 | 1);
                size_t at = lv.size();
                lv.resize(at + groups, 0);
                for (size_t i = a; i < b; ++i) {
                    bool present;
                    get(i, present);
                    if (present) lv[at + (i - a) / 8] |= uint8_t(1u << ((i - a) % 8));
                }
                uint32_t len = uint32_t(lv.size());
                page.insert(page.end(), reinterpret_cast<uint8_t*>(&len), reinterpret_cast<uint8_t*>(&len) + 4);
                page.insert(page.end(), lv.begin(), lv.end());
            }
            for (size_t i = a; i < b; ++i) {
                bool present = true;
                std::string_view s = get(i, present);
                if (!present) continue;
                uint32_t len = uint32_t(s.size());
                page.insert(page.end(), reinterpret_cast<uint8_t*>(&len), reinterpret_cast<uint8_t*>(&len) + 4);
                page.insert(page.end(), s.begin(), s.end());
            }
            flush_page(c, b - a);
            a = b;
        } while (a < n);
    }

    void finish(const fs::path& path) {
        std::vector<uint8_t> m;
        Thrift t{m};
        t.i32(1, 1);  // version
        t.list(2, 12, cols.size() + 1);
        t.begin_elem();
        t.str(4, "schema");
        t.i32(5, int64_t(cols.size()));
        t.end();
        for (auto& c : cols) {
            t.begin_elem();
            t.i32(1, c.type);
            t.i32(3, c.optional ? 1 : 0);
            t.str(4, c.name);
            if (c.converted >= 0) {
                t.i32(6, c.converted);
                t.begin(10);  // LogicalType union: STRING = 1, DATE = 6
                t.begin(c.converted == 0 ? 1 : 6);
                t.end();
                t.end();
            }
            t.end();
        }
        t.i64(3, int64_t(rows));
        t.list(4, 12, 1);
        t.begin_elem();
        t.list(1, 12, cols.size());
        int64_t total = 0;
        for (auto& c : cols) {
            total += c.uncompressed;
            t.begin_elem();
            t.i64(2, c.first_page);
            t.begin(3);
            t.i32(1, c.type);
            t.list(2, 5, 2);
            t.i32_elem(0);  // PLAIN
            t.i32_elem(3);  // RLE
            t.list(3, 8, 1);
            t.str_elem(c.name);
            t.i32(4, 6);  // ZSTD
            t.i64(5, int64_t(rows));
            t.i64(6, c.uncompressed);
            t.i64(7, c.compressed);
            t.i64(9, c.first_page);
            t.end();
            t.end();
        }
        t.i64(2, total);
        t.i64(3, int64_t(rows));
        t.end();
        t.str(6, "puck");
        m.push_back(0);
        uint32_t len = uint32_t(m.size());
        out.insert(out.end(), m.begin(), m.end());
        out.insert(out.end(), reinterpret_cast<uint8_t*>(&len), reinterpret_cast<uint8_t*>(&len) + 4);
        for (char ch : {'P', 'A', 'R', '1'}) out.push_back(uint8_t(ch));
        int fd = ::open(path.c_str(), O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0644);
        if (fd < 0) throw std::runtime_error("cannot create " + path.string());
        size_t done = 0;
        while (done < out.size()) {
            ssize_t w = ::write(fd, out.data() + done, out.size() - done);
            if (w <= 0) throw std::runtime_error("write failed");
            done += size_t(w);
        }
        close(fd);
    }
};

void write_users(const fs::path& path, const std::vector<std::pair<const UserRow*, size_t>>& ranges,
                 const std::vector<std::string>& cats) {
    std::vector<const UserRow*> rows;
    for (auto [p, n] : ranges)
        for (size_t j = 0; j < n; ++j) rows.push_back(p + j);
    std::string names;
    names.reserve(rows.size() * 10);
    std::vector<uint32_t> ends;
    ends.reserve(rows.size());
    for (const UserRow* r : rows) {
        key_append(r->pkey, names);
        ends.push_back(uint32_t(names.size()));
    }
    FileWriter f;
    f.cols = {{"user_id", BYTE_ARRAY, false, 0},       {"day", INT32, false, 6},     {"sessions", INT32},
              {"events", INT32},                       {"revenue_cents", INT64},     {"top_category", BYTE_ARRAY, true, 0}};
    f.rows = rows.size();
    size_t n = rows.size();
    f.strings(f.cols[0], n, [&](size_t i, bool&) {
        uint32_t b = i ? ends[i - 1] : 0;
        return std::string_view(names.data() + b, ends[i] - b);
    });
    f.fixed<int32_t>(f.cols[1], n, [&](size_t i) { return rows[i]->day; });
    f.fixed<int32_t>(f.cols[2], n, [&](size_t i) { return rows[i]->sessions; });
    f.fixed<int32_t>(f.cols[3], n, [&](size_t i) { return rows[i]->events; });
    f.fixed<int64_t>(f.cols[4], n, [&](size_t i) { return rows[i]->revenue; });
    f.strings(f.cols[5], n, [&](size_t i, bool& present) {
        uint32_t t = rows[i]->top;
        present = t != UINT32_MAX;
        return present ? std::string_view(cats[t]) : std::string_view();
    });
    f.finish(path);
}
}  // namespace

void write_outputs(const std::string& out, const std::vector<std::vector<UserRow>>& rows,
                   const std::vector<std::string>& cats, const std::vector<std::pair<std::string, std::string>>& camps,
                   const std::vector<CampRow>& camp_rows, size_t threads) {
    fs::path ud = fs::path(out) / "user_daily", cd = fs::path(out) / "campaign_daily";
    fs::create_directories(ud);
    fs::create_directories(cd);
    size_t total = 0;
    for (auto& r : rows) total += r.size();
    size_t nparts = total < (1u << 16) ? 1 : threads;
    size_t per = std::max<size_t>(1, (total + nparts - 1) / nparts);
    // Each part is a list of row ranges; pointer lists are built inside the parallel tasks.
    std::vector<std::vector<std::pair<const UserRow*, size_t>>> parts(1);
    size_t fill = 0;
    for (auto& v : rows) {
        size_t at = 0;
        while (at < v.size()) {
            if (fill == per) {
                parts.emplace_back();
                fill = 0;
            }
            size_t take = std::min(per - fill, v.size() - at);
            parts.back().push_back({v.data() + at, take});
            fill += take;
            at += take;
        }
    }
    std::thread camp([&] {
        FileWriter f;
        f.cols = {{"campaign_id", BYTE_ARRAY, false, 0}, {"channel", BYTE_ARRAY, false, 0}, {"day", INT32, false, 6},
                  {"sessions", INT64},                   {"purchasing_sessions", INT64},     {"revenue_cents", INT64},
                  {"assisted_sessions", INT64}};
        size_t n = camp_rows.size();
        f.rows = n;
        f.strings(f.cols[0], n, [&](size_t i, bool&) { return std::string_view(camps[camp_rows[i].camp].first); });
        f.strings(f.cols[1], n, [&](size_t i, bool&) { return std::string_view(camps[camp_rows[i].camp].second); });
        f.fixed<int32_t>(f.cols[2], n, [&](size_t i) { return camp_rows[i].day; });
        f.fixed<int64_t>(f.cols[3], n, [&](size_t i) { return camp_rows[i].sessions; });
        f.fixed<int64_t>(f.cols[4], n, [&](size_t i) { return camp_rows[i].purchasing; });
        f.fixed<int64_t>(f.cols[5], n, [&](size_t i) { return camp_rows[i].revenue; });
        f.fixed<int64_t>(f.cols[6], n, [&](size_t i) { return camp_rows[i].assisted; });
        f.finish(cd / "part-00000.parquet");
    });
    par_tasks<int>(parts.size(), threads, [] { return 0; }, [&](int&, size_t i) {
        char name[32];
        std::snprintf(name, sizeof name, "part-%05zu.parquet", i);
        write_users(ud / name, parts[i], cats);
    });
    camp.join();
}
