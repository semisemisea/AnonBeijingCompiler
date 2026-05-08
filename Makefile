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

.PHONY: test test-image test-compiler clean-results

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

# Build the test image if it doesn't exist or if Dockerfile/tests/test.py have changed
test-image: .docker-image

.docker-image: Dockerfile tests/test.py
	$(DOCKER) build -f Dockerfile -t "$(IMAGE)" .
	date '+%Y-%m-%dT%H:%M%z' > .docker-image

test-compiler:
	$(CARGO_TARGET_LINKER) cargo build -p soyo_compiler --release --target "$(MUSL_TARGET)" --target-dir "$(HOST_TARGET_DIR)" --quiet

clean-results:
	rm -rf "$(RESULTS)"
