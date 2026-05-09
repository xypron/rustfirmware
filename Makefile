TARGET := riscv64imac-unknown-none-elf
PROFILE ?= release
PACKAGE := rustfimware
BUILD_DIR := target/$(TARGET)/$(PROFILE)
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

.PHONY: all build docs check debug clean

all: $(BIN)

build: $(BIN)

$(ELF):
	PROFILE_NAME=$(PROFILE) cargo build --target $(TARGET) --profile $(PROFILE) --bin $(PACKAGE)

$(BIN): $(ELF)
	mkdir -p $(dir $(BIN))
	@objcopy="$(OBJCOPY)"; \
	test -n "$$objcopy" || (echo "error: need rust-objcopy or llvm-objcopy in PATH" >&2; exit 1); \
	"$$objcopy" -O binary $(ELF) $(BIN)
	@echo "raw firmware image: $(BIN)"

docs:
	PROFILE_NAME=$(PROFILE) cargo doc --no-deps

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

clean:
	cargo clean
	rm -f tests/data/in*.dtb tests/data/out*.dtb
