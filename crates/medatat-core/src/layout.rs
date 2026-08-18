//! Column layout arithmetic (R12). Pure — no gpui types, so it unit-tests headlessly.

/// Degrades a section's declared column count at narrow widths.
///
/// A 3-column section on a narrow window becomes 1 column rather than overflowing. The
/// tiers are deliberately coarse: form layout that reflows continuously is disorienting
/// when you are tabbing through it.
pub fn effective_columns(declared: u8, width_px: f32) -> u8 {
    let declared = declared.clamp(1, 3);
    if width_px < 720.0 {
        1
    } else if width_px < 1080.0 {
        declared.min(2)
    } else {
        declared
    }
}

/// A field may never span more columns than its section has.
pub fn clamp_col_span(col_span: u8, columns: u8) -> u8 {
    col_span.clamp(1, columns.clamp(1, 3))
}

/// Validates a placement at write time. The UI clamps defensively, but the API rejects.
pub fn col_span_is_valid(col_span: u8, columns: u8) -> bool {
    (1..=3).contains(&columns) && (1..=columns).contains(&col_span)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r12_narrow_collapses_to_one_column() {
        for declared in 1..=3 {
            assert_eq!(effective_columns(declared, 640.0), 1);
        }
    }

    #[test]
    fn r12_medium_caps_at_two() {
        assert_eq!(effective_columns(1, 900.0), 1);
        assert_eq!(effective_columns(2, 900.0), 2);
        assert_eq!(effective_columns(3, 900.0), 2);
    }

    #[test]
    fn r12_wide_honours_declared() {
        assert_eq!(effective_columns(1, 1600.0), 1);
        assert_eq!(effective_columns(2, 1600.0), 2);
        assert_eq!(effective_columns(3, 1600.0), 3);
    }

    #[test]
    fn r12_out_of_range_declared_is_clamped() {
        assert_eq!(effective_columns(0, 1600.0), 1);
        assert_eq!(effective_columns(99, 1600.0), 3);
    }

    #[test]
    fn r12_never_returns_out_of_range() {
        for declared in 0..=5u8 {
            for w in [0.0, 719.0, 720.0, 1079.0, 1080.0, 4000.0] {
                let c = effective_columns(declared, w);
                assert!((1..=3).contains(&c), "declared={declared} w={w} -> {c}");
            }
        }
    }

    #[test]
    fn r12_span_clamping() {
        assert_eq!(clamp_col_span(3, 2), 2);
        assert_eq!(clamp_col_span(1, 3), 1);
        assert_eq!(clamp_col_span(0, 3), 1);
        assert_eq!(clamp_col_span(9, 3), 3);
    }

    #[test]
    fn r12_span_validation_rejects_overflow() {
        assert!(col_span_is_valid(1, 1));
        assert!(col_span_is_valid(2, 3));
        assert!(!col_span_is_valid(3, 2));
        assert!(!col_span_is_valid(0, 2));
        assert!(!col_span_is_valid(1, 4));
    }
}
