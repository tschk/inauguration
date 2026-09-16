use super::{InError, PackCommands, Result};
use inauguration::native_emit::{LinkerLayout, Uf2Options, write_raw_binary, write_uf2};
use std::fs;
use std::path::{Path, PathBuf};

fn resolve(cwd: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn parse_u32(value: &str, flag: &str) -> Result<u32> {
    if let Some(stripped) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u32::from_str_radix(stripped, 16)
            .map_err(|_| InError::Message(format!("invalid hex {flag}: {value}")))
    } else {
        value
            .parse::<u32>()
            .map_err(|_| InError::Message(format!("invalid {flag}: {value}")))
    }
}

fn parse_u64(value: &str, flag: &str) -> Result<u64> {
    if let Some(stripped) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u64::from_str_radix(stripped, 16)
            .map_err(|_| InError::Message(format!("invalid hex {flag}: {value}")))
    } else {
        value
            .parse::<u64>()
            .map_err(|_| InError::Message(format!("invalid {flag}: {value}")))
    }
}

pub(crate) fn cmd_pack(cwd: &Path, action: PackCommands) -> Result<()> {
    match action {
        PackCommands::Uf2 {
            input,
            out,
            addr,
            family,
        } => {
            let input_path = resolve(cwd, &input);
            let out_path = resolve(cwd, &out);
            let payload = fs::read(&input_path)
                .map_err(|e| InError::Message(format!("read {}: {e}", input_path.display())))?;
            let options = Uf2Options {
                family_id: family
                    .as_deref()
                    .map(|v| parse_u32(v, "--family"))
                    .transpose()?,
                target_addr: parse_u32(&addr, "--addr")?,
            };
            write_uf2(&payload, &options, &out_path).map_err(InError::Message)?;
            println!("uf2: {} → {}", input_path.display(), out_path.display());
            Ok(())
        }
        PackCommands::Raw { input, out } => {
            let input_path = resolve(cwd, &input);
            let out_path = resolve(cwd, &out);
            let payload = fs::read(&input_path)
                .map_err(|e| InError::Message(format!("read {}: {e}", input_path.display())))?;
            write_raw_binary(&payload, &out_path).map_err(InError::Message)?;
            println!("raw: {} → {}", input_path.display(), out_path.display());
            Ok(())
        }
        PackCommands::Linker {
            out,
            flash_origin,
            flash_len,
            ram_origin,
            ram_len,
            entry,
        } => {
            let out_path = resolve(cwd, &out);
            let mut layout = LinkerLayout::cortex_m_default(
                parse_u64(&flash_origin, "--flash-origin")?,
                parse_u64(&flash_len, "--flash-len")?,
                parse_u64(&ram_origin, "--ram-origin")?,
                parse_u64(&ram_len, "--ram-len")?,
            );
            layout.entry = entry;
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| InError::Message(format!("create {}: {e}", parent.display())))?;
            }
            fs::write(&out_path, layout.to_ld_script())
                .map_err(|e| InError::Message(format!("write {}: {e}", out_path.display())))?;
            println!("linker: {}", out_path.display());
            Ok(())
        }
    }
}
