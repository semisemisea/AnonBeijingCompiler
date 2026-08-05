IMAGE ?= soyo-test-tools
IMAGE_STAMP ?= .docker-image
DOCKERFILE_CHECKSUM := $(shell cksum Dockerfile)
CONTAINER ?= soyo-test
RESULTS ?= results
ARGS ?=
TESTS ?=
DOCKER ?= docker

# gem5 performance testing
GEM5_BUILD ?= .gem5
GEM5_BIN ?= $(GEM5_BUILD)/build/ARM/gem5.opt
GEM5_TAG ?= v25.1.0.1
GEM5_DIR_IN ?= /work/gem5
GEM5_BIN_IN ?= $(GEM5_DIR_IN)/build/ARM/gem5.opt
GEM5_CONFIG ?= $(CURDIR)/gem5/a53_se.py
GEM5_ARGS ?=

ifeq ($(firstword $(MAKECMDGOALS)),test)
TEST_ARGS := $(wordlist 2,$(words $(MAKECMDGOALS)),$(MAKECMDGOALS))
$(eval $(TEST_ARGS):;@:)
endif

ifeq ($(firstword $(MAKECMDGOALS)),test-baseline)
TEST_ARGS := $(wordlist 2,$(words $(MAKECMDGOALS)),$(MAKECMDGOALS))
$(eval $(TEST_ARGS):;@:)
endif

ifeq ($(firstword $(MAKECMDGOALS)),test-llvm)
TEST_ARGS := $(wordlist 2,$(words $(MAKECMDGOALS)),$(MAKECMDGOALS))
$(eval $(TEST_ARGS):;@:)
endif

ifeq ($(firstword $(MAKECMDGOALS)),test-riscv)
TEST_ARGS := $(wordlist 2,$(words $(MAKECMDGOALS)),$(MAKECMDGOALS))
$(eval $(TEST_ARGS):;@:)
endif

ifeq ($(firstword $(MAKECMDGOALS)),run-elf)
RUN_ELF := $(word 2,$(MAKECMDGOALS))
RUN_ELF_PATH := $(abspath $(RUN_ELF))
$(eval $(RUN_ELF):;@:)
endif

ifeq ($(firstword $(MAKECMDGOALS)),run-elf-riscv)
RUN_ELF_RISCV := $(word 2,$(MAKECMDGOALS))
RUN_ELF_RISCV_PATH := $(abspath $(RUN_ELF_RISCV))
$(eval $(RUN_ELF_RISCV):;@:)
endif

ifeq ($(firstword $(MAKECMDGOALS)),debug-elf)
DEBUG_ELF := $(word 2,$(MAKECMDGOALS))
DEBUG_ELF_PATH := $(abspath $(DEBUG_ELF))
$(eval $(DEBUG_ELF):;@:)
endif

ifeq ($(firstword $(MAKECMDGOALS)),debug-elf-riscv)
DEBUG_ELF_RISCV := $(word 2,$(MAKECMDGOALS))
DEBUG_ELF_RISCV_PATH := $(abspath $(DEBUG_ELF_RISCV))
$(eval $(DEBUG_ELF_RISCV):;@:)
endif

ifeq ($(firstword $(MAKECMDGOALS)),mca)
MCA_FILE := $(word 2,$(MAKECMDGOALS))
MCA_FILE_PATH := $(abspath $(MCA_FILE))
$(eval $(MCA_FILE):;@:)
endif

ifeq ($(firstword $(MAKECMDGOALS)),gem5)
TEST_ARGS := $(wordlist 2,$(words $(MAKECMDGOALS)),$(MAKECMDGOALS))
$(eval $(TEST_ARGS):;@:)
endif

ifeq ($(firstword $(MAKECMDGOALS)),gem5-run)
GEM5_RUN_ELF := $(word 2,$(MAKECMDGOALS))
GEM5_RUN_ELF_PATH := $(abspath $(GEM5_RUN_ELF))
$(eval $(GEM5_RUN_ELF):;@:)
endif

