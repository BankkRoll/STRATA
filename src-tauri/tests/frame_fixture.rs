//! The app's frame encoder reproduces the committed UI fixture byte for
//! byte, so the UI decoder tests (`ui/src/lib/layout/*.test.ts`) cover the
//! frames the app actually sends.

use strata_app_lib::frame::{FrameMeta, ViewKind, write_frame};
use strata_layout::{
    CushionParams, LayoutSource, NodeId, SyntheticSpec, TreemapConfig, VecTree, ViewTransform,
    layout_treemap,
};

/// Same pseudo-random color keys as `export_fixtures.rs`.
struct PackedKeys<'a>(&'a VecTree);

fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl LayoutSource for PackedKeys<'_> {
    fn size(&self, id: NodeId) -> u64 {
        self.0.size(id)
    }
    fn children(&self, id: NodeId, out: &mut Vec<(NodeId, u64)>) {
        self.0.children(id, out);
    }
    fn is_dir(&self, id: NodeId) -> bool {
        self.0.is_dir(id)
    }
    fn color_key(&self, id: NodeId) -> u32 {
        let h = mix(u64::from(id) + 0x9E37_79B9);
        let category = self.0.color_key(id) & 0xF;
        let safety = (h % 5) as u32;
        let age = ((h >> 8) % 21) as u32;
        let file_type = if self.0.is_dir(id) {
            0
        } else {
            ((h >> 16) % 40) as u32 + 1
        };
        let app = ((h >> 24) % 12) as u32;
        let recent = u32::from((h >> 40).is_multiple_of(50));
        category | safety << 4 | age << 7 | file_type << 12 | app << 20 | recent << 30
    }
}

#[test]
fn treemap_frame_matches_ui_fixture() {
    let t = VecTree::synthetic(&SyntheticSpec {
        nodes: 4_000,
        mean_fanout: 16,
        dir_ratio: 0.2,
        max_depth: 12,
        seed: 42,
    });
    let src = PackedKeys(&t);
    let (w, h, dpr) = (960.0f32, 640.0f32, 1.25f32);
    let mut cfg = TreemapConfig::new(w, h, dpr);
    cfg.cushion = Some(CushionParams::default());
    let l = layout_treemap(&src, VecTree::ROOT, &cfg);
    let frame = write_frame(
        &FrameMeta {
            view: ViewKind::Treemap,
            seq: 1,
            root: VecTree::ROOT,
            root_bytes: t.size(VecTree::ROOT),
            width: w,
            height: h,
            dpr,
            transform: ViewTransform::IDENTITY,
            center: (0.0, 0.0),
            ring_width: 0.0,
        },
        [
            l.rects().as_bytes(),
            l.aggregates().as_bytes(),
            l.labels().as_bytes(),
            l.cushions().map_or(&[][..], |c| c.as_bytes()),
            &[],
        ],
    );
    let fixture = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../ui/src/lib/layout/__fixtures__/treemap.frame.bin"
    ))
    .expect("UI fixture present");
    assert_eq!(frame.len(), fixture.len());
    assert!(frame == fixture, "frame differs from the UI fixture");
}
