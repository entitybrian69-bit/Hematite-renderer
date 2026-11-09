//! The comparable state block.
//!
//! The frontend keeps one of these as *what the game has asked for* and the backend keeps
//! one as *what is actually applied*. A frame's worth of state changes is therefore a diff
//! between two `StateBlock`s rather than a sequence of "apply this field" calls.
//!
//! Two things are deliberate:
//!
//! * Every field is `Copy` and fixed-size, so comparison is memcmp-shaped and the struct can
//!   be moved into a command without allocating.
//! * Bits that GL exposes as an *enable* are folded into the field they modify
//!   (`depth_test`, `depth_write`, ...) rather than kept as a `GLenum` set. That keeps the
//!   diff meaningful: toggling `GL_DEPTH_TEST` changes exactly one bit.

/// Depth/scissor/stencil comparison function.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
#[repr(u8)]
pub enum CmpFunc {
    /// `GL_NEVER`.
    Never,
    /// `GL_LESS`.
    Less,
    /// `GL_EQUAL`.
    Equal,
    /// `GL_LEQUAL`.
    LessEqual,
    /// `GL_GREATER`.
    Greater,
    /// `GL_NOTEQUAL`.
    NotEqual,
    /// `GL_GEQUAL`.
    GreaterEqual,
    /// `GL_ALWAYS`.
    #[default]
    Always,
}

impl CmpFunc {
    /// The GL enum value, for round-tripping through `glGetInteger`.
    pub const fn to_gl(self) -> u32 {
        match self {
            CmpFunc::Never => 0x0200,
            CmpFunc::Less => 0x0201,
            CmpFunc::Equal => 0x0202,
            CmpFunc::LessEqual => 0x0203,
            CmpFunc::Greater => 0x0204,
            CmpFunc::NotEqual => 0x0205,
            CmpFunc::GreaterEqual => 0x0206,
            CmpFunc::Always => 0x0207,
        }
    }

    /// Parses a GL comparison enum.
    pub const fn from_gl(v: u32) -> Option<Self> {
        Some(match v {
            0x0200 => CmpFunc::Never,
            0x0201 => CmpFunc::Less,
            0x0202 => CmpFunc::Equal,
            0x0203 => CmpFunc::LessEqual,
            0x0204 => CmpFunc::Greater,
            0x0205 => CmpFunc::NotEqual,
            0x0206 => CmpFunc::GreaterEqual,
            0x0207 => CmpFunc::Always,
            _ => return None,
        })
    }
}

/// Which face a cull/stencil operation applies to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
#[repr(u8)]
pub enum Face {
    /// `GL_FRONT`.
    Front,
    /// `GL_BACK`.
    #[default]
    Back,
    /// `GL_FRONT_AND_BACK`.
    FrontAndBack,
}

/// Blend factor.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
#[repr(u8)]
pub enum BlendFactor {
    /// `GL_ZERO`.
    Zero,
    /// `GL_ONE`.
    #[default]
    One,
    /// `GL_SRC_COLOR`.
    SrcColor,
    /// `GL_ONE_MINUS_SRC_COLOR`.
    OneMinusSrcColor,
    /// `GL_SRC_ALPHA`.
    SrcAlpha,
    /// `GL_ONE_MINUS_SRC_ALPHA`.
    OneMinusSrcAlpha,
    /// `GL_DST_ALPHA`.
    DstAlpha,
    /// `GL_ONE_MINUS_DST_ALPHA`.
    OneMinusDstAlpha,
    /// `GL_DST_COLOR`.
    DstColor,
    /// `GL_ONE_MINUS_DST_COLOR`.
    OneMinusDstColor,
    /// `GL_SRC_ALPHA_SATURATE`.
    SrcAlphaSaturate,
    /// `GL_CONSTANT_COLOR`.
    ConstantColor,
    /// `GL_ONE_MINUS_CONSTANT_COLOR`.
    OneMinusConstantColor,
    /// `GL_CONSTANT_ALPHA`.
    ConstantAlpha,
    /// `GL_ONE_MINUS_CONSTANT_ALPHA`.
    OneMinusConstantAlpha,
}

/// Blend operation (`glBlendEquation`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
#[repr(u8)]
pub enum BlendOp {
    /// `GL_FUNC_ADD`.
    #[default]
    Add,
    /// `GL_FUNC_SUBTRACT`.
    Subtract,
    /// `GL_FUNC_REVERSE_SUBTRACT`.
    ReverseSubtract,
    /// `GL_MIN`.
    Min,
    /// `GL_MAX`.
    Max,
}

/// A GL viewport / scissor rectangle.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Viewport {
    /// Left edge, in pixels.
    pub x: i32,
    /// Top edge, in pixels.
    pub y: i32,
    /// Width, in pixels.
    pub width: u32,
    /// Height, in pixels.
    pub height: u32,
}

