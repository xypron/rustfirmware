# Recurring Project Instructions

- This is freestanding software. Do not use system libraries or `std`.
- This firmware runs on top of OpenSBI.
- Compile for the `riscv64` architecture.
- The load address is `0x80200000`.
- At entry, register `a0` contains the boot hart ID.
- At entry, register `a1` contains a pointer to the device tree.
- Save `a0` and `a1` so they can be passed on to the payload.
- For git commits in this repository, use the global Git user settings.
- All git commits in this repository must include a `Signed-off-by:`
  trailer.
- Prefer a maximum line length of 80 characters in both code and
	documentation.
- Add rustdoc comments for every module, static, constant, type alias, enum, struct, function, and method, including non-public helpers.
- Document every function and method parameter, and document every struct field.
- Place each rustdoc block immediately above the item it documents.