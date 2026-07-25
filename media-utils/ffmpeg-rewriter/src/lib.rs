// Copyright (C) The Strand-Braid Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Save an MP4 by piping frames through ffmpeg, then rewrite the result with
//! precise per-frame timestamps and Strand Camera metadata.
//!
//! # The two stages
//!
//! [`FfmpegReWriter`] records in two stages, because ffmpeg cannot embed our
//! per-frame capture times as it encodes:
//!
//! 1. **Capture.** Frames are piped to an ffmpeg process which encodes them into
//!    `<name>.mp4`, while each frame's capture time is appended to a temporary
//!    SubRip sidecar file, `<name>-ffmpeg-rewriter.srt`. Any [`H264Metadata`] is
//!    saved to `<name>-metadata.json` up front so it, too, survives a crash.
//! 2. **Rewrite.** [`FfmpegReWriter::close`] re-muxes that MP4 (no transcoding)
//!    into `<name>.mp4-rewritten.mp4`, now with each frame's capture time
//!    embedded as a MISB ST 0604 precision-timestamp SEI NAL unit and with the
//!    H264 metadata included. That file is renamed over `<name>.mp4`, and the
//!    two sidecar files are deleted.
//!
//! # Crash leftovers, and repairing them
//!
//! A crash, power loss or `kill -9` between those steps leaves the sidecar files
//! behind, and -- if it landed inside stage 2 -- a partial
//! `<name>.mp4-rewritten.mp4` as well. Such a recording still holds all the
//! video ffmpeg managed to write, but its timestamps live only in the `.srt`
//! sidecar instead of inside the MP4 where every downstream tool looks for them.
//!
//! [`inspect`] classifies such a leftover recording and [`repair`] finishes it,
//! from whichever stage the crash interrupted. [`find_interrupted`] locates them
//! by their sidecar files. The `ffmpeg-rewriter-doctor` command-line tool is a
//! thin wrapper over these three.
//!
//! Repair cannot invent the timestamps that were never written: the sidecar is
//! written one frame behind (a frame's stanza needs the *next* frame's time to
//! close it), so the frames of the group of pictures in progress when the crash
//! happened are dropped. See [`RewriteOutcome::truncated_reason`].

use chrono::{DateTime, Local};
use frame_source::{FrameDataSource, h264_source::SeekableH264Source};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use ffmpeg_writer::{FfmpegCodecArgs, FfmpegWriter};
use strand_cam_remote_control::{H264Metadata, Mp4Codec, Mp4RecordingConfig, RecordingFrameRate};

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("ffmpeg_writer {0}")]
    FfmpegWriter(#[from] ffmpeg_writer::Error),
    #[error("cannot reencode")]
    CannotReencode,
    #[error("filename does not end with '.mp4'")]
    FilenameDoesNotEndWithMp4,
    #[error("filename not unicode")]
    FilenameNotUnicode,
    #[error("source does not contain H264 video")]
    SourceIsNotH264,
    #[error("MP4 writer error: {0}")]
    Mp4WriterError(#[from] mp4_writer::Error),
    #[error("frame source error: {0}")]
    FrameSourceError(#[from] frame_source::Error),
    #[error("serde json error: {0}")]
    SerdeJsonError(#[from] serde_json::Error),
    #[error("glob pattern error: {0}")]
    GlobPatternError(#[from] glob::PatternError),
    #[error("glob error: {0}")]
    GlobError(#[from] glob::GlobError),
    #[error(
        "the rewrite of \"{mp4_filename}\" could cover only {frames_written} of {total_frames} \
        frames: {reason}"
    )]
    IncompleteRewrite {
        mp4_filename: String,
        frames_written: usize,
        total_frames: usize,
        reason: String,
    },
    #[error("\"{mp4_filename}\": no frame could be rewritten: {reason}")]
    NoFramesRewritten {
        mp4_filename: String,
        reason: String,
    },
    #[error(
        "\"{mp4_filename}\" was never finalized (it has no `moov` box), so its frames cannot be \
        read. Either ffmpeg is still writing it, or ffmpeg itself was killed before it could \
        write the index; in the latter case a tool such as `untrunc` may be able to rebuild it."
    )]
    Mp4NotFinalized { mp4_filename: String },
    #[error(
        "\"{mp4_filename}\" is missing or empty, but the leftover \"{rewritten}\" of an \
        interrupted rewrite is present. Inspect \"{rewritten}\" by hand: if it is complete, \
        rename it to \"{mp4_filename}\"."
    )]
    OriginalMissing {
        mp4_filename: String,
        rewritten: String,
    },
    #[error(
        "\"{mp4_filename}\" still needs its timestamps embedded, but the timestamp sidecar \
        \"{srt}\" is gone, so they are lost."
    )]
    MissingSrtSidecar { mp4_filename: String, srt: String },
    #[error(
        "\"{mp4_filename}\": no such recording, and none of the files an interrupted one leaves \
        behind are there either."
    )]
    NoSuchRecording { mp4_filename: String },
}
type Result<T> = std::result::Result<T, Error>;

/// Suffix of the temporary SubRip file holding capture times during recording.
///
/// Appended to the recording's name with the `.mp4` extension removed.
pub const SRT_SIDECAR_SUFFIX: &str = "-ffmpeg-rewriter.srt";

/// Suffix of the file holding the [`H264Metadata`] during recording, so it
/// survives a crash. Appended with the `.mp4` extension removed.
pub const METADATA_SIDECAR_SUFFIX: &str = "-metadata.json";

/// Suffix of the file the rewrite stage writes before renaming it over the
/// recording. Appended to the *full* filename, so `x.mp4` becomes
/// `x.mp4-rewritten.mp4`.
pub const REWRITTEN_SUFFIX: &str = "-rewritten.mp4";

/// The files that make up one recording: the MP4 itself, the two sidecar files
/// used while recording, and the temporary output of the rewrite stage.
#[derive(Debug, Clone)]
pub struct Sidecars {
    /// The recording itself, `<name>.mp4`.
    pub mp4: String,
    /// Capture times written during recording, deleted once they have been
    /// embedded into the MP4.
    pub srt: String,
    /// [`H264Metadata`] saved during recording, deleted along with `srt`.
    pub metadata_json: String,
    /// Where the rewrite stage writes its output before renaming it over `mp4`.
    pub rewritten: String,
}

impl Sidecars {
    /// The file names used for the recording `mp4_path` (which must end in
    /// `.mp4`).
    pub fn for_mp4<P: AsRef<Path>>(mp4_path: P) -> Result<Self> {
        let mp4 = PathBuf::from(mp4_path.as_ref())
            .into_os_string()
            .into_string()
            .map_err(|_| Error::FilenameNotUnicode)?;
        let Some(basename) = mp4.strip_suffix(".mp4") else {
            return Err(Error::FilenameDoesNotEndWithMp4);
        };
        Ok(Self {
            srt: format!("{basename}{SRT_SIDECAR_SUFFIX}"),
            metadata_json: format!("{basename}{METADATA_SIDECAR_SUFFIX}"),
            rewritten: format!("{mp4}{REWRITTEN_SUFFIX}"),
            mp4,
        })
    }

