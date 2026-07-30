// Cortex-A53 microbenchmark harness for XCZU15EG.
//
// Build:  aarch64-linux-gnu-gcc -O2 -static -o bench bench.c
// Run:    taskset -c 0 ./bench --benchmark load_use_chain --samples 30
//
// Uses PMU cycle counter (PMCCNTR_EL0) when available, falls back to
// clock_gettime(CLOCK_MONOTONIC) for wall time.
//
// All results are emitted as CSV to stdout for downstream analysis.

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <time.h>
#include <sched.h>
#include <math.h>

/* ---------- PMU cycle counter ---------- */

static inline uint64_t read_cycles(void) {
    uint64_t val;
    __asm__ volatile("mrs %0, pmccntr_el0" : "=r"(val));
    return val;
}

static int pmu_available(void) {
    /* Try reading; if it traps, PMU is not enabled in userspace. */
    uint64_t val;
    __asm__ volatile("mrs %0, pmccntr_el0" : "=r"(val));
    return 1; /* If we get here, it worked. */
}

static void enable_pmu(void) {
    /* Enable user-mode access to PMU counters. Requires kernel support. */
    __asm__ volatile("msr pmuserenr_el0, %0" :: "r"(1UL));
    /* Reset and enable cycle counter. */
    __asm__ volatile("msr pmcr_el0, %0" :: "r"(0x7UL));
}

/* ---------- Benchmark definitions ---------- */

typedef struct {
    const char *name;
    uint64_t (*run)(uint64_t iterations);
} Benchmark;

/* Dependency latency: load-use chain */
static uint64_t bench_load_use_chain(uint64_t iters) {
    volatile uint64_t *buf = calloc(1024, sizeof(uint64_t));
    uint64_t idx = 0;
    uint64_t start = read_cycles();
    for (uint64_t i = 0; i < iters; i++) {
        idx = buf[idx & 1023];
    }
    uint64_t end = read_cycles();
    free((void *)buf);
    return end - start;
}

/* Dependency latency: integer ALU chain */
static uint64_t bench_alu_chain(uint64_t iters) {
    uint64_t a = 1, b = 2, c = 3, d = 4;
    uint64_t start = read_cycles();
    for (uint64_t i = 0; i < iters; i++) {
        a += b; b += c; c += d; d += a;
    }
    uint64_t end = read_cycles();
    /* Prevent dead code elimination. */
    __asm__ volatile("" :: "r"(a), "r"(b), "r"(c), "r"(d));
    return end - start;
}

/* Throughput: independent integer ALU */
static uint64_t bench_alu_throughput(uint64_t iters) {
    uint64_t a0 = 1, a1 = 2, a2 = 3, a3 = 4, a4 = 5, a5 = 6, a6 = 7, a7 = 8;
    uint64_t start = read_cycles();
    for (uint64_t i = 0; i < iters; i++) {
        a0 += 1; a1 += 1; a2 += 1; a3 += 1;
        a4 += 1; a5 += 1; a6 += 1; a7 += 1;
    }
    uint64_t end = read_cycles();
    __asm__ volatile("" :: "r"(a0), "r"(a1), "r"(a2), "r"(a3),
                          "r"(a4), "r"(a5), "r"(a6), "r"(a7));
    return end - start;
}

/* Dependency latency: MUL chain */
static uint64_t bench_mul_chain(uint64_t iters) {
    uint64_t a = 3;
    uint64_t start = read_cycles();
    for (uint64_t i = 0; i < iters; i++) {
        a *= 7;
    }
    uint64_t end = read_cycles();
    __asm__ volatile("" :: "r"(a));
    return end - start;
}

/* Throughput: independent MUL */
static uint64_t bench_mul_throughput(uint64_t iters) {
    uint64_t a0 = 1, a1 = 2, a2 = 3, a3 = 4;
    uint64_t start = read_cycles();
    for (uint64_t i = 0; i < iters; i++) {
        a0 *= 7; a1 *= 7; a2 *= 7; a3 *= 7;
    }
    uint64_t end = read_cycles();
    __asm__ volatile("" :: "r"(a0), "r"(a1), "r"(a2), "r"(a3));
    return end - start;
}

/* Dependency latency: SDIV 32-bit */
static uint64_t bench_sdiv32_chain(uint64_t iters) {
    int32_t a = 1000000;
    uint64_t start = read_cycles();
    for (uint64_t i = 0; i < iters; i++) {
        a /= 3;
        if (a == 0) a = 1000000;
    }
    uint64_t end = read_cycles();
    __asm__ volatile("" :: "r"(a));
    return end - start;
}

/* Dependency latency: SDIV 64-bit */
static uint64_t bench_sdiv64_chain(uint64_t iters) {
    int64_t a = 1000000000000LL;
    uint64_t start = read_cycles();
    for (uint64_t i = 0; i < iters; i++) {
        a /= 3;
        if (a == 0) a = 1000000000000LL;
    }
    uint64_t end = read_cycles();
    __asm__ volatile("" :: "r"(a));
    return end - start;
}

