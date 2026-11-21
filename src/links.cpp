#include "common.hpp"

#include <bit>

namespace {
constexpr size_t STEP = 1 << 16;

template <class T>
T* zeroed(size_t n) {
    return huge_zeroed<T>(n);
}

/// Union-find over slots with parent stored as index + 1 (0 = root), so the array starts zeroed.
uint32_t find(uint32_t* p, uint32_t x) {
    for (;;) {
        uint32_t px = std::atomic_ref(p[x]).load(std::memory_order_relaxed);
        if (px == 0) return x;
        uint32_t gp = std::atomic_ref(p[px - 1]).load(std::memory_order_relaxed);
        if (gp == 0) return px - 1;
        std::atomic_ref(p[x]).store(gp, std::memory_order_relaxed);
        x = gp - 1;
    }
}
}  // namespace

Persons build_persons(const Input& input, size_t threads) {
    struct St {
        std::vector<std::array<uint64_t, 2>> v;
        std::vector<uint8_t> buf;
    };
    auto parts = par_tasks<St>(input.size(), threads, [] { return St{}; }, [&](St& s, size_t i) {
        auto [p, n] = input.read(i, s.buf);
        scan<2>(p, n, [&](const Line<2>& f) {
            if (!f.empty(0) && !f.empty(1)) s.v.push_back({f.key(0), f.key(1)});
        });
    });
    size_t sides = 0;
    for (auto& p : parts) sides += p.v.size() * 2;
    size_t cap = std::max<size_t>(16, std::bit_ceil(sides * 3 / 2 + 1));
    size_t mask = cap - 1;
    uint64_t* keys = zeroed<uint64_t>(cap);
    std::vector<std::pair<size_t, size_t>> tasks;
    for (size_t t = 0; t < parts.size(); ++t)
        for (size_t a = 0; a < parts[t].v.size(); a += STEP) tasks.push_back({t, a});
    auto insert = [&](uint64_t k) -> uint64_t {
        size_t h = Persons::slot(k, mask);
        for (;;) {
            uint64_t exp = 0;
            if (std::atomic_ref(keys[h]).compare_exchange_strong(exp, k, std::memory_order_relaxed) || exp == k) return h;
            h = (h + 1) & mask;
        }
    };
    // Tasks touch disjoint ranges of the pair vectors.
    par_tasks<int>(tasks.size(), threads, [] { return 0; }, [&](int&, size_t i) {
        auto [t, a] = tasks[i];
        auto& v = parts[t].v;
        for (size_t j = a, e = std::min(a + STEP, v.size()); j < e; ++j) v[j] = {insert(v[j][0]), insert(v[j][1])};
    });

    uint32_t* parent = zeroed<uint32_t>(cap);
    par_tasks<int>(tasks.size(), threads, [] { return 0; }, [&](int&, size_t i) {
        auto [t, a] = tasks[i];
        auto& v = parts[t].v;
        for (size_t j = a, e = std::min(a + STEP, v.size()); j < e; ++j) {
            uint32_t x = uint32_t(v[j][0]), y = uint32_t(v[j][1]);
            for (;;) {
                x = find(parent, x);
                y = find(parent, y);
                if (x == y) break;
                uint32_t lo = std::min(x, y), hi = std::max(x, y), exp = 0;
                if (std::atomic_ref(parent[hi]).compare_exchange_strong(exp, lo + 1, std::memory_order_relaxed)) break;
            }
        }
    });
    // Group minimum as the maximum of ~key, so the array starts zeroed.
    uint64_t* best = zeroed<uint64_t>(cap);
    size_t nblk = (cap + STEP - 1) / STEP;
    par_tasks<int>(nblk, threads, [] { return 0; }, [&](int&, size_t b) {
        for (size_t s = b * STEP, e = std::min((b + 1) * STEP, cap); s < e; ++s) {
            uint64_t k = keys[s];
            if (!k) continue;
            std::atomic_ref r(best[find(parent, uint32_t(s))]);
            uint64_t cur = r.load(std::memory_order_relaxed), want = ~k;
            while (cur < want && !r.compare_exchange_weak(cur, want, std::memory_order_relaxed)) {
            }
        }
    });
    if (any_interned.load()) {
        std::vector<uint64_t> b2(cap, 0);
        for (size_t s = 0; s < cap; ++s) {
            uint64_t k = keys[s];
            if (!k) continue;
            size_t r = find(parent, uint32_t(s));
            if (b2[r] == 0 || cmp_key(k, b2[r]) < 0) b2[r] = k;
        }
        for (size_t s = 0; s < cap; ++s)
            if (b2[s]) best[s] = ~b2[s];
    }
    Persons out;
    out.table = zeroed<std::array<uint64_t, 2>>(cap);
    out.mask = mask;
    par_tasks<int>(nblk, threads, [] { return 0; }, [&](int&, size_t b) {
        for (size_t s = b * STEP, e = std::min((b + 1) * STEP, cap); s < e; ++s) {
            uint64_t k = keys[s];
            if (k) out.table[s] = {k, ~best[find(parent, uint32_t(s))]};
        }
    });
    return out;
}
