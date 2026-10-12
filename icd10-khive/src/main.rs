//! The stock khive CLI (`kkernel`) with the `icd10` pack force-linked.
//!
//! Select it at runtime with `KHIVE_PACKS=kg,icd10` (or `[runtime].packs` in
//! `khive.toml`). Registration happens via `inventory::submit!` inside the pack
//! crate; this `use` only forces the linker to pull the crate in, since nothing
//! else here references it.
use khive_pack_icd10 as _;

fn main() -> anyhow::Result<()> {
    kkernel::cli::cli_main()
}