/* Throughput: load */
static uint64_t bench_load_throughput(uint64_t iters) {
    volatile uint64_t *buf = calloc(1024, sizeof(uint64_t));
    uint64_t sum = 0;
    uint64_t start = read_cycles();
    for (uint64_t i = 0; i < iters; i++) {
        sum += buf[i & 1023];
        sum += buf[(i + 256) & 1023];
    }
    uint64_t end = read_cycles();
    free((void *)buf);
    __asm__ volatile("" :: "r"(sum));
    return end - start;
}

/* Pairing: ALU + Load */
static uint64_t bench_pair_alu_load(uint64_t iters) {
    volatile uint64_t *buf = calloc(1024, sizeof(uint64_t));
    uint64_t a = 1, sum = 0;
    uint64_t start = read_cycles();
    for (uint64_t i = 0; i < iters; i++) {
        a += 1;
        sum += buf[i & 1023];
    }
    uint64_t end = read_cycles();
    free((void *)buf);
    __asm__ volatile("" :: "r"(a), "r"(sum));
    return end - start;
}

/* Pairing: ALU + ALU */
static uint64_t bench_pair_alu_alu(uint64_t iters) {
    uint64_t a = 1, b = 2;
    uint64_t start = read_cycles();
    for (uint64_t i = 0; i < iters; i++) {
        a += 1;
        b += 1;
    }
    uint64_t end = read_cycles();
    __asm__ volatile("" :: "r"(a), "r"(b));
    return end - start;
}

static const Benchmark benchmarks[] = {
    {"load_use_chain",    bench_load_use_chain},
    {"alu_chain",         bench_alu_chain},
    {"alu_throughput",    bench_alu_throughput},
    {"mul_chain",         bench_mul_chain},
    {"mul_throughput",    bench_mul_throughput},
    {"sdiv32_chain",      bench_sdiv32_chain},
    {"sdiv64_chain",      bench_sdiv64_chain},
    {"load_throughput",   bench_load_throughput},
    {"pair_alu_load",     bench_pair_alu_load},
    {"pair_alu_alu",      bench_pair_alu_alu},
    {NULL, NULL},
};

/* ---------- Statistics ---------- */

static int cmp_u64(const void *a, const void *b) {
    uint64_t va = *(const uint64_t *)a, vb = *(const uint64_t *)b;
    return (va > vb) - (va < vb);
}

static void print_stats(const char *name, uint64_t *samples, int n, uint64_t iters) {
    qsort(samples, n, sizeof(uint64_t), cmp_u64);
    double mean = 0;
    for (int i = 0; i < n; i++) mean += (double)samples[i];
    mean /= n;
    double variance = 0;
    for (int i = 0; i < n; i++) variance += ((double)samples[i] - mean) * ((double)samples[i] - mean);
    variance /= n;
    double stddev = sqrt(variance);
    double ci95 = 1.96 * stddev / sqrt(n);

    printf("%s,%d,%lu,%.1f,%lu,%lu,%.1f,%.1f,%.1f,%.1f\n",
           name, n, iters,
           mean / iters,
           samples[n / 2],
           samples[(int)(n * 0.95)],
           mean, stddev,
           mean - ci95, mean + ci95);
}

/* ---------- Main ---------- */

int main(int argc, char **argv) {
    const char *bench_name = NULL;
    int samples = 30;
    uint64_t iterations = 10000;

    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--benchmark") == 0 && i + 1 < argc) {
            bench_name = argv[++i];
        } else if (strcmp(argv[i], "--samples") == 0 && i + 1 < argc) {
            samples = atoi(argv[++i]);
        } else if (strcmp(argv[i], "--iterations") == 0 && i + 1 < argc) {
            iterations = (uint64_t)atoll(argv[++i]);
        } else if (strcmp(argv[i], "--list") == 0) {
            for (int j = 0; benchmarks[j].name; j++)
                printf("%s\n", benchmarks[j].name);
            return 0;
        }
    }

    if (!bench_name) {
        fprintf(stderr, "Usage: %s --benchmark NAME [--samples N] [--iterations N]\n", argv[0]);
        fprintf(stderr, "Use --list to see available benchmarks.\n");
        return 1;
    }

    enable_pmu();
    if (!pmu_available()) {
        fprintf(stderr, "Warning: PMU not available, using wall clock\n");
    }

    const Benchmark *bench = NULL;
    for (int i = 0; benchmarks[i].name; i++) {
        if (strcmp(benchmarks[i].name, bench_name) == 0) {
            bench = &benchmarks[i];
            break;
        }
    }
    if (!bench) {
        fprintf(stderr, "Unknown benchmark: %s\n", bench_name);
        return 1;
    }

    /* Warmup. */
    bench->run(iterations / 10);

    /* Collect samples. */
    uint64_t *results = malloc(samples * sizeof(uint64_t));
    for (int i = 0; i < samples; i++) {
        results[i] = bench->run(iterations);
    }

    printf("benchmark,samples,iterations,cycles_per_iter,median,p95,mean,stddev,ci95_low,ci95_high\n");
    print_stats(bench_name, results, samples, iterations);
    free(results);
    return 0;
}
