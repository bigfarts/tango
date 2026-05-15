pub mod export;
mod protos;

use byteorder::ReadBytesExt;
use byteorder::WriteBytesExt;
use prost::Message;
use std::io::Write;

pub use protos::replay11::metadata;
pub type Metadata = protos::replay11::Metadata;

pub const HEADER: &[u8] = b"TOOT";
pub const VERSION: u8 = 0x18;

/// GBA joyflags only use bits 0..=9 (the ten standard buttons). The replay
/// repurposes the unused high bits of the first joyflag of each record as
/// per-round and end-of-replay framing:
/// - `ROUND_START_FLAG` (bit 15) on the first input of a round.
/// - `END_OF_REPLAY_FLAG` (bit 14) on a standalone u16 marks a clean shutdown.
/// Bit 14 + bit 15 never coexist on a real input — we strip both when
/// surfacing joyflags to the rest of the engine.
const ROUND_START_FLAG: u16 = 0x8000;
const END_OF_REPLAY_FLAG: u16 = 0x4000;
const HIGH_BITS_MASK: u16 = ROUND_START_FLAG | END_OF_REPLAY_FLAG;

pub struct Writer {
    writer: Box<dyn Write + Send>,
    /// True once a round is open. The next [`write_input`] tags its p1
    /// joyflag with [`ROUND_START_FLAG`] and then clears this.
    next_input_is_round_start: bool,
}

#[derive(Clone)]
pub struct Replay {
    pub is_complete: bool,
    pub metadata: Metadata,
    pub is_offerer: bool,
    pub local_player_index: u8,
    pub rng_seed: [u8; 16],
    pub local_sram: Vec<u8>,
    pub remote_sram: Vec<u8>,
    pub rounds: Vec<Vec<crate::input::Pair<crate::input::PartialInput, crate::input::PartialInput>>>,
}

pub fn decode_metadata(version: u8, raw: &[u8]) -> Result<Metadata, std::io::Error> {
    Ok(match version {
        VERSION => protos::replay11::Metadata::decode(raw)?,
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid version: {:02x}", version),
            ));
        }
    })
}

pub fn read_metadata(r: &mut impl std::io::Read) -> Result<Metadata, std::io::Error> {
    let mut header = [0u8; 4];
    r.read_exact(&mut header)?;
    if header != HEADER {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid header"));
    }

    let version = r.read_u8()?;
    let metadata_len = r.read_u32::<byteorder::LittleEndian>()?;
    let mut raw = vec![0u8; metadata_len as usize];
    r.read_exact(&mut raw[..])?;
    decode_metadata(version, &raw)
}

fn read_compressed_blob(r: &mut impl std::io::Read) -> std::io::Result<Vec<u8>> {
    let len = r.read_u32::<byteorder::LittleEndian>()? as usize;
    let mut compressed = vec![0u8; len];
    r.read_exact(&mut compressed)?;
    zstd::stream::decode_all(&compressed[..])
}

fn write_compressed_blob(w: &mut impl Write, data: &[u8]) -> std::io::Result<()> {
    let compressed = zstd::stream::encode_all(data, 3)?;
    w.write_u32::<byteorder::LittleEndian>(compressed.len() as u32)?;
    w.write_all(&compressed)
}

impl Replay {
    pub fn into_remote(mut self) -> Self {
        std::mem::swap(&mut self.metadata.local_side, &mut self.metadata.remote_side);
        self.local_player_index = 1 - self.local_player_index;
        self.is_offerer = !self.is_offerer;
        std::mem::swap(&mut self.local_sram, &mut self.remote_sram);
        for round in self.rounds.iter_mut() {
            for ip in round.iter_mut() {
                std::mem::swap(&mut ip.local, &mut ip.remote);
            }
        }
        self
    }

    pub fn total_input_pairs(&self) -> usize {
        self.rounds.iter().map(|r| r.len()).sum()
    }

