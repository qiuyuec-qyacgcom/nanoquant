//! 最小 ZIP 读写：DEFLATE + CRC32，兼容 legacy 内核（已退役） `zipfile` 的明文备份包。
//!
//! 只实现备份需要的子集：单卷、无加密 ZIP 容器（加密由 Fernet 包在容器外）、
//! 条目名 UTF-8、STORE/DEFLATE 两种方法。读取时逐条校验 CRC，坏包直接拒绝，
//! 避免把半截数据写回库。

use std::io::{Cursor, Read, Write};

use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use flate2::Compression;

const LOCAL_SIG: u32 = 0x0403_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const END_SIG: u32 = 0x0605_4b50;
const MAX_ENTRIES: usize = 10_000;

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub data: Vec<u8>,
}

pub fn build(entries: &[Entry]) -> Result<Vec<u8>, String> {
    if entries.len() > MAX_ENTRIES {
        return Err(format!("ZIP 条目数超过上限 {MAX_ENTRIES}"));
    }
    let mut out = Vec::new();
    let mut central = Vec::new();
    for entry in entries {
        if entry.name.is_empty() || entry.name.len() > 1024 {
            return Err("ZIP 条目名为空或过长".to_string());
        }
        let crc = crc32fast::hash(&entry.data);
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(&entry.data)
            .map_err(|error| format!("压缩失败: {error}"))?;
        let compressed = encoder
            .finish()
            .map_err(|error| format!("压缩收尾失败: {error}"))?;
        let (method, payload) = if compressed.len() < entry.data.len() {
            (8_u16, compressed)
        } else {
            (0_u16, entry.data.clone())
        };
        let offset = out.len() as u32;
        write_local(&mut out, entry, method, crc, payload.len() as u32);
        out.extend_from_slice(&payload);
        write_central(
            &mut central,
            entry,
            method,
            crc,
            offset,
            payload.len() as u32,
        );
    }
    let central_offset = out.len() as u32;
    let central_size = central.len() as u32;
    out.extend_from_slice(&central);
    write_end(&mut out, entries.len() as u16, central_size, central_offset);
    Ok(out)
}

fn write_local(out: &mut Vec<u8>, entry: &Entry, method: u16, crc: u32, compressed: u32) {
    push_u32(out, LOCAL_SIG);
    push_u16(out, 20);
    push_u16(out, 0x0800);
    push_u16(out, method);
    push_u16(out, 0);
    push_u16(out, 0);
    push_u32(out, crc);
    push_u32(out, compressed);
    push_u32(out, entry.data.len() as u32);
    push_u16(out, entry.name.len() as u16);
    push_u16(out, 0);
    out.extend_from_slice(entry.name.as_bytes());
}

fn write_central(
    central: &mut Vec<u8>,
    entry: &Entry,
    method: u16,
    crc: u32,
    offset: u32,
    compressed: u32,
) {
    push_u32(central, CENTRAL_SIG);
    push_u16(central, 20);
    push_u16(central, 20);
    push_u16(central, 0x0800);
    push_u16(central, method);
    push_u16(central, 0);
    push_u16(central, 0);
    push_u32(central, crc);
    push_u32(central, compressed);
    push_u32(central, entry.data.len() as u32);
    push_u16(central, entry.name.len() as u16);
    push_u16(central, 0);
    push_u16(central, 0);
    push_u16(central, 0);
    push_u16(central, 0);
    push_u32(central, 0);
    push_u32(central, offset);
    central.extend_from_slice(entry.name.as_bytes());
}

