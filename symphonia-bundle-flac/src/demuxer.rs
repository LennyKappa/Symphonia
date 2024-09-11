// Symphonia
// Copyright (c) 2019-2022 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{Seek, SeekFrom};

use symphonia_core::support_format;

use symphonia_core::codecs::audio::{
    well_known::CODEC_ID_FLAC, AudioCodecParameters, VerificationCheck,
};
use symphonia_core::codecs::CodecParameters;
use symphonia_core::errors::{
    decode_error, seek_error, unsupported_error, Error, Result, SeekErrorKind,
};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::util::{SeekIndex, SeekSearchResult};
use symphonia_core::formats::well_known::FORMAT_ID_FLAC;
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataBuilder, MetadataLog};

use symphonia_utils_xiph::flac::metadata::*;

use log::{debug, info};

use super::parser::PacketParser;

/// The FLAC start of stream marker: "fLaC" in ASCII.
const FLAC_STREAM_MARKER: [u8; 4] = *b"fLaC";

const FLAC_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FORMAT_ID_FLAC,
    short_name: "flac",
    long_name: "Free Lossless Audio Codec Native",
};

/// Free Lossless Audio Codec (FLAC) native frame reader.
pub struct FlacReader<'s> {
    reader: MediaSourceStream<'s>,
    metadata: MetadataLog,
    tracks: Vec<Track>,
    cues: Vec<Cue>,
    index: Option<SeekIndex>,
    first_frame_offset: u64,
    parser: PacketParser,
}

impl<'s> FlacReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        // Read the first 4 bytes of the stream. Ideally this will be the FLAC stream marker.
        let marker = mss.read_quad_bytes()?;

        if marker != FLAC_STREAM_MARKER {
            return unsupported_error("flac: missing flac stream marker");
        }

        // Strictly speaking, the first metadata block must be a StreamInfo block. There is
        // no technical need for this from the reader's point of view. Additionally, if the
        // reader is fed a stream mid-way there is no StreamInfo block. Therefore, just read
        // all metadata blocks and handle the StreamInfo block as it comes.
        let flac = FlacReader::init_with_metadata(mss, opts)?;

        // Make sure that there is atleast one StreamInfo block.
        if flac.tracks.is_empty() {
            return decode_error("flac: no stream info block");
        }

        Ok(flac)
    }

    /// Reads all the metadata blocks, returning a fully populated `FlacReader`.
    fn init_with_metadata(mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        let mut metadata_builder = MetadataBuilder::new();

        let mut reader = mss;
        let mut tracks = Vec::new();
        let mut cues = Vec::new();
        let mut index = None;
        let mut parser = Default::default();

        loop {
            let header = MetadataBlockHeader::read(&mut reader)?;

            // Create a scoped bytestream to error if the metadata block read functions exceed the
            // stated length of the block.
            let mut block_stream = ScopedStream::new(&mut reader, u64::from(header.block_len));

            match header.block_type {
                MetadataBlockType::Application => {
                    // TODO: Store vendor data.
                    read_application_block(&mut block_stream, header.block_len)?;
                }
                // SeekTable blocks are parsed into a SeekIndex.
                MetadataBlockType::SeekTable => {
                    // Check if a SeekTable has already be parsed. If one has, then the file is
                    // invalid, atleast for seeking. Either way, it's a violation of the
                    // specification.
                    if index.is_none() {
                        let mut new_index = SeekIndex::new();
                        read_seek_table_block(&mut block_stream, header.block_len, &mut new_index)?;
                        index = Some(new_index);
                    }
                    else {
                        return decode_error("flac: found more than one seek table block");
                    }
                }
                // VorbisComment blocks are parsed into Tags.
                MetadataBlockType::VorbisComment => {
                    read_comment_block(&mut block_stream, &mut metadata_builder)?;
                }
                // Cuesheet blocks are parsed into Cues.
                MetadataBlockType::Cuesheet => {
                    read_cuesheet_block(&mut block_stream, &mut cues)?;
                }
                // Picture blocks are read as Visuals.
                MetadataBlockType::Picture => {
                    read_picture_block(&mut block_stream, &mut metadata_builder)?;
                }
                // StreamInfo blocks are parsed into Streams.
                MetadataBlockType::StreamInfo => {
                    read_stream_info_block(&mut block_stream, &mut tracks, &mut parser)?;
                }
                // Padding blocks are skipped.
                MetadataBlockType::Padding => {
                    block_stream.ignore_bytes(u64::from(header.block_len))?;
                }
                // Unknown block encountered. Skip these blocks as they may be part of a future
                // version of FLAC, but  print a message.
                MetadataBlockType::Unknown(id) => {
                    block_stream.ignore_bytes(u64::from(header.block_len))?;
                    info!("ignoring {} bytes of block width id={}.", header.block_len, id);
                }
            }

            // If the stated block length is longer than the number of bytes from the block read,
            // ignore the remaining unread data.
            let block_unread_len = block_stream.bytes_available();

            if block_unread_len > 0 {
                info!("under read block by {} bytes.", block_unread_len);
                block_stream.ignore_bytes(block_unread_len)?;
            }

            // Exit when the last header is read.
            if header.is_last {
                break;
            }
        }

        // Commit any read metadata to the metadata log.
        let mut metadata = opts.metadata.unwrap_or_default();
        metadata.push(metadata_builder.metadata());

        // Synchronize the packet parser to the first audio frame.
        let _ = parser.resync(&mut reader)?;

        // The first frame offset is the byte offset from the beginning of the stream after all the
        // metadata blocks have been read.
        let first_frame_offset = reader.pos();

        Ok(FlacReader { reader, metadata, tracks, cues, index, first_frame_offset, parser })
    }
}

