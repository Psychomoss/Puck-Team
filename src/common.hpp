#pragma once

#include <sys/mman.h>

#include <array>
#include <atomic>
#include <cstdint>
#include <cstdlib>
#include <string>
#include <utility>
#include <thread>
#include <vector>

#include "parse.hpp"

constexpr int64_t WEEK = 7 * DAY;
constexpr size_t NB = 1024;  // person / event-id buckets

inline size_t bucket(uint64_t k) { return size_t(mix(k) >> 54); }

inline void prefetch(const void* p) { _mm_prefetch(static_cast<const char*>(p), _MM_HINT_T0); }

/// Runs tasks 0..n on `threads` workers pulling from a shared counter (work stealing, so slow
/// cores just take fewer tasks); returns each worker's state.
template <class S, class Init, class F>
std::vector<S> par_tasks(size_t n, size_t threads, Init init, F f) {
    size_t w = std::max<size_t>(1, std::min(threads, std::max<size_t>(n, 1)));
    std::atomic<size_t> next{0};
    std::vector<S> out;
    out.reserve(w);
    for (size_t i = 0; i < w; ++i) out.emplace_back(init());
    auto work = [&](size_t t) {
        S& s = out[t];
        for (;;) {
            size_t i = next.fetch_add(1, std::memory_order_relaxed);
            if (i >= n) break;
            f(s, i);
        }
    };
    std::vector<std::thread> ts;
    for (size_t t = 1; t < w; ++t) ts.emplace_back(work, t);
    work(0);
    for (auto& t : ts) t.join();
    return out;
}

