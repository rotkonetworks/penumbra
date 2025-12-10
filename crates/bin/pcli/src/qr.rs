//! QR code support for airgap transactions
//!
//! Displays QR codes in the terminal for air-gapped transaction signing.
//! For large payloads, cycles through multiple frames.

use anyhow::{Context, Result};
use qrcode::{render::unicode, QrCode};
use std::io::{self, Write};

/// Maximum bytes per QR code frame
/// We use a conservative limit for reliable scanning
const MAX_BYTES_PER_FRAME: usize = 1000;

/// Display a QR code in the terminal using Unicode block characters
pub fn display_qr_terminal(data: &[u8]) -> Result<()> {
    if data.len() <= MAX_BYTES_PER_FRAME {
        display_single_qr(data, None)
    } else {
        display_animated_qr(data)
    }
}

/// Display a single QR code frame
fn display_single_qr(data: &[u8], frame_info: Option<(usize, usize)>) -> Result<()> {
    let code = QrCode::new(data).context("failed to generate QR code")?;

    let image = code
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Light)
        .light_color(unicode::Dense1x2::Dark)
        .quiet_zone(true)
        .build();

    // Clear screen and move cursor to top
    print!("\x1B[2J\x1B[1;1H");

    if let Some((current, total)) = frame_info {
        println!("frame {}/{}", current + 1, total);
    }

    println!("{}", image);
    io::stdout().flush()?;

    Ok(())
}

/// Display animated QR code sequence for large data
fn display_animated_qr(data: &[u8]) -> Result<()> {
    let frames = create_frames(data);
    let total = frames.len();

    println!("large payload: {} frames", total);
    println!("press Enter to cycle, 'q' to quit");
    println!();

    let mut current = 0;
    loop {
        display_single_qr(&frames[current], Some((current, total)))?;

        println!();
        println!("[Enter] next  [r] restart  [q] quit");

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;

        match input.trim() {
            "q" | "Q" => break,
            "r" | "R" => current = 0,
            _ => current = (current + 1) % total,
        }
    }

    Ok(())
}

/// Create frames from data with header
/// Format: [frame_num:u16 LE][total_frames:u16 LE][data...]
fn create_frames(data: &[u8]) -> Vec<Vec<u8>> {
    let chunk_size = MAX_BYTES_PER_FRAME - 4; // Reserve 4 bytes for header
    let chunks: Vec<&[u8]> = data.chunks(chunk_size).collect();
    let total = chunks.len() as u16;

    chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let mut frame = Vec::with_capacity(chunk.len() + 4);
            frame.extend_from_slice(&(i as u16).to_le_bytes());
            frame.extend_from_slice(&total.to_le_bytes());
            frame.extend_from_slice(chunk);
            frame
        })
        .collect()
}

/// Parse frame header
/// Returns (frame_number, total_frames, payload)
pub fn parse_frame_header(data: &[u8]) -> Result<(u16, u16, &[u8])> {
    if data.len() < 4 {
        anyhow::bail!("frame too short");
    }

    let frame_num = u16::from_le_bytes([data[0], data[1]]);
    let total = u16::from_le_bytes([data[2], data[3]]);
    let payload = &data[4..];

    Ok((frame_num, total, payload))
}

/// Reassemble data from frames
pub fn reassemble_frames(frames: &[Vec<u8>]) -> Result<Vec<u8>> {
    if frames.is_empty() {
        anyhow::bail!("no frames");
    }

    let (_, total, _) = parse_frame_header(&frames[0])?;

    if frames.len() != total as usize {
        anyhow::bail!("missing frames: got {}, expected {}", frames.len(), total);
    }

    let mut sorted: Vec<(u16, Vec<u8>)> = Vec::with_capacity(frames.len());
    for f in frames {
        let (num, _, payload) = parse_frame_header(f)?;
        sorted.push((num, payload.to_vec()));
    }

    sorted.sort_by_key(|(num, _)| *num);

    let mut result = Vec::new();
    for (_, payload) in sorted {
        result.extend_from_slice(&payload);
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_small_data() {
        let data = b"hello world";
        assert!(display_qr_terminal(data).is_ok());
    }

    #[test]
    fn test_frame_roundtrip() {
        let data: Vec<u8> = (0..2500).map(|i| (i % 256) as u8).collect();
        let frames = create_frames(&data);

        assert!(frames.len() > 1);

        let reassembled = reassemble_frames(&frames).unwrap();
        assert_eq!(data, reassembled);
    }
}