HOST_ARCH := $(shell uname -m)
ifeq ($(HOST_ARCH),x86_64)
MUSL_TARGET := x86_64-unknown-linux-musl
else ifeq ($(HOST_ARCH),aarch64)
MUSL_TARGET := aarch64-unknown-linux-musl
else ifeq ($(HOST_ARCH),arm64)
MUSL_TARGET := aarch64-unknown-linux-musl
else
$(error unsupported host arch: $(HOST_ARCH))
endif

HOST_OS := $(shell uname -s)
ifeq ($(HOST_OS),Darwin)
CARGO_TARGET_LINKER := CARGO_TARGET_$(shell echo $(MUSL_TARGET) | tr 'a-z-' 'A-Z_')_LINKER=ld.lld
endif

HOST_TARGET_DIR := $(CURDIR)/target/host-musl
COMPILER := /work/target/$(MUSL_TARGET)/release/compiler

.PHONY: help test test-baseline test-llvm test-riscv run-elf run-elf-riscv debug-elf debug-elf-riscv mca test-image test-compiler build-lib build-lib-riscv clean-results gem5 gem5-run gem5-build gen-runtime-templates check-runtime-templates

help:
	@printf '%s\n' 'make test [functional/case.sy]      Build the AArch64 compiler and run the AArch64 harness.'
	@printf '%s\n' 'make test ARGS="-O 1"             Pass compiler options to the AArch64 harness.'
	@printf '%s\n' 'make test-baseline [functional/case.sy] Run the AArch64 harness using container clang.'
	@printf '%s\n' 'make test-llvm [functional/case.sy] Validate the LLVM IR emitter through the harness.'
	@printf '%s\n' 'make test-riscv [functional/case.sy] Build and run the RISC-V harness.'
	@printf '%s\n' 'make run-elf path/to/program.elf  Execute an AArch64 ELF in the test container.'
	@printf '%s\n' 'make debug-elf path/to/program.elf Start the AArch64 QEMU/GDB workflow.'
	@printf '%s\n' 'make mca path/to/target.s        Analyse AArch64 assembly with llvm-mca.'
	@printf '%s\n' 'make gem5 perf/01_mm1.sy         Build gem5 once, then run the compiler test suite under'
	@printf '%s\n' '                                  the XCZU15EG Cortex-A53 gem5 model and print stats.'
	@printf '%s\n' 'make gem5-run path/to/program.elf Run an AArch64 ELF under the gem5 A53 model.'
	@printf '%s\n' 'make gem5-build                   Clone and build gem5 into .gem5 (first run only).'

test: check-runtime-templates test-compiler build-lib test-image
	mkdir -p "$(RESULTS)"
	@cleanup() { $(DOCKER) rm -f "$(CONTAINER)" >/dev/null 2>&1 || true; }; \
	trap cleanup EXIT INT TERM; \
	cleanup; \
	$(DOCKER) run -t --name "$(CONTAINER)" --network none \
		-e SOYO_COMPILER="$(COMPILER)" \
		-v "$(HOST_TARGET_DIR):/work/target:ro" \
		-v "$(CURDIR)/tests:/work/tests:ro" \
		-v "$(CURDIR)/sysylib:/work/sysylib:ro" \
		-v "$(CURDIR)/$(RESULTS):/work/results:rw" \
		"$(IMAGE)" $(ARGS) $(TESTS) $(TEST_ARGS)

test-baseline: build-lib .docker-image
	mkdir -p "$(RESULTS)"
	@cleanup() { $(DOCKER) rm -f "$(CONTAINER)" >/dev/null 2>&1 || true; }; \
	trap cleanup EXIT INT TERM; \
	cleanup; \
	$(DOCKER) run -t --name "$(CONTAINER)" --network none \
		-v "$(CURDIR)/tests:/work/tests:ro" \
		-v "$(CURDIR)/sysylib:/work/sysylib:ro" \
		-v "$(CURDIR)/$(RESULTS):/work/results:rw" \
		"$(IMAGE)" --baseline $(ARGS) $(TESTS) $(TEST_ARGS)