/// Per-thread bump allocator over 32 MB huge-page extents; nothing is ever freed (the process
/// exits with _exit), so blocks cost one pointer bump.
inline void* block_alloc(size_t bytes) {
    thread_local char* cur = nullptr;
    thread_local char* end = nullptr;
    if (size_t(end - cur) < bytes) {
        constexpr size_t EXT = 32u << 20, HUGE = 2u << 20;
        size_t want = std::max(bytes, EXT);
        void* m = mmap(nullptr, want + HUGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (m == MAP_FAILED) std::abort();
        char* p = reinterpret_cast<char*>((reinterpret_cast<uintptr_t>(m) + HUGE - 1) & ~uintptr_t(HUGE - 1));
        madvise(p, want, MADV_HUGEPAGE);
        cur = p;
        end = p + want;
    }
    void* r = cur;
    cur += (bytes + 63) & ~size_t(63);
    return r;
}

/// Zeroed memory on transparent huge pages (fresh anonymous mapping, 2 MB aligned; never freed).
template <class T>
T* huge_zeroed(size_t n) {
    constexpr size_t HUGE = 2u << 20;
    size_t bytes = std::max<size_t>(n * sizeof(T), 1);
    void* m = mmap(nullptr, bytes + HUGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (m == MAP_FAILED) std::abort();
    char* p = reinterpret_cast<char*>((reinterpret_cast<uintptr_t>(m) + HUGE - 1) & ~uintptr_t(HUGE - 1));
    madvise(p, bytes, MADV_HUGEPAGE);
    return reinterpret_cast<T*>(p);
}

/// Growable buffer of trivially copyable T that never value-initializes (contents start undefined).
template <class T>
struct RawBuf {
    T* p = nullptr;
    size_t cap = 0;
    T* get(size_t n) {
        if (n > cap) {
            // Large buffers come from huge pages and are kept (sized for the biggest use up front).
            cap = std::max(n, cap * 2);
            if (cap * sizeof(T) >= (1u << 20)) p = huge_zeroed<T>(cap);
            else {
                std::free(p);
                p = static_cast<T*>(std::malloc(cap * sizeof(T)));
            }
            if (!p) std::abort();
        }
        return p;
    }
};

/// Append-only list of fixed-size blocks: no reallocation copies; a push is one compare and one
/// store through a cursor.
template <class T>
struct BlockVec {
    static constexpr size_t BLOCK = 1024;
    std::vector<T*> blocks;
    T* cur = nullptr;
    T* end = nullptr;

    __attribute__((noinline)) void grow() {
        T* p = static_cast<T*>(block_alloc(sizeof(T) * BLOCK));
        blocks.push_back(p);
        cur = p;
        end = p + BLOCK;
    }
    void push(const T& x) {
        if (cur == end) grow();
        *cur++ = x;
    }
    size_t size() const { return blocks.empty() ? 0 : (blocks.size() - 1) * BLOCK + size_t(cur - blocks.back()); }
    size_t block_len(size_t i) const { return i + 1 == blocks.size() ? size_t(cur - blocks[i]) : BLOCK; }
    T& operator[](size_t i) { return blocks[i / BLOCK][i % BLOCK]; }
    const T& operator[](size_t i) const { return blocks[i / BLOCK][i % BLOCK]; }
    template <class F>
    void each(F&& f) const {
        for (size_t b = 0; b < blocks.size(); ++b) {
            const T* p = blocks[b];
            size_t l = block_len(b);
            for (size_t i = 0; i < l; ++i) f(p[i]);
        }
    }
};

template <class T>
std::vector<BlockVec<T>> buckets() {
    return std::vector<BlockVec<T>>(NB);
}

/// One event copy in the event-id buckets. ts is relative to window_start (UINT32_MAX = invalid
/// copy); pc is a purchase handle into the parse threads' side tables (0 = not a purchase);
/// ukey holds the person key once a valid copy is resolved.
struct Ev {
    uint64_t ekey, ukey;
    int64_t ingest;
    uint32_t ts, pc;
};

/// A surviving event in its person bucket.
struct PEv {
    uint64_t pkey;
    uint32_t ts, pc;
};

/// A valid click in its person bucket; ts relative to window_start - 7 days.
struct PCl {
    uint64_t pkey, ckey;
    uint32_t ts, camp;
};

struct Cred {
    uint32_t sessions = 0, purchasing = 0;
    uint64_t revenue = 0;
};

struct UserRow {
    uint64_t pkey;
    int32_t day;
    uint32_t sessions, events;
    uint64_t revenue;
    uint32_t top;
};

/// Maps a user-ID key to its person's name key (the byte-wise smallest linked ID).
struct Persons {
    std::array<uint64_t, 2>* table = nullptr;  // calloc'ed: zero pages until written
    size_t mask = 0;
    static size_t slot(uint64_t k, size_t mask) { return size_t(mix(k) >> 20) & mask; }
    void prefetch_key(uint64_t k) const { prefetch(table + slot(k, mask)); }
    uint64_t get(uint64_t k) const {
        size_t h = slot(k, mask);
        for (;;) {
            const auto& e = table[h];
            if (e[0] == k) return e[1];
            if (e[0] == 0) return k;
            h = (h + 1) & mask;
        }
    }
};

/// Input files cut into 1 MB chunks; a task reads its chunk and owns the lines that start in it.
struct Input {
    struct File {
        int fd;
        size_t len;
        const uint8_t* map;  // whole file mapped read-only, or null when reading with pread
    };
    std::vector<File> files;
    struct Chunk {
        size_t file, a, b;
    };
    std::vector<Chunk> chunks;
    size_t bytes = 0;

    static Input open(const std::string& dir, bool reverse);
    size_t size() const { return chunks.size(); }
    /// The lines that start in chunk i's byte range (each file's header line is skipped): a view
    /// into the file mapping, or (INPUT_MODE=pread) a copy read into buf.
    std::pair<const uint8_t*, size_t> read(size_t i, std::vector<uint8_t>& buf) const;
};

Persons build_persons(const Input& in, size_t threads);

struct PersonCtx {
    int64_t ws, cws, day0;
    size_t ndays;
    uint32_t ev_bits, cl_bits;
};

struct PersonScratch;
PersonScratch* new_person_scratch(size_t max_events, size_t max_clicks);
void process_bucket(const PersonCtx& ctx, const std::vector<const BlockVec<PEv>*>& evs,
                    const std::vector<const BlockVec<PCl>*>& cls, const std::vector<std::vector<uint64_t>>& sides,
                    PersonScratch& s, size_t ncamp, size_t ncat, std::vector<UserRow>& rows, Cred* cr, uint32_t* asst);

struct CampRow {
    uint32_t camp;
    int32_t day;
    uint64_t sessions, purchasing, revenue, assisted;
};

void write_outputs(const std::string& out, const std::vector<std::vector<UserRow>>& rows,
                   const std::vector<std::string>& cats, const std::vector<std::pair<std::string, std::string>>& camps,
                   const std::vector<CampRow>& camp_rows, size_t threads);
