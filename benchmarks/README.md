# Cortex-A53 Benchmark Harness for XCZU15EG

## Build

Cross-compile for AArch64:

```bash
aarch64-linux-gnu-gcc -O2 -static -o bench src/bench.c
```

Or natively on the board:

```bash
gcc -O2 -o bench src/bench.c
```

## Run

Pin to a single Cortex-A53 core:

```bash
taskset -c 0 ./bench --benchmark load_use_chain --samples 30
```

List available benchmarks:

```bash
./bench --list
```

## Output

CSV to stdout:

```
benchmark,samples,iterations,cycles_per_iter,median,p95,mean,stddev,ci95_low,ci95_high
load_use_chain,30,10000,3.0,30000,30100,30000.0,50.0,29982.1,30017.9
```

## Environment Requirements

- Single Cortex-A53 core (use `taskset -c N`).
- PMU userspace access enabled (`perf_event_paranoid <= 1` or kernel config).
- Fixed CPU frequency (`performance` governor recommended).
- Minimal background workload.

## Benchmarks

| Name | What it measures |
|------|-----------------|
| `load_use_chain` | L1 load-to-use latency |
| `alu_chain` | Integer ALU dependency latency |
| `alu_throughput` | Independent ALU throughput |
| `mul_chain` | MUL dependency latency |
| `mul_throughput` | Independent MUL throughput |
| `sdiv32_chain` | 32-bit SDIV latency |
| `sdiv64_chain` | 64-bit SDIV latency |
| `load_throughput` | Load throughput |
| `pair_alu_load` | ALU+Load dual-issue |
| `pair_alu_alu` | ALU+ALU dual-issue |