impl Viewport {
    /// True when the rectangle covers no pixels.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Current implemented subset of graphical pipeline state, excluding resource bindings.
/// Separate blend/stencil details and other GL state are not represented yet.
///
/// Field order is chosen so the whole struct compares with `PartialEq` in as few words as
/// possible; it is not semantically meaningful.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct StateBlock {
    // --- rasterisation ---
    /// Cull mode; `None` disables face culling.
    pub cull: Option<Face>,
    /// Front-face winding is counter-clockwise.
    pub front_ccw: bool,
    /// Polygon mode is line/point (emulated by the backend where unsupported).
    pub polygon_line: bool,
    /// Line width in pixels, scaled by 1/8 to fit a `u8` (GL clamps the useful range).
    pub line_width_eighths: u8,

    // --- depth ---
    /// Depth test enabled.
    pub depth_test: bool,
    /// Depth comparison function.
    pub depth_func: CmpFunc,
    /// Depth writes enabled.
    pub depth_write: bool,
    /// Clamp fragments past the far plane instead of clipping (`glEnable(GL_DEPTH_CLAMP)`).
    pub depth_clamp: bool,

    // --- stencil ---
    /// Stencil test enabled.
    pub stencil_test: bool,
    /// Stencil comparison function for front faces.
    pub stencil_front_func: CmpFunc,
    /// Stencil comparison function for back faces.
    pub stencil_back_func: CmpFunc,

    // --- blending ---
    /// Blending enabled.
    pub blend: bool,
    /// Blend equation, front/back share it unless separate blending is on.
    pub blend_op: BlendOp,
    /// Source factor.
    pub blend_src: BlendFactor,
    /// Destination factor.
    pub blend_dst: BlendFactor,

    // --- writes ---
    /// Colour write mask: bit 0 red, 1 green, 2 blue, 3 alpha.
    pub color_mask: u8,
    /// Alpha-to-coverage enabled.
    pub alpha_to_coverage: bool,

    // --- geometry ranges ---
    /// Viewport rectangle.
    pub viewport: Viewport,
    /// Scissor rectangle.
    pub scissor: Viewport,
    /// Scissor test enabled.
    pub scissor_test: bool,
}

impl StateBlock {
    /// True when this block differs from the other block.
    ///
    /// Same result as comparing with `!=`, but named for what call sites mean, and a place
    /// to put a faster comparison later without touching every caller.
    #[inline]
    pub fn differs_from(&self, other: &Self) -> bool {
        self != other
    }

    /// Colour mask as four booleans, in RGBA order.
    #[inline]
    pub const fn color_mask_rgba(&self) -> [bool; 4] {
        [
            self.color_mask & 1 != 0,
            self.color_mask & 2 != 0,
            self.color_mask & 4 != 0,
            self.color_mask & 8 != 0,
        ]
    }

    /// Sets the colour mask from four booleans, in RGBA order.
    #[inline]
    pub fn set_color_mask_rgba(&mut self, m: [bool; 4]) {
        let mut bits = 0u8;
        for (i, on) in m.iter().enumerate() {
            if *on {
                bits |= 1 << i;
            }
        }
        self.color_mask = bits;
    }
}

impl Default for StateBlock {
    /// GL's initial state, as it is when a context is created.
    fn default() -> Self {
        Self {
            cull: None,
            front_ccw: true,
            polygon_line: false,
            line_width_eighths: 8,
            depth_test: false,
            depth_func: CmpFunc::Less,
            depth_write: true,
            depth_clamp: false,
            stencil_test: false,
            stencil_front_func: CmpFunc::Always,
            stencil_back_func: CmpFunc::Always,
            blend: false,
            blend_op: BlendOp::Add,
            blend_src: BlendFactor::One,
            blend_dst: BlendFactor::Zero,
            color_mask: 0b1111,
            alpha_to_coverage: false,
            viewport: Viewport::default(),
            scissor: Viewport::default(),
            scissor_test: false,
        }
    }
}

/// Bit flags for the parts of `StateBlock` that changed.
///
/// The backend uses this to emit only the dynamic-state commands that matter, which is the
/// difference between one `vkCmdSetViewport` per frame and one per frame where the viewport
/// actually moved.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StateDirty(pub u16);

impl StateDirty {
    /// Nothing changed.
    pub const NONE: Self = Self(0);
    /// Rasterisation fields changed.
    pub const RASTER: Self = Self(1 << 0);
    /// Depth fields changed.
    pub const DEPTH: Self = Self(1 << 1);
    /// Stencil fields changed.
    pub const STENCIL: Self = Self(1 << 2);
    /// Blend fields changed.
    pub const BLEND: Self = Self(1 << 3);
    /// Write-mask fields changed.
    pub const WRITES: Self = Self(1 << 4);
    /// Viewport changed.
    pub const VIEWPORT: Self = Self(1 << 5);
    /// Scissor changed.
    pub const SCISSOR: Self = Self(1 << 6);

