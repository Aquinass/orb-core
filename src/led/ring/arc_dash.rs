use super::{render_lines, Animation};
use crate::{
    led::{AnimationState, RingFrame},
    mcu::main::Rgb,
};
use std::{any::Any, f64::consts::PI, ops::Range};

/// Maximum number of arcs.
pub const MAX_ARC_COUNT: usize = 4;

const WAVE_SPEED: f64 = PI * 2.0 / 3.0; // 3 seconds per blink
const WAVE_MIN: f64 = 0.1;
const GAP_SPEED: f64 = PI / 0.175; // 0.175 seconds to grow the gaps
const FLASH_ON_TIME: f64 = 0.1;
const FLASH_COLOR: Rgb = Rgb(255, 255, 255);

/// Dashed arc.
pub struct ArcDash {
    color: Rgb,
    arc_count: usize,
    flash_phase: Option<f64>,
    wave_phase: Option<f64>,
    pub(crate) shape: Shape,
}

#[derive(Clone)]
pub struct Shape {
    arc_count: usize,
    gap_phase: f64,
}

impl ArcDash {
    /// Creates a new [`ArcDash`].
    ///
    /// # Panics
    ///
    /// If `arc_count` exceeds [`MAX_ARC_COUNT`].
    #[must_use]
    pub fn new(color: Rgb, arc_count: usize) -> Self {
        assert!(arc_count <= MAX_ARC_COUNT);
        Self {
            color,
            arc_count,
            flash_phase: None,
            wave_phase: None,
            shape: Shape { arc_count, gap_phase: 0.0 },
        }
    }

    /// Runs the wave animation.
    pub fn wave(&mut self, color: Rgb) {
        self.shape = Shape { arc_count: self.arc_count, gap_phase: PI };
        self.flash_phase = Some(0.0);
        self.color = color;
    }
}

impl Animation for ArcDash {
    type Frame = RingFrame;

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn animate(&mut self, frame: &mut RingFrame, dt: f64, idle: bool) -> AnimationState {
        let mut current_color = self.color;
        if let Some(phase) = &mut self.wave_phase {
            current_color *= (1.0 - phase.cos()) / 2.0 * (1.0 - WAVE_MIN) + WAVE_MIN;
            *phase = (*phase + dt * WAVE_SPEED) % (PI * 2.0);
        } else if let Some(phase) = &mut self.flash_phase {
            current_color = FLASH_COLOR;
            *phase += dt;
            if *phase >= FLASH_ON_TIME {
                self.wave_phase = Some(0.0);
            }
        } else {
            self.shape.gap_phase = (self.shape.gap_phase + dt * GAP_SPEED).min(PI);
        };
        if !idle {
            self.shape.render(frame, current_color);
        }
        AnimationState::Running
    }
}

impl Shape {
    #[allow(clippy::cast_precision_loss)]
    pub fn render(&self, frame: &mut RingFrame, color: Rgb) {
        let mut ranges: [Range<f64>; MAX_ARC_COUNT] = [0.0..0.0, 0.0..0.0, 0.0..0.0, 0.0..0.0];
        for (i, range) in ranges.iter_mut().enumerate().take(self.arc_count) {
            let start = PI * 2.0 / self.arc_count as f64 * i as f64
                + (1.0 - self.gap_phase.cos()) * PI / (self.arc_count as f64 * 2.5);
            let end = PI * 2.0 / self.arc_count as f64 * (i + 1) as f64
                - (1.0 - self.gap_phase.cos()) * PI / (self.arc_count as f64 * 2.5);
            *range = start..end;
        }
        render_lines(frame, Rgb::OFF, color, &ranges);
    }
}
