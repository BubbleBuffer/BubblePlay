#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DamageRect {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HevcQpDeltaMap {
    pub columns: u32,
    pub rows: u32,
    pub values: Vec<i8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QpMapRejectReason {
    InvalidGeometry,
    EmptyDamage,
    CoverageAtLeastHalf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HevcQpMapDecision {
    Applied(HevcQpDeltaMap),
    Rejected {
        reason: QpMapRejectReason,
        changed_ctbs: u32,
        total_ctbs: u32,
    },
}

/// Expands a map on the 32 px CTB grid to the 16 px macroblock grid NVENC
/// uses for H.264. Each macroblock takes the value of the CTB that contains
/// it; sizes that are not multiples of 16 or 32 get a partial last row and
/// column on both grids.
pub fn expand_ctb_map_to_macroblocks(
    map: &HevcQpDeltaMap,
    output_width: u32,
    output_height: u32,
) -> HevcQpDeltaMap {
    let columns = output_width.div_ceil(16);
    let rows = output_height.div_ceil(16);
    let mut values = Vec::with_capacity((columns * rows) as usize);
    for row in 0..rows {
        let source_row = (row / 2).min(map.rows.saturating_sub(1));
        for column in 0..columns {
            let source_column = (column / 2).min(map.columns.saturating_sub(1));
            values.push(map.values[(source_row * map.columns + source_column) as usize]);
        }
    }
    HevcQpDeltaMap {
        columns,
        rows,
        values,
    }
}

pub fn build_hevc_qp_delta_map(
    output_width: u32,
    output_height: u32,
    source_width: u32,
    source_height: u32,
    crop: DamageRect,
    damage: &[DamageRect],
) -> Option<HevcQpDeltaMap> {
    match analyze_hevc_qp_delta_map(
        output_width,
        output_height,
        source_width,
        source_height,
        crop,
        damage,
    ) {
        HevcQpMapDecision::Applied(map) => Some(map),
        HevcQpMapDecision::Rejected { .. } => None,
    }
}

pub fn analyze_hevc_qp_delta_map(
    output_width: u32,
    output_height: u32,
    source_width: u32,
    source_height: u32,
    crop: DamageRect,
    damage: &[DamageRect],
) -> HevcQpMapDecision {
    const CTB: u32 = 32;
    if output_width == 0
        || output_height == 0
        || source_width == 0
        || source_height == 0
        || crop.left >= crop.right
        || crop.top >= crop.bottom
        || crop.right > source_width
        || crop.bottom > source_height
    {
        return HevcQpMapDecision::Rejected {
            reason: QpMapRejectReason::InvalidGeometry,
            changed_ctbs: 0,
            total_ctbs: 0,
        };
    }
    let columns = output_width.div_ceil(CTB);
    let rows = output_height.div_ceil(CTB);
    let mut changed = vec![false; (columns * rows) as usize];
    let crop_width = crop.right - crop.left;
    let crop_height = crop.bottom - crop.top;
    for rect in damage {
        let left = rect.left.max(crop.left);
        let top = rect.top.max(crop.top);
        let right = rect.right.min(crop.right);
        let bottom = rect.bottom.min(crop.bottom);
        if left >= right || top >= bottom {
            continue;
        }
        let output_left =
            (u64::from(left - crop.left) * u64::from(output_width) / u64::from(crop_width)) as u32;
        let output_top =
            (u64::from(top - crop.top) * u64::from(output_height) / u64::from(crop_height)) as u32;
        let output_right = (u64::from(right - crop.left) * u64::from(output_width))
            .div_ceil(u64::from(crop_width)) as u32;
        let output_bottom = (u64::from(bottom - crop.top) * u64::from(output_height))
            .div_ceil(u64::from(crop_height)) as u32;
        for row in output_top / CTB..output_bottom.div_ceil(CTB).min(rows) {
            for column in output_left / CTB..output_right.div_ceil(CTB).min(columns) {
                changed[(row * columns + column) as usize] = true;
            }
        }
    }
    let changed_count = changed.iter().filter(|value| **value).count() as u32;
    let total_ctbs = changed.len() as u32;
    if changed_count == 0 {
        return HevcQpMapDecision::Rejected {
            reason: QpMapRejectReason::EmptyDamage,
            changed_ctbs: 0,
            total_ctbs,
        };
    }
    if changed_count * 2 >= total_ctbs {
        return HevcQpMapDecision::Rejected {
            reason: QpMapRejectReason::CoverageAtLeastHalf,
            changed_ctbs: changed_count,
            total_ctbs,
        };
    }
    let focus_delta = if changed_count * 4 <= total_ctbs {
        -3
    } else if changed_count * 5 <= total_ctbs * 2 {
        -2
    } else {
        -1
    };
    let values = changed
        .into_iter()
        .map(|value| if value { focus_delta } else { 1 })
        .collect();
    HevcQpMapDecision::Applied(HevcQpDeltaMap {
        columns,
        rows,
        values,
    })
}

pub fn analyze_hevc_text_edge_qp_delta_map(
    output_width: u32,
    output_height: u32,
    source_width: u32,
    source_height: u32,
    crop: DamageRect,
    damage: &[DamageRect],
    edge_scores: &[u32],
) -> HevcQpMapDecision {
    let base = analyze_hevc_qp_delta_map(
        output_width,
        output_height,
        source_width,
        source_height,
        crop,
        damage,
    );
    let HevcQpMapDecision::Applied(mut map) = base else {
        return base;
    };
    if output_width != source_width
        || output_height != source_height
        || crop
            != (DamageRect {
                left: 0,
                top: 0,
                right: source_width,
                bottom: source_height,
            })
        || edge_scores.len() != map.values.len()
    {
        return HevcQpMapDecision::Rejected {
            reason: QpMapRejectReason::InvalidGeometry,
            changed_ctbs: 0,
            total_ctbs: 0,
        };
    }

    let mut changed = map
        .values
        .iter()
        .enumerate()
        .filter_map(|(index, delta)| (*delta < 0).then_some(index))
        .collect::<Vec<_>>();
    changed.sort_by(|left, right| {
        edge_scores[*right]
            .cmp(&edge_scores[*left])
            .then_with(|| left.cmp(right))
    });
    let priority_count = changed.len().div_ceil(4);
    let mut priority = vec![false; map.values.len()];
    for index in changed.into_iter().take(priority_count) {
        priority[index] = true;
    }
    for (index, delta) in map.values.iter_mut().enumerate() {
        *delta = if *delta < 0 {
            if priority[index] {
                -4
            } else {
                -2
            }
        } else {
            1
        }
        .clamp(-5, 2);
    }
    HevcQpMapDecision::Applied(map)
}

#[cfg(test)]
mod tests {
    use super::{
        analyze_hevc_qp_delta_map, analyze_hevc_text_edge_qp_delta_map, build_hevc_qp_delta_map,
        expand_ctb_map_to_macroblocks, DamageRect, HevcQpDeltaMap, HevcQpMapDecision,
        QpMapRejectReason,
    };

    #[test]
    fn changed_hevc_ctbs_receive_quality_while_unchanged_ctbs_fund_them() {
        let full = DamageRect {
            left: 0,
            top: 0,
            right: 128,
            bottom: 64,
        };
        let changed = DamageRect {
            left: 32,
            top: 0,
            right: 64,
            bottom: 32,
        };

        let map = build_hevc_qp_delta_map(128, 64, 128, 64, full, &[changed])
            .expect("one changed CTB should produce a map");

        assert_eq!((map.columns, map.rows), (4, 2));
        assert_eq!(map.values, vec![1, -3, 1, 1, 1, 1, 1, 1]);
    }

    #[test]
    fn text_edge_roi_prioritizes_the_highest_density_changed_quartile() {
        let full = DamageRect {
            left: 0,
            top: 0,
            right: 128,
            bottom: 64,
        };
        let changed = [
            DamageRect {
                left: 0,
                top: 0,
                right: 32,
                bottom: 32,
            },
            DamageRect {
                left: 32,
                top: 0,
                right: 64,
                bottom: 32,
            },
            DamageRect {
                left: 64,
                top: 0,
                right: 96,
                bottom: 32,
            },
        ];
        let edge_scores = [10, 90, 40, 0, 0, 0, 0, 0];

        let HevcQpMapDecision::Applied(map) =
            analyze_hevc_text_edge_qp_delta_map(128, 64, 128, 64, full, &changed, &edge_scores)
        else {
            panic!("sparse changed CTBs with edge features should produce a map");
        };

        assert_eq!(map.values, vec![-2, -4, -2, 1, 1, 1, 1, 1]);
    }

    #[test]
    fn damage_is_clipped_to_the_viewport_and_scaled_to_encoder_coordinates() {
        let crop = DamageRect {
            left: 32,
            top: 0,
            right: 96,
            bottom: 64,
        };
        let changed = DamageRect {
            left: 64,
            top: 0,
            right: 96,
            bottom: 32,
        };

        let map = build_hevc_qp_delta_map(64, 64, 128, 64, crop, &[changed])
            .expect("visible viewport damage should produce a map");

        assert_eq!((map.columns, map.rows), (2, 2));
        assert_eq!(map.values, vec![1, -3, 1, 1]);
    }

    #[test]
    fn near_full_frame_damage_keeps_normal_rate_control_authoritative() {
        let full = DamageRect {
            left: 0,
            top: 0,
            right: 128,
            bottom: 64,
        };

        assert_eq!(
            build_hevc_qp_delta_map(128, 64, 128, 64, full, &[full]),
            None
        );
        assert_eq!(
            analyze_hevc_qp_delta_map(128, 64, 128, 64, full, &[full]),
            HevcQpMapDecision::Rejected {
                reason: QpMapRejectReason::CoverageAtLeastHalf,
                changed_ctbs: 8,
                total_ctbs: 8,
            }
        );
    }

    #[test]
    fn empty_visible_damage_is_distinct_from_invalid_geometry() {
        let full = DamageRect {
            left: 0,
            top: 0,
            right: 128,
            bottom: 64,
        };
        let outside = DamageRect {
            left: 256,
            top: 256,
            right: 320,
            bottom: 320,
        };
        assert_eq!(
            analyze_hevc_qp_delta_map(128, 64, 128, 64, full, &[outside]),
            HevcQpMapDecision::Rejected {
                reason: QpMapRejectReason::EmptyDamage,
                changed_ctbs: 0,
                total_ctbs: 8,
            }
        );
        assert_eq!(
            analyze_hevc_qp_delta_map(
                128,
                64,
                128,
                64,
                DamageRect {
                    left: 64,
                    top: 0,
                    right: 32,
                    bottom: 64,
                },
                &[]
            ),
            HevcQpMapDecision::Rejected {
                reason: QpMapRejectReason::InvalidGeometry,
                changed_ctbs: 0,
                total_ctbs: 0,
            }
        );
    }

    #[test]
    fn ctb_map_expands_to_the_macroblock_grid() {
        for (width, height, ctb_cols, ctb_rows, mb_cols, mb_rows) in [
            (1920u32, 1080u32, 60u32, 34u32, 120u32, 68u32),
            (2560, 1440, 80, 45, 160, 90),
            (1366, 770, 43, 25, 86, 49),
        ] {
            assert_eq!(width.div_ceil(32), ctb_cols);
            assert_eq!(height.div_ceil(32), ctb_rows);
            let values = (0..ctb_cols * ctb_rows)
                .map(|i| (i % 5) as i8 - 2)
                .collect();
            let map = HevcQpDeltaMap {
                columns: ctb_cols,
                rows: ctb_rows,
                values,
            };
            let expanded = expand_ctb_map_to_macroblocks(&map, width, height);
            assert_eq!((expanded.columns, expanded.rows), (mb_cols, mb_rows));
            assert_eq!(expanded.values.len(), (mb_cols * mb_rows) as usize);
            for row in 0..mb_rows {
                for column in 0..mb_cols {
                    assert_eq!(
                        expanded.values[(row * mb_cols + column) as usize],
                        map.values[((row / 2) * ctb_cols + column / 2) as usize]
                    );
                }
            }
        }
    }
}