test-llvm: test-compiler build-lib test-image
	mkdir -p "$(RESULTS)"
	@cleanup() { $(DOCKER) rm -f "$(CONTAINER)" >/dev/null 2>&1 || true; }; \
	trap cleanup EXIT INT TERM; \
	cleanup; \
	$(DOCKER) run -t --name "$(CONTAINER)" --network none \
        -e RUST_BACKTRACE=1 \
		-e SOYO_COMPILER="$(COMPILER)" \
		-v "$(HOST_TARGET_DIR):/work/target:ro" \
		-v "$(CURDIR)/tests:/work/tests:ro" \
		-v "$(CURDIR)/sysylib:/work/sysylib:ro" \
		-v "$(CURDIR)/$(RESULTS):/work/results:rw" \
		"$(IMAGE)" --backend llvm $(ARGS) $(TESTS) $(TEST_ARGS)

test-riscv: check-runtime-templates test-compiler build-lib-riscv test-image
	mkdir -p "$(RESULTS)"
	@cleanup() { $(DOCKER) rm -f "$(CONTAINER)" >/dev/null 2>&1 || true; }; \
	trap cleanup EXIT INT TERM; \
	cleanup; \
	$(DOCKER) run -t --name "$(CONTAINER)" --network none \
		-e SOYO_COMPILER="$(COMPILER)" \
		-v "$(HOST_TARGET_DIR):/work/target:ro" \
		-v "$(CURDIR)/tests:/work/tests:ro" \
		-v "$(CURDIR)/sysylib:/work/sysylib:ro" \
		-v "$(CURDIR)/$(RESULTS):/work/results:rw" \
		"$(IMAGE)" --target riscv64 $(ARGS) $(TESTS) $(TEST_ARGS)

run-elf: test-image
	@if [ -z "$(RUN_ELF)" ]; then \
		printf 'usage: make run-elf path/to/program.elf\n' >&2; \
		exit 2; \
	fi; \
	if [ ! -f "$(RUN_ELF_PATH)" ]; then \
		printf 'ELF not found: %s\n' "$(RUN_ELF)" >&2; \
		exit 2; \
	fi
	$(DOCKER) run --rm -t --network none \
		-v "$(RUN_ELF_PATH):/work/program.elf:ro" \
		--entrypoint qemu-aarch64-static \
		"$(IMAGE)" "/work/program.elf"

debug-elf: test-image
	@if [ -z "$(DEBUG_ELF)" ]; then \
		printf 'usage: make debug-elf path/to/program.elf\n' >&2; \
		exit 2; \
	fi; \
	if [ ! -f "$(DEBUG_ELF_PATH)" ]; then \
		printf 'ELF not found: %s\n' "$(DEBUG_ELF)" >&2; \
		exit 2; \
	fi
	$(DOCKER) run --rm -it --network none \
		-v "$(DEBUG_ELF_PATH):/work/program.elf:ro" \
		--entrypoint /bin/sh \
		"$(IMAGE)" -c 'qemu-aarch64-static -g 1234 /work/program.elf & gdb-multiarch /work/program.elf -ex "target remote localhost:1234" \
			-ex "break main" \
			-ex "layout asm" \
			-ex "focus cmd"'

run-elf-riscv: test-image
	@if [ -z "$(RUN_ELF_RISCV)" ]; then \
		printf 'usage: make run-elf-riscv path/to/program.elf\n' >&2; \
		exit 2; \
	fi; \
	if [ ! -f "$(RUN_ELF_RISCV_PATH)" ]; then \
		printf 'ELF not found: %s\n' "$(RUN_ELF_RISCV)" >&2; \
		exit 2; \
	fi
	$(DOCKER) run --rm -t --network none \
		-v "$(RUN_ELF_RISCV_PATH):/work/program.elf:ro" \
		--entrypoint qemu-riscv64-static \
		"$(IMAGE)" "/work/program.elf"