    pub fn decode(mut r: impl std::io::Read) -> std::io::Result<Self> {
        let metadata = read_metadata(&mut r)?;

        let is_offerer = r.read_u8()? != 0;
        let local_player_index = r.read_u8()?;

        let mut rng_seed = [0u8; 16];
        r.read_exact(&mut rng_seed)?;

        let local_sram = read_compressed_blob(&mut r)?;
        let remote_sram = read_compressed_blob(&mut r)?;

        // Streaming round decode: each record is `(p1_jf u16, p2_jf u16)`.
        // The first record of each round has `ROUND_START_FLAG` set in
        // p1_jf; subsequent records are bare. Reading a u16 with
        // `END_OF_REPLAY_FLAG` set (in p1's slot) marks a clean shutdown.
        // Any unexpected EOF mid-record drops the in-progress round and
        // leaves is_complete=false.
        let mut rounds: Vec<Vec<crate::input::Pair<crate::input::PartialInput, crate::input::PartialInput>>> =
            Vec::new();
        let mut current_round: Vec<crate::input::Pair<crate::input::PartialInput, crate::input::PartialInput>> =
            Vec::new();
        let mut is_complete = false;

        loop {
            let p1_raw = match r.read_u16::<byteorder::LittleEndian>() {
                Ok(v) => v,
                Err(_) => break,
            };
            if p1_raw & END_OF_REPLAY_FLAG != 0 {
                is_complete = true;
                break;
            }
            let p2_raw = match r.read_u16::<byteorder::LittleEndian>() {
                Ok(v) => v,
                Err(_) => break,
            };

            if p1_raw & ROUND_START_FLAG != 0 && !current_round.is_empty() {
                rounds.push(std::mem::take(&mut current_round));
            }

            let p1_jf = p1_raw & !HIGH_BITS_MASK;
            let p2_jf = p2_raw & !HIGH_BITS_MASK;
            let p1_input = crate::input::PartialInput { joyflags: p1_jf };
            let p2_input = crate::input::PartialInput { joyflags: p2_jf };
            let (local, remote) = if local_player_index == 0 {
                (p1_input, p2_input)
            } else {
                (p2_input, p1_input)
            };
            current_round.push(crate::input::Pair { local, remote });
        }

        if !current_round.is_empty() {
            rounds.push(current_round);
        }

        Ok(Self {
            is_complete,
            metadata,
            is_offerer,
            local_player_index,
            rng_seed,
            local_sram,
            remote_sram,
            rounds,
        })
    }
}

impl Writer {
    pub fn new(
        mut writer: impl Write + Send + 'static,
        metadata: Metadata,
        is_offerer: bool,
        local_player_index: u8,
        rng_seed: [u8; 16],
        local_sram: &[u8],
        remote_sram: &[u8],
    ) -> std::io::Result<Self> {
        writer.write_all(HEADER)?;
        writer.write_u8(VERSION)?;
        let raw_metadata = metadata.encode_to_vec();
        writer.write_u32::<byteorder::LittleEndian>(raw_metadata.len() as u32)?;
        writer.write_all(&raw_metadata[..])?;

        let mut writer = Box::new(writer) as Box<dyn Write + Send>;
        writer.write_u8(if is_offerer { 1 } else { 0 })?;
        writer.write_u8(local_player_index)?;
        writer.write_all(&rng_seed)?;
        write_compressed_blob(&mut writer, local_sram)?;
        write_compressed_blob(&mut writer, remote_sram)?;
        writer.flush()?;
        Ok(Writer {
            writer,
            next_input_is_round_start: false,
        })
    }

    pub fn start_round(&mut self) -> std::io::Result<()> {
        // The marker is stamped on the next [`write_input`]'s p1_jf; we
        // don't emit anything here, so a crash mid-round just leaves the
        // partial inputs on disk and the recovery path will treat the next
        // ROUND_START as starting a fresh round.
        self.next_input_is_round_start = true;
        Ok(())
    }

    pub fn write_input(
        &mut self,
        local_player_index: u8,
        ip: &crate::input::Pair<crate::input::PartialInput, crate::input::PartialInput>,
    ) -> std::io::Result<()> {
        let (p1, p2) = if local_player_index == 0 {
            (&ip.local, &ip.remote)
        } else {
            (&ip.remote, &ip.local)
        };

        let mut p1_jf = p1.joyflags;
        if self.next_input_is_round_start {
            p1_jf |= ROUND_START_FLAG;
            self.next_input_is_round_start = false;
        }
        self.writer.write_u16::<byteorder::LittleEndian>(p1_jf)?;
        self.writer.write_u16::<byteorder::LittleEndian>(p2.joyflags)?;
        Ok(())
    }

    pub fn finish(mut self) -> std::io::Result<()> {
        self.writer
            .write_u16::<byteorder::LittleEndian>(END_OF_REPLAY_FLAG)?;
        self.writer.flush()?;
        Ok(())
    }
}
