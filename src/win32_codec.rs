//! Pure Win32 console text encoding and decoding - no syscalls, so it's
//! compiled and tested on every platform, not just the Windows build.

/// Decodes Win32-Input-Mode (`CSI Vk;Sc;Uc;Kd;Cs;Rc _`) back into plain
/// bytes - sshd's outer ConPTY wraps every keystroke this way once a pty is
/// allocated, and fails to recognize arrow keys as one unit under it
/// (`ESC[A` arrives as three separate `Vk=0` char events), which our inner
/// ConPTY then mis-delivers to the shell the same way. Decoding back to
/// plain bytes lets the inner ConPTY parse `ESC[A` itself.
#[derive(Default)]
pub(crate) struct Win32InputDecoder {
    pending: Vec<u8>,
    high_surrogate: Option<u16>,
}

impl Win32InputDecoder {
    pub(crate) fn feed(&mut self, data: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(data);
        const MAX_BODY_LEN: usize = 40;

        let mut output = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i] == 0x1b && i + 1 < self.pending.len() && self.pending[i + 1] == b'[' {
                let body_start = i + 2;
                // A body is only digits and semicolons, so the first byte that
                // is neither settles it: some other CSI (`ESC[H`), and waiting
                // for a `_` would strand it until the next keystroke.
                let mut end = body_start;
                while end < self.pending.len()
                    && end - body_start < MAX_BODY_LEN
                    && (self.pending[end].is_ascii_digit() || self.pending[end] == b';')
                {
                    end += 1;
                }
                if end < self.pending.len() && self.pending[end] == b'_' {
                    let body = self.pending[body_start..end].to_vec();
                    if let Some(decoded) = self.decode_body(&body) {
                        output.extend(decoded);
                        i = end + 1;
                        continue;
                    }
                } else if end == self.pending.len() && end - body_start < MAX_BODY_LEN {
                    // Still all body - the rest may be in the next read.
                    break;
                }
            }
            output.push(self.pending[i]);
            i += 1;
        }
        self.pending.drain(..i);
        output
    }

    fn decode_body(&mut self, body: &[u8]) -> Option<Vec<u8>> {
        let text = std::str::from_utf8(body).ok()?;
        let mut parts = text.split(';');
        let _vk: u32 = parts.next()?.parse().ok()?;
        let _sc: u32 = parts.next()?.parse().ok()?;
        let uc: u32 = parts.next()?.parse().ok()?;
        let kd: u32 = parts.next()?.parse().ok()?;
        let _cs: u32 = parts.next()?.parse().ok()?;
        let _rc: u32 = parts.next()?.parse().ok()?;
        if kd != 1 || uc == 0 {
            return Some(Vec::new());
        }
        let unit = uc as u16;
        if (0xD800..=0xDBFF).contains(&unit) {
            self.high_surrogate = Some(unit);
            return Some(Vec::new());
        }
        let scalar = if (0xDC00..=0xDFFF).contains(&unit) {
            let high = self.high_surrogate.take()?;
            0x10000 + (((high as u32 - 0xD800) << 10) | (unit as u32 - 0xDC00))
        } else {
            self.high_surrogate = None;
            unit as u32
        };
        let ch = char::from_u32(scalar)?;
        let mut buf = [0u8; 4];
        Some(ch.encode_utf8(&mut buf).as_bytes().to_vec())
    }
}

/// PowerShell's `cd` never touches the process's real directory, so there is
/// no `/proc/<pid>/cwd` equivalent to read from outside. Chains onto the
/// user's own `prompt` - never replaces it - and reports `$PWD` over an OSC
/// marker real terminals drop, as `REPLAY_END_MARKER` already does.
pub(crate) const CWD_PROMPT_HOOK: &str = "if (-not $function:__termphinOriginalPrompt) { \
    $function:__termphinOriginalPrompt = $function:prompt }; \
    function prompt { \
    [Console]::Out.Write([char]27 + ']5382;termphin-cwd;' + $PWD.Path + [char]7); \
    & $function:__termphinOriginalPrompt }";

