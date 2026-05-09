rustfimware
===========

Freestanding RISC-V firmware written in Rust.

Project constraints
-------------------

- This is freestanding software. Do not use system libraries or ``std``.
- Compile for the ``riscv64`` architecture.
- The load address is ``0x80200000``.
- At entry, register ``a0`` contains the boot hart ID.
- At entry, register ``a1`` contains a pointer to the device tree.
- Save ``a0`` and ``a1`` so they can be passed on to the payload.

Ubuntu setup
------------

Install the required Ubuntu packages:

.. code-block:: bash

   sudo apt update
   sudo apt install -y make rustup llvm lld binutils-riscv64-unknown-elf curl xz-utils

Initialize Rust and add the target:

.. code-block:: bash

   rustup default stable
   rustup target add riscv64imac-unknown-none-elf

Optional tools:

- ``qemu-system-misc`` for boot testing in QEMU
- ``gdb-multiarch`` for debugging

If you want ``rust-objcopy`` instead of relying on ``llvm-objcopy``, install:

.. code-block:: bash

   rustup component add llvm-tools-preview
   cargo install cargo-binutils

Build
-----

Build the raw firmware image:

.. code-block:: bash

   make bin

The raw binary output is written to ``build/rustfimware.bin``.

Run in QEMU
-----------

Boot the raw firmware image with QEMU. The first run downloads the Ubuntu 26.04 preinstalled RISC-V server image and extracts it to ``test.img``:

.. code-block:: bash

   make check

Debug with GDB
--------------

Start QEMU halted at reset and listen for GDB on TCP port ``1234``:

.. code-block:: bash

   make debug

In another terminal, connect with ``gdb-multiarch`` using the ELF image for symbols:

.. code-block:: bash

   cd rustfimware
   gdb-multiarch target/riscv64imac-unknown-none-elf/release/rustfimware

Inside GDB, use:

.. code-block:: gdb

   set architecture riscv:rv64
   target remote :1234
   break rust_entry
   continue