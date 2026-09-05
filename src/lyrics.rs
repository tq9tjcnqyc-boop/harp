use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordTiming {
    pub at: Duration,

    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricLine {
    pub at: Option<Duration>,
    pub text: String,
    pub words: Vec<WordTiming>,
}

#[derive(Debug, Clone, Default)]
pub struct Lyrics {
    pub lines: Vec<LyricLine>,
    pub synced: bool,
}

impl Lyrics {
    pub fn parse(input: &str) -> Self {
        let mut lines = Vec::new();
        let mut found_timestamp = false;

        for raw in input.lines() {
            let raw = raw.trim_end_matches('\r');

            let (timestamps, text) = parse_prefixes(raw);
            if timestamps.is_empty() {
                if !raw.trim().is_empty() {
                    lines.push(LyricLine {
                        at: None,
                        text: raw.trim().to_owned(),
                        words: Vec::new(),
                    });
                }
            } else {
                found_timestamp = true;

                let (plain_text, words) = parse_word_timings(text);
                for at in timestamps {
                    lines.push(LyricLine {
                        at: Some(at),
                        text: plain_text.trim().to_owned(),
                        words: words.clone(),
                    });
                }
            }
        }

        if found_timestamp {
            lines.retain(|line| line.at.is_some());
            lines.sort_by_key(|line| line.at);
        }

        Self {
            lines,
            synced: found_timestamp,
        }
    }

    pub fn active_index(&self, position: Duration) -> Option<usize> {
        if !self.synced {
            return None;
        }

        self.lines
            .partition_point(|line| line.at.is_some_and(|at| at <= position))
            .checked_sub(1)
    }

    pub fn active_word(&self, line_index: usize, position: Duration) -> Option<usize> {
        let words = &self.lines.get(line_index)?.words;
        if words.is_empty() {
            return None;
        }
        words
            .partition_point(|word| word.at <= position)
            .checked_sub(1)
    }
}

fn parse_prefixes(mut input: &str) -> (Vec<Duration>, &str) {
    let mut timestamps = Vec::new();
    while let Some(rest) = input.strip_prefix('[') {
        let Some(end) = rest.find(']') else {
            break;
        };

        let token = &rest[..end];

        let Some(timestamp) = parse_timestamp(token) else {
            break;
        };
        timestamps.push(timestamp);

        input = &rest[end + 1..];
    }
    (timestamps, input)
}

fn parse_word_timings(input: &str) -> (String, Vec<WordTiming>) {
    let mut plain = String::new();
    let mut words: Vec<WordTiming> = Vec::new();
    let len = input.len();

    let mut pos = 0; 
    let mut chunk_start = 0; 

    while pos < len {
        let Some(tag_open) = input[pos..].find('<').map(|i| pos + i) else {
            if chunk_start < len {
                plain.push_str(&input[chunk_start..]);
            }
            break;
        };

        if chunk_start < tag_open {
            plain.push_str(&input[chunk_start..tag_open]);
        }

        let after_open = &input[tag_open + 1..];
        let Some(rel_close) = after_open.find('>') else {
            pos = tag_open + 1;
            continue;
        };
        let tag_close = tag_open + 1 + rel_close;
        let token = &input[tag_open + 1..tag_close];

        match parse_timestamp(token) {
            None => {
                pos = tag_open + 1;
            }
            Some(at) => {
                let after_tag = &input[tag_close + 1..];
                let word_end =
                    after_tag.find('<').map(|i| tag_close + 1 + i).unwrap_or(len);
                let word_text = &input[tag_close + 1..word_end];
                if !word_text.is_empty() {
                    words.push(WordTiming {
                        at,
                        text: word_text.to_owned(),
                    });
                }
                plain.push_str(word_text);

                pos = word_end;
                chunk_start = word_end;
            }
        }
    }

    (plain, words)
}

fn parse_timestamp(token: &str) -> Option<Duration> {
    let (minutes, seconds) = token.split_once(':')?;
    let minutes: u64 = minutes.parse().ok()?;
    let seconds: f64 = seconds.parse().ok()?;
    if !(0.0..60.0).contains(&seconds) {
        return None;
    }
    Some(Duration::from_secs_f64(minutes as f64 * 60.0 + seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_synced_and_multi_timestamp_lines() {
        let lyrics = Lyrics::parse("[00:01.50][00:03.00]你好\n[00:05]世界");
        assert!(lyrics.synced);
        assert_eq!(lyrics.lines.len(), 3);
        assert_eq!(lyrics.lines[0].text, "你好");

        assert_eq!(lyrics.active_index(Duration::from_millis(3200)), Some(1));
    }

    #[test]
    fn keeps_plain_lyrics() {
        let lyrics = Lyrics::parse("第一行\n\n第二行");
        assert!(!lyrics.synced);
        assert_eq!(lyrics.lines.len(), 2);
    }

    #[test]
    fn rejects_metadata_as_timestamps() {
        let lyrics = Lyrics::parse("[ar:歌手]\n正文");
        assert!(!lyrics.synced);
        assert_eq!(lyrics.lines[0].text, "[ar:歌手]");
    }

    #[test]
    fn parses_qrc_word_timings() {
        let lyrics =
            Lyrics::parse("[00:00.00]<00:00.00>晴<00:00.16>天<00:00.32> <00:00.48>-");
        assert!(lyrics.synced);
        assert_eq!(lyrics.lines.len(), 1);
        let line = &lyrics.lines[0];

        assert_eq!(line.text, "晴天 -");

        assert_eq!(line.words.len(), 4);
        assert_eq!(line.words[0].text, "晴");
        assert_eq!(line.words[0].at, Duration::from_millis(0));
        assert_eq!(line.words[1].text, "天");
        assert_eq!(line.words[1].at, Duration::from_millis(160));
        assert_eq!(line.words[2].text, " ");
        assert_eq!(line.words[2].at, Duration::from_millis(320));
        assert_eq!(line.words[3].text, "-");
        assert_eq!(line.words[3].at, Duration::from_millis(480));

        assert_eq!(lyrics.active_word(0, Duration::from_millis(200)), Some(1));

        assert_eq!(lyrics.active_word(0, Duration::from_millis(360)), Some(2));
    }

    #[test]
    fn qrc_without_tags_falls_back_to_plain() {
        let lyrics = Lyrics::parse("[00:01.00]你好世界");
        let line = &lyrics.lines[0];
        assert!(line.words.is_empty());
        assert_eq!(line.text, "你好世界");
    }

    #[test]
    fn standard_lrc_has_no_words() {
        let lyrics = Lyrics::parse("[00:01.50][00:03.00]你好");
        assert!(lyrics.lines.iter().all(|l| l.words.is_empty()));

        assert_eq!(lyrics.active_word(0, Duration::from_millis(2000)), None);
    }
}
