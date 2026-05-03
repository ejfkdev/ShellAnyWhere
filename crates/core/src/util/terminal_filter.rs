//! Terminal escape sequence filtering utilities shared by agent and client.
//! These are pure functions with no dependencies beyond std.

/// Check if a CPR (Cursor Position Report) response.
/// CPR response: ESC [ <row> ; <col> R (e.g., ESC[10;20R)
/// Must have at least one semicolon and digits on both sides.
pub fn is_cpr_response(params: &[u8]) -> bool {
    let semicolon_pos = match params.iter().position(|&b| b == b';') {
        Some(p) => p,
        None => return false,
    };
    let before = &params[..semicolon_pos];
    let after = &params[semicolon_pos + 1..];
    !before.is_empty()
        && before.iter().all(|b| b.is_ascii_digit())
        && !after.is_empty()
        && after.iter().all(|b| b.is_ascii_digit())
}

/// Check if an OSC sequence (starting after ESC ]) is a query.
/// OSC queries have the form: Ps ; ? ... (the data starts with a number,
/// then a semicolon, then a question mark).
pub fn is_osc_query(data: &[u8], start: usize) -> bool {
    let mut i = start;
    let len = data.len();
    while i < len && data[i].is_ascii_digit() {
        i += 1;
    }
    if i >= len || data[i] != b';' {
        return false;
    }
    i += 1;
    i < len && data[i] == b'?'
}

/// Skip past an OSC sequence from the current position (after ESC ]).
/// Returns the index after the sequence terminator.
pub fn skip_osc_sequence(data: &[u8], start: usize) -> usize {
    let mut i = start;
    let len = data.len();
    while i < len {
        if data[i] == 0x07 {
            return i + 1;
        }
        if i + 1 < len && data[i] == 0x1b && data[i + 1] == b'\\' {
            return i + 2;
        }
        i += 1;
    }
    i
}

/// Skip past a DCS sequence from the current position (after ESC P).
/// Returns the index after the sequence terminator (ST = ESC \ or BEL).
pub fn skip_dcs_sequence(data: &[u8], start: usize) -> usize {
    let mut i = start;
    let len = data.len();
    while i < len {
        if data[i] == 0x07 {
            return i + 1;
        }
        if i + 1 < len && data[i] == 0x1b && data[i + 1] == b'\\' {
            return i + 2;
        }
        i += 1;
    }
    i
}

enum FinalAction {
    Strip,
    Keep,
}

/// Classify a CSI sequence for the final defense filter.
fn classify_final_defense_csi(data: &[u8], start: usize) -> (FinalAction, usize) {
    let mut i = start;
    let len = data.len();

    let param_start = i;
    while i < len && (data[i] >= 0x30 && data[i] <= 0x3F) {
        i += 1;
    }
    let params = &data[param_start..i];

    let intermediate_start = i;
    while i < len && (data[i] >= 0x20 && data[i] <= 0x2F) {
        i += 1;
    }
    let has_intermediate = i > intermediate_start;

    if i >= len {
        return (FinalAction::Strip, i);
    }

    let final_byte = data[i];
    i += 1;

    match final_byte {
        b'c' => (FinalAction::Strip, i),
        b'n' => (FinalAction::Strip, i),
        b'l' => {
            if params == b"?1004" {
                (FinalAction::Strip, i)
            } else {
                (FinalAction::Keep, i)
            }
        }
        b'q' => {
            if has_intermediate {
                (FinalAction::Keep, i)
            } else {
                (FinalAction::Strip, i)
            }
        }
        b'R' => {
            if is_cpr_response(params) {
                (FinalAction::Strip, i)
            } else {
                (FinalAction::Keep, i)
            }
        }
        _ => (FinalAction::Keep, i),
    }
}

/// Final defense filter for terminal data.
///
/// Strips terminal query/response sequences that would cause problems
/// if sent to the remote terminal:
/// - Strips ALL CSI query/response sequences (DA, DSR, CPR, XTVERSION, etc.)
/// - Strips ALL DCS sequences (both queries and responses)
/// - Strips ALL OSC query sequences (contains ? after ;)
/// - Strips ALL OSC response sequences with hex data (DCS-like patterns)
///
/// Display-safe sequences (colors, cursor movement, mode settings, etc.) are preserved.
pub fn final_defense_filter(data: &[u8]) -> Vec<u8> {
    if !data.contains(&0x1b) {
        return data.to_vec();
    }
    let mut result = Vec::with_capacity(data.len());
    let mut i = 0;
    let len = data.len();

    while i < len {
        if data[i] == 0x1b {
            if i + 1 >= len {
                break;
            }
            match data[i + 1] {
                b'[' => {
                    let (action, new_i) = classify_final_defense_csi(data, i + 2);
                    match action {
                        FinalAction::Strip => {
                            i = new_i;
                            continue;
                        }
                        FinalAction::Keep => {
                            result.push(data[i]);
                            i += 1;
                            continue;
                        }
                    }
                }
                b'P' => {
                    i += 2;
                    i = skip_dcs_sequence(data, i);
                    continue;
                }
                b']' => {
                    // Strip ALL OSC sequences — they are metadata (title,
                    // color palette, clipboard, etc.) that wterm cannot
                    // handle and whose text payload could leak as visible
                    // content if the ESC byte is lost in transit.
                    i += 2;
                    i = skip_osc_sequence(data, i);
                    continue;
                }
                _ => {
                    result.push(data[i]);
                    i += 1;
                    continue;
                }
            }
        }
        result.push(data[i]);
        i += 1;
    }

    result
}

