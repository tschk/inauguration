//! Generated freestanding linker layout descriptors.
//!
//! Produces a minimal GNU ld script body for closed-world MCU images.
//! Architecture capsules (board bases, IRQ vectors) remain product-owned.

use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRegion {
    pub name: String,
    pub origin: u64,
    pub length: u64,
    pub attrs: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkerLayout {
    pub entry: String,
    pub regions: Vec<MemoryRegion>,
    pub text_region: String,
    pub data_region: String,
    pub vector_symbol: String,
}

impl LinkerLayout {
    pub fn cortex_m_default(
        flash_origin: u64,
        flash_len: u64,
        ram_origin: u64,
        ram_len: u64,
    ) -> Self {
        Self {
            entry: "Reset".to_string(),
            regions: vec![
                MemoryRegion {
                    name: "FLASH".to_string(),
                    origin: flash_origin,
                    length: flash_len,
                    attrs: "rx".to_string(),
                },
                MemoryRegion {
                    name: "RAM".to_string(),
                    origin: ram_origin,
                    length: ram_len,
                    attrs: "rwx".to_string(),
                },
            ],
            text_region: "FLASH".to_string(),
            data_region: "RAM".to_string(),
            vector_symbol: "__vector_table".to_string(),
        }
    }

    pub fn to_ld_script(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "ENTRY({})", self.entry);
        out.push_str("MEMORY\n{\n");
        for region in &self.regions {
            let _ = writeln!(
                out,
                "  {} ({}) : ORIGIN = 0x{:08X}, LENGTH = 0x{:X}",
                region.name, region.attrs, region.origin, region.length
            );
        }
        out.push_str("}\n\nSECTIONS\n{\n");
        let _ = writeln!(
            out,
            "  .vector : {{ KEEP(*(.vector)) {} = .; }} > {}",
            self.vector_symbol, self.text_region
        );
        let _ = writeln!(
            out,
            "  .text : {{ *(.text*) *(.rodata*) }} > {}",
            self.text_region
        );
        let _ = writeln!(
            out,
            "  .data : {{ *(.data*) }} > {} AT > {}",
            self.data_region, self.text_region
        );
        let _ = writeln!(
            out,
            "  .bss : {{ *(.bss*) *(COMMON) }} > {}",
            self.data_region
        );
        out.push_str("}\n");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an521_layout_script_contains_regions() {
        let layout =
            LinkerLayout::cortex_m_default(0x1000_0000, 512 * 1024, 0x3800_0000, 256 * 1024);
        let script = layout.to_ld_script();
        assert!(script.contains("ENTRY(Reset)"));
        assert!(script.contains("ORIGIN = 0x10000000"));
        assert!(script.contains("LENGTH = 0x80000"));
        assert!(script.contains("ORIGIN = 0x38000000"));
        assert!(script.contains(".vector"));
        assert!(script.contains("__vector_table"));
    }
}
