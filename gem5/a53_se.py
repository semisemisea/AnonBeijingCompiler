#!/usr/bin/env python3
"""gem5 SE model of the Xilinx XCZU15EG Cortex-A53 subsystem.

Models the Zynq UltraScale+ XCZU15EG quad-core ARM Cortex-A53 MPCore
(ARMv8-A 64-bit, NEON, single/double-precision FP) for compiler
performance evaluation:

    CPU clock   : 1.2 GHz            (--cpu-clock)
    Cores       : 4x in-order dual-issue; cores 0-1 are the two
                  benchmark-isolated cores (the workload runs on core 0,
                  gem5 SE has no OS so it is single-threaded anyway)
    L1 I-cache  : 32 KiB, 2-way, 64 B line
    L1 D-cache  : 32 KiB, 4-way, 64 B line
    L2 cache    : 1 MiB, 16-way, 64 B line, shared by all four cores
    Memory      : 2 GiB DDR4-2400

The MinorCPU is a generic in-order, dual-issue pipeline that approximates
the Cortex-A53 (gem5 no longer ships a dedicated A53 MinorCPU parameter
set); the cache sizes/associativity match the XCZU15EG spec exactly.

The gem5 binary embeds the absolute source directory it was built in, so
this config must be run from a checkout mounted at that same path (the
Makefile mounts the build volume at /work/gem5).

Usage (from the gem5 source tree that produced the binary):
    ./build/ARM/gem5.opt --outdir=m5out /path/to/a53_se.py \
        /work/program.elf [--input=FILE] [--output=FILE] \
        [--exitcode=FILE] [--cpu-clock=1.2GHz] [--maxinsts=N]
"""

import argparse

import m5
from m5.objects import (
    AddrRange,
    ArmMinorCPU,
    Cache,
    DDR4_2400_8x8,
    L2XBar,
    Process,
    Root,
    SEWorkload,
    SrcClockDomain,
    System,
    SystemXBar,
    VoltageDomain,
)

L1I_SIZE = "32KiB"
L1I_ASSOC = 2
L1D_SIZE = "32KiB"
L1D_ASSOC = 4
L2_SIZE = "1MiB"
L2_ASSOC = 16
CACHE_LINE = 64
MEM_SIZE = "2GB"
NUM_CORES = 4
BENCH_CORES = 2


class L1ICache(Cache):
    size = L1I_SIZE
    assoc = L1I_ASSOC
    tag_latency = 2
    data_latency = 2
    response_latency = 2
    mshrs = 4
    tgts_per_mshr = 20
    is_read_only = True


class L1DCache(Cache):
    size = L1D_SIZE
    assoc = L1D_ASSOC
    tag_latency = 2
    data_latency = 2
    response_latency = 2
    mshrs = 6
    tgts_per_mshr = 20
    write_buffers = 8


class L2Cache(Cache):
    size = L2_SIZE
    assoc = L2_ASSOC
    tag_latency = 12
    data_latency = 12
    response_latency = 12
    mshrs = 20
    tgts_per_mshr = 12
    write_buffers = 8


def build_system(clock, num_cpus):
    system = System()
    system.clk_domain = SrcClockDomain(
        clock=clock, voltage_domain=VoltageDomain()
    )
    system.mem_mode = "timing"
    system.mem_ranges = [AddrRange(MEM_SIZE)]
    system.cache_line_size = CACHE_LINE

    system.membus = SystemXBar()
    system.system_port = system.membus.cpu_side_ports

    dram = DDR4_2400_8x8()
    dram.range = system.mem_ranges[0]
    system.mem_ctrl = dram.controller()
    system.mem_ctrl.port = system.membus.mem_side_ports

    # Shared L2 for the whole A53 cluster.
    system.l2 = L2Cache(clk_domain=system.clk_domain)
    system.tol2bus = L2XBar(clk_domain=system.clk_domain)
    system.l2.cpu_side = system.tol2bus.mem_side_ports
    system.l2.mem_side = system.membus.cpu_side_ports

    system.cpu = [
        ArmMinorCPU(cpu_id=i, numThreads=0 if i > 0 else 1)
        for i in range(num_cpus)
    ]
    for cpu in system.cpu:
        cpu.clk_domain = system.clk_domain
        cpu.addPrivateSplitL1Caches(L1ICache(), L1DCache())
        cpu.connectAllPorts(
            system.tol2bus.cpu_side_ports,
            system.membus.cpu_side_ports,
            system.membus.mem_side_ports,
        )
        cpu.createThreads()
        cpu.createInterruptController()
    return system


def build_process(executable, input_path, output_path, errout_path):
    process = Process(pid=100)
    process.executable = executable
    process.cmd = [executable]
    if input_path:
        process.input = input_path
    if output_path:
        process.output = output_path
    if errout_path:
        process.errout = errout_path
    return process


def main():
    parser = argparse.ArgumentParser(
        description="gem5 SE model of the XCZU15EG Cortex-A53"
    )
    parser.add_argument("executable", help="static AArch64 ELF to simulate")
    parser.add_argument("--input", default=None, help="stdin file")
    parser.add_argument("--output", default=None, help="stdout file")
    parser.add_argument("--errout", default=None, help="stderr file")
    parser.add_argument(
        "--exitcode",
        default=None,
        help="write the simulated program exit code to this file",
    )
    parser.add_argument(
        "--cpu-clock", default="1.2GHz", help="CPU clock (default: 1.2GHz)"
    )
    parser.add_argument(
        "--num-cpus",
        type=int,
        default=1,
        help=(
            "number of A53 cores to instantiate (1-%d, default 1); the "
            "workload runs on core 0 and any additional cores are idle"
            % NUM_CORES
        ),
    )
    parser.add_argument(
        "--maxinsts",
        type=int,
        default=None,
        help="cap on total simulated instructions",
    )
    args = parser.parse_args()

    num_cpus = max(1, min(args.num_cpus, NUM_CORES))
    system = build_system(args.cpu_clock, num_cpus)
    system.workload = SEWorkload.init_compatible(args.executable)

    process = build_process(args.executable, args.input, args.output, args.errout)
    system.cpu[0].workload = [process]

    # Remaining cores model the other A53 cores of the cluster (benchmark
    # isolation keeps them idle); they are thread-less so they never execute
    # but still share the L2.  gem5 SE requires cpu.workload to match
    # numThreads, hence workload=[] + numThreads=0.
    for i in range(1, num_cpus):
        system.cpu[i].workload = []

    root = Root(full_system=False, system=system)
    m5.instantiate()

    if args.maxinsts:
        exit_event = m5.simulate(max_insts=args.maxinsts)
    else:
        exit_event = m5.simulate()
    code = exit_event.getCode()
    print(
        "gem5: exited at tick %d (%s, code %d)"
        % (m5.curTick(), exit_event.getCause(), code)
    )
    if args.exitcode:
        with open(args.exitcode, "w") as f:
            f.write(str(code))


if __name__ == "__m5_main__":
    main()