    /// Whether every flag in `other` is set here.
    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Union of two flag sets.
    #[inline]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether nothing is dirty.
    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// Computes which groups of `StateBlock` differ between `a` and `b`.
///
/// This is the function the frontend calls once per recorded bind to decide whether the
/// backend needs a new state command at all.
pub fn diff(a: &StateBlock, b: &StateBlock) -> StateDirty {
    let mut d = StateDirty::NONE;
    if a.cull != b.cull
        || a.front_ccw != b.front_ccw
        || a.polygon_line != b.polygon_line
        || a.line_width_eighths != b.line_width_eighths
    {
        d = d.union(StateDirty::RASTER);
    }
    if a.depth_test != b.depth_test
        || a.depth_func != b.depth_func
        || a.depth_write != b.depth_write
        || a.depth_clamp != b.depth_clamp
    {
        d = d.union(StateDirty::DEPTH);
    }
    if a.stencil_test != b.stencil_test
        || a.stencil_front_func != b.stencil_front_func
        || a.stencil_back_func != b.stencil_back_func
    {
        d = d.union(StateDirty::STENCIL);
    }
    if a.blend != b.blend
        || a.blend_op != b.blend_op
        || a.blend_src != b.blend_src
        || a.blend_dst != b.blend_dst
    {
        d = d.union(StateDirty::BLEND);
    }
    if a.color_mask != b.color_mask || a.alpha_to_coverage != b.alpha_to_coverage {
        d = d.union(StateDirty::WRITES);
    }
    if a.viewport != b.viewport {
        d = d.union(StateDirty::VIEWPORT);
    }
    if a.scissor != b.scissor || a.scissor_test != b.scissor_test {
        d = d.union(StateDirty::SCISSOR);
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_gl_initial_state() {
        let s = StateBlock::default();
        // GL starts with depth test disabled, depth writes on, no culling, blending off.
        assert!(!s.depth_test);
        assert!(s.depth_write);
        assert_eq!(s.cull, None);
        assert!(!s.blend);
        assert_eq!(s.depth_func, CmpFunc::Less);
        assert_eq!(s.color_mask_rgba(), [true, true, true, true]);
    }

    #[test]
    fn colour_mask_roundtrips() {
        let mut s = StateBlock::default();
        s.set_color_mask_rgba([true, false, true, false]);
        assert_eq!(s.color_mask, 0b0101);
        assert_eq!(s.color_mask_rgba(), [true, false, true, false]);
    }

    #[test]
    fn cmpfunc_roundtrips_through_gl_enums() {
        for f in [
            CmpFunc::Never,
            CmpFunc::Less,
            CmpFunc::Equal,
            CmpFunc::LessEqual,
            CmpFunc::Greater,
            CmpFunc::NotEqual,
            CmpFunc::GreaterEqual,
            CmpFunc::Always,
        ] {
            assert_eq!(CmpFunc::from_gl(f.to_gl()), Some(f));
        }
        assert_eq!(CmpFunc::from_gl(0x1234), None);
    }

    #[test]
    fn diff_of_identical_blocks_is_empty() {
        let s = StateBlock::default();
        assert!(diff(&s, &s).is_empty());
    }

    #[test]
    fn diff_reports_only_the_group_that_changed() {
        let a = StateBlock::default();
        let mut b = a;
        b.depth_test = true;
        let d = diff(&a, &b);
        assert!(d.contains(StateDirty::DEPTH));
        assert!(!d.contains(StateDirty::BLEND));
        assert!(!d.contains(StateDirty::VIEWPORT));
    }

    #[test]
    fn diff_tracks_viewport_and_scissor_separately() {
        let a = StateBlock::default();
        let mut b = a;
        b.viewport = Viewport { x: 0, y: 0, width: 800, height: 600 };
        let d = diff(&a, &b);
        assert!(d.contains(StateDirty::VIEWPORT));
        assert!(!d.contains(StateDirty::SCISSOR));
    }

    #[test]
    fn dirty_flags_compose() {
        let d = StateDirty::DEPTH.union(StateDirty::BLEND);
        assert!(d.contains(StateDirty::DEPTH));
        assert!(d.contains(StateDirty::BLEND));
        assert!(!d.contains(StateDirty::VIEWPORT));
        assert!(StateDirty::NONE.is_empty());
    }

    #[test]
    fn empty_viewport_is_detected() {
        assert!(Viewport::default().is_empty());
        assert!(!Viewport { x: 0, y: 0, width: 1, height: 1 }.is_empty());
    }
}