fn write_end(out: &mut Vec<u8>, count: u16, size: u32, offset: u32) {
    push_u32(out, END_SIG);
    push_u16(out, 0);
    push_u16(out, 0);
    push_u16(out, count);
    push_u16(out, count);
    push_u32(out, size);
    push_u32(out, offset);
    push_u16(out, 0);
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn read_u16(raw: &[u8], offset: usize) -> Result<u16, String> {
    raw.get(offset..offset + 2)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .ok_or_else(|| "ZIP 数据截断".to_string())
}
fn read_u32(raw: &[u8], offset: usize) -> Result<u32, String> {
    raw.get(offset..offset + 4)
        .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .ok_or_else(|| "ZIP 数据截断".to_string())
}

/// 读取 ZIP 全部条目。任一 CRC 不符即整包失败。
pub fn read(raw: &[u8]) -> Result<Vec<Entry>, String> {
    if raw.len() < 22 {
        return Err("ZIP 文件过短".to_string());
    }
    let search_start = raw.len().saturating_sub(65_557);
    let mut end_offset = None;
    for offset in (search_start..=raw.len() - 22).rev() {
        if read_u32(raw, offset).ok() == Some(END_SIG) {
            end_offset = Some(offset);
            break;
        }
    }
    let end = end_offset.ok_or_else(|| "找不到 ZIP 目录末尾标记".to_string())?;
    let count = read_u16(raw, end + 10)? as usize;
    if count > MAX_ENTRIES {
        return Err(format!("ZIP 条目数超过上限 {MAX_ENTRIES}"));
    }
    let central_offset = read_u32(raw, end + 16)? as usize;
    let mut cursor = central_offset;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        if read_u32(raw, cursor)? != CENTRAL_SIG {
            return Err("ZIP 中央目录损坏".to_string());
        }
        let method = read_u16(raw, cursor + 10)?;
        let crc = read_u32(raw, cursor + 16)?;
        let compressed = read_u32(raw, cursor + 20)? as usize;
        let uncompressed = read_u32(raw, cursor + 24)? as usize;
        let name_len = read_u16(raw, cursor + 28)? as usize;
        let extra_len = read_u16(raw, cursor + 30)? as usize;
        let comment_len = read_u16(raw, cursor + 32)? as usize;
        let local_offset = read_u32(raw, cursor + 42)? as usize;
        let name_bytes = raw
            .get(cursor + 46..cursor + 46 + name_len)
            .ok_or_else(|| "ZIP 条目名截断".to_string())?;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| "ZIP 条目名不是 UTF-8".to_string())?
            .replace('\\', "/");
        if name.is_empty()
            || name.starts_with('/')
            || name.split('/').any(|part| part == ".." || part == ".")
        {
            return Err(format!("ZIP 条目名非法: {name}"));
        }
        if read_u32(raw, local_offset)? != LOCAL_SIG {
            return Err(format!("ZIP 条目 {name} 本地头损坏"));
        }
        let local_name_len = read_u16(raw, local_offset + 26)? as usize;
        let local_extra_len = read_u16(raw, local_offset + 28)? as usize;
        let data_start = local_offset + 30 + local_name_len + local_extra_len;
        let payload = raw
            .get(data_start..data_start + compressed)
            .ok_or_else(|| format!("ZIP 条目 {name} 数据截断"))?;
        let data = match method {
            0 => payload.to_vec(),
            8 => {
                let mut decoder = DeflateDecoder::new(Cursor::new(payload));
                let mut buffer = Vec::with_capacity(uncompressed);
                decoder
                    .read_to_end(&mut buffer)
                    .map_err(|error| format!("ZIP 条目 {name} 解压失败: {error}"))?;
                buffer
            }
            other => return Err(format!("ZIP 条目 {name} 使用了不支持的方法 {other}")),
        };
        if data.len() != uncompressed {
            return Err(format!("ZIP 条目 {name} 解压长度不符"));
        }
        if crc32fast::hash(&data) != crc {
            return Err(format!("ZIP 条目 {name} CRC 校验失败"));
        }
        entries.push(Entry { name, data });
        cursor += 46 + name_len + extra_len + comment_len;
    }
    Ok(entries)
}

pub fn find<'a>(entries: &'a [Entry], name: &str) -> Option<&'a Entry> {
    entries.iter().find(|entry| entry.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_handles_compressible_and_random_payloads() {
        let random: Vec<u8> = (0..2048u32)
            .map(|value| (value.wrapping_mul(2654435761) >> 24) as u8)
            .collect();
        let entries = vec![
            Entry {
                name: "manifest.json".into(),
                data: br#"{"version":1}"#.to_vec(),
            },
            Entry {
                name: "settings/engine.json".into(),
                data: "{\"enabled\":false}".repeat(500).into_bytes(),
            },
            Entry {
                name: "files/blob.bin".into(),
                data: random,
            },
        ];
        let raw = build(&entries).unwrap();
        let decoded = read(&raw).unwrap();
        assert_eq!(decoded, entries);
        assert_eq!(
            find(&decoded, "manifest.json").unwrap().data,
            br#"{"version":1}"#
        );
    }

    #[test]
    fn a_tampered_entry_is_rejected_by_crc() {
        let mut raw = build(&[Entry {
            name: "manifest.json".into(),
            data: br#"{"version":1}"#.to_vec(),
        }])
        .unwrap();
        let position = raw
            .windows(4)
            .position(|window| window == b"{\"ve")
            .expect("manifest payload");
        raw[position + 2] ^= 0xff;
        assert!(read(&raw).is_err(), "篡改后的备份包必须校验失败");
    }

    #[test]
    fn path_traversal_entries_are_rejected() {
        let raw = build(&[Entry {
            name: "../evil.json".into(),
            data: b"{}".to_vec(),
        }])
        .unwrap();
        assert!(read(&raw).is_err());
    }
}