impl Scoreable for FlacReader<'_> {
    fn score(_src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        Ok(Score::Supported(255))
    }
}

impl ProbeableFormat<'_> for FlacReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(FlacReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(FLAC_FORMAT_INFO, &["flac"], &["audio/flac"], &[b"fLaC"])]
    }
}

impl FormatReader for FlacReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &FLAC_FORMAT_INFO
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        self.parser.parse(&mut self.reader)
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn cues(&self) -> &[Cue] {
        &self.cues
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        if self.tracks.is_empty() {
            return seek_error(SeekErrorKind::Unseekable);
        }

        let track = &self.tracks[0];

        // Get the timestamp of the desired audio frame.
        let ts = match to {
            // Frame timestamp given.
            SeekTo::TimeStamp { ts, .. } => ts,
            // Time value given, calculate frame timestamp using the track's timebase.
            SeekTo::Time { time, .. } => {
                // Use the timebase to calculate the frame timestamp. If timebase is not known, the
                // seek cannot be completed.
                if let Some(tb) = track.time_base {
                    tb.calc_timestamp(time)
                }
                else {
                    return seek_error(SeekErrorKind::Unseekable);
                }
            }
        };

        debug!("seeking to frame_ts={}", ts);

        // If the total number of frames in the stream is known, verify the desired frame timestamp
        // does not exceed it.
        if let Some(num_frames) = track.num_frames {
            if ts > num_frames {
                return seek_error(SeekErrorKind::OutOfRange);
            }
        }

        // If the reader supports seeking, coarsely seek to the nearest packet with a timestamp
        // lower than the desired timestamp using a binary search.
        if self.reader.is_seekable() {
            // The range formed by start_byte_offset..end_byte_offset defines an area where the
            // binary search for the packet containing the desired timestamp will be performed. The
            // lower bound is set to the byte offset of the first frame, while the upper bound is
            // set to the length of the stream.
            let mut start_byte_offset = self.first_frame_offset;
            let mut end_byte_offset = self.reader.seek(SeekFrom::End(0))?;

            // If there is an index, use it to refine the binary search range.
            if let Some(ref index) = self.index {
                // Search the index for the timestamp. Adjust the search based on the result.
                match index.search(ts) {
                    // Search from the start of stream up-to an ending point.
                    SeekSearchResult::Upper(upper) => {
                        end_byte_offset = self.first_frame_offset + upper.byte_offset;
                    }
                    // Search from a starting point up-to the end of the stream.
                    SeekSearchResult::Lower(lower) => {
                        start_byte_offset = self.first_frame_offset + lower.byte_offset;
                    }
                    // Search between two points of the stream.
                    SeekSearchResult::Range(lower, upper) => {
                        start_byte_offset = self.first_frame_offset + lower.byte_offset;
                        end_byte_offset = self.first_frame_offset + upper.byte_offset;
                    }
                    // Search the entire stream (default behaviour, so do nothing).
                    SeekSearchResult::Stream => (),
                }
            }

            // Binary search the range of bytes formed by start_by_offset..end_byte_offset for the
            // desired frame timestamp. When the difference of the range reaches 2x the maximum
            // frame size, exit the loop and search from the start_byte_offset linearly. The binary
            // search becomes inefficient when the range is small.
            while end_byte_offset - start_byte_offset > 2 * 8096 {
                let mid_byte_offset = (start_byte_offset + end_byte_offset) / 2;
                self.reader.seek(SeekFrom::Start(mid_byte_offset))?;

                let sync = self.parser.resync(&mut self.reader)?;

                if ts < sync.ts {
                    end_byte_offset = mid_byte_offset;
                }
                else if ts >= sync.ts && ts < sync.ts + sync.dur {
                    debug!("seeked to ts={} (delta={})", sync.ts, sync.ts as i64 - ts as i64);

                    return Ok(SeekedTo { track_id: 0, actual_ts: sync.ts, required_ts: ts });
                }
                else {
                    start_byte_offset = mid_byte_offset;
                }
            }

            // The binary search did not find an exact frame, but the range has been narrowed. Seek
            // to the start of the range, and continue with a linear search.
            self.reader.seek(SeekFrom::Start(start_byte_offset))?;
        }

        // Linearly search the stream packet-by-packet for the packet that contains the desired
        // timestamp. This search is used to find the exact packet containing the desired timestamp
        // after the search range was narrowed by the binary search. It is also the ONLY way for a
        // unseekable stream to be "seeked" forward.
        let packet = loop {
            // Resync the FLAC frame parser.
            //
            // If the duration of the track is not known, then it is not possible to know if the
            // source has been truncated or not when an UnexpectedEof error occurs. Therefore, in
            // this case, return an out-of-range error. However, if the duration is known, then the
            // only reason why an UnexpectedEof would occur is if the source has been truncated
            // since the required timestamp was checked to be shorter than the duration earlier. In
            // this case the UnexpectedEof error should be passed-on to the caller.
            let sync = match self.parser.resync(&mut self.reader) {
                Ok(sync) => sync,
                Err(Error::IoError(err))
                    if err.kind() == std::io::ErrorKind::UnexpectedEof
                        && track.num_frames.is_none() =>
                {
                    return seek_error(SeekErrorKind::OutOfRange);
                }
                Err(err) => return Err(err),
            };

            // The desired timestamp precedes the current packet's timestamp.
            if ts < sync.ts {
                // Attempted to seek backwards on an unseekable stream.
                if !self.reader.is_seekable() {
                    return seek_error(SeekErrorKind::ForwardOnly);
                }
                // Overshot a regular seek, or the stream is corrupted, not necessarily an error
                // per-say.
                else {
                    break sync;
                }
            }
            // The desired timestamp is contained within the current packet.
            else if ts >= sync.ts && ts < sync.ts + sync.dur {
                break sync;
            }

            // Advance the reader such that the next iteration will sync to a different frame.
            self.reader.read_byte()?;
        };

        debug!("seeked to packet_ts={} (delta={})", packet.ts, packet.ts as i64 - ts as i64);

        Ok(SeekedTo { track_id: 0, actual_ts: packet.ts, required_ts: ts })
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.reader
    }

    fn stream_mut(&mut self, func: Box<dyn FnOnce(&mut MediaSourceStream<'_>) + '_>) -> Result<()> {
        let seek_pos = self.reader.stream_position()?;
        func(&mut self.reader);
        self.reader.seek(std::io::SeekFrom::Start(seek_pos))?;
        Ok(())
    }

    fn stream(&self, func: Box<dyn FnOnce(&MediaSourceStream<'_>) + '_>) -> Result<()> {
        func(&self.reader);
        Ok(())
    }
}

/// Reads a StreamInfo block and populates the reader with stream information.
fn read_stream_info_block<B: ReadBytes + FiniteStream>(
    reader: &mut B,
    tracks: &mut Vec<Track>,
    parser: &mut PacketParser,
) -> Result<()> {
    // Only one StreamInfo block, and therefore only one Track, is allowed per media source stream.
    if tracks.is_empty() {
        // Ensure the block length is correct for a stream information block before allocating a
        // buffer for it.
        if !StreamInfo::is_valid_size(reader.byte_len()) {
            return decode_error("flac: invalid stream info block size");
        }

        // Read the stream information block as a boxed slice so that it may be attached as extra
        // data on the codec parameters.
        let extra_data = reader.read_boxed_slice_exact(reader.byte_len() as usize)?;

        // Parse the stream info block.
        let info = StreamInfo::read(&mut BufReader::new(&extra_data))?;

        // Populate the codec parameters with the basic audio parameters of the track.
        let mut codec_params = AudioCodecParameters::new();

        codec_params
            .for_codec(CODEC_ID_FLAC)
            .with_extra_data(extra_data)
            .with_sample_rate(info.sample_rate)
            .with_bits_per_sample(info.bits_per_sample)
            .with_channels(info.channels.clone());

        if let Some(md5) = info.md5 {
            codec_params.with_verification_code(VerificationCheck::Md5(md5));
        }

        // Populate the track.
        let mut track = Track::new(0);

        track.with_codec_params(CodecParameters::Audio(codec_params));

        // Total samples per channel (also the total number of frames) is optional.
        if let Some(num_frames) = info.n_samples {
            track.with_num_frames(num_frames);
        }

        // Reset the packet parser.
        parser.reset(info);

        // Add the track.
        tracks.push(track);
    }
    else {
        return decode_error("flac: found more than one stream info block");
    }

    Ok(())
}
