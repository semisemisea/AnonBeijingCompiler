IMAGE ?= soyo-test-tools
CONTAINER ?= soyo-test
RESULTS ?= results
ARGS ?=
TESTS ?=
DOCKER ?= docker

ifeq ($(firstword $(MAKECMDGOALS)),test)
TEST_ARGS := $(wordlist 2,$(words $(MAKECMDGOALS)),$(MAKECMDGOALS))
$(eval $(TEST_ARGS):;@:)
endif

ifeq ($(firstword $(MAKECMDGOALS)),run-elf)
RUN_ELF := $(word 2,$(MAKECMDGOALS))
RUN_ELF_PATH := $(abspath $(RUN_ELF))
$(eval $(RUN_ELF):;@:)
endif

ifeq ($(firstword $(MAKECMDGOALS)),debug-elf)
DEBUG_ELF := $(word 2,$(MAKECMDGOALS))
DEBUG_ELF_PATH := $(abspath $(DEBUG_ELF))
$(eval $(DEBUG_ELF):;@:)
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

.PHONY: test run-elf debug-elf test-image test-compiler clean-results

test: test-compiler .docker-image
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

run-elf: .docker-image
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

debug-elf: .docker-image
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

# Build the test image if it doesn't exist or if Dockerfile/tests/test.py have changed
test-image: .docker-image

.docker-image: Dockerfile tests/test.py
	$(DOCKER) build -f Dockerfile -t "$(IMAGE)" .
	date '+%Y-%m-%dT%H:%M%z' > .docker-image

test-compiler:
	$(CARGO_TARGET_LINKER) cargo build -p soyo_compiler --release --target "$(MUSL_TARGET)" --target-dir "$(HOST_TARGET_DIR)" --quiet

clean-results:
	rm -rf "$(RESULTS)"
