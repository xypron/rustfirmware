HOST_TARGET := $(shell rustc -vV | sed -n 's/^host: //p')
TARGET := riscv64imac-unknown-none-elf
PROFILE ?= release
PACKAGE := rustfimware
PROFILE_DIR := $(if $(filter $(PROFILE),dev),debug,$(PROFILE))
BUILD_DIR := target/$(TARGET)/$(PROFILE_DIR)
ELF := $(BUILD_DIR)/$(PACKAGE)
BIN := build/$(PACKAGE).bin
# UBUNTU_IMG_URL := https://cdimage.ubuntu.com/releases/26.04/release/ubuntu-26.04-preinstalled-server-riscv64.img.xz
UBUNTU_IMG_URL := https://cdimage.ubuntu.com/releases/jammy/release/ubuntu-22.04.5-preinstalled-server-riscv64+unmatched.img.xz
OBJCOPY := $(shell command -v rust-objcopy 2>/dev/null || command -v llvm-objcopy 2>/dev/null)
QEMU := qemu-system-riscv64
QEMU_MACHINE := virt
QEMU_CPU := rva23s64
QEMU_FLAGS := -nographic
QEMU_SMP := 2
QEMU_MEMORY := 1G
QEMU_VIRTIO_MMIO_BUS := virtio-mmio-bus.0
QEMU_VIRTIO_MMIO_FLAGS := -global virtio-mmio.force-legacy=false
QEMU_NETDEV_ID := net0
QEMU_NETDEV_FLAGS := -netdev user,id=$(QEMU_NETDEV_ID)
QEMU_VIRTIO_NET_FLAGS := -device virtio-net-device,netdev=$(QEMU_NETDEV_ID)
QEMU_GDB_PORT := 1234

.PHONY: all build build_diagnostic docs check debug clean gpt-test fat-test ext4-test

all: $(BIN)

build:
	$(MAKE) -B RUSTFW_PRINT_MEMORY_LAYOUT=0 $(BIN)

build_diagnostic:
	$(MAKE) -B RUSTFW_PRINT_MEMORY_LAYOUT=1 $(BIN)

$(ELF):
	RUSTFW_PRINT_MEMORY_LAYOUT=$(RUSTFW_PRINT_MEMORY_LAYOUT) PROFILE_NAME=$(PROFILE) cargo build --target $(TARGET) --profile $(PROFILE) --bin $(PACKAGE)

$(BIN): $(ELF)
	mkdir -p $(dir $(BIN))
	@objcopy="$(OBJCOPY)"; \
	test -n "$$objcopy" || (echo "error: need rust-objcopy or llvm-objcopy in PATH" >&2; exit 1); \
	"$$objcopy" -O binary $(ELF) $(BIN)
	@echo "raw firmware image: $(BIN)"

docs:
	PROFILE_NAME=$(PROFILE) cargo doc --no-deps --bin $(PACKAGE)
	rm -rf target/doc
	mkdir -p target/doc
	cp -a target/$(TARGET)/doc/. target/doc/
	@echo "API docs: target/doc/$(PACKAGE)/index.html"

test.img:
	if [ ! -f test.img.xz ]; then wget $(UBUNTU_IMG_URL) -O test.img.xz; fi
	xz -dk test.img.xz

check: $(BIN) test.img
	$(QEMU) \
		-M $(QEMU_MACHINE) \
		-cpu $(QEMU_CPU) \
		-smp $(QEMU_SMP) \
		-m $(QEMU_MEMORY) \
		$(QEMU_FLAGS) \
		$(QEMU_VIRTIO_MMIO_FLAGS) \
		$(QEMU_NETDEV_FLAGS) \
		$(QEMU_VIRTIO_NET_FLAGS) \
		-kernel $(BIN) \
		-drive file=test.img,format=raw,id=drv0,if=none \
		-device virtio-blk-device,drive=drv0,bus=$(QEMU_VIRTIO_MMIO_BUS),bootindex=1

debug: $(BIN) test.img
	$(QEMU) \
		-M $(QEMU_MACHINE) \
		-cpu $(QEMU_CPU) \
		-smp $(QEMU_SMP) \
		-m $(QEMU_MEMORY) \
		$(QEMU_FLAGS) \
		$(QEMU_VIRTIO_MMIO_FLAGS) \
		$(QEMU_NETDEV_FLAGS) \
		$(QEMU_VIRTIO_NET_FLAGS) \
		-S \
		-gdb tcp::$(QEMU_GDB_PORT) \
		-kernel $(BIN) \
		-drive file=test.img,format=raw,id=drv0,if=none \
		-device virtio-blk-device,drive=drv0,bus=$(QEMU_VIRTIO_MMIO_BUS),bootindex=1

gpt-test:
	cargo run --target $(HOST_TARGET) --bin gpt_test -- $(ARGS)

fat-test:
	cargo run --target $(HOST_TARGET) --bin fat_test -- $(ARGS)

ext4-test:
	cargo run --target $(HOST_TARGET) --bin ext4_test -- $(ARGS)

clean:
	cargo clean
	rm -f tests/data/in*.dtb tests/data/out*.dtb