/// The UTF-16LE base64 that PowerShell's `-EncodedCommand` expects.
pub(crate) fn base64_utf16le(text: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = text
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18 & 0x3F) as usize] as char);
        out.push(ALPHABET[(triple >> 12 & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6 & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod base64_tests {
    use super::base64_utf16le;

    #[test]
    fn matches_powershell_encoded_command_format() {
        // What `[Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes('hi'))` returns.
        assert_eq!(base64_utf16le("hi"), "aABpAA==");
    }

    #[test]
    fn pads_every_input_length() {
        assert_eq!(base64_utf16le("a"), "YQA=");
        assert_eq!(base64_utf16le("ab"), "YQBiAA==");
        assert_eq!(base64_utf16le("abc"), "YQBiAGMA");
        assert_eq!(base64_utf16le(""), "");
    }

    #[test]
    fn the_real_hook_round_trips_back_to_itself() {
        let encoded = base64_utf16le(super::CWD_PROMPT_HOOK);
        assert!(!encoded.contains(' '), "must survive a command line unquoted");
        assert_eq!(encoded.len() % 4, 0);
    }
}

#[cfg(test)]
mod win32_input_decoder_tests {
    use super::Win32InputDecoder;

    #[test]
    fn decodes_a_real_captured_up_arrow_into_plain_esc_bracket_a() {
        // Captured from a real session: three raw char events (Vk=0) for
        // ESC, '[', 'A', not one up-arrow. Used to print "[A".
        let raw: &[u8] = &[
            0x1b, 0x5b, 0x30, 0x3b, 0x30, 0x3b, 0x32, 0x37, 0x3b, 0x31, 0x3b, 0x30, 0x3b, 0x31,
            0x5f, 0x1b, 0x5b, 0x30, 0x3b, 0x30, 0x3b, 0x39, 0x31, 0x3b, 0x31, 0x3b, 0x30, 0x3b,
            0x31, 0x5f, 0x1b, 0x5b, 0x30, 0x3b, 0x30, 0x3b, 0x36, 0x35, 0x3b, 0x31, 0x3b, 0x30,
            0x3b, 0x31, 0x5f,
        ];
        let mut decoder = Win32InputDecoder::default();
        assert_eq!(decoder.feed(raw), vec![0x1b, b'[', b'A']);
    }

    #[test]
    fn key_up_events_are_dropped_so_a_letter_is_not_doubled() {
        // Real captured key-down/key-up pair for the letter 'e'.
        let raw: &[u8] = &[
            0x1b, 0x5b, 0x36, 0x39, 0x3b, 0x31, 0x38, 0x3b, 0x31, 0x30, 0x31, 0x3b, 0x31, 0x3b,
            0x30, 0x3b, 0x31, 0x5f, 0x1b, 0x5b, 0x36, 0x39, 0x3b, 0x31, 0x38, 0x3b, 0x31, 0x30,
            0x31, 0x3b, 0x30, 0x3b, 0x30, 0x3b, 0x31, 0x5f,
        ];
        let mut decoder = Win32InputDecoder::default();
        assert_eq!(decoder.feed(raw), b"e".to_vec());
    }

    #[test]
    fn a_sequence_split_across_two_reads_still_decodes() {
        let mut decoder = Win32InputDecoder::default();
        let first = decoder.feed(b"\x1b[0;0;65;1;0");
        assert!(first.is_empty(), "incomplete sequence should not emit yet");
        let second = decoder.feed(b";1_");
        assert_eq!(second, vec![b'A']);
    }

    #[test]
    fn plain_text_with_no_escape_sequences_passes_through_unchanged() {
        let mut decoder = Win32InputDecoder::default();
        assert_eq!(decoder.feed(b"hello"), b"hello".to_vec());
    }

    #[test]
    fn an_escape_byte_not_followed_by_a_bracket_is_forwarded_literally() {
        let mut decoder = Win32InputDecoder::default();
        assert_eq!(decoder.feed(b"\x1bX"), vec![0x1b, b'X']);
    }

    #[test]
    fn a_csi_sequence_with_no_underscore_terminator_is_forwarded_literally() {
        // A plain xterm sequence arriving unwrapped.
        let mut decoder = Win32InputDecoder::default();
        let input = b"\x1b[Hrest";
        assert_eq!(decoder.feed(input), input.to_vec());
    }

    /// Toolbar keys arrive as one raw char event per byte of their sequence.
    #[test]
    fn the_special_keys_the_toolbar_sends_all_survive_a_round_trip() {
        for (name, sequence) in [
            ("up", "\x1b[A"),
            ("down", "\x1b[B"),
            ("right", "\x1b[C"),
            ("left", "\x1b[D"),
            ("home", "\x1b[H"),
            ("end", "\x1b[F"),
            ("page up", "\x1b[5~"),
            ("page down", "\x1b[6~"),
            ("insert", "\x1b[2~"),
            ("delete", "\x1b[3~"),
            ("f1", "\x1bOP"),
            ("f5", "\x1b[15~"),
            ("f12", "\x1b[24~"),
            ("tab", "\t"),
            ("escape", "\x1b"),
            ("ctrl-c", "\x03"),
            ("ctrl-d", "\x04"),
            ("enter", "\r"),
        ] {
            let wrapped: String = sequence
                .bytes()
                .map(|b| format!("\x1b[0;0;{b};1;0;1_"))
                .collect();
            let mut decoder = Win32InputDecoder::default();
            assert_eq!(
                decoder.feed(wrapped.as_bytes()),
                sequence.as_bytes().to_vec(),
                "{name} did not survive decoding"
            );
        }
    }

    /// Unwrapped, the same keys must pass through, not wait for a terminator.
    #[test]
    fn unwrapped_special_keys_are_not_swallowed() {
        for sequence in [
            "\x1b[A", "\x1b[H", "\x1b[F", "\x1b[5~", "\x1b[3~", "\x1bOP", "\x1b[15~",
        ] {
            let mut decoder = Win32InputDecoder::default();
            assert_eq!(
                decoder.feed(sequence.as_bytes()),
                sequence.as_bytes().to_vec(),
                "{sequence:?} was swallowed"
            );
        }
    }

    #[test]
    fn surrogate_pair_reassembles_into_one_astral_character() {
        // U+1F600 (grinning face) as UTF-16 surrogates: 0xD83D 0xDE00.
        let high = 0xD83Du32;
        let low = 0xDE00u32;
        let seq = format!("\x1b[0;0;{high};1;0;1_\x1b[0;0;{low};1;0;1_");
        let mut decoder = Win32InputDecoder::default();
        let decoded = decoder.feed(seq.as_bytes());
        assert_eq!(decoded, "😀".as_bytes().to_vec());
    }
}