    /// Delete the sidecar files (and any leftover rewrite output), ignoring
    /// those that are already gone.
    fn remove(&self) -> Result<()> {
        for path in [&self.srt, &self.metadata_json, &self.rewritten] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct SrtMsg {
    timestamp: DateTime<chrono::Local>,
}

/// Save to a video using [FfmpegWriter] but when done, read the newly-written
/// file and resave the data (without transcoding) with timestamps and other
/// metadata.
///
/// See the [crate-level docs](crate) for the two stages this involves, the
/// sidecar files they use, and how to repair a recording interrupted partway
/// through.
pub struct FfmpegReWriter {
    sidecars: Sidecars,
    ffmpeg_wtr: FfmpegWriter,
    mp4_cfg: Mp4RecordingConfig,
    swtr: srt_writer::BufferingSrtFrameWriter,
    /// Whether the [`H264Metadata`] was saved to
    /// [`Sidecars::metadata_json`] (only done when there is any).
    wrote_metadata_json: bool,
}

impl FfmpegReWriter {
    pub fn new(
        mp4_path: impl AsRef<std::path::Path>,
        ffmpeg_codec_args: FfmpegCodecArgs,
        rate: Option<(usize, usize)>,
        h264_metadata: Option<H264Metadata>,
    ) -> Result<Self> {
        tracing::debug!(
            "Creating FfmpegReWriter for {} with h264_metadata: {h264_metadata:?}",
            mp4_path.as_ref().display()
        );
        // Note the SRT sidecar name ([`SRT_SIDECAR_SUFFIX`]) makes a conflict
        // unlikely if the user also writes an SRT file. They are likely to use
        // "{basename}.srt" as this will then play in VLC and likely other
        // players. As our SRT file is only temporary, it doesn't matter much what
        // exactly it is called, but it shouldn't have a high likelihood of
        // conflict.
        let sidecars = Sidecars::for_mp4(mp4_path.as_ref())?;

        let wrote_metadata_json = if let Some(h264_metadata) = &h264_metadata {
            // Save the metadata to a file in case we crash before
            // Self::close(). That way this information can be recovered (by
            // `repair`).
            let buf = serde_json::to_string(h264_metadata)?;
            std::fs::write(&sidecars.metadata_json, buf)?;
            true
        } else {
            false
        };

        let ffmpeg_wtr = FfmpegWriter::new(&sidecars.mp4, ffmpeg_codec_args, rate)?;
        let mp4_cfg = mp4_cfg(h264_metadata);

        let out_fd = std::fs::File::create(&sidecars.srt)?;
        let swtr = srt_writer::BufferingSrtFrameWriter::new(Box::new(out_fd));
        Ok(Self {
            sidecars,
            ffmpeg_wtr,
            mp4_cfg,
            swtr,
            wrote_metadata_json,
        })
    }

    /// Write a frame and timestamp.
    pub fn write_dynamic_frame<TS>(
        &mut self,
        frame: &strand_dynamic_frame::DynamicFrame,
        timestamp: TS,
    ) -> Result<()>
    where
        TS: Into<DateTime<Local>>,
    {
        let timestamp = timestamp.into();

        let mp4_pts = self
            .ffmpeg_wtr
            .write_dynamic_frame(frame)
            .map_err(Error::FfmpegWriter)?;

        let msg = SrtMsg { timestamp };
        let msg = serde_json::to_string(&msg).unwrap();
        self.swtr.add_frame(mp4_pts, msg)?;
        self.swtr.flush()?;

        Ok(())
    }

    pub fn close(self) -> Result<()> {
        // finish with ffmpeg and finish writing SRT
        self.ffmpeg_wtr.close()?;
        self.swtr.close()?;
        tracing::debug!("Done creating original .mp4 and .srt files.");

        let outcome = write_rewritten(&self.sidecars, self.mp4_cfg)?;

        // Having just written the SRT sidecar ourselves, it covers every frame,
        // so a truncated rewrite means something is wrong. Fail without
        // committing: the original MP4 and its sidecars are left in place, so
        // the recording stays repairable (see [`repair`]).
        if let Some(reason) = outcome.truncated_reason {
            return Err(Error::IncompleteRewrite {
                mp4_filename: self.sidecars.mp4,
                frames_written: outcome.frames_written,
                total_frames: outcome.total_frames,
                reason,
            });
        }

        commit_rewritten(&self.sidecars, self.wrote_metadata_json)?;

        Ok(())
    }
}

fn mp4_cfg(h264_metadata: Option<H264Metadata>) -> Mp4RecordingConfig {
    Mp4RecordingConfig {
        codec: Mp4Codec::H264RawStream,
        max_framerate: RecordingFrameRate::Unlimited,
        h264_metadata,
    }
}

/// Result of the rewrite stage (see the [crate-level docs](crate)).
#[derive(Debug, Clone)]
pub struct RewriteOutcome {
    /// Number of frames written to the rewritten MP4.
    pub frames_written: usize,
    /// Number of frames the source MP4 contains.
    pub total_frames: usize,
    /// Set when `frames_written < total_frames`, explaining why. After a crash
    /// this is expected: the SRT sidecar is written one frame behind, so the
    /// frames of the group of pictures in progress at the time of the crash have
    /// no capture time and cannot be kept.
    pub truncated_reason: Option<String>,
}

/// Run the rewrite stage: re-mux `sidecars.mp4` (without transcoding) into
/// `sidecars.rewritten`, embedding the capture times from `sidecars.srt` as
/// precision-timestamp SEI NAL units and `mp4_cfg`'s metadata.
///
/// Nothing is moved or deleted here; [`commit_rewritten`] does that once the
/// caller has decided the result is acceptable.
///
/// If the capture times run out partway through -- which is the normal situation
/// after a crash -- as many whole groups of pictures as are covered are written
/// and the reason is reported in the returned [`RewriteOutcome`] rather than as
/// an error. Only failing before any frame could be written is an error, in
/// which case no output file is left behind.
fn write_rewritten(sidecars: &Sidecars, mp4_cfg: Mp4RecordingConfig) -> Result<RewriteOutcome> {
    // Create reader for h264 data from .mp4 and timestamps from .srt.
    let mut frame_src = frame_source::FrameSourceBuilder::new(&sidecars.mp4)
        .do_decode_h264(false)
        .timestamp_source(frame_source::TimestampSource::SrtFile)
        .srt_file_path(Some(PathBuf::from(&sidecars.srt)))
        .build_h264_in_mp4_source()?;

    let frame0_time = frame_src.frame0_time().unwrap();

    // The source is truncated to whole groups of pictures the SRT has capture
    // times for, so this is known up front rather than discovered mid-loop.
    let srt_truncation_reason = frame_src
        .srt_truncation()
        .map(|t| match t.malformed_at_line {
            Some(line) => format!(
                "the timestamp sidecar \"{}\" is incomplete from line {line} on (an \
                 interrupted write); {} of {} frames have a capture time, kept {} (whole \
                 groups of pictures only)",
                sidecars.srt, t.usable_stanzas, t.total_frames, t.kept_frames
            ),
            None => format!(
                "the timestamp sidecar \"{}\" has capture times for only {} of {} frames, \
                 kept {} (whole groups of pictures only)",
                sidecars.srt, t.usable_stanzas, t.total_frames, t.kept_frames
            ),
        });
    // Only known up front for an MP4 source (which is what we always have here);
    // otherwise it is however many frames the copy below manages to write.
    let total_frames = frame_src
        .srt_truncation()
        .map(|t| t.total_frames)
        .or_else(|| frame_src.mp4_sample_timing().map(|t| t.len()));

    tracing::debug!(
        "Copying \"{}\" into \"{}\" with timestamps and metadata. frame0_time: {frame0_time}, \
        mp4_cfg: {mp4_cfg:?}",
        sidecars.mp4,
        sidecars.rewritten,
    );
    let fd = std::fs::File::create(&sidecars.rewritten)?;
    let mut new_mp4 = mp4_writer::Mp4Writer::new(fd, mp4_cfg, None)?;
    let h264_src = frame_src.as_seekable_h264_source();
    new_mp4.set_first_sps_pps(h264_src.first_sps(), h264_src.first_pps());

    let insert_precision_timestamp = true;
    let width = frame_src.width();
    let height = frame_src.height();

    // Snapshot the source's per-sample timing (stts + ctts) before
    // iterating (which borrows `frame_src` mutably). Preserving this timing
    // verbatim is what keeps reordered (B-frame) streams correct: the
    // container ordering comes from the source, while the precise capture
    // time is carried per-frame in the precision-timestamp SEI.
    let sample_timing: Option<Vec<_>> = frame_src.mp4_sample_timing().map(|t| t.to_vec());

    let mut count = 0;
    // Salvage as much as possible: a frame that cannot be read (e.g. because a
    // crash left the source MP4 damaged) ends the copy rather than discarding
    // the frames already written.
    let mut loop_error = None;
    for frame in frame_src.decode_order_iter() {
        let frame = match frame {
            Ok(frame) => frame,
            Err(e) => {
                loop_error = Some(format!("frame {count} could not be read: {e}"));
                break;
            }
        };
        let timestamp = frame0_time + frame.timestamp().unwrap_duration();
        let idx = frame.idx();
        let data = match frame.image() {
            frame_source::ImageData::EncodedH264(data) => &data.data,
            _ => {
                return Err(Error::SourceIsNotH264);
            }
        };
        let write_result = match sample_timing.as_ref().and_then(|t| t.get(idx)) {
            Some(st) => new_mp4.write_h264_buf_passthrough(
                data,
                width,
                height,
                st.decode_duration,
                st.composition_offset,
                timestamp,
                insert_precision_timestamp,
            ),
            None => new_mp4.write_h264_buf(
                data,
                width,
                height,
                timestamp,
                frame0_time,
                insert_precision_timestamp,
            ),
        };
        if let Err(e) = write_result {
            loop_error = Some(format!("frame {idx} could not be written: {e}"));
            break;
        }
        count += 1;
        maybe_crash_mid_rewrite(count);
    }

    if count == 0 {
        drop(new_mp4);
        let _ = std::fs::remove_file(&sidecars.rewritten);
        let reason = loop_error
            .or(srt_truncation_reason)
            .unwrap_or_else(|| "the source has no frames".to_string());
        return Err(Error::NoFramesRewritten {
            mp4_filename: sidecars.mp4.clone(),
            reason,
        });
    }

    new_mp4.finish()?;
    tracing::debug!(
        "Finished writing \"{}\" with {count} frames.",
        sidecars.rewritten
    );

    let truncated_reason = match (srt_truncation_reason, loop_error) {
        (Some(srt_reason), Some(loop_reason)) => Some(format!("{srt_reason}; {loop_reason}")),
        (Some(reason), None) | (None, Some(reason)) => Some(reason),
        (None, None) => None,
    };

    Ok(RewriteOutcome {
        frames_written: count,
        total_frames: total_frames.unwrap_or(count),
        truncated_reason,
    })
}

/// Put the output of [`write_rewritten`] in place: rename it over the original
/// recording and delete the now-redundant sidecar files.
fn commit_rewritten(sidecars: &Sidecars, remove_metadata_json: bool) -> Result<()> {
    tracing::debug!(
        "Renaming \"{}\" to \"{}\", thereby deleting the original.",
        sidecars.rewritten,
        sidecars.mp4,
    );
    std::fs::rename(&sidecars.rewritten, &sidecars.mp4)?;

    // Remove no longer needed .srt and .json files.
    std::fs::remove_file(&sidecars.srt)?;
    if remove_metadata_json {
        std::fs::remove_file(&sidecars.metadata_json)?;
    }
    Ok(())
}

/// The state [`inspect`] found a recording in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// The recording was closed normally: none of the sidecar files are present.
    NotInterrupted,
    /// The recording holds video, but its capture times are still only in the
    /// SRT sidecar. [`repair`] embeds them.
    NeedsRewrite,
    /// The crash came after the rewrite was already in place, leaving only the
    /// sidecar files behind. [`repair`] removes them.
    StaleSidecars,
    /// Sidecar files are present but no video was ever written (ffmpeg is
    /// spawned on the first frame), so there is nothing to recover. [`repair`]
    /// removes the sidecar files.
    NothingRecorded,
}

/// Classify the recording `mp4_path`, without modifying anything.
///
/// Returns an error when a recording that needs repair cannot be repaired, e.g.
/// because its MP4 was never finalized. Note that this is also what happens for
/// a recording still in progress: an MP4 ffmpeg has not closed yet has no `moov`
/// box, so a live recording is reported as [`Error::Mp4NotFinalized`] rather
/// than being mistaken for a crashed one.
pub fn inspect<P: AsRef<Path>>(mp4_path: P) -> Result<State> {
    let sidecars = Sidecars::for_mp4(mp4_path.as_ref())?;
    let has_srt = Path::new(&sidecars.srt).exists();
    let has_metadata_json = Path::new(&sidecars.metadata_json).exists();
    let has_rewritten = Path::new(&sidecars.rewritten).exists();

    let mp4_size = match std::fs::metadata(&sidecars.mp4) {
        Ok(md) => Some(md.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };

    if !has_srt && !has_metadata_json && !has_rewritten {
        // With nothing left behind there is nothing to repair -- but say so
        // rather than reporting a mistyped path as a healthy recording.
        return match mp4_size {
            Some(_) => Ok(State::NotInterrupted),
            None => Err(Error::NoSuchRecording {
                mp4_filename: sidecars.mp4,
            }),
        };
    }

    match mp4_size {
        None | Some(0) => {
            if has_rewritten {
                // The rewrite output is only ever renamed *onto* the recording,
                // so this combination cannot arise from a crash. Rather than
                // guess, say what was found.
                return Err(Error::OriginalMissing {
                    mp4_filename: sidecars.mp4,
                    rewritten: sidecars.rewritten,
                });
            }
            Ok(State::NothingRecorded)
        }
        Some(_) => {
            if has_precision_timestamps(&sidecars.mp4)? {
                Ok(State::StaleSidecars)
            } else if !has_srt {
                Err(Error::MissingSrtSidecar {
                    mp4_filename: sidecars.mp4,
                    srt: sidecars.srt,
                })
            } else {
                Ok(State::NeedsRewrite)
            }
        }
    }
}

/// What [`repair`] did.
#[derive(Debug, Clone)]
pub enum Repair {
    /// Nothing needed doing.
    NotInterrupted,
    /// The rewrite stage was run and its result is now in place.
    Rewritten(RewriteOutcome),
    /// The rewrite itself had completed before the crash; only the stale sidecar
    /// files were removed.
    RemovedStaleSidecars,
    /// The recording never got any video; only the stale sidecar files were
    /// removed.
    RemovedEmptyRecording,
}

/// Finish a recording that a crash interrupted, whichever stage it was in.
///
/// Embeds the capture times from the SRT sidecar (and the metadata from the JSON
/// sidecar) into the recording exactly as [`FfmpegReWriter::close`] would have,
/// then renames the result over `mp4_path` and removes the sidecar files. A
/// partial `-rewritten.mp4` left by a crash inside the rewrite stage is
/// discarded and the stage redone: an interrupted [`mp4_writer::Mp4Writer`] never
/// wrote the index its output needs, while the inputs the stage reads are still
/// intact.
///
/// Doing nothing is a normal outcome: a recording with no sidecar files, or one
/// whose rewrite had already been put in place, needs no repair (see [`State`]).
/// Repairing a recording twice is therefore harmless.
pub fn repair<P: AsRef<Path>>(mp4_path: P) -> Result<Repair> {
    let sidecars = Sidecars::for_mp4(mp4_path.as_ref())?;
    match inspect(&sidecars.mp4)? {
        State::NotInterrupted => Ok(Repair::NotInterrupted),
        State::StaleSidecars => {
            sidecars.remove()?;
            Ok(Repair::RemovedStaleSidecars)
        }
        State::NothingRecorded => {
            sidecars.remove()?;
            Ok(Repair::RemovedEmptyRecording)
        }
        State::NeedsRewrite => {
            let h264_metadata = read_metadata_sidecar(&sidecars)?;
            let remove_metadata_json = h264_metadata.is_some();
            let outcome = write_rewritten(&sidecars, mp4_cfg(h264_metadata))?;
            commit_rewritten(&sidecars, remove_metadata_json)?;
            Ok(Repair::Rewritten(outcome))
        }
    }
}

/// The [`H264Metadata`] saved when recording started, if there was any.
fn read_metadata_sidecar(sidecars: &Sidecars) -> Result<Option<H264Metadata>> {
    match std::fs::read_to_string(&sidecars.metadata_json) {
        Ok(buf) => Ok(Some(serde_json::from_str(&buf)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Does this MP4 already carry the per-frame precision timestamps that the
/// rewrite stage embeds? True only of a file that stage has already produced;
/// the plain ffmpeg output of the capture stage never has them.
fn has_precision_timestamps(mp4_filename: &str) -> Result<bool> {
    match frame_source::FrameSourceBuilder::new(mp4_filename)
        .do_decode_h264(false)
        .timestamp_source(frame_source::TimestampSource::MispMicrosectime)
        .build_h264_in_mp4_source()
    {
        Ok(_) => Ok(true),
        // The file is readable H264-in-MP4, it just has no precision timestamps.
        Err(frame_source::Error::H264TimestampError(_)) => Ok(false),
        Err(e) => {
            // A missing `moov` box is by far the most likely reason a recording
            // cannot be read at all, and it means something quite specific, so
            // check for it rather than passing on a low-level parse error.
            if !is_mp4_finalized(mp4_filename)? {
                return Err(Error::Mp4NotFinalized {
                    mp4_filename: mp4_filename.to_string(),
                });
            }
            Err(e.into())
        }
    }
}

/// Does this MP4 have the `moov` box that holds its sample index?
///
/// ffmpeg writes `moov` when it closes the file, so its absence means the file
/// was never finalized: ffmpeg is either still writing it or was killed. Only
/// the top-level box headers are read, so this works on a file whose `mdat` is
/// truncated.
fn is_mp4_finalized(mp4_filename: &str) -> Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut fd = std::fs::File::open(mp4_filename)?;
    let file_size = fd.metadata()?.len();
    let mut pos = 0u64;
    // Each box starts with a 32-bit size (of the whole box, header included) and
    // a four-character type. ISO/IEC 14496-12 §4.2.
    while pos + 8 <= file_size {
        fd.seek(SeekFrom::Start(pos))?;
        let mut header = [0u8; 8];
        fd.read_exact(&mut header)?;
        if &header[4..8] == b"moov" {
            return Ok(true);
        }
        let size = match u32::from_be_bytes(header[..4].try_into().unwrap()) {
            // 0 means "extends to end of file", so there is nothing after it.
            0 => break,
            // 1 means the real size is in the 64-bit `largesize` that follows.
            1 => {
                let mut largesize = [0u8; 8];
                fd.read_exact(&mut largesize)?;
                u64::from_be_bytes(largesize)
            }
            size => size.into(),
        };
        if size < 8 {
            // Malformed: a box cannot be smaller than its header.
            break;
        }
        pos += size;
    }
    Ok(false)
}

/// The recording a leftover file belongs to, if `path` is one of the files this
/// crate leaves behind when interrupted -- its SRT sidecar, or the partial
/// output of the rewrite stage. `None` for anything else, including a plain
/// `.mp4`.
pub fn recording_for_leftover<P: AsRef<Path>>(path: P) -> Option<PathBuf> {
    let path = path.as_ref().to_str()?;
    match path.strip_suffix(SRT_SIDECAR_SUFFIX) {
        Some(basename) => Some(PathBuf::from(format!("{basename}.mp4"))),
        // Stripping this suffix leaves the recording's own name, `<name>.mp4`.
        None => path.strip_suffix(REWRITTEN_SUFFIX).map(PathBuf::from),
    }
}

/// Find the recordings under `dir` (searched recursively) whose rewrite stage
/// did not finish, by the files it leaves behind. Returns their `.mp4` paths,
/// deduplicated and sorted.
///
/// The metadata sidecar is deliberately not searched for: `-metadata.json` is a
/// generic enough name that another program could own such a file, and we only
/// ever write it together with the SRT sidecar.
pub fn find_interrupted<P: AsRef<Path>>(dir: P) -> Result<Vec<PathBuf>> {
    let dir = dir.as_ref().to_str().ok_or(Error::FilenameNotUnicode)?;
    // The directory name is data, not pattern: a `[` or `*` in it must not be
    // read as a glob metacharacter.
    let escaped_dir = glob::Pattern::escape(dir);
    let mut found = std::collections::BTreeSet::new();
    for suffix in [SRT_SIDECAR_SUFFIX, REWRITTEN_SUFFIX] {
        let pattern = Path::new(&escaped_dir)
            .join("**")
            .join(format!("*{suffix}"));
        let pattern = pattern.to_str().ok_or(Error::FilenameNotUnicode)?;
        for entry in glob::glob(pattern)? {
            if let Some(mp4) = recording_for_leftover(entry?) {
                found.insert(mp4);
            }
        }
    }
    Ok(found.into_iter().collect())
}

/// Test hook simulating a crash partway through the rewrite stage, so the tests
/// can produce a genuine leftover partial `-rewritten.mp4`. Compiled only for
/// this crate's own tests; a no-op otherwise.
#[cfg(test)]
fn maybe_crash_mid_rewrite(frames_written: usize) {
    if let Ok(after) = std::env::var(test::ABORT_AFTER_REWRITE_FRAMES_ENV)
        && frames_written >= after.parse::<usize>().unwrap()
    {
        std::process::abort();
    }
}

#[cfg(not(test))]
fn maybe_crash_mid_rewrite(_frames_written: usize) {}

#[cfg(test)]
mod test {
    use super::*;
    use machine_vision_formats::{owned::OImage, pixel_format::RGB8};

    use test_log::test;

    #[test]
    fn test_ffmpeg_rewriter() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let mp4_fname = tempdir.path().join("out.mp4");

        // let mp4_fname = "out.mp4";

        let timestamp_micros: i64 = 1_662_921_288_000_000; // Sun, 11 Sep 2022 18:34:48 UTC

        let mut timestamps = vec![
            DateTime::from_timestamp_micros(timestamp_micros).unwrap(),
            DateTime::from_timestamp_micros(timestamp_micros + 1).unwrap(),
            DateTime::from_timestamp_micros(timestamp_micros + 100).unwrap(),
        ];

        for delta in 1..10 {
            let micros = delta * 10_000;
            timestamps.push(DateTime::from_timestamp_micros(timestamp_micros + micros).unwrap());
        }

        tracing::debug!("Encoding {} frames", timestamps.len());

        let w = 640;
        let h = 480;
        {
            // let ffmpeg_codec_args = ffmpeg_writer::platform_hardware_encoder()?;
            let ffmpeg_codec_args = Default::default();

            let rate = None;
            let h264_metadata = None;
            let mut wtr = FfmpegReWriter::new(&mp4_fname, ffmpeg_codec_args, rate, h264_metadata)?;

            for (i, ts) in timestamps.iter().enumerate() {
                let value = (i % 255) as u8;
                let frame: OImage<RGB8> = OImage::new(
                    w,
                    h,
                    w as usize * 3,
                    vec![value; w as usize * h as usize * 3],
                )
                .unwrap();
                let frame = strand_dynamic_frame::DynamicFrameOwned::from_static(frame);
                wtr.write_dynamic_frame(&frame.borrow(), *ts)?;
            }
            wtr.close()?;
        }

        let mut frame_src = frame_source::FrameSourceBuilder::new(&mp4_fname)
            .do_decode_h264(false)
            .timestamp_source(frame_source::TimestampSource::MispMicrosectime)
            .build_source()?;

        let frame0_time = frame_src.frame0_time().unwrap();
        assert_eq!(frame0_time, timestamps[0]);

        assert_eq!(frame_src.width(), w);
        assert_eq!(frame_src.height(), h);

        // Frames are read back in decode order, which differs from
        // presentation (input) order for B-frame streams. Every precise
        // timestamp must nonetheless round-trip intact; the per-frame SEI
        // carries each frame's own capture time regardless of decode order.
        // (Correct playback *ordering* is covered by the end-to-end smoke test.)
        let mut got: Vec<_> = Vec::new();
        for frame in frame_src.decode_order_iter() {
            let frame = frame?;
            got.push(frame0_time + frame.timestamp().unwrap_duration());
        }
        assert_eq!(got.len(), timestamps.len());
        got.sort();
        let mut expected = timestamps.clone();
        expected.sort();
        assert_eq!(got, expected);

        Ok(())
    }

    /// Regression test for re-muxing a reordered (B-frame) stream so that it
    /// plays back in the correct presentation order.
    ///
    /// The intermediate libx264 pass stores frames in *decode* order with
    /// non-zero composition offsets (B-frames). Before the fix, [`FfmpegReWriter`]
    /// could not represent this: `mp4-writer` hardcoded a zero composition
    /// offset (no `ctts`) and paired each SRT capture time with the frame by
    /// decode index, so the re-muxed file had its capture times scrambled
    /// relative to the true display order (frames played e.g. 5,1,2,3,4,...).
    ///
    /// Here we force B-frames, re-mux, and then read the result back. We
    /// reconstruct each sample's presentation time from the container timing
    /// (`stts` decode duration + `ctts` composition offset) and assert that,
    /// walked in presentation order, the per-frame precision-timestamp SEI
    /// capture times are strictly increasing — i.e. the file plays in order.
    /// Prior to the fix (composition offset forced to zero, SEI paired by
    /// decode index) this ordering was violated.
    #[test]
    fn test_bframe_stream_remuxes_in_presentation_order() -> Result<()> {
        use frame_source::h264_source::Mp4SampleTiming;

        let tempdir = tempfile::tempdir()?;
        let mp4_fname = tempdir.path().join("out.mp4");

        // 25 fps nominal cadence; the SRT carries the real (here identical)
        // capture times.
        let n_frames = 24usize;
        let base_micros: i64 = 1_662_921_288_000_000; // Sun, 11 Sep 2022 18:34:48 UTC
        let frame_interval_micros = 40_000i64; // 25 fps
        let timestamps: Vec<_> = (0..n_frames)
            .map(|i| {
                DateTime::from_timestamp_micros(base_micros + i as i64 * frame_interval_micros)
                    .unwrap()
            })
            .collect();

        let w = 64u32;
        let h = 48u32;
        {
            // Force libx264 to insert a fixed pattern of B-frames (b_adapt=0
            // takes the content out of the decision) so the re-mux definitely
            // exercises the reordered path. A single keyframe keeps one GOP.
            let ffmpeg_codec_args = FfmpegCodecArgs {
                device_args: None,
                pre_codec_args: None,
                codec: Some("libx264".to_string()),
                post_codec_args: Some(vec![
                    ("-bf".to_string(), "3".to_string()),
                    (
                        "-x264-params".to_string(),
                        "b_adapt=0:scenecut=0:keyint=1000:min-keyint=1000".to_string(),
                    ),
                ]),
                pixfmt: Some("yuv420p".to_string()),
                // This test deliberately forces B-frames via `-bf 3` in
                // `post_codec_args` to exercise reordering, so do not emit the
                // default `-bf 0`.
                max_bframes: None,
            };

            let mut wtr = FfmpegReWriter::new(&mp4_fname, ffmpeg_codec_args, None, None)?;

            for (i, ts) in timestamps.iter().enumerate() {
                // Vary the content per frame so the encoder has real motion to
                // reorder around.
                let mut data = vec![0u8; w as usize * h as usize * 3];
                for (px, chunk) in data.chunks_exact_mut(3).enumerate() {
                    let v = ((px + i * 7) % 256) as u8;
                    chunk[0] = v;
                    chunk[1] = v.wrapping_mul(3);
                    // Wrapping: `i as u8 * 11` overflows past 23 frames.
                    chunk[2] = v.wrapping_add((i as u8).wrapping_mul(11));
                }
                let frame: OImage<RGB8> = OImage::new(w, h, w as usize * 3, data).unwrap();
                let frame = strand_dynamic_frame::DynamicFrameOwned::from_static(frame);
                wtr.write_dynamic_frame(&frame.borrow(), *ts)?;
            }
            wtr.close()?;
        }

        // Read the re-muxed file back, keeping the H264 in decode order and
        // recovering the container timing so we can reconstruct presentation
        // order.
        let mut frame_src = frame_source::FrameSourceBuilder::new(&mp4_fname)
            .do_decode_h264(false)
            .timestamp_source(frame_source::TimestampSource::MispMicrosectime)
            .build_h264_in_mp4_source()?;

        let frame0_time = frame_src.frame0_time().unwrap();

        // Snapshot per-sample timing (stts + ctts) before iterating (which
        // borrows the source mutably).
        let sample_timing: Vec<Mp4SampleTiming> = frame_src
            .mp4_sample_timing()
            .expect("MP4 source must expose per-sample timing")
            .to_vec();
        assert_eq!(sample_timing.len(), n_frames);

        // The re-mux is only meaningful as a reordering test if the encoder
        // actually produced B-frames (non-zero composition offsets).
        let has_reordering = sample_timing
            .iter()
            .any(|t| t.composition_offset != chrono::Duration::zero());
        assert!(
            has_reordering,
            "expected libx264 to emit B-frames (non-zero ctts); test would be vacuous otherwise"
        );

        // Collect the SEI capture time for each sample, in decode order.
        let mut sei_times = vec![None; n_frames];
        for frame in frame_src.decode_order_iter() {
            let frame = frame?;
            sei_times[frame.idx()] = Some(frame0_time + frame.timestamp().unwrap_duration());
        }

        // Reconstruct each sample's presentation time: presentation = decode +
        // composition_offset, where the decode time is the running sum of the
        // per-sample decode durations (stts), all in decode order.
        let mut decode_time = chrono::Duration::zero();
        let mut presentation = Vec::with_capacity(n_frames);
        for (i, timing) in sample_timing.iter().enumerate() {
            let pts = decode_time
                + chrono::Duration::from_std(timing.decode_duration).unwrap()
                + timing.composition_offset;
            let sei = sei_times[i].expect("every sample must carry a SEI timestamp");
            presentation.push((pts, sei));
            decode_time += chrono::Duration::from_std(timing.decode_duration).unwrap();
        }

        // Walk samples in presentation order and assert the SEI capture times
        // are strictly increasing: the file plays back in the order it was
        // recorded.
        presentation.sort_by_key(|(pts, _)| *pts);
        let ordered_sei: Vec<_> = presentation.iter().map(|(_, sei)| *sei).collect();
        for pair in ordered_sei.windows(2) {
            assert!(
                pair[0] < pair[1],
                "SEI capture times must strictly increase in presentation order, \
                 but got {:?} then {:?} (out-of-order playback)",
                pair[0],
                pair[1]
            );
        }

        // And the set of capture times must match what we wrote.
        assert_eq!(ordered_sei, timestamps);

        Ok(())
    }

    // ----------------------------------------------------------------------
    // Crash recovery
    //
    // These tests kill a recording process outright -- no unwinding, no
    // destructors, no `close()` -- so the files left behind are the real
    // article rather than a hand-built imitation, then check that [`repair`]
    // finishes the recording. The crashing writer runs in a child process,
    // which is this very test binary re-invoked to run the `#[ignore]`d
    // [`crash_writer_child`] helper (see [`run_crash_child`]).
    // ----------------------------------------------------------------------

    /// Names the stage [`crash_writer_child`] should die in.
    const CHILD_STAGE_ENV: &str = "FFMPEG_REWRITER_TEST_CRASH_STAGE";
    /// Directory the child records into.
    const CHILD_DIR_ENV: &str = "FFMPEG_REWRITER_TEST_CRASH_DIR";
    /// Read by [`maybe_crash_mid_rewrite`]: abort once the rewrite stage has
    /// written this many frames.
    pub(crate) const ABORT_AFTER_REWRITE_FRAMES_ENV: &str =
        "FFMPEG_REWRITER_TEST_ABORT_AFTER_REWRITE_FRAMES";

    /// Die while recording frames, before `close()` is ever reached.
    const STAGE_CAPTURE: &str = "capture";
    /// Die inside `close()`, partway through the rewrite stage.
    const STAGE_REWRITE: &str = "rewrite";

    /// How many frames the child records before dying.
    const CRASH_TEST_FRAMES: usize = 24;
    /// Group-of-pictures length forced on the encoder. A recording interrupted
    /// mid-GOP can only be recovered up to a GOP boundary, so a short GOP keeps
    /// the loss (and the test) small.
    const CRASH_TEST_GOP: usize = 4;
    const CRASH_TEST_CAMERA_NAME: &str = "crash-test-cam";
    const CRASH_TEST_WIDTH: u32 = 64;
    const CRASH_TEST_HEIGHT: u32 = 48;

    fn crash_test_mp4(dir: &Path) -> String {
        dir.join("crashed.mp4").to_str().unwrap().to_string()
    }

    /// The capture times the child hands to the writer: a strictly increasing
    /// 25 fps cadence, so the repaired file's timestamps can be compared against
    /// them exactly.
    fn crash_test_timestamps() -> Vec<DateTime<chrono::Utc>> {
        let base_micros: i64 = 1_662_921_288_000_000; // Sun, 11 Sep 2022 18:34:48 UTC
        (0..CRASH_TEST_FRAMES)
            .map(|i| DateTime::from_timestamp_micros(base_micros + i as i64 * 40_000).unwrap())
            .collect()
    }

    /// Force short groups of pictures (so an interrupted recording has GOP
    /// boundaries to round down to) and B-frames, so repair also has to get
    /// reordered streams right.
    fn crash_test_codec_args() -> FfmpegCodecArgs {
        FfmpegCodecArgs {
            device_args: None,
            pre_codec_args: None,
            codec: Some("libx264".to_string()),
            post_codec_args: Some(vec![
                ("-bf".to_string(), "2".to_string()),
                (
                    "-x264-params".to_string(),
                    format!(
                        "b_adapt=0:scenecut=0:keyint={CRASH_TEST_GOP}:min-keyint={CRASH_TEST_GOP}"
                    ),
                ),
            ]),
            pixfmt: Some("yuv420p".to_string()),
            // B-frames are wanted here, so do not emit the default `-bf 0`.
            max_bframes: None,
        }
    }

    /// Frame `i` of the test recording, with content that varies per frame so the
    /// encoder has real motion to reorder around.
    fn crash_test_frame(i: usize) -> strand_dynamic_frame::DynamicFrameOwned {
        let (w, h) = (CRASH_TEST_WIDTH, CRASH_TEST_HEIGHT);
        let mut data = vec![0u8; w as usize * h as usize * 3];
        for (px, chunk) in data.chunks_exact_mut(3).enumerate() {
            let v = ((px + i * 7) % 256) as u8;
            chunk[0] = v;
            chunk[1] = v.wrapping_mul(3);
            // Wrapping: `i as u8 * 11` overflows past 23 frames.
            chunk[2] = v.wrapping_add((i as u8).wrapping_mul(11));
        }
        let frame: OImage<RGB8> = OImage::new(w, h, w as usize * 3, data).unwrap();
        strand_dynamic_frame::DynamicFrameOwned::from_static(frame)
    }

    /// Helper process for the crash tests: records [`CRASH_TEST_FRAMES`] frames
    /// into `<dir>/crashed.mp4` and then dies suddenly, in the stage named by
    /// [`CHILD_STAGE_ENV`].
    ///
    /// Ignored so that it only runs when [`run_crash_child`] invokes this binary
    /// for it explicitly.
    #[test]
    #[ignore = "helper process for the crash tests; invoked via run_crash_child"]
    fn crash_writer_child() {
        let dir = std::env::var(CHILD_DIR_ENV).expect("crash child needs a directory");
        let stage = std::env::var(CHILD_STAGE_ENV).expect("crash child needs a stage");
        let mp4 = crash_test_mp4(Path::new(&dir));

        let timestamps = crash_test_timestamps();
        let mut h264_metadata =
            H264Metadata::new("ffmpeg-rewriter-crash-test", timestamps[0].fixed_offset());
        h264_metadata.camera_name = Some(CRASH_TEST_CAMERA_NAME.to_string());

        let mut wtr = FfmpegReWriter::new(
            &mp4,
            crash_test_codec_args(),
            None,
            Some(h264_metadata.clone()),
        )
        .unwrap();
        for (i, ts) in timestamps.iter().enumerate() {
            wtr.write_dynamic_frame(&crash_test_frame(i).borrow(), *ts)
                .unwrap();
        }

        if stage == STAGE_CAPTURE {
            // Vanish as `kill -9` would: no unwinding, no destructors, no
            // `close()`. The last frame's capture time is still buffered inside
            // the SRT writer and is lost with us.
            std::process::abort();
        }

        assert_eq!(stage, STAGE_REWRITE);
        // The parent armed `ABORT_AFTER_REWRITE_FRAMES_ENV`, so this aborts
        // partway through writing the rewritten MP4.
        wtr.close().unwrap();
        unreachable!("close() should have aborted partway through the rewrite");
    }

    /// Run [`crash_writer_child`] in a child process and wait for it to die.
    fn run_crash_child(dir: &Path, stage: &str, abort_after_rewrite_frames: Option<usize>) {
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "--ignored", "test::crash_writer_child"])
            .env(CHILD_DIR_ENV, dir)
            .env(CHILD_STAGE_ENV, stage)
            // Have ffmpeg inherit the child's stdout/stderr (both /dev/null
            // below) instead of pipes owned by the child: killing the child
            // would break those pipes, and ffmpeg would then die on SIGPIPE
            // before writing the MP4 index. A real recording's ffmpeg inherits
            // the console and keeps running long enough to finalize the file,
            // which is the situation being tested.
            .env("FFMPEG_WRITER_SHOW", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Some(n) = abort_after_rewrite_frames {
            cmd.env(ABORT_AFTER_REWRITE_FRAMES_ENV, n.to_string());
        }
        let status = cmd.status().unwrap();
        assert!(
            !status.success(),
            "the crash helper was supposed to die, but exited with {status}"
        );
    }

    /// Wait for the ffmpeg process orphaned by the crashed child to finish
    /// writing the MP4.
    ///
    /// ffmpeg writes the sample index (`moov`) only when its input ends, which
    /// for it is when the dead child's pipe closes; until then the file cannot be
    /// read. Waiting for a successful open (rather than merely for the `moov`
    /// header to appear) avoids racing a half-written index.
    fn wait_for_finalized_mp4(mp4: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let readable = frame_source::FrameSourceBuilder::new(mp4)
                .do_decode_h264(false)
                .timestamp_source(frame_source::TimestampSource::Mp4Pts)
                .build_h264_in_mp4_source()
                .is_ok();
            if readable {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "ffmpeg never finished writing \"{mp4}\""
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Check a repaired recording: the sidecar files are gone, the MP4 is
    /// readable, it kept the metadata saved before the crash, and its per-frame
    /// precision timestamps are exactly the first `expected_frames` capture times
    /// the crashed writer was given.
    fn assert_repaired(mp4: &str, expected_frames: usize) -> Result<()> {
        let sidecars = Sidecars::for_mp4(mp4)?;
        for leftover in [&sidecars.srt, &sidecars.metadata_json, &sidecars.rewritten] {
            assert!(
                !Path::new(leftover).exists(),
                "\"{leftover}\" should have been cleaned up"
            );
        }

        let mut frame_src = frame_source::FrameSourceBuilder::new(mp4)
            .do_decode_h264(false)
            .timestamp_source(frame_source::TimestampSource::MispMicrosectime)
            .build_h264_in_mp4_source()?;
        // Only the metadata sidecar could have supplied this, so it proves the
        // repair picked it up.
        assert_eq!(frame_src.camera_name(), Some(CRASH_TEST_CAMERA_NAME));
        assert_eq!(frame_src.width(), CRASH_TEST_WIDTH);
        assert_eq!(frame_src.height(), CRASH_TEST_HEIGHT);

        let frame0_time = frame_src.frame0_time().unwrap();

        // Snapshot the per-sample container timing (stts + ctts) before iterating
        // (which borrows the source mutably), so presentation order can be
        // reconstructed below.
        let sample_timing: Vec<_> = frame_src
            .mp4_sample_timing()
            .expect("a repaired recording is an MP4 with per-sample timing")
            .to_vec();
        assert_eq!(sample_timing.len(), expected_frames);
        assert!(
            sample_timing
                .iter()
                .any(|t| t.composition_offset != chrono::Duration::zero()),
            "the test recording is meant to contain B-frames (non-zero ctts); without \
            reordering this check would be vacuous"
        );

        // Frames come back in decode order, which for a B-frame stream is not the
        // order they were captured in.
        let mut sei_times = vec![None; expected_frames];
        for frame in frame_src.decode_order_iter() {
            let frame = frame?;
            sei_times[frame.idx()] = Some(frame0_time + frame.timestamp().unwrap_duration());
        }

        // Put the samples back in the order a player shows them --
        // `presentation = decode + composition_offset`, decode time being the
        // running sum of the per-sample durations -- and check the capture times
        // come out in exactly the order they were recorded in. This is what
        // proves the repaired file plays back correctly, not merely that it
        // carries the right set of timestamps.
        let mut decode_time = chrono::Duration::zero();
        let mut presentation = Vec::with_capacity(expected_frames);
        for (i, timing) in sample_timing.iter().enumerate() {
            let duration = chrono::Duration::from_std(timing.decode_duration).unwrap();
            let pts = decode_time + duration + timing.composition_offset;
            let sei = sei_times[i].expect("every sample must carry a capture time");
            presentation.push((pts, sei));
            decode_time += duration;
        }
        presentation.sort_by_key(|(pts, _)| *pts);
        let ordered_sei: Vec<_> = presentation.into_iter().map(|(_, sei)| sei).collect();
        let expected: Vec<_> = crash_test_timestamps()
            .into_iter()
            .take(expected_frames)
            .collect();
        assert_eq!(ordered_sei, expected);

        // Repairing a repaired recording is a no-op.
        assert!(matches!(repair(mp4)?, Repair::NotInterrupted));
        Ok(())
    }

    /// A writer killed while recording leaves a finished MP4 (ffmpeg closes it
    /// when our end of the pipe dies) whose capture times are still only in the
    /// SRT sidecar. [`repair`] must embed them.
    ///
    /// The last frames cannot be recovered: the SRT sidecar is written one frame
    /// behind (a stanza needs the following frame's time to close it), so the
    /// group of pictures in progress at the moment of the crash has no usable
    /// timing and is dropped.
    #[test]
    fn test_repair_after_crash_while_recording() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let mp4 = crash_test_mp4(tempdir.path());

        run_crash_child(tempdir.path(), STAGE_CAPTURE, None);
        wait_for_finalized_mp4(&mp4);

        // The crash left exactly the state repair is meant to pick up: video
        // plus sidecars, and no rewrite output (it never got that far).
        let sidecars = Sidecars::for_mp4(&mp4)?;
        assert!(Path::new(&sidecars.srt).exists());
        assert!(Path::new(&sidecars.metadata_json).exists());
        assert!(!Path::new(&sidecars.rewritten).exists());
        assert_eq!(inspect(&mp4)?, State::NeedsRewrite);
        assert_eq!(
            find_interrupted(tempdir.path())?,
            vec![PathBuf::from(&mp4)],
            "the interrupted recording must be discoverable from its sidecar files"
        );

        let Repair::Rewritten(outcome) = repair(&mp4)? else {
            panic!("an interrupted recording must be rewritten");
        };

        // One capture time was still buffered when the process died, so the
        // trailing partial GOP had to be dropped -- but only that.
        let reason = outcome
            .truncated_reason
            .as_ref()
            .expect("the frames left without a capture time must be reported");
        assert!(
            reason.contains("capture times for only"),
            "reason: {reason}"
        );
        assert_eq!(outcome.total_frames, CRASH_TEST_FRAMES);
        assert_eq!(
            outcome.frames_written % CRASH_TEST_GOP,
            0,
            "only whole groups of pictures can be kept, got {}",
            outcome.frames_written
        );
        assert!(
            outcome.frames_written >= CRASH_TEST_FRAMES - 2 * CRASH_TEST_GOP
                && outcome.frames_written < CRASH_TEST_FRAMES,
            "expected all but the last group of pictures, got {} of {CRASH_TEST_FRAMES}",
            outcome.frames_written
        );

        assert_repaired(&mp4, outcome.frames_written)?;
        Ok(())
    }

    /// A writer killed *inside* the rewrite stage leaves a partial
    /// `<name>.mp4-rewritten.mp4` behind, next to a complete MP4 and complete
    /// sidecars. [`repair`] must discard that unusable partial file and redo the
    /// stage -- losing nothing, since every capture time was already written.
    #[test]
    fn test_repair_after_crash_during_rewrite() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let mp4 = crash_test_mp4(tempdir.path());

        run_crash_child(tempdir.path(), STAGE_REWRITE, Some(CRASH_TEST_FRAMES / 3));
        wait_for_finalized_mp4(&mp4);

        let sidecars = Sidecars::for_mp4(&mp4)?;
        assert!(
            Path::new(&sidecars.rewritten).exists(),
            "the crash should have left a partial \"{}\" behind",
            sidecars.rewritten
        );
        assert!(Path::new(&sidecars.srt).exists());
        assert_eq!(inspect(&mp4)?, State::NeedsRewrite);
        assert_eq!(find_interrupted(tempdir.path())?, vec![PathBuf::from(&mp4)]);

        let Repair::Rewritten(outcome) = repair(&mp4)? else {
            panic!("an interrupted recording must be rewritten");
        };
        // `close()` had already written every capture time to the sidecar before
        // the crash, so nothing is lost here.
        assert_eq!(outcome.truncated_reason, None);
        assert_eq!(outcome.frames_written, CRASH_TEST_FRAMES);
        assert_eq!(outcome.total_frames, CRASH_TEST_FRAMES);

        assert_repaired(&mp4, CRASH_TEST_FRAMES)?;
        Ok(())
    }

    /// A crash can also cut the SRT sidecar mid-stanza, leaving a final entry
    /// whose JSON payload is half-written. Repair must treat that entry (and
    /// everything after it) as timing it does not have, rather than choking on
    /// it.
    #[test]
    fn test_repair_with_srt_cut_mid_stanza() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let mp4 = crash_test_mp4(tempdir.path());

        run_crash_child(tempdir.path(), STAGE_CAPTURE, None);
        wait_for_finalized_mp4(&mp4);

        // Cut the sidecar in the middle of stanza 11's JSON payload, as a
        // process killed between two writes would leave it.
        let sidecars = Sidecars::for_mp4(&mp4)?;
        const USABLE_STANZAS: usize = 10;
        let srt = std::fs::read_to_string(&sidecars.srt)?;
        let stanzas: Vec<&str> = srt.split("\n\n").collect();
        assert!(stanzas.len() > USABLE_STANZAS + 1);
        let cut_stanza = stanzas[USABLE_STANZAS];
        let partial = &cut_stanza[..cut_stanza.len() - 20];
        assert!(
            partial.contains("timestamp") && !partial.contains('}'),
            "the cut must land inside the JSON payload, got {partial:?}"
        );
        std::fs::write(
            &sidecars.srt,
            format!("{}\n\n{partial}", stanzas[..USABLE_STANZAS].join("\n\n")),
        )?;

        let Repair::Rewritten(outcome) = repair(&mp4)? else {
            panic!("an interrupted recording must be rewritten");
        };

        // Stanzas are 3 lines plus a blank separator, so the half-written one
        // starts at line 41.
        let reason = outcome.truncated_reason.as_ref().unwrap();
        assert!(
            reason.contains("incomplete from line 41"),
            "reason: {reason}"
        );
        // Ten usable capture times, rounded down to a whole group of pictures.
        let expected_frames = USABLE_STANZAS - USABLE_STANZAS % CRASH_TEST_GOP;
        assert_eq!(outcome.frames_written, expected_frames);
        assert_eq!(outcome.total_frames, CRASH_TEST_FRAMES);

        assert_repaired(&mp4, expected_frames)?;
        Ok(())
    }

    /// Sidecar files left behind by a crash *after* the rewrite was already in
    /// place are stale: the recording is complete and must be left alone, with
    /// only the sidecars cleaned up.
    #[test]
    fn test_repair_removes_stale_sidecars() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let mp4 = crash_test_mp4(tempdir.path());

        // A normally-closed recording, i.e. one already rewritten.
        {
            let timestamps = crash_test_timestamps();
            let mut h264_metadata =
                H264Metadata::new("ffmpeg-rewriter-crash-test", timestamps[0].fixed_offset());
            h264_metadata.camera_name = Some(CRASH_TEST_CAMERA_NAME.to_string());
            let mut wtr =
                FfmpegReWriter::new(&mp4, crash_test_codec_args(), None, Some(h264_metadata))?;
            for (i, ts) in timestamps.iter().enumerate() {
                wtr.write_dynamic_frame(&crash_test_frame(i).borrow(), *ts)?;
            }
            wtr.close()?;
        }
        let before = std::fs::read(&mp4)?;

        // Now put the sidecars back, as a crash between renaming the rewritten
        // file into place and deleting them would have.
        let sidecars = Sidecars::for_mp4(&mp4)?;
        std::fs::write(&sidecars.srt, "1\n00:00:00,000 --> 00:00:00,040\n{}\n\n")?;
        std::fs::write(&sidecars.metadata_json, "{}")?;

        assert_eq!(inspect(&mp4)?, State::StaleSidecars);
        assert!(matches!(repair(&mp4)?, Repair::RemovedStaleSidecars));
        assert!(!Path::new(&sidecars.srt).exists());
        assert!(!Path::new(&sidecars.metadata_json).exists());
        assert_eq!(
            std::fs::read(&mp4)?,
            before,
            "an already-rewritten recording must not be touched"
        );
        Ok(())
    }

    /// A path with neither a recording nor any leftover files is a mistake worth
    /// reporting, not a healthy recording.
    #[test]
    fn test_missing_recording_is_reported() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let missing = crash_test_mp4(tempdir.path());
        assert!(matches!(
            inspect(&missing),
            Err(Error::NoSuchRecording { .. })
        ));
        Ok(())
    }

    /// An MP4 ffmpeg never finalized cannot be repaired -- and must not be
    /// mistaken for one that can. This is also what protects a recording that is
    /// still in progress from a doctor run over its directory.
    #[test]
    fn test_unfinalized_mp4_is_reported() -> Result<()> {
        let tempdir = tempfile::tempdir()?;
        let mp4 = crash_test_mp4(tempdir.path());

        run_crash_child(tempdir.path(), STAGE_CAPTURE, None);
        wait_for_finalized_mp4(&mp4);
        assert!(is_mp4_finalized(&mp4)?);

        // Drop the index, as a recording ffmpeg has not closed yet lacks.
        let complete = std::fs::read(&mp4)?;
        let moov_at = complete
            .windows(4)
            .position(|w| w == b"moov")
            .expect("a finalized MP4 has a moov box");
        std::fs::write(&mp4, &complete[..moov_at - 4])?;

        assert!(!is_mp4_finalized(&mp4)?);
        assert!(matches!(inspect(&mp4), Err(Error::Mp4NotFinalized { .. })));
        assert!(matches!(repair(&mp4), Err(Error::Mp4NotFinalized { .. })));
        Ok(())
    }
}
