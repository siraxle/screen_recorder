//! SRT/VTT formatting from stored transcript segments (F2 "Export
//! subtitles"). Pure functions, no subprocess — segment timecodes come
//! straight from the DB, already aligned to the source video.

use crate::db::Segment;

fn format_timecode_srt(ms: i64) -> String {
    let ms = ms.max(0);
    let hours = ms / 3_600_000;
    let minutes = (ms % 3_600_000) / 60_000;
    let seconds = (ms % 60_000) / 1000;
    let millis = ms % 1000;
    format!("{hours:02}:{minutes:02}:{seconds:02},{millis:03}")
}

fn format_timecode_vtt(ms: i64) -> String {
    let ms = ms.max(0);
    let hours = ms / 3_600_000;
    let minutes = (ms % 3_600_000) / 60_000;
    let seconds = (ms % 60_000) / 1000;
    let millis = ms % 1000;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}

pub fn segments_to_srt(segments: &[Segment]) -> String {
    let mut out = String::new();
    for (index, segment) in segments.iter().enumerate() {
        out.push_str(&format!("{}\n", index + 1));
        out.push_str(&format!(
            "{} --> {}\n",
            format_timecode_srt(segment.start_ms),
            format_timecode_srt(segment.end_ms)
        ));
        out.push_str(segment.text.trim());
        out.push_str("\n\n");
    }
    out
}

pub fn segments_to_vtt(segments: &[Segment]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for segment in segments {
        out.push_str(&format!(
            "{} --> {}\n",
            format_timecode_vtt(segment.start_ms),
            format_timecode_vtt(segment.end_ms)
        ));
        out.push_str(segment.text.trim());
        out.push_str("\n\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_segments() -> Vec<Segment> {
        vec![
            Segment {
                start_ms: 0,
                end_ms: 1500,
                text: "Hello there.".to_string(),
            },
            Segment {
                start_ms: 1500,
                end_ms: 3661_250,
                text: "  Second line.  ".to_string(),
            },
        ]
    }

    #[test]
    fn srt_formats_timecodes_and_trims_text() {
        let srt = segments_to_srt(&sample_segments());
        assert_eq!(
            srt,
            "1\n00:00:00,000 --> 00:00:01,500\nHello there.\n\n\
             2\n00:00:01,500 --> 01:01:01,250\nSecond line.\n\n"
        );
    }

    #[test]
    fn vtt_has_header_and_dot_separated_millis() {
        let vtt = segments_to_vtt(&sample_segments());
        assert!(vtt.starts_with("WEBVTT\n\n"));
        assert!(vtt.contains("00:00:00.000 --> 00:00:01.500\nHello there.\n\n"));
        assert!(vtt.contains("01:01:01.250"));
    }

    #[test]
    fn empty_segments_produce_srt_with_only_trailing_content() {
        assert_eq!(segments_to_srt(&[]), "");
        assert_eq!(segments_to_vtt(&[]), "WEBVTT\n\n");
    }
}
