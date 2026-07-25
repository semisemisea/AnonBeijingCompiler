IMAGE ?= soyo-test-tools
IMAGE_STAMP ?= .docker-image
DOCKERFILE_CHECKSUM := $(shell cksum Dockerfile)
CONTAINER ?= soyo-test
RESULTS ?= results
ARGS ?=
TESTS ?=
DOCKER ?= docker

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
COMPILER := /work/target/$(MUSL_TARGET)/release/soyo_compiler

.PHONY: help test test-baseline test-llvm test-riscv run-elf run-elf-riscv debug-elf debug-elf-riscv test-image test-compiler build-lib build-lib-riscv clean-results

help:
	@printf '%s\n' 'make test [functional/case.sy]      Build the AArch64 compiler and run the AArch64 harness.'
	@printf '%s\n' 'make test ARGS="-O 1"             Pass compiler options to the AArch64 harness.'
	@printf '%s\n' 'make test-baseline [functional/case.sy] Run the AArch64 harness using container clang.'
	@printf '%s\n' 'make test-llvm [functional/case.sy] Validate the LLVM IR emitter through the harness.'
	@printf '%s\n' 'make test-riscv [functional/case.sy] Build and run the RISC-V harness.'
	@printf '%s\n' 'make run-elf path/to/program.elf  Execute an AArch64 ELF in the test container.'
	@printf '%s\n' 'make debug-elf path/to/program.elf Start the AArch64 QEMU/GDB workflow.'

test: test-compiler build-lib test-image
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

test-riscv: test-compiler build-lib-riscv test-image
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
	@echo "Building soyo_compiler..."
	@$(CARGO_TARGET_LINKER) cargo build -p soyo_compiler --release --target "$(MUSL_TARGET)" --target-dir "$(HOST_TARGET_DIR)" --quiet >/dev/null 2>&1
	@echo "Built soyo_compiler at $(COMPILER)"

build-lib: test-image
	$(DOCKER) run --rm -u "$$(id -u):$$(id -g)" \
		-v "$(CURDIR)/sysylib:/work/sysylib" \
		-w /work/sysylib \
		--entrypoint /bin/sh \
		"$(IMAGE)" -c 'aarch64-linux-gnu-gcc -O9 -c sylib.c -o sylib_arm.o && rm -f libsysy_arm.a && aarch64-linux-gnu-ar rcs libsysy_arm.a sylib_arm.o'

build-lib-riscv: test-image
	$(DOCKER) run --rm -u "$$(id -u):$$(id -g)" \
		-v "$(CURDIR)/sysylib:/work/sysylib" \
		-w /work/sysylib \
		--entrypoint /bin/sh \
		"$(IMAGE)" -c 'riscv64-linux-gnu-gcc -O9 -c sylib.c -o sylib_riscv.o && rm -f libsysy_riscv.a && riscv64-linux-gnu-ar rcs libsysy_riscv.a sylib_riscv.o'

clean-results:
	rm -rf "$(RESULTS)"
