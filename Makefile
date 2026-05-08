IMAGE ?= soyo-test-tools
RESULTS ?= results
ARGS ?=
TESTS ?=

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
	docker run -t --rm --network none \
		-e SOYO_COMPILER="$(COMPILER)" \
		-v "$(HOST_TARGET_DIR):/work/target:ro" \
		-v "$(CURDIR)/tests:/work/tests:ro" \
		-v "$(CURDIR)/sysylib:/work/sysylib:ro" \
		-v "$(CURDIR)/$(RESULTS):/work/results:rw" \
		"$(IMAGE)" $(ARGS) $(TESTS)

# Build the test image if it doesn't exist or if Dockerfile/tests/test.py have changed
test-image: .docker-image

.docker-image: Dockerfile tests/test.py
	docker build -f Dockerfile -t "$(IMAGE)" .
	date --iso-8601=minutes > .docker-image

test-compiler:
	$(CARGO_TARGET_LINKER) cargo build -p soyo_compiler --release --target "$(MUSL_TARGET)" --target-dir "$(HOST_TARGET_DIR)" --quiet

clean-results:
	rm -rf "$(RESULTS)"