debug-elf-riscv: test-image
	@if [ -z "$(DEBUG_ELF_RISCV)" ]; then \
		printf 'usage: make debug-elf-riscv path/to/program.elf\n' >&2; \
		exit 2; \
	fi; \
	if [ ! -f "$(DEBUG_ELF_RISCV_PATH)" ]; then \
		printf 'ELF not found: %s\n' "$(DEBUG_ELF_RISCV)" >&2; \
		exit 2; \
	fi
	$(DOCKER) run --rm -it --network none \
		-v "$(DEBUG_ELF_RISCV_PATH):/work/program.elf:ro" \
		--entrypoint /bin/sh \
		"$(IMAGE)" -c 'qemu-riscv64-static -g 1234 /work/program.elf & gdb-multiarch /work/program.elf -ex "target remote localhost:1234" \
			-ex "break main" \
			-ex "layout asm" \
			-ex "focus cmd"'

mca: test-image
	@if [ -z "$(MCA_FILE)" ]; then \
		printf 'usage: make mca path/to/target.s\n' >&2; \
		exit 2; \
	fi; \
	if [ ! -f "$(MCA_FILE_PATH)" ]; then \
		printf 'assembly file not found: %s\n' "$(MCA_FILE)" >&2; \
		exit 2; \
	fi
	$(DOCKER) run --rm -t --network none \
		-v "$(MCA_FILE_PATH):/work/target.s:ro" \
		--entrypoint llvm-mca \
		"$(IMAGE)" -march=aarch64 -mcpu=cortex-a53 -timeline /work/target.s

# Run the compiler test suite under the gem5 Cortex-A53 model (XCZU15EG).
# gem5 SE simulates a few hundred K instructions/second, so pass small-input
# cases (e.g. `make gem5 perf/conv2d-1`) rather than the MB-sized ones.
gem5: test-compiler build-lib test-image gem5-build
	mkdir -p "$(RESULTS)"
	@cleanup() { $(DOCKER) rm -f "$(CONTAINER)" >/dev/null 2>&1 || true; }; \
	trap cleanup EXIT INT TERM; \
	cleanup; \
	$(DOCKER) run -t --name "$(CONTAINER)" --network none \
		-e SOYO_COMPILER="$(COMPILER)" \
		-e SOYO_GEM5="$(GEM5_BIN_IN)" \
		-e SOYO_GEM5_CONFIG="/work/gem5-config/a53_se.py" \
		-e SOYO_GEM5_EXTRA="$(GEM5_ARGS)" \
		-v "$(HOST_TARGET_DIR):/work/target:ro" \
		-v "$(CURDIR)/tests:/work/tests:ro" \
		-v "$(CURDIR)/sysylib:/work/sysylib:ro" \
		-v "$(CURDIR)/$(RESULTS):/work/results:rw" \
		-v "$(CURDIR)/$(GEM5_BUILD):$(GEM5_DIR_IN):ro" \
		-v "$(CURDIR)/gem5:/work/gem5-config:ro" \
		"$(IMAGE)" --runner gem5 $(ARGS) $(TESTS) $(TEST_ARGS)

# Run a single pre-built AArch64 ELF under the gem5 Cortex-A53 model.
# Optional: GEM5_INPUT=path/to/input (fed as stdin), GEM5_ARGS="--cpu-clock=1.5GHz --maxinsts=100000000".
gem5-run: test-image gem5-build
	@if [ -z "$(GEM5_RUN_ELF)" ]; then \
		printf 'usage: make gem5-run path/to/program.elf [GEM5_INPUT=path] [GEM5_ARGS="..."]\n' >&2; \
		exit 2; \
	fi; \
	if [ ! -f "$(GEM5_RUN_ELF_PATH)" ]; then \
		printf 'ELF not found: %s\n' "$(GEM5_RUN_ELF)" >&2; \
		exit 2; \
	fi
	mkdir -p "$(RESULTS)/gem5"
	$(DOCKER) run --rm -t --network none \
		-v "$(CURDIR)/$(GEM5_BUILD):$(GEM5_DIR_IN):ro" \
		-v "$(CURDIR)/gem5:/work/gem5-config:ro" \
		-v "$(GEM5_RUN_ELF_PATH):/work/program.elf:ro" \
		$(if $(GEM5_INPUT),-v "$(abspath $(GEM5_INPUT)):/work/program.in:ro",) \
		-v "$(CURDIR)/$(RESULTS):/work/results:rw" \
		--entrypoint /bin/sh \
		"$(IMAGE)" -c 'cd $(GEM5_DIR_IN) && ./build/ARM/gem5.opt \
			--outdir=/work/results/gem5 \
			/work/gem5-config/a53_se.py \
			/work/program.elf \
			$(if $(GEM5_INPUT),--input=/work/program.in,) \
			--output=/work/results/gem5/program.stdout \
			$(GEM5_ARGS)'

