# Recurring Project Instructions

- This is freestanding software. Do not use system libraries or `std`.
- This firmware runs on top of OpenSBI.
- Compile for the `riscv64` architecture.
- The load address is `0x80200000`.
- At entry, register `a0` contains the boot hart ID.
- At entry, register `a1` contains a pointer to the device tree.
- Save `a0` and `a1` so they can be passed on to the payload.
- For git commits in this repository, use the global Git user settings.