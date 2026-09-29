//! Microsoft UF2 image packing for freestanding flash payloads.
//!
//! Generic helper: not branded to any OS product. Family IDs and load addresses
//! are caller-supplied.

use std::fs;
use std::path::Path;

pub const UF2_MAGIC_START0: u32 = 0x0A32_4655; // "UF2\n"
pub const UF2_MAGIC_START1: u32 = 0x9E5D_5157;
pub const UF2_MAGIC_END: u32 = 0x0AB1_6F30;
pub const UF2_BLOCK_SIZE: usize = 512;
pub const UF2_PAYLOAD_MAX: usize = 476;
pub const UF2_FLAG_FAMILY_ID_PRESENT: u32 = 0x0000_2000;

/// RP2350 / Pico 2 absolute family id used by picotool (optional).
pub const UF2_FAMILY_RP2350_ARM_S: u32 = 0xE48B_FF59;

pub struct Uf2Options {
    pub family_id: Option<u32>,
    pub target_addr: u32,
}

impl Default for Uf2Options {
    fn default() -> Self {
        Self {
            family_id: None,
            target_addr: 0x1000_0000,
        }
    }
}

pub fn encode_uf2(payload: &[u8], options: &Uf2Options) -> Result<Vec<u8>, String> {
    if payload.is_empty() {
        return Err("uf2: empty payload".to_string());
    }
    let num_blocks = payload.len().div_ceil(UF2_PAYLOAD_MAX) as u32;
    let mut out = Vec::with_capacity(num_blocks as usize * UF2_BLOCK_SIZE);
    let mut flags = 0u32;
    if options.family_id.is_some() {
        flags |= UF2_FLAG_FAMILY_ID_PRESENT;
    }
    for block_no in 0..num_blocks {
        let start = block_no as usize * UF2_PAYLOAD_MAX;
        let end = (start + UF2_PAYLOAD_MAX).min(payload.len());
        let chunk = &payload[start..end];
        let mut block = [0u8; UF2_BLOCK_SIZE];
        write_u32(&mut block, 0, UF2_MAGIC_START0);
        write_u32(&mut block, 4, UF2_MAGIC_START1);
        write_u32(&mut block, 8, flags);
        write_u32(
            &mut block,
            12,
            options.target_addr.wrapping_add(start as u32),
        );
        write_u32(&mut block, 16, chunk.len() as u32);
        write_u32(&mut block, 20, block_no);
        write_u32(&mut block, 24, num_blocks);
        write_u32(&mut block, 28, options.family_id.unwrap_or(0));
        block[32..32 + chunk.len()].copy_from_slice(chunk);
        write_u32(&mut block, 508, UF2_MAGIC_END);
        out.extend_from_slice(&block);
    }
    Ok(out)
}

pub fn write_uf2(payload: &[u8], options: &Uf2Options, out_path: &Path) -> Result<(), String> {
    let bytes = encode_uf2(payload, options)?;
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("uf2: create parent `{}`: {err}", parent.display()))?;
    }
    fs::write(out_path, bytes).map_err(|err| format!("uf2: write `{}`: {err}", out_path.display()))
}

fn write_u32(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_u32(bytes: &[u8]) -> Result<u32, String> {
        let arr: [u8; 4] = bytes.try_into().map_err(|_| "invalid slice length")?;
        Ok(u32::from_le_bytes(arr))
    }

    #[test]
    fn encodes_single_block_with_family() -> Result<(), String> {
        let payload = b"hello-thumb";
        let bytes = encode_uf2(
            payload,
            &Uf2Options {
                family_id: Some(UF2_FAMILY_RP2350_ARM_S),
                target_addr: 0x1000_0000,
            },
        )?;
        assert_eq!(bytes.len(), UF2_BLOCK_SIZE);
        assert_eq!(read_u32(&bytes[0..4])?, UF2_MAGIC_START0);
        assert_eq!(read_u32(&bytes[4..8])?, UF2_MAGIC_START1);
        assert_eq!(
            read_u32(&bytes[8..12])? & UF2_FLAG_FAMILY_ID_PRESENT,
            UF2_FLAG_FAMILY_ID_PRESENT
        );
        assert_eq!(read_u32(&bytes[12..16])?, 0x1000_0000);
        assert_eq!(read_u32(&bytes[16..20])?, payload.len() as u32);
        assert_eq!(&bytes[32..32 + payload.len()], payload);
        assert_eq!(read_u32(&bytes[508..512])?, UF2_MAGIC_END);
        Ok(())
    }

    #[test]
    fn splits_large_payload() -> Result<(), String> {
        let payload = vec![0xABu8; UF2_PAYLOAD_MAX + 10];
        let bytes = encode_uf2(&payload, &Uf2Options::default())?;
        assert_eq!(bytes.len(), UF2_BLOCK_SIZE * 2);
        assert_eq!(read_u32(&bytes[24..28])?, 2);
        assert_eq!(
            read_u32(&bytes[UF2_BLOCK_SIZE + 20..UF2_BLOCK_SIZE + 24])?,
            1
        );
        Ok(())
    }

    #[test]
    fn rejects_empty() {
        let err = encode_uf2(&[], &Uf2Options::default()).expect_err("empty");
        assert!(err.contains("empty payload"));
    }
}