# Build gem5 (clone + scons) into .gem5/. The gem5 binary embeds the absolute
# build path, so the volume is always mounted at $(GEM5_DIR_IN) (/work/gem5).
gem5-build: test-image
	@mkdir -p "$(GEM5_BUILD)"
	$(DOCKER) run --rm -t -u "$$(id -u):$$(id -g)" \
		--entrypoint /bin/sh \
		-v "$(CURDIR)/$(GEM5_BUILD):$(GEM5_DIR_IN)" \
		-w "$(GEM5_DIR_IN)" \
		"$(IMAGE)" -c \
			'if [ -x "$(GEM5_BIN_IN)" ]; then \
				printf "gem5 already built: %s\n" "$(GEM5_BIN_IN)"; \
				exit 0; \
			fi; \
			if [ ! -d src ]; then \
				printf "cloning gem5 $(GEM5_TAG)...\n"; \
				git clone --depth 1 --branch $(GEM5_TAG) https://github.com/gem5/gem5.git .; \
			fi; \
			printf "building gem5 (this takes a while)...\n"; \
			scons -j4 --ignore-style --linker=lld CC=clang CXX=clang++ build/ARM/gem5.opt'

# Build the test image if the tag is missing or the Dockerfile has changed.
# The stamp records a checksum rather than a timestamp: cloning the repository
# or switching branches rewrites file mtimes, which used to leave a stale image
# in place while `make` believed it was current. The harness itself is no longer
# baked into the image, so editing tests/test.py needs no rebuild.
test-image:
	@if [ "$$(cat "$(IMAGE_STAMP)" 2>/dev/null)" = "$(DOCKERFILE_CHECKSUM)" ] \
		&& $(DOCKER) image inspect "$(IMAGE)" >/dev/null 2>&1; then \
		exit 0; \
	fi; \
	$(DOCKER) build -f Dockerfile -t "$(IMAGE)" . \
		&& printf '%s\n' "$(DOCKERFILE_CHECKSUM)" > "$(IMAGE_STAMP)"

test-compiler:
	@echo "Building compiler..."
	@$(CARGO_TARGET_LINKER) cargo build -p soyo_compiler --bin compiler --release --target "$(MUSL_TARGET)" --target-dir "$(HOST_TARGET_DIR)" --quiet >/dev/null 2>&1
	@echo "Built compiler at $(COMPILER)"

build-lib: test-image
	$(DOCKER) run --rm -u "$$(id -u):$$(id -g)" \
		-v "$(CURDIR)/sysylib:/work/sysylib" \
		-w /work/sysylib \
		--entrypoint /bin/sh \
		"$(IMAGE)" -c 'aarch64-linux-gnu-gcc -O9 -fno-builtin -fno-tree-loop-distribute-patterns -c sylib.c -o sylib_arm.o && rm -f libsysy_arm.a && aarch64-linux-gnu-ar rcs libsysy_arm.a sylib_arm.o'

gen-runtime-templates:
	bash scripts/gen_runtime_templates.sh

check-runtime-templates: test-image
	bash scripts/gen_runtime_templates.sh --check

build-lib-riscv: test-image
	$(DOCKER) run --rm -u "$$(id -u):$$(id -g)" \
		-v "$(CURDIR)/sysylib:/work/sysylib" \
		-w /work/sysylib \
		--entrypoint /bin/sh \
		"$(IMAGE)" -c 'riscv64-linux-gnu-gcc -O9 -fno-builtin -fno-tree-loop-distribute-patterns -c sylib.c -o sylib_riscv.o && rm -f libsysy_riscv.a && riscv64-linux-gnu-ar rcs libsysy_riscv.a sylib_riscv.o'

clean-results:
	rm -rf "$(RESULTS)"