/// Owned version of `final_defense_filter`: takes ownership of the input Vec,
/// returns it directly if no filtering is needed (fast path: no ESC bytes).
pub fn final_defense_filter_owned(data: Vec<u8>) -> Vec<u8> {
    if !data.contains(&0x1b) {
        return data;
    }
    final_defense_filter(&data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_cpr_response() {
        assert!(is_cpr_response(b"10;20"));
        assert!(is_cpr_response(b"1;1"));
        assert!(!is_cpr_response(b""));
        assert!(!is_cpr_response(b"10"));
        assert!(!is_cpr_response(b";20"));
        assert!(!is_cpr_response(b"abc;def"));
    }

    #[test]
    fn test_is_osc_query() {
        assert!(is_osc_query(b"0;?123", 0));
        assert!(!is_osc_query(b"0;title", 0));
        assert!(!is_osc_query(b"abc", 0));
    }

    #[test]
    fn test_skip_osc_sequence() {
        let data = b"hello\x07world";
        assert_eq!(skip_osc_sequence(data, 0), 6); // after BEL
        let data2 = b"hello\x1b\\world";
        assert_eq!(skip_osc_sequence(data2, 0), 7); // after ST
    }

    #[test]
    fn test_skip_dcs_sequence() {
        let data = b"abc\x07def";
        assert_eq!(skip_dcs_sequence(data, 0), 4);
    }

    #[test]
    fn test_final_defense_filter_no_esc() {
        assert_eq!(final_defense_filter(b"hello world"), b"hello world");
    }

    #[test]
    fn test_final_defense_filter_owned_no_esc() {
        let data = vec![b'h', b'i'];
        let result = final_defense_filter_owned(data.clone());
        assert_eq!(result, data);
    }

    #[test]
    fn test_final_defense_filter_strips_da() {
        // CSI c (DA request) should be stripped
        let data = b"\x1b[c";
        assert!(final_defense_filter(data).is_empty());
    }

    #[test]
    fn test_final_defense_filter_strips_dsr() {
        // CSI n (DSR) should be stripped
        let data = b"\x1b[6n";
        assert!(final_defense_filter(data).is_empty());
    }

    #[test]
    fn test_final_defense_filter_strips_dcs() {
        let data = b"\x1bP+q1234\x1b\\";
        assert!(final_defense_filter(data).is_empty());
    }

    #[test]
    fn test_final_defense_filter_keeps_colors() {
        // CSI m (SGR — colors) should be kept
        let data = b"\x1b[31m";
        assert_eq!(final_defense_filter(data), data);
    }

    #[test]
    fn test_final_defense_filter_strips_osc_query() {
        let data = b"\x1b]0;?123\x07";
        assert!(final_defense_filter(data).is_empty());
    }

    #[test]
    fn test_final_defense_filter_strips_osc_title() {
        let data = b"\x1b]0;my title\x07";
        assert!(final_defense_filter(data).is_empty());
    }

    #[test]
    fn test_final_defense_filter_strips_osc4_color() {
        let data = b"\x1b]4;0;#1e1e1e\x07";
        assert!(final_defense_filter(data).is_empty());
    }

    #[test]
    fn test_final_defense_filter_strips_cpr() {
        let data = b"\x1b[10;20R";
        assert!(final_defense_filter(data).is_empty());
    }

    #[test]
    fn test_final_defense_filter_strips_focus_disable() {
        let data = b"\x1b[?1004l";
        assert!(final_defense_filter(data).is_empty());
    }

    #[test]
    fn test_final_defense_filter_keeps_focus_enable() {
        let data = b"\x1b[?1004h";
        assert_eq!(final_defense_filter(data), data);
    }

    #[test]
    fn test_final_defense_filter_keeps_cursor_style() {
        // CSI Ps SP q (DECSCUSR) has intermediate byte — keep
        let data = b"\x1b[1 q";
        assert_eq!(final_defense_filter(data), data);
    }

    #[test]
    fn test_final_defense_filter_strips_xtversion() {
        // CSI > Ps q without intermediate — strip
        let data = b"\x1b[>0q";
        assert!(final_defense_filter(data).is_empty());
    }

    #[test]
    fn test_final_defense_filter_mixed() {
        let data = b"hello\x1b[31m\x1b[6nworld";
        let expected = b"hello\x1b[31mworld";
        assert_eq!(final_defense_filter(data), expected);
    }
}
