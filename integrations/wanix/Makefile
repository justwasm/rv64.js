RV64_DIR := ../..
ADAPTER := adapter
DIST := dist
RV64_RUST_SOURCES := $(shell find $(RV64_DIR)/crates/rv64-core $(RV64_DIR)/crates/rv64-system $(RV64_DIR)/crates/rv64-dbt $(RV64_DIR)/crates/rv64-wasm -type f -name '*.rs')
RELEASE_TAG := v0.3.0
RELEASE_URL := https://github.com/justwasm/rv64.js/releases/download/$(RELEASE_TAG)
RELEASE_JS_SHA256 := 000389d6b33c7556c98b6ee87fa984c18e11b70b1c4e243cce86f568e1b13292
RELEASE_WASM_SHA256 := 2c8036fba60dca9e62736338c985aac909dd13db536bc2227a19770be04bd3ba
RELEASE_STAGE := $(DIST)/rv64-release
LEGACY_TAG := v0.1.0
LEGACY_RELEASE := https://github.com/ibuildthecloud/rv64.js/releases/download/$(LEGACY_TAG)
LEGACY_JS_SHA256 := 54df79c8b35cf50bcee34c4af02d7eb02b09e0439b717ee75bb830e733595b12
LEGACY_WASM_SHA256 := 001e5158b8f47f981371eae201079e6ec3eb632ab39ba1d0caf9a1ca5412b049
LEGACY_STAGE := $(DIST)/rv64-legacy
JIT_STAGE := $(DIST)/rv64-jit

build: $(DIST)/rv64.tgz
.PHONY: build

adapter: $(DIST)/rv64-vm.wasm
.PHONY: adapter

bundle: $(DIST)/rv64.tgz
.PHONY: bundle

bundle-workspace: $(DIST)/rv64-workspace.tgz
.PHONY: bundle-workspace

comparison: $(DIST)/rv64-legacy.tgz $(DIST)/rv64-jit.tgz $(DIST)/wanix-linux-rv64.tgz $(DIST)/wanix-linux-x86.tgz
.PHONY: comparison

$(DIST)/rv64.tgz: $(DIST)/rv64-vm.wasm $(RELEASE_STAGE)/rv64_wasm.wasm $(RELEASE_STAGE)/rv64.js
	tar -czf $(abspath $@) -C $(DIST) rv64-vm.wasm -C $(abspath $(RELEASE_STAGE)) rv64_wasm.wasm rv64.js

$(DIST)/rv64-workspace.tgz: $(DIST)/rv64-vm.wasm $(DIST)/rv64_wasm.wasm $(RV64_DIR)/web/rv64.js
	tar -czf $(abspath $@) -C $(DIST) rv64-vm.wasm rv64_wasm.wasm -C $(abspath $(RV64_DIR)/web) rv64.js

$(DIST)/rv64-vm.wasm: $(wildcard $(ADAPTER)/*.go) $(ADAPTER)/p9-handler.js
	mkdir -p $(DIST)
	cd $(ADAPTER) && GOWORK=off GOOS=js GOARCH=wasm go build -o ../$@ .

$(DIST)/rv64_wasm.wasm: $(RV64_RUST_SOURCES) $(RV64_DIR)/Cargo.toml $(RV64_DIR)/Cargo.lock
	mkdir -p $(DIST)
	cargo build --manifest-path $(RV64_DIR)/Cargo.toml -p rv64-wasm --target wasm32-unknown-unknown --release
	cp $(RV64_DIR)/target/wasm32-unknown-unknown/release/rv64_wasm.wasm $@

$(RELEASE_STAGE)/rv64.js: Makefile
	mkdir -p $(RELEASE_STAGE)
	curl -fsSL $(RELEASE_URL)/rv64.js -o $@.download
	printf '%s  %s\n' '$(RELEASE_JS_SHA256)' '$@.download' | sha256sum -c -
	mv $@.download $@

$(RELEASE_STAGE)/rv64_wasm.wasm: Makefile
	mkdir -p $(RELEASE_STAGE)
	curl -fsSL $(RELEASE_URL)/rv64_wasm.wasm -o $@.download
	printf '%s  %s\n' '$(RELEASE_WASM_SHA256)' '$@.download' | sha256sum -c -
	mv $@.download $@

$(JIT_STAGE)/rv64-jit-vm.wasm: $(DIST)/rv64-vm.wasm
	mkdir -p $(JIT_STAGE)
	cp $< $@

$(JIT_STAGE)/rv64_wasm.wasm: $(DIST)/rv64_wasm.wasm
	mkdir -p $(JIT_STAGE)
	cp $< $@

$(JIT_STAGE)/rv64.js: $(RV64_DIR)/web/rv64.js
	mkdir -p $(JIT_STAGE)
	cp $< $@

$(DIST)/rv64-jit.tgz: $(JIT_STAGE)/rv64-jit-vm.wasm $(JIT_STAGE)/rv64_wasm.wasm $(JIT_STAGE)/rv64.js
	tar -czf $(abspath $@) -C $(JIT_STAGE) rv64-jit-vm.wasm rv64_wasm.wasm rv64.js

$(LEGACY_STAGE)/rv64-legacy-vm.wasm: $(DIST)/rv64-vm.wasm
	mkdir -p $(LEGACY_STAGE)
	cp $< $@

$(LEGACY_STAGE)/rv64.js: Makefile
	mkdir -p $(LEGACY_STAGE)
	curl -fsSL $(LEGACY_RELEASE)/rv64.js -o $@.download
	printf '%s  %s\n' '$(LEGACY_JS_SHA256)' '$@.download' | sha256sum -c -
	mv $@.download $@

$(LEGACY_STAGE)/rv64_wasm.wasm: Makefile
	mkdir -p $(LEGACY_STAGE)
	curl -fsSL $(LEGACY_RELEASE)/rv64_wasm.wasm -o $@.download
	printf '%s  %s\n' '$(LEGACY_WASM_SHA256)' '$@.download' | sha256sum -c -
	mv $@.download $@

$(DIST)/rv64-legacy.tgz: $(LEGACY_STAGE)/rv64-legacy-vm.wasm $(LEGACY_STAGE)/rv64_wasm.wasm $(LEGACY_STAGE)/rv64.js
	tar -czf $(abspath $@) -C $(LEGACY_STAGE) rv64-legacy-vm.wasm rv64_wasm.wasm rv64.js

LINUX_COMMON_DEPS := build-linux-bundle.sh wanix-wexec-js.patch wanix-wexec-poll.patch wanix-wexec-signal.patch wanix-wexec-live-read.patch guest/init

$(DIST)/wanix-linux-rv64.tgz: $(LINUX_COMMON_DEPS) wanix-riscv64.patch $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/rv64-config.nix
	WANIX_GUEST_ARCH=riscv64 WANIX_KERNEL_PROFILE=minimal WANIX_ROOTFS_PROFILE=minimal ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-rv64-crush.tgz: $(LINUX_COMMON_DEPS) wanix-riscv64.patch $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/rv64-config.nix
	WANIX_GUEST_ARCH=riscv64 WANIX_KERNEL_PROFILE=minimal WANIX_ROOTFS_PROFILE=crush ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-rv64-python.tgz: $(LINUX_COMMON_DEPS) wanix-riscv64.patch $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/rv64-config.nix
	WANIX_GUEST_ARCH=riscv64 WANIX_KERNEL_PROFILE=minimal WANIX_ROOTFS_PROFILE=python ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-rv64-nodejs.tgz: $(LINUX_COMMON_DEPS) wanix-riscv64.patch $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/rv64-config.nix
	WANIX_GUEST_ARCH=riscv64 WANIX_KERNEL_PROFILE=minimal WANIX_ROOTFS_PROFILE=nodejs ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-rv64-claude.tgz: $(LINUX_COMMON_DEPS) wanix-riscv64.patch $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/rv64-config.nix
	WANIX_GUEST_ARCH=riscv64 WANIX_KERNEL_PROFILE=minimal WANIX_ROOTFS_PROFILE=claude ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-rv64-pi.tgz: $(LINUX_COMMON_DEPS) wanix-riscv64.patch $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/rv64-config.nix
	WANIX_GUEST_ARCH=riscv64 WANIX_KERNEL_PROFILE=minimal WANIX_ROOTFS_PROFILE=pi ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-rv64-golang.tgz: $(LINUX_COMMON_DEPS) wanix-riscv64.patch $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/rv64-config.nix
	WANIX_GUEST_ARCH=riscv64 WANIX_KERNEL_PROFILE=minimal WANIX_ROOTFS_PROFILE=golang ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-rv64-container.tgz: $(LINUX_COMMON_DEPS) wanix-riscv64.patch $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/rv64-container-config.nix
	WANIX_GUEST_ARCH=riscv64 WANIX_KERNEL_PROFILE=container WANIX_ROOTFS_PROFILE=minimal ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-rv64-container-full.tgz: $(LINUX_COMMON_DEPS) wanix-riscv64.patch $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/rv64-container-config.nix
	WANIX_GUEST_ARCH=riscv64 WANIX_KERNEL_PROFILE=container WANIX_ROOTFS_PROFILE=full ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-x86.tgz: $(LINUX_COMMON_DEPS) $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/x86-v86-config.nix
	WANIX_GUEST_ARCH=x86 WANIX_KERNEL_PROFILE=minimal WANIX_ROOTFS_PROFILE=minimal ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-x86-crush.tgz: $(LINUX_COMMON_DEPS) $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/x86-v86-config.nix
	WANIX_GUEST_ARCH=x86 WANIX_KERNEL_PROFILE=minimal WANIX_ROOTFS_PROFILE=crush ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-x86-python.tgz: $(LINUX_COMMON_DEPS) $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/x86-v86-config.nix
	WANIX_GUEST_ARCH=x86 WANIX_KERNEL_PROFILE=minimal WANIX_ROOTFS_PROFILE=python ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-x86-nodejs.tgz: $(LINUX_COMMON_DEPS) $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/x86-v86-config.nix
	WANIX_GUEST_ARCH=x86 WANIX_KERNEL_PROFILE=minimal WANIX_ROOTFS_PROFILE=nodejs ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-x86-claude.tgz: $(LINUX_COMMON_DEPS) $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/x86-v86-config.nix
	WANIX_GUEST_ARCH=x86 WANIX_KERNEL_PROFILE=minimal WANIX_ROOTFS_PROFILE=claude ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-x86-pi.tgz: $(LINUX_COMMON_DEPS) $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/x86-v86-config.nix
	WANIX_GUEST_ARCH=x86 WANIX_KERNEL_PROFILE=minimal WANIX_ROOTFS_PROFILE=pi ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-x86-golang.tgz: $(LINUX_COMMON_DEPS) $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/x86-v86-config.nix
	WANIX_GUEST_ARCH=x86 WANIX_KERNEL_PROFILE=minimal WANIX_ROOTFS_PROFILE=golang ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-x86-container.tgz: $(LINUX_COMMON_DEPS) $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/x86-v86-container-config.nix
	WANIX_GUEST_ARCH=x86 WANIX_KERNEL_PROFILE=container WANIX_ROOTFS_PROFILE=minimal ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-x86-container-full.tgz: $(LINUX_COMMON_DEPS) $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/x86-v86-container-config.nix
	WANIX_GUEST_ARCH=x86 WANIX_KERNEL_PROFILE=container WANIX_ROOTFS_PROFILE=full ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-arm64.tgz: $(LINUX_COMMON_DEPS) $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/arm64-config.nix
	WANIX_GUEST_ARCH=arm64 WANIX_KERNEL_PROFILE=minimal WANIX_ROOTFS_PROFILE=minimal ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-arm64-crush.tgz: $(LINUX_COMMON_DEPS) $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/arm64-config.nix
	WANIX_GUEST_ARCH=arm64 WANIX_KERNEL_PROFILE=minimal WANIX_ROOTFS_PROFILE=crush ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-arm64-python.tgz: $(LINUX_COMMON_DEPS) $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/arm64-config.nix
	WANIX_GUEST_ARCH=arm64 WANIX_KERNEL_PROFILE=minimal WANIX_ROOTFS_PROFILE=python ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-arm64-nodejs.tgz: $(LINUX_COMMON_DEPS) $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/arm64-config.nix
	WANIX_GUEST_ARCH=arm64 WANIX_KERNEL_PROFILE=minimal WANIX_ROOTFS_PROFILE=nodejs ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-arm64-claude.tgz: $(LINUX_COMMON_DEPS) $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/arm64-config.nix
	WANIX_GUEST_ARCH=arm64 WANIX_KERNEL_PROFILE=minimal WANIX_ROOTFS_PROFILE=claude ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-arm64-pi.tgz: $(LINUX_COMMON_DEPS) $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/arm64-config.nix
	WANIX_GUEST_ARCH=arm64 WANIX_KERNEL_PROFILE=minimal WANIX_ROOTFS_PROFILE=pi ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-arm64-golang.tgz: $(LINUX_COMMON_DEPS) $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/arm64-config.nix
	WANIX_GUEST_ARCH=arm64 WANIX_KERNEL_PROFILE=minimal WANIX_ROOTFS_PROFILE=golang ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-arm64-container.tgz: $(LINUX_COMMON_DEPS) $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/arm64-container-config.nix
	WANIX_GUEST_ARCH=arm64 WANIX_KERNEL_PROFILE=container WANIX_ROOTFS_PROFILE=minimal ./build-linux-bundle.sh $@

$(DIST)/wanix-linux-arm64-container-full.tgz: $(LINUX_COMMON_DEPS) $(RV64_DIR)/flake.nix $(RV64_DIR)/kernel/arm64-container-config.nix
	WANIX_GUEST_ARCH=arm64 WANIX_KERNEL_PROFILE=container WANIX_ROOTFS_PROFILE=full ./build-linux-bundle.sh $@

linux-rv64: $(DIST)/wanix-linux-rv64.tgz
.PHONY: linux-rv64

linux-rv64-crush: $(DIST)/wanix-linux-rv64-crush.tgz
.PHONY: linux-rv64-crush

linux-rv64-python: $(DIST)/wanix-linux-rv64-python.tgz
.PHONY: linux-rv64-python

linux-rv64-nodejs: $(DIST)/wanix-linux-rv64-nodejs.tgz
.PHONY: linux-rv64-nodejs

linux-rv64-claude: $(DIST)/wanix-linux-rv64-claude.tgz
.PHONY: linux-rv64-claude

linux-rv64-pi: $(DIST)/wanix-linux-rv64-pi.tgz
.PHONY: linux-rv64-pi

linux-rv64-golang: $(DIST)/wanix-linux-rv64-golang.tgz
.PHONY: linux-rv64-golang

linux-rv64-container: $(DIST)/wanix-linux-rv64-container.tgz
.PHONY: linux-rv64-container

linux-rv64-container-full: $(DIST)/wanix-linux-rv64-container-full.tgz
.PHONY: linux-rv64-container-full

linux-x86: $(DIST)/wanix-linux-x86.tgz
.PHONY: linux-x86

linux-x86-crush: $(DIST)/wanix-linux-x86-crush.tgz
.PHONY: linux-x86-crush

linux-x86-python: $(DIST)/wanix-linux-x86-python.tgz
.PHONY: linux-x86-python

linux-x86-nodejs: $(DIST)/wanix-linux-x86-nodejs.tgz
.PHONY: linux-x86-nodejs

linux-x86-claude: $(DIST)/wanix-linux-x86-claude.tgz
.PHONY: linux-x86-claude

linux-x86-pi: $(DIST)/wanix-linux-x86-pi.tgz
.PHONY: linux-x86-pi

linux-x86-golang: $(DIST)/wanix-linux-x86-golang.tgz
.PHONY: linux-x86-golang

linux-x86-container: $(DIST)/wanix-linux-x86-container.tgz
.PHONY: linux-x86-container

linux-x86-container-full: $(DIST)/wanix-linux-x86-container-full.tgz
.PHONY: linux-x86-container-full

linux-arm64: $(DIST)/wanix-linux-arm64.tgz
.PHONY: linux-arm64

linux-arm64-crush: $(DIST)/wanix-linux-arm64-crush.tgz
.PHONY: linux-arm64-crush

linux-arm64-python: $(DIST)/wanix-linux-arm64-python.tgz
.PHONY: linux-arm64-python

linux-arm64-nodejs: $(DIST)/wanix-linux-arm64-nodejs.tgz
.PHONY: linux-arm64-nodejs

linux-arm64-claude: $(DIST)/wanix-linux-arm64-claude.tgz
.PHONY: linux-arm64-claude

linux-arm64-pi: $(DIST)/wanix-linux-arm64-pi.tgz
.PHONY: linux-arm64-pi

linux-arm64-golang: $(DIST)/wanix-linux-arm64-golang.tgz
.PHONY: linux-arm64-golang

linux-arm64-container: $(DIST)/wanix-linux-arm64-container.tgz
.PHONY: linux-arm64-container

linux-arm64-container-full: $(DIST)/wanix-linux-arm64-container-full.tgz
.PHONY: linux-arm64-container-full

linux: linux-rv64 linux-x86 linux-arm64
.PHONY: linux

linux-container: linux-rv64-container linux-x86-container linux-arm64-container
.PHONY: linux-container

clean:
	rm -rf $(DIST)
.PHONY: clean
